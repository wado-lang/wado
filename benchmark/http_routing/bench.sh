#!/usr/bin/env bash
# HTTP routing benchmark: `wado serve` vs Hono (Node.js).
#
# Starts each server in turn and drives load against a set of
# representative routes with `oha`, reporting requests/sec per route.
#
# Env overrides: DURATION (default 5s), CONNECTIONS (default 50).
set -euo pipefail
cd "$(dirname "$0")"

DURATION="${DURATION:-5s}"
CONNECTIONS="${CONNECTIONS:-50}"
WADO_ADDR="127.0.0.1:8080"
HONO_PORT="3000"
WADO_BIN="../../target/release/wado"

# Representative routes: shallow/medium/deep static, 1/2/3-param,
# wildcard, and a miss (404).
PATHS=(
  "/health"
  "/api/v1/users/list"
  "/api/v1/admin/system/cache/stats"
  "/api/v1/users/4242"
  "/api/v1/users/4242/posts/77"
  "/api/v1/users/4242/posts/77/comments/9"
  "/static/css/site/main.css"
  "/no/such/route"
)

SERVER_PID=""
cleanup() { [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true; }
trap cleanup EXIT

wait_ready() {
  local url="$1"
  for _ in $(seq 1 120); do
    curl -fs -o /dev/null "$url" 2>/dev/null && return 0
    sleep 0.5
  done
  echo "ERROR: server not ready at $url" >&2
  return 1
}

bench() {
  local base="$1"
  for p in "${PATHS[@]}"; do
    local out rps
    out=$(oha -z "$DURATION" -c "$CONNECTIONS" --no-tui "$base$p" 2>/dev/null)
    rps=$(echo "$out" | awk '/Requests\/sec/ {print $2}')
    printf '  %-44s %12.0f req/s\n' "$p" "$rps"
  done
}

stop_server() {
  [ -n "$SERVER_PID" ] || return 0
  kill "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
  SERVER_PID=""
}

echo "=== Building wado (release) ==="
cargo build --release --quiet --manifest-path ../../wado-cli/Cargo.toml

echo "=== Installing Hono dependencies ==="
npm install --prefix . --silent --no-audit --no-fund

echo
echo "=== wado serve ==="
"$WADO_BIN" serve --addr "$WADO_ADDR" app.wado >/dev/null 2>&1 &
SERVER_PID=$!
wait_ready "http://${WADO_ADDR}/health"
bench "http://${WADO_ADDR}"
stop_server

echo
echo "=== Hono (Node.js) ==="
PORT="$HONO_PORT" node app.js >/dev/null 2>&1 &
SERVER_PID=$!
wait_ready "http://127.0.0.1:${HONO_PORT}/health"
bench "http://127.0.0.1:${HONO_PORT}"
stop_server

echo
echo "Done (duration=${DURATION}, connections=${CONNECTIONS} per route)."
