#!/usr/bin/env bash
# JSON-RPC helpers for the Twine multi-fnn e2e harness.

rpc_call() {
  local url="$1"
  local method="$2"
  local params="${3:-}"
  local body
  if [[ -n "$params" ]]; then
    body=$(printf '{"jsonrpc":"2.0","id":1,"method":"%s","params":[%s]}' "$method" "$params")
  else
    body=$(printf '{"jsonrpc":"2.0","id":1,"method":"%s","params":[]}' "$method")
  fi
  curl -sS "$url" \
    -H 'Content-Type: application/json' \
    -d "$body"
}

rpc_result() {
  local response="$1"
  python3 -c '
import json, sys
data = json.loads(sys.argv[1])
if data.get("error"):
    raise SystemExit("rpc error: {}".format(data["error"]))
print(json.dumps(data.get("result")))
' "$response"
}

rpc_json_field() {
  local response="$1"
  local path="$2"
  python3 -c '
import json, sys
data = json.loads(sys.argv[1])
if data.get("error"):
    raise SystemExit("rpc error: {}".format(data["error"]))
cur = data.get("result")
for key in sys.argv[2].split("."):
    if cur is None:
        break
    if isinstance(cur, list):
        cur = cur[int(key)]
    else:
        cur = cur.get(key)
if cur is None:
    raise SystemExit("missing field {}".format(sys.argv[2]))
if isinstance(cur, (dict, list)):
    print(json.dumps(cur))
else:
    print(cur)
' "$response" "$path"
}

channel_state_name() {
  local response="$1"
  python3 -c '
import json, sys
data = json.loads(sys.argv[1])
channels = (data.get("result") or {}).get("channels") or []
if not channels:
    print("")
    raise SystemExit(0)
state = channels[0].get("state") or {}
print(state.get("state_name") or "")
' "$response"
}
