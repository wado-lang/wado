# wado-manifest

`wado.toml` parsing and validation, `wado.lock` management, dependency
resolution, and the workspace / dependency discovery that reads them off disk.
See `README.md` for the crate's scope and status, and
[WEP: Package Manifest](../docs/wep-2026-02-14-package-manifest.md) for the
specification.

## Rules

- The wasm contract is `wasm32-wasip1` / `wasm32-wasip2` — targets with a real
  filesystem, where this crate is expected to *work*. CI checks both.
- It must still **compile** for `wasm32-unknown-unknown`: the browser playground
  links it through `wado-lsp`, whose own check enforces that. Nothing here may
  need a wasi-only API. Working there is not promised — that target has no
  filesystem, so the local reads below simply fail and read as "not present".
- Network and git fetching are injected by `wado-cli` through the
  `DependencyProvider` seam — keep that boundary. The resolver itself
  (`resolve.rs`, `provider.rs`, `version.rs`) stays pure so it can be driven
  from memory.
- Local reads are confined to `workspace.rs` and `dependency.rs`: finding the
  governing `wado.toml`, and placing an already lock-pinned dependency in the
  warm cache. Neither fetches.
