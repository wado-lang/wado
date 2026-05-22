#!/usr/bin/env bash
# HTTP routing benchmark: `wado serve` vs Hono (Node.js & Bun) vs Axum.
#
# Methodology for stable cross-server ratios on a noisy (cloud) host:
#
#   * CPU pinning — servers run on one core set, the `oha` load
#     generator on a disjoint set, so the two never contend for a core.
#   * Round-robin interleaving — every server stays up for the whole
#     run; each request is measured in short slices that rotate across
#     servers, repeated for ROUNDS rounds. A throttling episode then
#     hits every server within the same time window, so ratios survive
#     it even when absolute throughput drifts.
#   * Max aggregation — contention and throttling only ever lower
#     throughput, so the fastest slice across rounds is the cleanest
#     estimate of true capacity. Round 1 doubles as a warmup: its cold
#     numbers lose to later rounds and drop out.
#
# Absolute numbers are not comparable across machines or runs — only the
# ratios between servers within one run are.
#
# Env overrides: SLICE (default 3s), ROUNDS (default 3),
# CONNECTIONS (default 50), OHA_CORE_COUNT (default nproc/4).
set -euo pipefail
cd "$(dirname "$0")"

SLICE="${SLICE:-3}"
ROUNDS="${ROUNDS:-3}"
CONNECTIONS="${CONNECTIONS:-50}"
WADO_BIN="../../target/release/wado"
WADO_ADDR="127.0.0.1:8080"
HONO_PORT="3000"
AXUM_PORT="3001"
BUN_PORT="3002"

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

# `oha` is installed via `cargo install oha` and may not be on PATH
# under `mise run`; fall back to the default cargo bin location.
OHA_BIN="$(command -v oha || true)"
[ -z "$OHA_BIN" ] && OHA_BIN="$HOME/.cargo/bin/oha"

# CPU pinning: split cores between the servers and the load generator so
# they never share a core. The load generator gets a quarter of the
# cores (at least one), the servers the rest — `oha` is far lighter than
# a server, and an even split would cram a multi-worker server onto too
# few cores. Multi-threaded servers are then sized to the server core
# count (see SERVER_CORE_COUNT below) so none is over-threaded.
# `env` is a no-op prefix when pinning is off, which keeps the command
# arrays safe to expand under `set -u`.
NPROC="$(nproc)"
if command -v taskset >/dev/null 2>&1 && [ "$NPROC" -ge 2 ]; then
  # OHA_CORE_COUNT may be overridden to retune the split (e.g. =2 on a
  # 4-core host gives an even server/oha split, raising the oha
  # throughput ceiling at the cost of fewer server cores).
  OHA_CORE_COUNT="${OHA_CORE_COUNT:-$((NPROC / 4))}"
  [ "$OHA_CORE_COUNT" -lt 1 ] && OHA_CORE_COUNT=1
  SERVER_CORE_COUNT=$((NPROC - OHA_CORE_COUNT))
  SERVER_CORES="0-$((SERVER_CORE_COUNT - 1))"
  OHA_CORES="${SERVER_CORE_COUNT}-$((NPROC - 1))"
  SERVER_PIN=(taskset -c "$SERVER_CORES")
  OHA_PIN=(taskset -c "$OHA_CORES")
  echo "CPU pinning: servers on cores ${SERVER_CORES} (${SERVER_CORE_COUNT}), oha on cores ${OHA_CORES}"
else
  SERVER_PIN=(env)
  OHA_PIN=(env)
  SERVER_CORE_COUNT="$NPROC"
  echo "CPU pinning: disabled (nproc=${NPROC}, taskset unavailable)"
fi

PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do
    [ -n "$p" ] && kill "$p" 2>/dev/null || true
  done
}
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

echo "=== Building wado (release) ==="
cargo build --release --quiet --manifest-path ../../wado-cli/Cargo.toml

echo "=== Building Axum server (release) ==="
cargo build --release --quiet --manifest-path Cargo.toml

echo "=== Installing Hono dependencies ==="
npm install --prefix . --silent --no-audit --no-fund

# Server roster, populated in start order. Hono on Bun is optional.
SERVER_NAMES=()
SERVER_URLS=()
register() {
  SERVER_NAMES+=("$1")
  SERVER_URLS+=("$2")
}

# Multi-threaded servers are sized to the server core budget so none is
# over-threaded onto its pinned cores: `wado serve` via --workers, Axum
# (Tokio) via TOKIO_WORKER_THREADS. Node and Bun drive their HTTP server
# from a single thread and need no knob.
echo "=== Starting servers ==="
"${SERVER_PIN[@]}" "$WADO_BIN" serve --addr "$WADO_ADDR" \
  --workers "$SERVER_CORE_COUNT" app.wado >/dev/null 2>&1 &
PIDS+=($!)
register "wado serve" "http://${WADO_ADDR}"

PORT="$HONO_PORT" "${SERVER_PIN[@]}" node app.js >/dev/null 2>&1 &
PIDS+=($!)
register "Hono (Node)" "http://127.0.0.1:${HONO_PORT}"

if command -v bun >/dev/null 2>&1; then
  PORT="$BUN_PORT" "${SERVER_PIN[@]}" bun run app.bun.js >/dev/null 2>&1 &
  PIDS+=($!)
  register "Hono (Bun)" "http://127.0.0.1:${BUN_PORT}"
else
  echo "  SKIP: bun not found (install bun or add it to benchmark/mise.toml)"
fi

PORT="$AXUM_PORT" TOKIO_WORKER_THREADS="$SERVER_CORE_COUNT" \
  "${SERVER_PIN[@]}" ./target/release/axum_server >/dev/null 2>&1 &
PIDS+=($!)
register "Axum (native)" "http://127.0.0.1:${AXUM_PORT}"

for url in "${SERVER_URLS[@]}"; do
  wait_ready "${url}/status"
done

# Per-(server, request) best result, keyed "<server_idx>|<request_idx>".
declare -A BEST

echo "=== Measuring (slice=${SLICE}s, rounds=${ROUNDS}, connections=${CONNECTIONS}) ==="
for round in $(seq 1 "$ROUNDS"); do
  echo "--- round ${round}/${ROUNDS} ---"
  for ri in "${!REQUESTS[@]}"; do
    req="${REQUESTS[$ri]}"
    method="${req%% *}"
    path="${req#* }"
    for si in "${!SERVER_NAMES[@]}"; do
      out=$("${OHA_PIN[@]}" "$OHA_BIN" -m "$method" -z "${SLICE}s" \
        -c "$CONNECTIONS" --no-tui "${SERVER_URLS[$si]}${path}" 2>/dev/null)
      rps=$(echo "$out" | awk '/Requests\/sec/ {printf "%.0f", $2}')
      [ -z "$rps" ] && rps=0
      key="${si}|${ri}"
      if [ "$rps" -gt "${BEST[$key]:-0}" ]; then
        BEST[$key]="$rps"
      fi
    done
  done
done

echo
echo "=== Results: max req/s over ${ROUNDS} rounds (higher is better) ==="
printf '%-44s' "Request"
for name in "${SERVER_NAMES[@]}"; do
  printf '%15s' "$name"
done
printf '\n'
for ri in "${!REQUESTS[@]}"; do
  printf '%-44s' "${REQUESTS[$ri]}"
  for si in "${!SERVER_NAMES[@]}"; do
    printf '%15s' "${BEST[${si}|${ri}]:-0}"
  done
  printf '\n'
done

echo
echo "Done (slice=${SLICE}s, rounds=${ROUNDS}, connections=${CONNECTIONS} per slice)."
