#!/usr/bin/env bash
# Open seller→hub and hub→buyer channels on Fiber's local 3-node stack.
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
# shellcheck source=rpc.sh
source "$script_dir/rpc.sh"

HUB_RPC="${HUB_RPC:-http://127.0.0.1:21714}"
SELLER_RPC="${SELLER_RPC:-http://127.0.0.1:21715}"
BUYER_RPC="${BUYER_RPC:-http://127.0.0.1:21716}"
CKB_RPC="${CKB_RPC:-http://127.0.0.1:8114}"
FUNDING_AMOUNT="${FUNDING_AMOUNT:-0x377aab54d000}"

require_deps() {
  command -v curl >/dev/null || {
    echo "curl is required" >&2
    exit 1
  }
  command -v python3 >/dev/null || {
    echo "python3 is required" >&2
    exit 1
  }
}

wait_rpc() {
  local url="$1"
  local label="$2"
  local i
  for i in $(seq 1 120); do
    if rpc_call "$url" "node_info" >/dev/null 2>&1; then
      echo "$label ready at $url"
      return 0
    fi
    sleep 2
  done
  echo "$label RPC not ready at $url" >&2
  exit 1
}

node_pubkey() {
  local response
  response=$(rpc_call "$1" "node_info")
  rpc_json_field "$response" "pubkey"
}

node_addr() {
  local response
  response=$(rpc_call "$1" "node_info")
  python3 -c '
import json, sys
data = json.loads(sys.argv[1])
addrs = (data.get("result") or {}).get("addresses") or []
for addr in addrs:
    if "127.0.0.1" in addr or "0.0.0.0" in addr:
        print(addr.replace("0.0.0.0", "127.0.0.1"))
        raise SystemExit(0)
if not addrs:
    raise SystemExit("node_info returned no addresses")
print(addrs[0])
' "$response"
}

connect_peer() {
  local from_rpc="$1"
  local address="$2"
  echo "connect_peer via $from_rpc -> $address"
  rpc_call "$from_rpc" "connect_peer" "{\"address\":\"$address\"}" >/dev/null
  sleep 1
}

open_channel() {
  local from_rpc="$1"
  local peer_pubkey="$2"
  echo "open_channel via $from_rpc -> $peer_pubkey"
  local response
  response=$(rpc_call "$from_rpc" "open_channel" \
    "{\"pubkey\":\"$peer_pubkey\",\"funding_amount\":\"$FUNDING_AMOUNT\"}")
  rpc_result "$response" >/dev/null
  sleep 2
}

generate_epochs() {
  echo "generate_epochs on CKB"
  rpc_call "$CKB_RPC" "generate_epochs" "\"0x2\"" >/dev/null
  sleep 5
}

wait_channel_ready() {
  local rpc="$1"
  local peer_pubkey="$2"
  local label="$3"
  local i response state
  for i in $(seq 1 60); do
    response=$(rpc_call "$rpc" "list_channels" "{\"pubkey\":\"$peer_pubkey\"}")
    state=$(channel_state_name "$response")
    if [[ "$state" == "ChannelReady" ]]; then
      echo "$label channel ready"
      return 0
    fi
    echo "waiting for $label channel (state=${state:-none}) ..."
    sleep 2
  done
  echo "$label channel did not become ChannelReady" >&2
  exit 1
}

main() {
  require_deps
  wait_rpc "$HUB_RPC" "hub"
  wait_rpc "$SELLER_RPC" "seller"
  wait_rpc "$BUYER_RPC" "buyer"

  local hub_pubkey seller_pubkey buyer_pubkey hub_addr buyer_addr
  hub_pubkey=$(node_pubkey "$HUB_RPC")
  seller_pubkey=$(node_pubkey "$SELLER_RPC")
  buyer_pubkey=$(node_pubkey "$BUYER_RPC")
  hub_addr=$(node_addr "$HUB_RPC")
  buyer_addr=$(node_addr "$BUYER_RPC")

  echo "hub=$hub_pubkey"
  echo "seller=$seller_pubkey"
  echo "buyer=$buyer_pubkey"

  connect_peer "$SELLER_RPC" "$hub_addr"
  connect_peer "$HUB_RPC" "$buyer_addr"

  open_channel "$SELLER_RPC" "$hub_pubkey"
  generate_epochs
  wait_channel_ready "$SELLER_RPC" "$hub_pubkey" "seller->hub"

  open_channel "$HUB_RPC" "$buyer_pubkey"
  generate_epochs
  wait_channel_ready "$HUB_RPC" "$buyer_pubkey" "hub->buyer"

  echo "channels ready"
}

main "$@"
