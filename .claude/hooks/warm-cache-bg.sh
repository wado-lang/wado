#!/usr/bin/env bash
# Background warm-up, launched detached by cargo-cache-restore.mts once the
# sccache cache is restored. It waits for the sibling mise-setup.sh hook (which
# runs in parallel) to finish installing mise + sccache, then runs
# `mise run warm-cache` to reconstruct target/ from the restored cache. This
# makes a session warm transparently — nobody needs to know to run
# on-task-started first. It never blocks session start: a concurrent
# `cargo build` is serialized by cargo's build lock and then reuses the
# already-compiled dependencies.
set -uo pipefail

export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
mkdir -p "$HOME/.cache/wado"
exec >>"$HOME/.cache/wado/warm-cache.log" 2>&1
echo "[warm-cache-bg $(date -u +%H:%M:%S)] waiting for mise + sccache..."

# mise-setup.sh installs both, in a parallel SessionStart hook.
for _ in $(seq 1 300); do
  if command -v mise >/dev/null 2>&1 && command -v sccache >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
if ! command -v mise >/dev/null 2>&1; then
  echo "[warm-cache-bg] mise unavailable after wait; aborting (on-task-started will warm instead)"
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-$PWD}" || exit 0
eval "$(mise activate bash 2>/dev/null)" || true
mise trust --all >/dev/null 2>&1 || true
echo "[warm-cache-bg $(date -u +%H:%M:%S)] running warm-cache"
mise run warm-cache
echo "[warm-cache-bg $(date -u +%H:%M:%S)] done (rc=$?)"
