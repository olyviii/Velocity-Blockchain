"""Off-chain netting engine.

Matches USDC orders flowing in opposite directions between two regions
(a -> b against b -> a). A matched amount is paid to both recipients from the
other side's deposit, so it can be paid out from local liquidity and never has
to cross the border. Whatever cannot be matched by the deadline is sent across
to the recipient or refunded, as the sender chose.

The engine is deliberately pure: no network, no clock, no database. The caller
supplies `now`. That keeps it deterministic and easy to test; the Soroban
contract re-checks every match independently before moving funds.
"""
from __future__ import annotations

from .models import BPS, Fallback, FallbackAction, Fill, Mode, Order, Status


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

    def fee(self, amount: int) -> int:
        """Per-side fee on a matched amount, rounded up (same as the contract)."""
        return _ceil_div(amount * self.fee_bps, BPS)

    # ---- matching ---------------------------------------------------------

    def match(self, region_a: str, region_b: str, now: int) -> list[Fill]:
        """Match all eligible a -> b orders against b -> a orders. Mutates order
        state and returns the fills."""
        xs = self._eligible(region_a, region_b, now)
        ys = self._eligible(region_b, region_a, now)

        fills, rem = self._allocate(xs, ys)
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
            fills, rem = self._allocate(xs, ys)

        for f in fills:
            for oid in (f.x_order, f.y_order):
                o = self._orders[oid]
                o.remaining -= f.amount
                if o.remaining == 0:
                    o.status = Status.FILLED
        return fills

    def _eligible(self, src: str, dst: str, now: int) -> list[Order]:
        orders = [
            o
            for o in self._orders.values()
            if o.status is Status.OPEN
            and o.remaining > 0
            and o.from_region == src
            and o.to_region == dst
            and now <= o.deadline
        ]
        orders.sort(key=lambda o: (o.created_at, o.id))  # FIFO, deterministic
        return orders

    def _allocate(
        self, xs: list[Order], ys: list[Order]
    ) -> tuple[list[Fill], dict[str, int]]:
        rem = {o.id: o.remaining for o in (*xs, *ys)}
        fills: list[Fill] = []
        i = j = 0
        while i < len(xs) and j < len(ys):
            x, y = xs[i], ys[j]
            m = min(rem[x.id], rem[y.id])
            fee = self.fee(m)
            if m - fee <= 0:
                # too small to pay anything after the fee; skip the smaller side
                if rem[x.id] <= rem[y.id]:
                    i += 1
                else:
                    j += 1
                continue
            rem[x.id] -= m
            rem[y.id] -= m
            fills.append(Fill(x.id, y.id, m, fee))
            if rem[x.id] == 0:
                i += 1
            if rem[y.id] == 0:
                j += 1
        return fills, rem

    # ---- deadline ---------------------------------------------------------

    def settle_fallbacks(self, now: int) -> list[FallbackAction]:
        """Close every open order whose wait has run out and say where its
        unmatched remainder goes. Call this *after* match() for the same `now`,
        so each order gets one last matching pass at its deadline. Orders are
        closed here so a remainder can never be sent or refunded twice."""
        actions: list[FallbackAction] = []
        for o in sorted(self._orders.values(), key=lambda o: (o.created_at, o.id)):
            if o.status is not Status.OPEN or o.remaining == 0 or now < o.deadline:
                continue
            send = o.fallback is Fallback.SEND_REMAINDER
            actions.append(
                FallbackAction(o.id, o.fallback, o.recipient if send else o.owner, o.remaining)
            )
            o.remaining = 0
            o.status = Status.RELEASED if send else Status.CANCELLED
        return actions

    # ---- reporting --------------------------------------------------------

    def estimate_savings(self, fills: list[Fill], cross_border_cost_bps: int) -> dict[str, int]:
        """Rough saving versus sending every matched amount across the border.

        `cross_border_cost_bps` is an assumption supplied by the caller: what it
        costs to move USDC into the other country and cash it out there (FX
        spread, off-ramp fee, agent rebalancing). The result is an estimate in
        USDC stroops, not a measured outcome."""
        matched = sum(f.amount for f in fills)
        net_bps = cross_border_cost_bps - self.fee_bps
        return {
            "matched": matched,
            "cross_border_cost_bps_assumed": cross_border_cost_bps,
            "fee_bps": self.fee_bps,
            "net_saving_bps": net_bps,
            "est_saving": matched * 2 * net_bps // BPS,  # both legs stay local
            "kept_local": matched * 2,  # USDC that never had to cross a border
        }
