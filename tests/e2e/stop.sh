#!/usr/bin/env bash
# Tear down the Fiber e2e node stack started by start.sh.
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
pid_file="$script_dir/fiber-start.pid"
log_file="$script_dir/fiber-nodes.log"

kill_port_listeners() {
  local port="$1"
  local pids=""
  if command -v lsof >/dev/null; then
    pids=$(lsof -tiTCP:"$port" -sTCP:LISTEN 2>/dev/null || true)
  fi
  if [[ -n "$pids" ]]; then
    # shellcheck disable=SC2086
    kill $pids 2>/dev/null || true
  fi
}

if [[ -f "$pid_file" ]]; then
  start_pid=$(tr -d '[:space:]' <"$pid_file" || true)
  if [[ -n "${start_pid:-}" ]]; then
    echo "stopping Fiber start.sh pid $start_pid"
    pkill -P "$start_pid" 2>/dev/null || true
    kill "$start_pid" 2>/dev/null || true
    sleep 1
    kill -9 "$start_pid" 2>/dev/null || true
  fi
  rm -f "$pid_file"
fi

# CKB 8114, node RPC 21714-21716, P2P 8344-8346.
for port in 8114 21714 21715 21716 8344 8345 8346; do
  kill_port_listeners "$port"
done

rm -f "$log_file"
echo "e2e nodes stopped"
