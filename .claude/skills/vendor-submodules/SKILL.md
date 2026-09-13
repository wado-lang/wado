---
name: vendor-submodules
description: Locate and sync the reference specs and runtimes vendored under vendor/ — the Wasm, WASI P3, and Component Model specs, and the wasm-tools and wasmtime sources. Use whenever an answer should come from one of those sources rather than memory, and whenever vendor/ is missing or stale.
---

## What is under `vendor/`

- `vendor/wasm/` - WebAssembly/spec
- `vendor/wasi/` - WebAssembly/WASI
- `vendor/component-model/` - WebAssembly/component-model (CM spec)
  - Canonical built-ins: `vendor/component-model/design/mvp/CanonicalABI.md`
  - Concurrency (async, streams, futures): `vendor/component-model/design/mvp/Concurrency.md`
  - Explainer: `vendor/component-model/design/mvp/Explainer.md`
- `vendor/wasmtime/` - a Wasm runtime with WASI P3 support
- `vendor/wasm-tools/` - the Wasm toolchain the Wado compiler builds on

`git submodule update --init --recommend-shallow` fetches them.

## Syncing

```sh
mise run sync-vendor
```

`vendor/wasmtime` goes to the exact version in `Cargo.lock`, which WASI P3
compatibility requires. The other submodules go to their remote HEAD.

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

Those four names redirect 31 crates. The `cranelift-*`, `pulley-*`,
`wasmtime-internal-*` and `wiggle*` crates follow through the dependency graph.
`sync-vendor` already holds the submodule at the `Cargo.lock` version, so the
sources match what the workspace expects and nothing needs porting.

Keep the stanza on a throwaway branch. CI checks out without submodules
(`submodules: false`, also `actions/checkout`'s default), so on `main` every job
dies while parsing the manifest. Landing it means turning submodule checkout on
across every workflow.

`reference/` holds patches written this way, each with its measurement:

- `current_thread_split.patch` — splits the cold deferred-frame replay out of
  `StoreOpaque::current_thread` so the fast path inlines. Removes ~1 point of
  CPU; throughput flat.
- `trace_fiber_roots.patch` — tracks fiber-owning reps instead of scanning the
  whole component `ResourceTable` per GC. Not measurable.
