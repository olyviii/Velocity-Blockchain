"""Off-chain netting engine.

Matches opposite-direction orders (A->B against B->A) at a single reference rate
(taken from an oracle by the caller), so the matched portion never touches a
DEX and never pays its spread or slippage. Whatever cannot be matched is handed
back as a fallback action: route through a Stellar path payment, or refund.

The engine is deliberately pure: no network, no clock, no database. The caller
supplies `rate` and `now`. That keeps it deterministic and easy to test; the
Soroban contract re-checks every fill independently before moving funds.
"""
from __future__ import annotations

from .models import (
    BPS,
    SCALE,
    Fallback,
    Fill,
    FallbackAction,
    Mode,
    Order,
    Status,
)


def _ceil_div(a: int, b: int) -> int:
    return -(-a // b)


class NettingEngine:
    def __init__(self, fee_bps: int = 10) -> None:
        if not 0 <= fee_bps < BPS:
            raise ValueError("fee_bps must be in [0, 10000)")
        self.fee_bps = fee_bps
        self._orders: dict[str, Order] = {}

    # ---- order book -------------------------------------------------------

    def submit(self, order: Order) -> None:
        if order.id in self._orders:
            raise ValueError(f"duplicate order id {order.id}")
        self._orders[order.id] = order

    def get(self, order_id: str) -> Order:
        return self._orders[order_id]

    def cancel(self, order_id: str) -> int:
        """Cancel an open order; returns the stroops to refund to the sender."""
        order = self._orders[order_id]
        if order.status is not Status.OPEN:
            raise ValueError(f"order {order_id} is {order.status.value}")
        refund = order.remaining
        order.remaining = 0
        order.status = Status.CANCELLED
        return refund

    # ---- matching ---------------------------------------------------------

    def match(self, asset_a: str, asset_b: str, rate: int, now: int) -> list[Fill]:
        """Match all eligible A->B orders against B->A orders at `rate` (B per A,
        scaled by SCALE). Mutates order state and returns the fills."""
        if rate <= 0:
            raise ValueError("rate must be positive")
        xs = self._eligible(asset_a, asset_b, now)
        ys = self._eligible(asset_b, asset_a, now)

        fills, rem = self._allocate(xs, ys, rate, asset_a, asset_b)
        # FULL orders are all-or-nothing: drop any that ended up partly filled,
        # then re-allocate without them until the allocation is stable.
        while True:
            partial = {
                o.id for o in (*xs, *ys) if o.mode is Mode.FULL and rem[o.id] > 0
            }
            if not partial:
                break
            xs = [o for o in xs if o.id not in partial]
            ys = [o for o in ys if o.id not in partial]
            fills, rem = self._allocate(xs, ys, rate, asset_a, asset_b)

        for f in fills:
            self._orders[f.x_order].remaining -= f.a_amount
            self._orders[f.y_order].remaining -= f.b_amount
        for f in fills:
            for oid in (f.x_order, f.y_order):
                o = self._orders[oid]
                if o.remaining == 0:
                    o.status = Status.FILLED
        return fills

    def _eligible(self, sell: str, buy: str, now: int) -> list[Order]:
        orders = [
            o
            for o in self._orders.values()
            if o.status is Status.OPEN
            and o.remaining > 0
            and o.sell_asset == sell
            and o.buy_asset == buy
            and now <= o.deadline
        ]
        orders.sort(key=lambda o: (o.created_at, o.id))  # FIFO, deterministic
        return orders

    def _allocate(
        self,
        xs: list[Order],
        ys: list[Order],
        rate: int,
        asset_a: str,
        asset_b: str,
    ) -> tuple[list[Fill], dict[str, int]]:
        rem = {o.id: o.remaining for o in (*xs, *ys)}
        fills: list[Fill] = []
        i = j = 0
        while i < len(xs) and j < len(ys):
            x, y = xs[i], ys[j]
            if rem[x.id] == 0:
                i += 1
                continue
            if rem[y.id] == 0:
                j += 1
                continue

            # Largest A amount that both sides can still take at this rate.
            m_a = min(rem[x.id], rem[y.id] * SCALE // rate)
            b_gross = m_a * rate // SCALE  # floor: never gives Y's side more than it has
            fee_a = _ceil_div(m_a * self.fee_bps, BPS)  # ceil: rounding favours the pool
            fee_b = _ceil_div(b_gross * self.fee_bps, BPS)
            x_recv = b_gross - fee_b
            y_recv = m_a - fee_a

            # Each side's signed floor, pro rata to what it sells in this fill.
            x_ok = x_recv > 0 and x_recv * x.amount >= x.min_receive * m_a
            y_ok = y_recv > 0 and y_recv * y.amount >= y.min_receive * b_gross
            if m_a == 0:
                j += 1  # y's remainder is too small to buy even one stroop of A
                continue
            if not x_ok:
                i += 1  # x cannot be satisfied at this rate; leave it open
                continue
            if not y_ok:
                j += 1
                continue

            rem[x.id] -= m_a
            rem[y.id] -= b_gross
            fills.append(
                Fill(x.id, y.id, asset_a, asset_b, m_a, b_gross, fee_a, fee_b, rate)
            )
        return fills, rem

    # ---- fallback ---------------------------------------------------------

    def settle_fallbacks(self, now: int) -> list[FallbackAction]:
        """Close every open order whose wait has run out and return what to do with
        its unmatched remainder. Call this *after* match() for the same `now`, so
        each order gets one last matching pass at its deadline. Orders are closed
        here so a remainder can never be routed or refunded twice."""
        actions: list[FallbackAction] = []
        for o in sorted(self._orders.values(), key=lambda o: (o.created_at, o.id)):
            if o.status is not Status.OPEN or o.remaining == 0 or now < o.deadline:
                continue
            # Remainder's pro-rata floor, rounded up so the sender is never shorted.
            floor = _ceil_div(o.min_receive * o.remaining, o.amount)
            actions.append(
                FallbackAction(
                    o.id, o.fallback, o.sell_asset, o.buy_asset, o.remaining, floor
                )
            )
            o.remaining = 0
            o.status = (
                Status.CANCELLED if o.fallback is Fallback.REFUND else Status.FILLED
            )
        return actions

    # ---- reporting --------------------------------------------------------

    def estimate_savings(self, fills: list[Fill], dex_cost_bps: int) -> dict[str, int]:
        """Rough saving versus sending both legs through the DEX.

        `dex_cost_bps` is an assumption supplied by the caller (spread plus
        slippage of a quoted path payment at order time). The result is an
        estimate in asset-B stroops, not a measured outcome."""
        matched_b = sum(f.b_amount for f in fills)
        net_bps = dex_cost_bps - self.fee_bps
        return {
            "matched_b": matched_b,
            "dex_cost_bps_assumed": dex_cost_bps,
            "fee_bps": self.fee_bps,
            "net_saving_bps": net_bps,
            "est_saving_b": matched_b * 2 * net_bps // BPS,  # two legs avoid the DEX
        }
