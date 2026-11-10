#!/usr/bin/env bash
# End-to-end check of the netting escrow on a real Stellar network.
#
# Creates fresh funded accounts, issues a test "USDC" (a classic asset wrapped
# by its Stellar Asset Contract, the same way real USDC works), deploys the
# escrow, then runs one two-way transfer:
#
#   alice (CA) sends 100 USDC to r_alice (IN), waits 180s
#   carol (IN) sends  60 USDC to r_carol (CA)
#   -> 60 is netted: both recipients get 59.94 straight away (10 bps fee)
#   -> at the deadline the other 40 is released to r_alice
#
# and asserts every balance. Exits non-zero on the first mismatch.
#
# Usage: scripts/e2e_testnet.sh [network]     (default: testnet; "local" works
#        with a running Quickstart container)
# Needs: stellar CLI (v23+), cargo. Everything it creates is throwaway.
set -euo pipefail

NET="${1:-testnet}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
P="velo-e2e"            # key name prefix, so we never touch your own keys
WAIT=180                # seconds the CA->IN sender is willing to wait (testnet calls take ~10s each)
FEE_BPS=10
U=10000000              # 1 USDC in stroops

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\033[31mFAIL: %s\033[0m\n' "$*"; exit 1; }
addr() { stellar keys address "$P-$1"; }
ERR="$(mktemp)"; trap 'rm -f "$ERR"' EXIT
# Invoke a contract. Retries transient RPC failures; prints the real error if
# every attempt fails. Set TRIES=1 for calls that are expected to fail.
inv()  { local who="$1"; shift; local n
         for n in $(seq 1 "${TRIES:-3}"); do
           if stellar contract invoke -q --network "$NET" --source-account "$P-$who" --id "$@" 2>"$ERR"; then
             return 0; fi
           [[ "${TRIES:-3}" == 1 ]] || sleep 3
         done
         [[ "${QUIET_FAIL:-}" == 1 ]] || { echo "invoke failed: $*" >&2; tail -5 "$ERR" >&2; }
         return 1; }
bal()  { inv admin "$USDC" -- balance --id "$1" | tr -d '"'; }
expect_bal() { local name="$1" who="$2" want="$3" got
               got="$(bal "$who")"
               [[ "$got" == "$want" ]] || fail "$name balance: want $want, got $got"
               echo "ok  $name = $got"; }

say "Unit tests"
(cd "$ROOT/contracts" && cargo test -q 2>&1 | tail -3)
(cd "$ROOT/engine" && python3 -I -m unittest discover -s tests -t . 2>&1 | tail -3)

say "Build wasm"
(cd "$ROOT/contracts" && stellar contract build -q)
WASM="$ROOT/contracts/target/wasm32v1-none/release/netting_escrow.wasm"
[[ -f "$WASM" ]] || fail "wasm not found at $WASM"

say "Fresh accounts on $NET"
for k in admin issuer settler treasury alice carol r_alice r_carol; do
  stellar keys generate "$P-$k" --network "$NET" --fund --overwrite -q
done

say "Test USDC (classic asset + SAC)"
ASSET="USDC:$(addr issuer)"
USDC="$(stellar contract asset deploy --network "$NET" --source-account "$P-admin" --asset "$ASSET" -q 2>/dev/null \
        || stellar contract id asset --network "$NET" --asset "$ASSET")"
echo "USDC contract: $USDC"
for k in treasury alice carol r_alice r_carol; do
  stellar tx new change-trust --network "$NET" --source-account "$P-$k" --line "$ASSET" -q
done
for k in alice carol; do
  stellar tx new payment --network "$NET" --source-account "$P-issuer" \
    --destination "$(addr $k)" --asset "$ASSET" --amount $((1000 * U)) -q
done

say "Deploy escrow"
ESCROW="$(stellar contract deploy -q --network "$NET" --source-account "$P-admin" --wasm "$WASM" -- \
  --admin "$(addr admin)" --settler "$(addr settler)" --treasury "$(addr treasury)" \
  --token "$USDC" --fee_bps $FEE_BPS)"
echo "Escrow: $ESCROW"

say "Deposits"
# Each order's deadline is set when it is submitted, like a real sender would.
# Carol's order is already waiting when alice arrives; alice waits $WAIT seconds.
Y="$(inv carol "$ESCROW" -- deposit --owner "$(addr carol)" --recipient "$(addr r_carol)" \
  --from_region IN --to_region CA --amount $((60 * U)) --deadline $(( $(date +%s) + 3600 )) \
  --full false --on_expiry SendRemainder)"
X="$(inv alice "$ESCROW" -- deposit --owner "$(addr alice)" --recipient "$(addr r_alice)" \
  --from_region CA --to_region IN --amount $((100 * U)) --deadline $(( $(date +%s) + WAIT )) \
  --full false --on_expiry SendRemainder)"
echo "orders: x=$X (CA->IN 100), y=$Y (IN->CA 60)"
expect_bal escrow "$ESCROW" $((160 * U))

# Checked right after the deposit, while the deadline is surely ahead.
say "Release before the deadline must fail"
if TRIES=1 QUIET_FAIL=1 inv admin "$ESCROW" -- release --order_id "$X" >/dev/null; then
  fail "release succeeded before the deadline"
fi
echo "ok  rejected"

say "Net the opposite flows"
inv settler "$ESCROW" -- settle_batch --fills "[{\"x_id\":$X,\"y_id\":$Y,\"amount\":\"$((60 * U))\"}]" >/dev/null
PAID=$(( 60 * U - 60 * U * FEE_BPS / 10000 ))
expect_bal r_alice "$(addr r_alice)" $PAID
expect_bal r_carol "$(addr r_carol)" $PAID
expect_bal treasury "$(addr treasury)" $(( 2 * (60 * U - PAID) ))
expect_bal escrow "$ESCROW" $((40 * U))

say "Wait for deadline, then release the remainder (any account may call)"
for _ in $(seq 1 30); do
  if TRIES=1 QUIET_FAIL=1 inv treasury "$ESCROW" -- release --order_id "$X" >/dev/null; then break; fi
  sleep 5
done
expect_bal r_alice "$(addr r_alice)" $(( PAID + 40 * U ))
expect_bal escrow "$ESCROW" 0

printf '\n\033[32mPASS\033[0m  escrow %s on %s\n' "$ESCROW" "$NET"
