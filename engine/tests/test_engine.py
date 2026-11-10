import random
import unittest
from decimal import Decimal

from netting import SCALE, Fallback, Mode, NettingEngine, Order, Status

CA, IN = "CA", "IN"


def u(x) -> int:
    """Human amount -> stroops."""
    return int(Decimal(str(x)) * SCALE)


def order(oid, src, dst, amount, t=0, wait=3600, mode=Mode.PARTIAL,
          fallback=Fallback.SEND_REMAINDER):
    return Order(oid, f"owner-{oid}", f"rcpt-{oid}", src, dst, u(amount), t, wait, mode, fallback)


class MatchingTests(unittest.TestCase):
    def setUp(self):
        self.e = NettingEngine(fee_bps=10)

    def test_equal_opposite_orders_net_out(self):
        self.e.submit(order("x", CA, IN, 100))
        self.e.submit(order("y", IN, CA, 100))
        (f,) = self.e.match(CA, IN, now=0)
        self.assertEqual(f.amount, u(100))
        self.assertEqual(f.x_receives, u(100) - u("0.1"))
        self.assertEqual(self.e.get("x").status, Status.FILLED)
        self.assertEqual(self.e.get("y").status, Status.FILLED)

    def test_partial_match_leaves_remainder_open(self):
        self.e.submit(order("x", CA, IN, 100))
        self.e.submit(order("y", IN, CA, 60))
        self.e.match(CA, IN, 0)
        self.assertEqual(self.e.get("x").remaining, u(40))
        self.assertEqual(self.e.get("x").status, Status.OPEN)
        self.assertEqual(self.e.get("y").status, Status.FILLED)

    def test_argument_order_of_regions_does_not_matter(self):
        self.e.submit(order("x", CA, IN, 100))
        self.e.submit(order("y", IN, CA, 100))
        self.assertEqual(len(self.e.match(IN, CA, 0)), 1)

    def test_one_order_filled_by_several_counterparties(self):
        self.e.submit(order("x", CA, IN, 100))
        self.e.submit(order("y1", IN, CA, 50, t=0))
        self.e.submit(order("y2", IN, CA, 50, t=1))
        self.assertEqual(len(self.e.match(CA, IN, 1)), 2)
        self.assertEqual(self.e.get("x").status, Status.FILLED)

    def test_fifo_priority(self):
        self.e.submit(order("late", CA, IN, 100, t=5))
        self.e.submit(order("early", CA, IN, 100, t=1))
        self.e.submit(order("y", IN, CA, 100, t=2))
        self.e.match(CA, IN, 5)
        self.assertEqual(self.e.get("early").status, Status.FILLED)
        self.assertEqual(self.e.get("late").remaining, u(100))

    def test_same_direction_and_other_corridors_are_not_matched(self):
        self.e.submit(order("x1", CA, IN, 100))
        self.e.submit(order("x2", CA, IN, 100))
        self.e.submit(order("m", "MX", CA, 100))
        self.assertEqual(self.e.match(CA, IN, 0), [])

    def test_expired_orders_are_not_matched(self):
        self.e.submit(order("x", CA, IN, 100, wait=10))
        self.e.submit(order("y", IN, CA, 100, wait=10))
        self.assertEqual(self.e.match(CA, IN, now=11), [])

    def test_dust_below_fee_is_not_matched(self):
        e = NettingEngine(fee_bps=10)
        e.submit(Order("x", "o", "r", CA, IN, 1, 0, 10))  # 1 stroop: fee rounds up to 1
        e.submit(Order("y", "o", "r", IN, CA, 1, 0, 10))
        self.assertEqual(e.match(CA, IN, 0), [])


class FullModeTests(unittest.TestCase):
    def setUp(self):
        self.e = NettingEngine(fee_bps=10)

    def test_full_waits_until_whole_order_can_be_filled(self):
        self.e.submit(order("x", CA, IN, 100, mode=Mode.FULL))
        self.e.submit(order("y1", IN, CA, 50))
        self.assertEqual(self.e.match(CA, IN, 0), [])
        self.e.submit(order("y2", IN, CA, 50, t=1))
        self.assertEqual(len(self.e.match(CA, IN, 1)), 2)
        self.assertEqual(self.e.get("x").status, Status.FILLED)

    def test_dropping_a_full_order_frees_liquidity_for_others(self):
        self.e.submit(order("big", CA, IN, 1000, mode=Mode.FULL, t=0))
        self.e.submit(order("small", CA, IN, 50, t=1))
        self.e.submit(order("y", IN, CA, 50, t=2))
        fills = self.e.match(CA, IN, 2)
        self.assertEqual([f.x_order for f in fills], ["small"])
        self.assertEqual(self.e.get("big").remaining, u(1000))


class DeadlineTests(unittest.TestCase):
    def setUp(self):
        self.e = NettingEngine(fee_bps=10)

    def test_instant_matches_once_then_releases_immediately(self):
        self.e.submit(order("y", IN, CA, 30, t=5))
        self.e.submit(order("x", CA, IN, 100, t=5, mode=Mode.INSTANT))
        self.e.match(CA, IN, 5)
        actions = self.e.settle_fallbacks(5)
        self.assertEqual([a.order_id for a in actions], ["x"])
        self.assertEqual((actions[0].amount, actions[0].to), (u(70), "rcpt-x"))
        self.assertEqual(self.e.get("x").status, Status.RELEASED)

    def test_waiting_orders_are_not_closed_early(self):
        self.e.submit(order("x", CA, IN, 100, wait=60))
        self.assertEqual(self.e.settle_fallbacks(59), [])
        self.assertEqual(len(self.e.settle_fallbacks(60)), 1)

    def test_refund_goes_back_to_owner(self):
        self.e.submit(order("x", CA, IN, 100, wait=0, fallback=Fallback.REFUND))
        (act,) = self.e.settle_fallbacks(0)
        self.assertEqual((act.action, act.to), (Fallback.REFUND, "owner-x"))
        self.assertEqual(self.e.get("x").status, Status.CANCELLED)

    def test_remainder_cannot_be_handled_twice(self):
        self.e.submit(order("x", CA, IN, 100, wait=0))
        self.assertEqual(len(self.e.settle_fallbacks(0)), 1)
        self.assertEqual(self.e.settle_fallbacks(1), [])

    def test_cancel_returns_remaining(self):
        self.e.submit(order("x", CA, IN, 100))
        self.e.submit(order("y", IN, CA, 50))
        self.e.match(CA, IN, 0)
        self.assertEqual(self.e.cancel("x"), u(50))
        with self.assertRaises(ValueError):
            self.e.cancel("x")


class ValidationTests(unittest.TestCase):
    def test_rejects_bad_orders(self):
        with self.assertRaises(ValueError):
            order("x", CA, CA, 1)
        with self.assertRaises(ValueError):
            order("x", CA, IN, 0)

    def test_rejects_duplicate_ids(self):
        e = NettingEngine()
        e.submit(order("x", CA, IN, 1))
        with self.assertRaises(ValueError):
            e.submit(order("x", CA, IN, 1))


class SavingsTests(unittest.TestCase):
    def test_estimate_uses_assumed_cross_border_cost_minus_fee(self):
        e = NettingEngine(fee_bps=10)
        e.submit(order("x", CA, IN, 100))
        e.submit(order("y", IN, CA, 100))
        s = e.estimate_savings(e.match(CA, IN, 0), cross_border_cost_bps=150)
        self.assertEqual(s["net_saving_bps"], 140)
        self.assertEqual(s["est_saving"], u(100) * 2 * 140 // 10_000)
        self.assertEqual(s["kept_local"], u(200))


class InvariantTests(unittest.TestCase):
    """Randomised checks that no sequence of orders can create or destroy value."""

    def test_random_books_conserve_value(self):
        rng = random.Random(1234)
        for _ in range(200):
            e = NettingEngine(fee_bps=rng.choice([0, 5, 10, 30]))
            orders = []
            for n in range(rng.randint(2, 25)):
                src, dst = (CA, IN) if rng.random() < 0.5 else (IN, CA)
                amt = rng.randint(1, 5_000) * rng.choice([1, 1_000, SCALE])
                o = Order(f"o{n}", "own", "rcp", src, dst, amt, rng.randint(0, 5),
                          rng.randint(0, 10), rng.choice(list(Mode)))
                orders.append(o)
                e.submit(o)
            now = rng.randint(0, 8)
            fills = e.match(CA, IN, now)
            matched = {o.id: 0 for o in orders}
            for f in fills:
                self.assertEqual(f.x_receives + f.y_receives + 2 * f.fee, 2 * f.amount)
                self.assertGreater(f.x_receives, 0)
                self.assertEqual(f.fee, e.fee(f.amount))
                x, y = e.get(f.x_order), e.get(f.y_order)
                self.assertEqual((x.from_region, x.to_region), (y.to_region, y.from_region))
                matched[f.x_order] += f.amount
                matched[f.y_order] += f.amount
            for o in orders:
                self.assertGreaterEqual(o.remaining, 0)
                self.assertEqual(o.amount - o.remaining, matched[o.id])
                if o.mode is Mode.FULL and matched[o.id] > 0:
                    self.assertEqual(o.remaining, 0)
            released = sum(a.amount for a in e.settle_fallbacks(now))
            # every stroop is either matched, released/refunded, or still waiting
            still_open = sum(o.remaining for o in orders if o.status is Status.OPEN)
            total = sum(o.amount for o in orders)
            self.assertEqual(total, sum(matched.values()) + released + still_open)


if __name__ == "__main__":
    unittest.main()
