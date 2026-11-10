"""Run one matching pass from the command line.

Reads JSON on stdin:
    {"fee_bps": 10, "now": 1794327773,
     "orders": [{"id": "0", "from_region": "CA", "to_region": "IN",
                 "remaining": 1000000000, "created_at": 1794327700,
                 "deadline": 1794327900, "full": false}, ...]}

Writes JSON on stdout: {"fills": [{"x_id": "0", "y_id": "1", "amount": 600000000}]}

The engine proposes; the contract re-checks every fill before moving funds.
Run with `python3 -I engine/match_cli.py` from the repo root.
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from netting import Mode, NettingEngine, Order  # noqa: E402


def main() -> None:
    req = json.load(sys.stdin)
    now = int(req["now"])
    engine = NettingEngine(fee_bps=int(req.get("fee_bps", 10)))
    regions = set()
    for o in req["orders"]:
        created = int(o["created_at"])
        engine.submit(
            Order(
                id=str(o["id"]),
                owner="",
                recipient="",
                from_region=o["from_region"],
                to_region=o["to_region"],
                amount=int(o["remaining"]),
                created_at=created,
                max_wait=max(0, int(o["deadline"]) - created),
                mode=Mode.FULL if o.get("full") else Mode.PARTIAL,
            )
        )
        regions.add(tuple(sorted((o["from_region"], o["to_region"]))))
    fills = []
    for a, b in sorted(regions):
        for f in engine.match(a, b, now):
            fills.append({"x_id": f.x_order, "y_id": f.y_order, "amount": f.amount})
    json.dump({"fills": fills}, sys.stdout)


if __name__ == "__main__":
    main()
