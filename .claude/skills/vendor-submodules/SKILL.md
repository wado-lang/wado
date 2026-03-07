---
name: vendor-submodules
description: to interact with the specs of Wasm and WASI (wasp-p3), or the implementations of wasm-tools and wasmtime
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
