# Velocity-Blockchain: architecture

Status: design + off-chain netting engine. The Soroban contract, backend and
frontend are not built yet.

## What this system saves, and where

Stellar is one global ledger. Moving an asset between two accounts costs a
fraction of a cent whatever the countries involved, so there is no cross-border
fee to net away on-chain. The real cost sits in the **conversion**: sending
USDC and having the recipient receive another asset means crossing the SDEX /
AMM pools (spread + slippage), or paying a liquidity provider's margin.

Netting removes that cost for the part of the flow that offsets:

```
Alice: sells USDC, recipient should get EURC
Bob:   sells EURC, recipient should get USDC
```

Without netting, both trade through the DEX and each pays spread and slippage.
With netting, Alice's USDC goes to Bob's recipient and Bob's EURC goes to
Alice's recipient, at one oracle reference rate minus a small fee. The matched
amount never touches a pool. Only the unmatched remainder is routed through a
Stellar path payment (or refunded), according to the sender's choice.

Savings per matched unit = (DEX cost the sender would have paid) - (netting fee).
The DEX cost varies with pool depth, so the engine takes it as an input and
labels the result an estimate. Measure it against real path-payment quotes
before claiming a number.

## Why not CAD <-> INR first

Checked against the Stellar stablecoin index (Scout, data as of 2026-11-10):

- No INR-pegged stablecoin is tracked among the 41 listed.
- The only CAD one, C1CAD, shows about $21M cap, 4 holders, no 24h volume.
- USD and EUR are the liquid pairs: USDC (about 2.5M holders) and EURC
  (about 52k holders on the main issuer).

The engine is asset-agnostic (any SEP-41 / classic asset pair), but the first
corridor should be **USDC <-> EURC** on Testnet, where both sides exist and a
path-payment baseline can actually be quoted. Add corridors as local stablecoins
appear; Reflector and RedStone both publish SEP-40 price feeds for several
fiat-pegged Stellar assets.

## Components

| Piece | Where | Status |
|---|---|---|
| Netting engine (matching, FIFO, partial/full/instant, fallbacks, savings estimate) | `engine/netting` | built, 20 tests passing |
| Escrow + settlement contract (Soroban, Rust) | `contracts/` | not started |
| Rate source: SEP-40 oracle (Reflector) | read by contract and backend | not started |
| Backend API + order book (FastAPI, Postgres) | `backend/` | not started |
| Path-payment router for remainders (stellar-sdk) | `backend/` | not started |
| Frontend (Next.js + Freighter) | `frontend/` | not started |

### Trust split

- **Contract** custodies funds and is the only thing that moves them. It
  re-checks every fill: opposite directions, rate within a tolerance of the
  oracle, each side's signed `min_receive`, balances, deadlines. A buggy or
  malicious backend can then only fail to match, never take funds.
- **Engine / backend** finds matches and proposes them. It holds no funds.
- **Order book** lives off-chain; only escrowed balances and fills are on-chain.

### Order lifecycle

1. Sender signs an order: sell asset, buy asset, amount, `min_receive`,
   recipient, mode (INSTANT / PARTIAL / FULL), max wait, fallback.
2. Sender deposits the sell asset into escrow.
3. Engine matches at the oracle rate; contract executes each fill atomically.
4. At the deadline the remainder goes to the fallback: path payment through the
   DEX with the pro-rata `min_receive` as `destMin`, or a refund.

## Engine rules (implemented)

- Integers only, 7 decimals; rate is B per A scaled by 10^7.
- FIFO by `(created_at, id)`; deterministic.
- Fee is taken from what each side receives, rounded up (rounding favours the pool).
- Each side's `min_receive` is enforced pro rata per fill with exact integer math.
- FULL orders are all-or-nothing; the allocation is recomputed without any FULL
  order that ends up partly filled, until stable.
- Each order gets one last match at its deadline before fallback; the remainder
  is closed in the same step so it cannot be routed or refunded twice.
- Conservation holds exactly per asset: amount in = amount out + fee.

## Known limits

- Matching is bilateral per pair; no multi-hop cycles.
- One reference rate per matching pass; no per-order price improvement.
- No compliance layer. Moving value for other people can be a regulated
  activity; get legal advice before any real-money use.
- The engine has not been run against a contract or Testnet yet.
