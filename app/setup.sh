#!/usr/bin/env bash
# One-time testnet setup for the demo app. Creates throwaway accounts, a test
# USDC, and an escrow deployment, then writes app/state.json (gitignored; it
# holds testnet secret keys, so never reuse these accounts for anything real).
#
# Rerun after a testnet reset, or any time you want a clean slate.
set -euo pipefail

NET=testnet
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
P="velo-app"
U=10000000
FEE_BPS=10
addr()   { stellar keys address "$P-$1"; }
secret() { stellar keys secret "$P-$1"; }

echo "== build contract"
(cd "$ROOT/contracts" && stellar contract build -q)
WASM="$ROOT/contracts/target/wasm32v1-none/release/netting_escrow.wasm"

echo "== accounts"
ROLES=(admin issuer settler treasury)
PEOPLE=(alice dave bob carol)
for k in "${ROLES[@]}" "${PEOPLE[@]}"; do
  stellar keys generate "$P-$k" --network $NET --fund --overwrite -q
done

echo "== test USDC"
ASSET="USDC:$(addr issuer)"
USDC="$(stellar contract asset deploy --network $NET --source-account "$P-admin" --asset "$ASSET" -q)"
for k in treasury "${PEOPLE[@]}"; do
  stellar tx new change-trust --network $NET --source-account "$P-$k" --line "$ASSET" -q
  if [[ "$k" != treasury ]]; then
    stellar tx new payment --network $NET --source-account "$P-issuer" \
      --destination "$(addr "$k")" --asset "$ASSET" --amount $((1000 * U)) -q
  fi
done

echo "== escrow"
ESCROW="$(stellar contract deploy -q --network $NET --source-account "$P-admin" --wasm "$WASM" -- \
  --admin "$(addr admin)" --settler "$(addr settler)" --treasury "$(addr treasury)" \
  --token "$USDC" --fee_bps $FEE_BPS)"

person() { # key name region
  printf '{"key":"%s","name":"%s","region":"%s","address":"%s","secret":"%s"}' \
    "$1" "$2" "$3" "$(addr "$1")" "$(secret "$1")"
}
cat > "$ROOT/app/state.json" <<EOF
{
  "network": "$NET",
  "rpcUrl": "https://soroban-testnet.stellar.org",
  "horizonUrl": "https://horizon-testnet.stellar.org",
  "passphrase": "Test SDF Network ; September 2015",
  "usdcAsset": "$ASSET",
  "usdc": "$USDC",
  "escrow": "$ESCROW",
  "feeBps": $FEE_BPS,
  "settler": {"address": "$(addr settler)", "secret": "$(secret settler)"},
  "treasury": "$(addr treasury)",
  "people": [
    $(person alice "Alice" CA),
    $(person dave "Dave" CA),
    $(person bob "Bob" IN),
    $(person carol "Carol" IN)
  ],
  "orders": []
}
EOF
echo "== done: escrow $ESCROW"
