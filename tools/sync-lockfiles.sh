#!/usr/bin/env bash
set -euo pipefail

# Syncs every lockfile that holds the workspace's version with it, so a release
# PR builds under `--locked`. wado-run-webgpu is a workspace of its own, and
# wado-host is what its lockfile records of the root's.

cargo update --workspace
cargo update -p wado-host --manifest-path wado-run-webgpu/Cargo.toml
