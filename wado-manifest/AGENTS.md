# wado-manifest

`wado.toml` parsing and validation, `wado.lock` management, and dependency
resolution. See `README.md` for the crate's scope and status, and
[WEP: Package Manifest](../docs/wep-2026-02-14-package-manifest.md) for the
specification.

## Rules

- This crate must compile for `wasm32-unknown-unknown`. CI enforces it.
- It performs no I/O. Network, git, and filesystem fetching are injected by
  `wado-cli` through the `DependencyProvider` seam — keep that boundary.
