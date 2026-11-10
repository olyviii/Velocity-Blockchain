# Velocity-Blockchain: architecture

Status: contract, netting engine, testnet end-to-end script and a demo UI are
built. Not deployed anywhere but testnet. No real cash-out partners yet.

## The idea

When someone sends money to another country, they can choose to wait a number
of hours. Their USDC is locked in a contract. While it waits, the system looks
for people sending money the other way. For a matched amount:

```
Alice (CA) sends 100 to Bob (IN)      Carol (IN) sends 60 to Dave (CA)

Matched 60:   Alice's deposit pays Dave, Carol's deposit pays Bob, straight away.
              Both of those payouts are local: Alice and Dave are in Canada,
              Carol and Bob in India.
Remainder 40: at the deadline it goes across to Bob (or back to Alice, if she
              chose a refund).
```

Everything is USDC, 1:1. There is no exchange rate, no oracle and no DEX in the
contract.

## Where the saving really is

On Stellar, moving USDC between two accounts costs a fraction of a cent wherever
they are, so the on-chain transfer itself is not the expensive part. The cost
and the liquidity problem sit at the edges: turning local currency into USDC,
and USDC back into local currency, through agents or anchors. A matched amount
can be paid out from money that came in locally, so it skips the currency
conversion and the agent never has to rebalance across the border for it. That
is the "kept local" figure the engine and the UI report.

This only pays off once payouts go through local cash-out agents (anchors).
With recipients just holding USDC in a wallet, matching moves the same amounts
on-chain. The next step is wiring payouts to anchors (SEP-24 / SEP-31); see
"Next" below.

## Components

| Piece | Where | Status |
|---|---|---|
| Escrow contract (Soroban, Rust, SDK 29) | `contracts/escrow` | 16 tests pass |
| Netting engine (Python, pure) | `engine/netting` | 19 tests pass |
| Engine CLI used by the app | `engine/match_cli.py` | built |
| Testnet end-to-end script | `scripts/e2e_testnet.sh` | built |
| Demo app (Node server + browser page) | `app/` | built, testnet only, custodial demo keys |
| Anchor / cash-out integration | | not started |

### Trust split

- **Contract** custodies funds and is the only thing that moves them. It
  re-checks every match: opposite corridors, both orders open and within their
  deadline, amount within both remainders. Payouts only go to addresses the
  senders signed. The fee is fixed at deployment.
- **Settler** (the backend's key) can only choose which orders to pair and how
  much. It cannot redirect funds.
- **Release is permissionless.** Once an order's deadline is reached anyone can
  call `release`, and the remainder goes where the sender said. Funds do not
  depend on the backend staying up.

### Order lifecycle

1. Sender calls `deposit(owner, recipient, from_region, to_region, amount,
   deadline, full, on_expiry)`. The USDC moves into the escrow.
2. While `now <= deadline`, the settler calls `settle_batch` with pairs the
   engine proposed. Each pair pays both recipients `amount - fee` immediately.
3. Once `now >= deadline`, anyone calls `release`: the remainder goes to the
   recipient (`SendRemainder`) or back to the sender (`Refund`).
4. The owner can `cancel` at any time to take back the unmatched remainder.

Statuses: `Open`, `Filled` (fully matched), `Released` (remainder sent),
`Cancelled` (remainder refunded).

## Engine rules

- Integers only, 7 decimals (stroops).
- FIFO by `(created_at, id)`; deterministic.
- Fee is per matched side, rounded up, same formula as the contract.
- FULL orders are all-or-nothing; the allocation is recomputed without any FULL
  order that ends up partly filled, until stable.
- Each order gets one last matching pass at its deadline before it is closed.
- Conservation is exact: every stroop is matched, released, refunded or still
  waiting.

## Testing

- `cd contracts && cargo test` and
  `cd engine && python3 -I -m unittest discover -s tests -t .`: unit tests.
- `scripts/e2e_testnet.sh`: runs both suites, builds the wasm, deploys to
  testnet with fresh accounts and a test USDC, runs a 100-vs-60 two-way
  transfer and checks every balance. About 4-6 minutes, mostly testnet
  latency and the wait window. `scripts/e2e_testnet.sh local` works against a
  Quickstart container if Docker is installed.
- Testnet is reset a few times a year (next: 2026-12-16). Both the script and
  `app/setup.sh` create everything from scratch, so just rerun them.
- `stellar/stellar-demo-wallet` is a manual tool for anchor flows (SEP-6/24/31).
  It does not call Soroban contracts, so it becomes useful once payouts go
  through an anchor, e.g. against `testanchor.stellar.org`.

## Known limits

- Matching is bilateral per corridor; no multi-hop cycles (CA→IN→MX→CA).
- `full` is enforced on-chain only within one `settle_batch`.
- Instant transfers still wait about 30 seconds in the app, so the deposit can
  land and get one matching pass.
- A recipient needs a USDC trustline or the payout reverts. The app must check
  this before creating an order.
- Order storage lives 60 days from its last write. Very long waits would need a
  TTL bump.
- No compliance layer. Moving value for other people is a regulated activity;
  get legal advice and licensed partners before any real-money use.

## Next

1. Payouts through anchors in each country (SEP-31 receive side, or SEP-24
   withdrawals), so matched amounts become local cash.
2. Non-custodial sending: senders sign in their own wallet (Freighter / Stellar
   Wallets Kit) instead of the demo server holding keys.
3. Pick the first corridor based on where cash-out partners exist on both ends.
