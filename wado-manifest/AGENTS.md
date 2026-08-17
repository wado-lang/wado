# wado-manifest

`wado.toml` parsing and validation, `wado.lock` management, dependency
resolution, and the workspace / dependency discovery that reads them off disk.
See `README.md` for the crate's scope and status, and
[WEP: Package Manifest](../docs/wep-2026-02-14-package-manifest.md) for the
specification.

## Rules

- This crate must compile for `wasm32-unknown-unknown`. CI enforces it.
- Network and git fetching are injected by `wado-cli` through the
  `DependencyProvider` seam — keep that boundary. The resolver itself
  (`resolve.rs`, `provider.rs`, `version.rs`) stays pure so it can be driven
  from memory.
- Local reads (`workspace.rs`, `dependency.rs`) are the exception, confined to
  those two modules: finding the governing `wado.toml`, and placing an already
  lock-pinned dependency in the warm cache. Neither fetches.
- `wasm32-unknown-unknown` has no filesystem, so those reads no-op in the
  browser playground. Every caller already treats a failed read as "not
  present"; keep it that way rather than growing a second code path.
