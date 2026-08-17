#!/usr/bin/env bash
# HTTP routing benchmark: `wado serve` vs Hono (Node.js & Bun) vs Axum, over
# one worker shape per entry in SHAPES. README.md covers the methodology.
#
# Overrides: SLICE, ROUNDS, SHAPES, CONNECTIONS_PER_WORKER, OHA_CORE_COUNT,
# HEADROOM_CHECK.
set -euo pipefail
cd "$(dirname "$0")"

SLICE="${SLICE:-10}"
ROUNDS="${ROUNDS:-3}"
SHAPES="${SHAPES:-1 4}"
CONNECTIONS_PER_WORKER="${CONNECTIONS_PER_WORKER:-200}"
HEADROOM_CHECK="${HEADROOM_CHECK:-0}"
WADO_BIN="../../target/release/wado"
HONO_PORT="3000"
AXUM_PORT="3001"
BUN_PORT="3002"
WADO_PORT="8080"

# One request per routing behaviour the routers differ on.
REQUESTS=(
  "GET /user"
  "GET /user/lookup/username/hey"
  "POST /event/abcd1234/comment"
  "GET /static/index.html"
)

OHA_BIN="$(command -v oha || true)"
[ -z "$OHA_BIN" ] && OHA_BIN="$HOME/.cargo/bin/oha"

NPROC="$(nproc)"
OHA_CORE_COUNT="${OHA_CORE_COUNT:-4}"
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
    [ -n "$p" ] || continue
    # Children first: `bun run` serves from one, and reparenting would orphan
    # it past the parent's death.
    pkill -P "$p" 2>/dev/null || true
    kill "$p" 2>/dev/null || true
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

# A survivor from an earlier shape keeps serving its port — under SO_REUSEPORT
# alongside the new one — and inflates the row by however many are left.
assert_port_free() {
  for _ in $(seq 1 40); do
    curl -fs -o /dev/null "$1" 2>/dev/null || return 0
    sleep 0.5
  done
  echo "ERROR: $1 still served after cleanup; a stale server would inflate this row" >&2
  exit 1
}

# Whole req/s, so the callers can compare with -gt.
measure() {
  "${OHA_PIN[@]}" "$OHA_BIN" -m "$2" -z "${SLICE}s" -c "$CONNECTIONS" \
    --no-tui "${1}${3}" 2>/dev/null |
    awk '/Requests\/sec/ {printf "%.0f", $2}'
}

SERVER_KEYS=(wado node bun axum)
if ! command -v bun >/dev/null 2>&1; then
  echo "SKIP: bun not found (install bun or add it to benchmark/mise.toml)"
  SERVER_KEYS=(wado node axum)
fi

server_name() {
  case "$1" in
    wado) echo "Wado (wado serve)" ;;
    node) echo "JavaScript (Hono on Node)" ;;
    bun) echo "JavaScript (Hono on Bun)" ;;
    axum) echo "Rust (Axum)" ;;
  esac
}

server_url() {
  case "$1" in
    wado) echo "http://127.0.0.1:${WADO_PORT}" ;;
    node) echo "http://127.0.0.1:${HONO_PORT}" ;;
    bun) echo "http://127.0.0.1:${BUN_PORT}" ;;
    axum) echo "http://127.0.0.1:${AXUM_PORT}" ;;
  esac
}

# Only the server under measurement runs: an idle peer still costs the shape's
# cores its runtime threads, GC and timers, and costs them unevenly.
start_server() {
  local key="$1" workers="$2"
  assert_port_free "$(server_url "$key")/status"
  case "$key" in
    wado)
      "${SERVER_PIN[@]}" "$WADO_BIN" serve --addr "127.0.0.1:${WADO_PORT}" \
        --workers "$workers" app.wado >/dev/null 2>&1 &
      PIDS+=($!)
      ;;
    node)
      PORT="$HONO_PORT" WORKERS="$workers" "${SERVER_PIN[@]}" node app.js >/dev/null 2>&1 &
      PIDS+=($!)
      ;;
    bun)
      # Bun has no cluster primary; the processes share the port via SO_REUSEPORT.
      for _ in $(seq 1 "$workers"); do
        PORT="$BUN_PORT" "${SERVER_PIN[@]}" bun run app.bun.js >/dev/null 2>&1 &
        PIDS+=($!)
      done
      ;;
    axum)
      PORT="$AXUM_PORT" TOKIO_WORKER_THREADS="$workers" \
        "${SERVER_PIN[@]}" ./target/release/axum_server >/dev/null 2>&1 &
      PIDS+=($!)
      ;;
  esac
  wait_ready "$(server_url "$key")/status"
}

run_shape() {
  local workers="$1"
  local key si ri req method path rps url best_rps best_method best_path
  local pinned="" saturated="" got differs=""
  CONNECTIONS=$((CONNECTIONS_PER_WORKER * workers))

  # `oha` gets a fixed core count, not "the rest": spreading the same
  # connections over more generator threads thins each one's batch, and the
  # server pays for the extra wakeups — measured 105k req/s on 4 generator
  # cores against 69k on 8, for the same server. Four sustains >320k req/s,
  # well past anything measured here; the headroom check guards the low side.
  if [ "$PIN_AVAILABLE" -eq 0 ]; then
    echo "CPU pinning: disabled (nproc=${NPROC}, taskset unavailable)"
  elif [ $((workers + OHA_CORE_COUNT)) -gt "$NPROC" ]; then
    echo "CPU pinning: disabled (${workers} workers + ${OHA_CORE_COUNT} generator cores exceed nproc=${NPROC})"
  else
    pinned=yes
  fi
  if [ -n "$pinned" ]; then
    SERVER_PIN=(taskset -c "0-$((workers - 1))")
    OHA_PIN=(taskset -c "$((NPROC - OHA_CORE_COUNT))-$((NPROC - 1))")
    echo "CPU pinning: servers on cores 0-$((workers - 1)), oha on cores $((NPROC - OHA_CORE_COUNT))-$((NPROC - 1))"
  else
    SERVER_PIN=(env)
    OHA_PIN=(env)
  fi

  declare -A BEST=() REF=()
  for si in "${!SERVER_KEYS[@]}"; do
    key="${SERVER_KEYS[$si]}"
    url="$(server_url "$key")"
    echo "--- $(server_name "$key") ---"
    start_server "$key" "$workers"

    # The rows only compare if the servers answer alike. One request each,
    # outside the timed slices, against the first server's answers.
    for ri in "${!REQUESTS[@]}"; do
      req="${REQUESTS[$ri]}"
      got=$(curl -s -X "${req%% *}" -w '|%{http_code}' "${url}${req#* }" 2>/dev/null)
      if [ -z "${REF[$ri]:-}" ]; then
        REF[$ri]="$got"
      elif [ "$got" != "${REF[$ri]}" ]; then
        echo "    WARNING: ${req} answers ${got}, $(server_name "${SERVER_KEYS[0]}") answers ${REF[$ri]}"
        differs="yes"
      fi
    done

    # Discarded: the JS rows need it to reach steady state.
    for req in "${REQUESTS[@]}"; do
      measure "$url" "${req%% *}" "${req#* }" >/dev/null
    done
    best_rps=0
    best_method=""
    best_path=""
    for ri in "${!REQUESTS[@]}"; do
      req="${REQUESTS[$ri]}"
      method="${req%% *}"
      path="${req#* }"
      BEST[${si}|${ri}]=0
      for _ in $(seq 1 "$ROUNDS"); do
        rps=$(measure "$url" "$method" "$path")
        [ -z "$rps" ] && rps=0
        [ "$rps" -gt "${BEST[${si}|${ri}]}" ] && BEST[${si}|${ri}]="$rps"
      done
      if [ "${BEST[${si}|${ri}]}" -gt "$best_rps" ]; then
        best_rps="${BEST[${si}|${ri}]}"
        best_method="$method"
        best_path="$path"
      fi
    done

    # A gain at 2x connections means `oha` set that number, not the server.
    # Off by default: it passes at the tuned settings, so it earns its slice
    # only when CONNECTIONS_PER_WORKER, OHA_CORE_COUNT or a shape changes.
    if [ "$HEADROOM_CHECK" = "1" ] && [ -n "$best_path" ]; then
      CONNECTIONS=$((CONNECTIONS * 2))
      rps=$(measure "$url" "$best_method" "$best_path")
      CONNECTIONS=$((CONNECTIONS / 2))
      if [ "${rps:-0}" -gt $((best_rps * 105 / 100)) ]; then
        echo "    WARNING: ${best_method} ${best_path}: ${best_rps} -> ${rps} req/s at 2x connections; this row is a floor"
        saturated="yes"
      fi
    fi

    cleanup
    sleep 1
  done

  echo
  echo "=== ${workers} worker(s): max req/s over ${ROUNDS} rounds (higher is better) ==="
  printf '%-44s' "Request"
  for key in "${SERVER_KEYS[@]}"; do printf '%26s' "$(server_name "$key")"; done
  printf '\n'
  for ri in "${!REQUESTS[@]}"; do
    printf '%-44s' "${REQUESTS[$ri]}"
    for si in "${!SERVER_KEYS[@]}"; do printf '%26s' "${BEST[${si}|${ri}]:-0}"; done
    printf '\n'
  done
  echo
  if [ -z "$pinned" ]; then
    echo "These rows are unpinned: the servers and the load generator shared cores."
  fi
  if [ -n "$differs" ]; then
    echo "Response check: FAILED — the servers do not answer alike; see above."
  else
    echo "Response check: ok — every server returns the same body and status."
  fi
  [ "$HEADROOM_CHECK" = "1" ] || return 0
  echo
  if [ -n "$saturated" ]; then
    echo "Headroom check: FAILED — see the warnings above."
  else
    echo "Headroom check: ok — no row gained at 2x connections."
  fi
}

for shape in $SHAPES; do
  echo
  echo "########## ${shape} worker(s) per server ##########"
  run_shape "$shape"
done
