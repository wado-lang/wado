---
name: vendor-submodules
description: Locate and sync the vendored reference specs and runtimes under vendor/ (Wasm, WASI P3, and Component Model specs; wasm-tools, wasmtime sources). Use when you need to read a Wasm/WASI/CM spec or wasmtime/wasm-tools source, when vendor/ submodules are missing and need initializing, or when building and profiling against a locally patched wasmtime.
---

## The Wasm and WASI (p3) specifications

There are external references in the module for convenience:

- `vendor/wasm/` - WebAssembly/spec
- `vendor/wasi/` - WebAssembly/WASI
- `vendor/component-model/` - WebAssembly/component-model (CM spec)
  - Canonical built-ins: `vendor/component-model/design/mvp/CanonicalABI.md`
  - Concurrency (async, streams, futures): `vendor/component-model/design/mvp/Concurrency.md`
  - Explainer: `vendor/component-model/design/mvp/Explainer.md`
- `vendor/wasmtime/` - a Wasm runtime with WASI P3 support
- `vendor/wasm-tools/` - a Wasm toolchain, where the Wado compiler relies on

Use `git submodule update --init` to have the vendor modules.

### Syncing Vendor Submodules

Run the following to sync all vendor submodules:

```sh
mise run sync-vendor
```

This syncs `vendor/wasmtime` to the exact version in `Cargo.lock` (required for WASI P3 compatibility), and updates other vendor submodules (`vendor/wasm`, `vendor/wasi`, `vendor/wasm-tools`, `vendor/component-model`) to their latest remote HEAD.

## Building against a patched wasmtime

To test a change to wasmtime itself, edit `vendor/wasmtime` in place and add to
the workspace `Cargo.toml`:

```toml
[patch.crates-io]
wasmtime = { path = "vendor/wasmtime/crates/wasmtime" }
wasmtime-wasi = { path = "vendor/wasmtime/crates/wasi" }
wasmtime-wasi-http = { path = "vendor/wasmtime/crates/wasi-http" }
wasmtime-wasi-tls = { path = "vendor/wasmtime/crates/wasi-tls" }
```

Those four names redirect 31 crates, reaching the whole `cranelift-*`,
`pulley-*`, `wasmtime-internal-*` and `wiggle*` set through the dependency
graph. `sync-vendor` already holds the submodule at the `Cargo.lock` version, so
the sources match what the workspace expects and nothing needs porting.

Keep the stanza on a throwaway branch. CI checks out without submodules
(`submodules: false`, also `actions/checkout`'s default), so on `main` every job
would fail parsing the manifest, before building anything. Landing it means
turning submodule checkout on across every workflow.

Profile with the `profiling` cargo profile, not `release`: `release` sets
`strip = "symbols"`, which leaves a sampling profile unsymbolicatable. samply
resolves symbols against the path it recorded, so profile each arm from a path
that still holds that binary — overwrite `target/profiling/wado` and an earlier
profile silently symbolicates against the new build.

`reference/` holds patches written this way, each with its measurement:

- `current_thread_split.patch` — splits the cold deferred-frame replay out of
  `StoreOpaque::current_thread` so the fast path inlines. Removes ~1 point of
  CPU; throughput flat.
- `trace_fiber_roots.patch` — tracks fiber-owning reps instead of scanning the
  whole component `ResourceTable` per GC. Not measurable.
