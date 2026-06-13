# wado-bundled-icu (technical-validation spike)

A proof-of-concept that bundles [ICU4X](https://github.com/unicode-org/icu4x)
into a self-contained **Wasm Component Model** component, the way
`wado-bundled-libm` bundles libm. This crate is **not** wired into the compiler
yet; it exists to validate feasibility, the toolchain, and artifact size.

It is its own Cargo workspace root so its heavy dependency tree (icu +
wit-bindgen) never perturbs the main workspace's pinned wasm-tools generation.

## What it exposes

See [`wit/world.wit`](wit/world.wit). The surface is a deliberately small
cross-section chosen to exercise every marshalling shape (string in/out,
`resource` handles, `borrow<resource>`, `result<_, string>`): `locale` parsing
and `casemap` (locale-aware upper/lower casing). It grows once the surface is
agreed.

## Build (no_std, zero-import, self-contained — the libm model)

The crate is `#![no_std]`, supplies its own global allocator (dlmalloc), and
maps `panic` straight to a Wasm trap (Wado has no exceptions). It builds for
`wasm32-unknown-unknown` (no WASI, no std) so the module imports nothing.

```sh
# 1. Compile the no_std core module (target set in .cargo/config.toml)
cargo build --release

# 2. Wrap it into a component using the component-type section wit-bindgen
#    embedded. No WASI adapter is needed because there are no imports.
wasm-tools component new \
  target/wasm32-unknown-unknown/release/wado_bundled_icu.wasm \
  -o target/wado_bundled_icu.component.wasm

# 3. Confirm it is a valid, import-free component
wasm-tools validate target/wado_bundled_icu.component.wasm
wasm-tools component wit target/wado_bundled_icu.component.wasm
```

Result: a ~72 KB component whose `world` has only exports, no imports.

### Alternative: wasm32-wasip2

Targeting `wasm32-wasip2` with std lets `cargo build` emit a component directly
(its linker runs `wasm-component-ld`), but std pulls in ~13 `wasi:cli`/`wasi:io`
imports. Kept here only as a note; the no_std path above is preferred for a
bundled asset.

## Runtime check

[`runtime-check/`](runtime-check/) instantiates the built component under
wasmtime and calls into it, proving the baked CLDR data is live across the
component boundary (e.g. Turkish `istanbul` → `İSTANBUL`, not ASCII toupper).

```sh
cd runtime-check && cargo run --release
```

## Key findings

- Full `icu` (all components, `compiled_data`) compiles to wasm cleanly.
- `wit-bindgen` marshals strings, resources, borrows and results across CM WIT.
- Rust LTO tree-shakes ICU by reachability: only what the WIT surface uses
  survives (locale + casemap ⇒ ~72 KB). Exposing more in WIT pulls in more data.
- no_std + wasm32-unknown-unknown yields a zero-import, fully self-contained
  component — the same self-contained model as `wado-bundled-libm`.
