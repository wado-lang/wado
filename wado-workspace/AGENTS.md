# wado-workspace

The filesystem side of `wado-manifest`: workspace discovery, manifest
inheritance, and offline resolution of `[dependencies]` to real files.

## Rules

- `wado-manifest` performs no I/O by design; this crate is where that I/O lives.
  Keep it a thin read-only layer — never a second home for parsing or resolution.
- It must compile for `wasm32-unknown-unknown`. CI enforces it.
- Both `wado-lsp` and `wado-cli` depend on it. A helper either serves both or
  belongs in the crate that uses it.

## Architecture

| File            | Role                                                                                                                    |
| --------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `workspace.rs`  | Nearest `wado.toml`, `[workspace.package]` inheritance, and the glob options the member and file walkers share          |
| `dependency.rs` | `resolve_all`: every `[dependencies]` entry resolved offline to the source module or cached component that satisfies it |

`resolve_all` reads `wado.lock` once per batch. `wado-lsp` turns its result into
the compiler's `DependencyIndex`; `wado-cli` fetches against the same coordinates.
