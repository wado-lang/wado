#!/usr/bin/env bash
# HTTP routing benchmark: `wado serve` vs Hono (Node.js & Bun) vs Axum.
#
# Two deployment shapes are measured, because they rank differently:
#
#   * 1 worker  — a 1-core container scaled out horizontally (k8s).
#   * 4 workers — a small VM running one instance.
#
# Every server gets the same worker count and the same pinned cores in a
# given shape, so the table compares servers rather than core budgets.
# Node scales out with `node:cluster`, Bun with SO_REUSEPORT, `wado serve`
# with --workers, Axum with TOKIO_WORKER_THREADS.
#
# The load generator is pinned to a disjoint core set and given the larger
# share; a saturated `oha` silently caps the fastest servers and compresses
# every ratio, so each shape ends with a headroom check that re-runs the
# fastest result with twice the connections and reports any gain.
#
# Env overrides: SLICE (default 10s), ROUNDS (default 3), SHAPES
# (default "1 4"), CONNECTIONS_PER_WORKER (default 100).
set -euo pipefail
cd "$(dirname "$0")"

SLICE="${SLICE:-10}"
ROUNDS="${ROUNDS:-3}"
SHAPES="${SHAPES:-1 4}"
CONNECTIONS_PER_WORKER="${CONNECTIONS_PER_WORKER:-100}"
WADO_BIN="../../target/release/wado"
HONO_PORT="3000"
AXUM_PORT="3001"
BUN_PORT="3002"
WADO_PORT="8080"

# One request per routing behaviour the routers differ on: static match,
# dynamic parameter, method dispatch over a mixed static/dynamic path, and
# wildcard. Hono's official router benchmark set, minus the entries that
# only vary depth or the radix sibling of a case already covered.
REQUESTS=(
  "GET /user"
  "GET /user/lookup/username/hey"
  "POST /event/abcd1234/comment"
  "GET /static/index.html"
)

OHA_BIN="$(command -v oha || true)"
[ -z "$OHA_BIN" ] && OHA_BIN="$HOME/.cargo/bin/oha"

NPROC="$(nproc)"
PIN_AVAILABLE=0
command -v taskset >/dev/null 2>&1 && [ "$NPROC" -ge 4 ] && PIN_AVAILABLE=1

echo "=== Building wado (release) ==="
cargo build --release --quiet --manifest-path ../../wado-cli/Cargo.toml

echo "=== Building Axum server (release) ==="
cargo build --release --quiet --manifest-path Cargo.toml

echo "=== Installing Hono dependencies ==="
npm install --prefix . --silent --no-audit --no-fund

PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do
    [ -n "$p" ] && kill "$p" 2>/dev/null || true
  done
  PIDS=()
}
trap cleanup EXIT

wait_ready() {
  for _ in $(seq 1 120); do
    curl -fs -o /dev/null "$1" 2>/dev/null && return 0
    sleep 0.5
  done
  echo "ERROR: server not ready at $1" >&2
  return 1
}

# Throughput of one (url, method, path) slice, in whole req/s.
measure() {
  "${OHA_PIN[@]}" "$OHA_BIN" -m "$2" -z "${SLICE}s" -c "$CONNECTIONS" \
    --no-tui "${1}${3}" 2>/dev/null |
    awk '/Requests\/sec/ {printf "%.0f", $2}'
}

SERVER_NAMES=()
SERVER_URLS=()

start_servers() {
  local workers="$1"
  SERVER_NAMES=()
  SERVER_URLS=()

  "${SERVER_PIN[@]}" "$WADO_BIN" serve --addr "127.0.0.1:${WADO_PORT}" \
    --workers "$workers" app.wado >/dev/null 2>&1 &
  PIDS+=($!)
  SERVER_NAMES+=("Wado (wado serve)")
  SERVER_URLS+=("http://127.0.0.1:${WADO_PORT}")

  PORT="$HONO_PORT" WORKERS="$workers" "${SERVER_PIN[@]}" node app.js >/dev/null 2>&1 &
  PIDS+=($!)
  SERVER_NAMES+=("JavaScript (Hono on Node)")
  SERVER_URLS+=("http://127.0.0.1:${HONO_PORT}")

  if command -v bun >/dev/null 2>&1; then
    # Bun has no cluster primary: one process per worker, all sharing the
    # port through SO_REUSEPORT.
    for _ in $(seq 1 "$workers"); do
      PORT="$BUN_PORT" "${SERVER_PIN[@]}" bun run app.bun.js >/dev/null 2>&1 &
      PIDS+=($!)
    done
    SERVER_NAMES+=("JavaScript (Hono on Bun)")
    SERVER_URLS+=("http://127.0.0.1:${BUN_PORT}")
  else
    echo "  SKIP: bun not found (install bun or add it to benchmark/mise.toml)"
  fi

  PORT="$AXUM_PORT" TOKIO_WORKER_THREADS="$workers" \
    "${SERVER_PIN[@]}" ./target/release/axum_server >/dev/null 2>&1 &
  PIDS+=($!)
  SERVER_NAMES+=("Rust (Axum)")
  SERVER_URLS+=("http://127.0.0.1:${AXUM_PORT}")

  for url in "${SERVER_URLS[@]}"; do wait_ready "${url}/status"; done
}

run_shape() {
  local workers="$1"
  CONNECTIONS=$((CONNECTIONS_PER_WORKER * workers))

  if [ "$PIN_AVAILABLE" -eq 1 ]; then
    SERVER_PIN=(taskset -c "0-$((workers - 1))")
    OHA_PIN=(taskset -c "${workers}-$((NPROC - 1))")
    echo "CPU pinning: servers on cores 0-$((workers - 1)), oha on cores ${workers}-$((NPROC - 1))"
  else
    SERVER_PIN=(env)
    OHA_PIN=(env)
    echo "CPU pinning: disabled (nproc=${NPROC}, taskset unavailable)"
  fi

  start_servers "$workers"

  declare -A BEST=()
  local si ri req method path rps
  for si in "${!SERVER_NAMES[@]}"; do
    echo "--- ${SERVER_NAMES[$si]} ---"
    # Warm up every route once; the JITs need it and the discarded slice
    # also settles the accept queues.
    for req in "${REQUESTS[@]}"; do
      measure "${SERVER_URLS[$si]}" "${req%% *}" "${req#* }" >/dev/null
    done
    for ri in "${!REQUESTS[@]}"; do
      req="${REQUESTS[$ri]}"
      method="${req%% *}"
      path="${req#* }"
      for _ in $(seq 1 "$ROUNDS"); do
        rps=$(measure "${SERVER_URLS[$si]}" "$method" "$path")
        [ -z "$rps" ] && rps=0
        [ "$rps" -gt "${BEST[${si}|${ri}]:-0}" ] && BEST[${si}|${ri}]="$rps"
      done
    done
  done

  echo
  echo "=== ${workers} worker(s): max req/s over ${ROUNDS} rounds (higher is better) ==="
  printf '%-44s' "Request"
  for name in "${SERVER_NAMES[@]}"; do printf '%26s' "$name"; done
  printf '\n'
  for ri in "${!REQUESTS[@]}"; do
    printf '%-44s' "${REQUESTS[$ri]}"
    for si in "${!SERVER_NAMES[@]}"; do printf '%26s' "${BEST[${si}|${ri}]:-0}"; done
    printf '\n'
  done

  # Headroom check: re-run the fastest cell with twice the connections. A
  # gain means `oha` — not the server — set that number, and every result
  # in the shape is suspect.
  local top=0 top_si=0 top_ri=0
  for si in "${!SERVER_NAMES[@]}"; do
    for ri in "${!REQUESTS[@]}"; do
      if [ "${BEST[${si}|${ri}]:-0}" -gt "$top" ]; then
        top="${BEST[${si}|${ri}]}"; top_si="$si"; top_ri="$ri"
      fi
    done
  done
  req="${REQUESTS[$top_ri]}"
  CONNECTIONS=$((CONNECTIONS * 2))
  local retry
  retry=$(measure "${SERVER_URLS[$top_si]}" "${req%% *}" "${req#* }")
  echo
  printf 'Headroom check: %s @ %s — %s req/s, %s req/s at 2x connections' \
    "${SERVER_NAMES[$top_si]}" "$req" "$top" "$retry"
  if [ "$retry" -gt $((top * 105 / 100)) ]; then
    printf ' — WARNING: load generator saturated, results are a floor\n'
  else
    printf ' — ok\n'
  fi

  cleanup
  sleep 1
}

for shape in $SHAPES; do
  echo
  echo "########## ${shape} worker(s) per server ##########"
  run_shape "$shape"
done
