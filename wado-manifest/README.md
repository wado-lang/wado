# wado-manifest

Parsing, validation, and dependency resolution for `wado.toml` and `wado.lock`,
plus the workspace and dependency discovery that reads them off disk.

`wasm32-unknown-unknown`-safe. The resolver models the dependency graph purely;
network and git fetching are injected by `wado-cli` through the
`DependencyProvider` seam. Only `workspace.rs` and `dependency.rs` touch the
filesystem, and only to read.

See [WEP: Package Manifest](../docs/wep-2026-02-14-package-manifest.md) for the
specification.

## What it does

- `Manifest` — parse + validate `wado.toml` (`[package]`, `[world]`,
  `[registries]`, `[dependencies]`, `[workspace]`, …).
- `LockFile` — read/write `wado.lock`.
- `resolve(manifest, provider)` — walk the dependency graph through a
  `DependencyProvider` and produce locked packages.

## Status

- [x] Open-coordinate registry deps (`"ns:pkg" = { version = "^x" }`) and `lib:`
      nicknames; bare keys are accepted with a deprecation warning.
- [x] Resolver: registry deps (highest-compatible + transitive); path deps are
      traversed but never locked.
- [x] `DependencyProvider` seam + in-memory provider; `wado update` (in
      `wado-cli`) resolves and writes the lock.
- [ ] OCI registry fetch — the live backend, implemented in `wado-cli`. (warg is
      dropped: `bytecodealliance/registry` is archived and OCI is the direction;
      wa.dev's warg registry is reachable only via `wkg`.)
- [ ] Git and workspace resolution.
- [ ] Full PubGrub conflict resolution (current: highest-compatible, first-wins).
