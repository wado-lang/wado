#!/usr/bin/env bash
# HTTP routing benchmark: `wado serve` vs Hono (Node.js).
#
# Starts each server in turn and drives load with `oha` against Hono's
# official router-benchmark request set (honojs/hono,
# benchmarks/routers/src/bench.mts), reporting requests/sec per request.
#
# Env overrides: DURATION (default 5s), CONNECTIONS (default 50).
set -euo pipefail
cd "$(dirname "$0")"

DURATION="${DURATION:-5s}"
CONNECTIONS="${CONNECTIONS:-50}"
WADO_ADDR="127.0.0.1:8080"
HONO_PORT="3000"
WADO_BIN="../../target/release/wado"

# Hono's official router-benchmark request set ("METHOD PATH" per entry):
# short static, static sharing a radix, dynamic, mixed static/dynamic,
# POST, long static, wildcard.
REQUESTS=(
  "GET /user"
  "GET /user/comments"
  "GET /user/lookup/username/hey"
  "GET /event/abcd1234/comments"
  "POST /event/abcd1234/comment"
  "GET /very/deeply/nested/route/hello/there"
  "GET /static/index.html"
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
  for req in "${REQUESTS[@]}"; do
    local method="${req%% *}" path="${req#* }" out rps
    out=$(oha -m "$method" -z "$DURATION" -c "$CONNECTIONS" --no-tui "$base$path" 2>/dev/null)
    rps=$(echo "$out" | awk '/Requests\/sec/ {print $2}')
    printf '  %-5s %-40s %12.0f req/s\n' "$method" "$path" "$rps"
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
wait_ready "http://${WADO_ADDR}/status"
bench "http://${WADO_ADDR}"
stop_server

echo
echo "=== Hono (Node.js) ==="
PORT="$HONO_PORT" node app.js >/dev/null 2>&1 &
SERVER_PID=$!
wait_ready "http://127.0.0.1:${HONO_PORT}/status"
bench "http://127.0.0.1:${HONO_PORT}"
stop_server

echo
echo "Done (duration=${DURATION}, connections=${CONNECTIONS} per request)."
