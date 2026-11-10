import random
import unittest
from decimal import Decimal

from netting import (
    SCALE,
    Fallback,
    Mode,
    NettingEngine,
    Order,
    Status,
)

A, B = "USDC", "EURC"
RATE = 9_200_000  # 0.92 EURC per USDC


def u(x) -> int:
    """Human amount -> stroops."""
    return int(Decimal(str(x)) * SCALE)


def order(oid, sell, buy, amount, min_receive=0, t=0, wait=3600, mode=Mode.PARTIAL,
          fallback=Fallback.ROUTE_DEX):
    return Order(oid, f"owner-{oid}", f"rcpt-{oid}", sell, buy, u(amount), u(min_receive),
                 t, wait, mode, fallback)


class MatchingTests(unittest.TestCase):
    def setUp(self):
        self.e = NettingEngine(fee_bps=10)

    def test_exact_swap_fills_both_orders(self):
        self.e.submit(order("x", A, B, 100, min_receive=91))
        self.e.submit(order("y", B, A, 92, min_receive=99))
        fills = self.e.match(A, B, RATE, now=0)
        self.assertEqual(len(fills), 1)
        f = fills[0]
        self.assertEqual((f.a_amount, f.b_amount), (u(100), u(92)))
        self.assertEqual(f.x_receives, u(92) - u("0.092"))
        self.assertEqual(f.y_receives, u(100) - u("0.1"))
        self.assertEqual(self.e.get("x").status, Status.FILLED)
        self.assertEqual(self.e.get("y").status, Status.FILLED)

    def test_partial_fill_leaves_remainder_open(self):
        self.e.submit(order("x", A, B, 100))
        self.e.submit(order("y", B, A, 46))  # worth 50 USDC at 0.92
        fills = self.e.match(A, B, RATE, 0)
        self.assertEqual(sum(f.a_amount for f in fills), u(50))
        self.assertEqual(self.e.get("x").remaining, u(50))
        self.assertEqual(self.e.get("x").status, Status.OPEN)
        self.assertEqual(self.e.get("y").status, Status.FILLED)

    def test_one_order_filled_by_several_counterparties(self):
        self.e.submit(order("x", A, B, 100))
        self.e.submit(order("y1", B, A, 46, t=0))
        self.e.submit(order("y2", B, A, 46, t=1))
        fills = self.e.match(A, B, RATE, 1)
        self.assertEqual(len(fills), 2)
        self.assertEqual(self.e.get("x").status, Status.FILLED)

    def test_fifo_priority(self):
        self.e.submit(order("late", A, B, 100, t=5))
        self.e.submit(order("early", A, B, 100, t=1))
        self.e.submit(order("y", B, A, 92, t=2))
        self.e.match(A, B, RATE, 5)
        self.assertEqual(self.e.get("early").status, Status.FILLED)
        self.assertEqual(self.e.get("late").remaining, u(100))

    def test_no_counterparty_no_fills(self):
        self.e.submit(order("x", A, B, 100))
        self.assertEqual(self.e.match(A, B, RATE, 0), [])

    def test_limit_price_is_respected(self):
        # wants 95 EURC for 100 USDC; the 0.92 rate only gives ~91.9
        self.e.submit(order("x", A, B, 100, min_receive=95))
        self.e.submit(order("y", B, A, 92))
        self.assertEqual(self.e.match(A, B, RATE, 0), [])
        self.assertEqual(self.e.get("x").remaining, u(100))

    def test_both_limits_checked_on_the_other_side_too(self):
        self.e.submit(order("x", A, B, 100))
        self.e.submit(order("y", B, A, 92, min_receive=100))  # wants a full 100 A, fee makes it 99.9
        self.assertEqual(self.e.match(A, B, RATE, 0), [])

    def test_expired_orders_are_not_matched(self):
        self.e.submit(order("x", A, B, 100, wait=10))
        self.e.submit(order("y", B, A, 92, wait=10))
        self.assertEqual(self.e.match(A, B, RATE, now=11), [])


class FullModeTests(unittest.TestCase):
    def setUp(self):
        self.e = NettingEngine(fee_bps=10)

    def test_full_waits_until_whole_order_can_be_filled(self):
        self.e.submit(order("x", A, B, 100, mode=Mode.FULL))
        self.e.submit(order("y1", B, A, 46))
        self.assertEqual(self.e.match(A, B, RATE, 0), [])
        self.assertEqual(self.e.get("x").remaining, u(100))
        self.assertEqual(self.e.get("y1").remaining, u(46))
        self.e.submit(order("y2", B, A, 46, t=1))
        fills = self.e.match(A, B, RATE, 1)
        self.assertEqual(len(fills), 2)
        self.assertEqual(self.e.get("x").status, Status.FILLED)

    def test_full_counterparty_blocks_partial_fill_of_other_side(self):
        self.e.submit(order("x", A, B, 40))
        self.e.submit(order("y", B, A, 92, mode=Mode.FULL))  # needs 100 USDC, only 40 around
        self.assertEqual(self.e.match(A, B, RATE, 0), [])
        self.assertEqual(self.e.get("x").remaining, u(40))

    def test_dropping_a_full_order_frees_liquidity_for_others(self):
        self.e.submit(order("big", A, B, 1000, mode=Mode.FULL, t=0))
        self.e.submit(order("small", A, B, 50, t=1))
        self.e.submit(order("y", B, A, 46, t=2))  # 50 USDC worth
        fills = self.e.match(A, B, RATE, 2)
        self.assertEqual([f.x_order for f in fills], ["small"])
        self.assertEqual(self.e.get("big").remaining, u(1000))


class FallbackTests(unittest.TestCase):
    def setUp(self):
        self.e = NettingEngine(fee_bps=10)

    def test_instant_matches_once_then_falls_back_immediately(self):
        self.e.submit(order("y", B, A, 46, t=5))
        self.e.submit(order("x", A, B, 100, min_receive=90, t=5, mode=Mode.INSTANT))
        self.e.match(A, B, RATE, 5)
        actions = self.e.settle_fallbacks(5)
        self.assertEqual([a.order_id for a in actions], ["x"])
        self.assertEqual(actions[0].amount, u(50))
        self.assertEqual(actions[0].action, Fallback.ROUTE_DEX)
        # half the order was matched, so the DEX leg only needs half the floor
        self.assertEqual(actions[0].min_receive, u(45))

    def test_waiting_orders_are_not_closed_early(self):
        self.e.submit(order("x", A, B, 100, wait=60))
        self.assertEqual(self.e.settle_fallbacks(59), [])
        self.assertEqual(len(self.e.settle_fallbacks(60)), 1)

    def test_refund_fallback_cancels_order(self):
        self.e.submit(order("x", A, B, 100, wait=0, fallback=Fallback.REFUND))
        (act,) = self.e.settle_fallbacks(0)
        self.assertEqual(act.action, Fallback.REFUND)
        self.assertEqual(self.e.get("x").status, Status.CANCELLED)

    def test_remainder_cannot_be_handled_twice(self):
        self.e.submit(order("x", A, B, 100, wait=0))
        self.assertEqual(len(self.e.settle_fallbacks(0)), 1)
        self.assertEqual(self.e.settle_fallbacks(1), [])

    def test_cancel_returns_remaining(self):
        self.e.submit(order("x", A, B, 100))
        self.e.submit(order("y", B, A, 46))
        self.e.match(A, B, RATE, 0)
        self.assertEqual(self.e.cancel("x"), u(50))
        with self.assertRaises(ValueError):
            self.e.cancel("x")


class ValidationTests(unittest.TestCase):
    def test_rejects_bad_orders(self):
        with self.assertRaises(ValueError):
            order("x", A, A, 1)
        with self.assertRaises(ValueError):
            order("x", A, B, 0)

    def test_rejects_duplicate_ids(self):
        e = NettingEngine()
        e.submit(order("x", A, B, 1))
        with self.assertRaises(ValueError):
            e.submit(order("x", A, B, 1))


class SavingsTests(unittest.TestCase):
    def test_estimate_uses_assumed_dex_cost_minus_fee(self):
        e = NettingEngine(fee_bps=10)
        e.submit(order("x", A, B, 100))
        e.submit(order("y", B, A, 92))
        fills = e.match(A, B, RATE, 0)
        s = e.estimate_savings(fills, dex_cost_bps=50)
        self.assertEqual(s["net_saving_bps"], 40)
        self.assertEqual(s["est_saving_b"], u(92) * 2 * 40 // 10_000)


class InvariantTests(unittest.TestCase):
    """Randomised checks that no sequence of orders can create or destroy value."""

    def test_random_books_conserve_value_and_respect_limits(self):
        rng = random.Random(1234)
        for trial in range(200):
            e = NettingEngine(fee_bps=rng.choice([0, 5, 10, 30]))
            orders = []
            for n in range(rng.randint(2, 25)):
                sell, buy = (A, B) if rng.random() < 0.5 else (B, A)
                amt = rng.randint(1, 5_000) * rng.choice([1, 1_000, 10_000_000])
                # sender floor somewhere between very lax and slightly too strict
                ref = RATE if sell == A else SCALE * SCALE // RATE
                floor = amt * ref // SCALE * rng.randint(85, 101) // 100
                o = Order(f"o{n}", "own", "rcp", sell, buy, amt, floor, rng.randint(0, 5),
                          rng.randint(0, 10), rng.choice(list(Mode)))
                orders.append(o)
                e.submit(o)
            fills = e.match(A, B, RATE, now=rng.randint(0, 8))

            sold = {o.id: 0 for o in orders}
            got = {o.id: 0 for o in orders}
            for f in fills:
                # per-fill conservation: what leaves one side equals what the other
                # side receives plus the fee, exactly
                self.assertEqual(f.x_receives + f.fee_b, f.b_amount)
                self.assertEqual(f.y_receives + f.fee_a, f.a_amount)
                self.assertGreater(f.x_receives, 0)
                self.assertGreater(f.y_receives, 0)
                sold[f.x_order] += f.a_amount
                sold[f.y_order] += f.b_amount
                got[f.x_order] += f.x_receives
                got[f.y_order] += f.y_receives
            for o in orders:
                self.assertGreaterEqual(o.remaining, 0)
                self.assertEqual(o.amount - o.remaining, sold[o.id])
                # signed floor holds across the order's fills (pro rata)
                self.assertGreaterEqual(got[o.id] * o.amount, o.min_receive * sold[o.id])
                if o.mode is Mode.FULL and sold[o.id] > 0:
                    self.assertEqual(o.remaining, 0)  # FULL is all-or-nothing


if __name__ == "__main__":
    unittest.main()
