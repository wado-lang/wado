# wado-bundled-icu (technical-validation spike)

A proof-of-concept that bundles [ICU4X](https://github.com/unicode-org/icu4x)
into a self-contained **Wasm Component Model** component, the way
`wado-bundled-libm` bundles libm. This crate is **not** wired into the compiler
yet; it exists to validate feasibility, the toolchain, and artifact size.

It is its own Cargo workspace root so its heavy dependency tree (icu +
wit-bindgen) never perturbs the main workspace's pinned wasm-tools generation.

## What it exposes

See [`wit/world.wit`](wit/world.wit). The current surface is the
**string-oriented** slice of ICU4X plus character properties:

| interface    | operations                                                                     |
| ------------ | ------------------------------------------------------------------------------ |
| `locale`     | parse BCP-47, canonical string (opaque `resource`)                             |
| `casemap`    | upper / lower / title casing, case folding                                     |
| `collator`   | locale-aware string comparison (opaque `resource`)                             |
| `normalizer` | NFC / NFD / NFKC / NFKD, is-nfc                                                |
| `segmenter`  | grapheme / word / sentence / line boundaries                                   |
| `properties` | General_Category, Script, Alphabetic, White_Space, Uppercase, Lowercase, Emoji |

It exercises every marshalling shape Wado needs: `string` in/out, `list<u32>`,
`char`, `enum`, `result<_, string>`, opaque `resource` handles, and
`borrow<resource>` params.

## Size

Import-free component, **~3.7 MB** with all six interfaces. Because Rust LTO
slices ICU by reachability, the WIT surface is the size knob when rebuilding. Per-interface
attribution (measured by building with interfaces removed):

| interface          | added size | notes                                                        |
| ------------------ | ---------: | ------------------------------------------------------------ |
| segmenter (`auto`) |   ~2.35 MB | LSTM + CJK/SE-Asian **dictionary** data — by far the largest |
| collator           |   ~1.12 MB | root UCA collation table                                     |
| normalizer         |    ~125 KB | NFC/NFD/NFKC/NFKD tables                                     |
| locale + casemap   |     ~92 KB | baseline                                                     |
| properties         |     ~44 KB | the gc/script tries + binary sets are cheap                  |

So segmenter+collator are ~93% of the bytes; dropping word/line segmentation
(the `auto` dictionary) or collation shrinks the bundle dramatically.

## Post-hoc slicing: one asset, sliced per program

The per-interface table above comes from rebuilding with interfaces removed. The
same slicing is reachable **without rebuilding**, which is what lets a toolchain
carry one ICU asset and give each program only what it reaches.

wasm-ld's `--gc-sections` (its default) collects over the symbol graph the
`linking` and `reloc.*` sections carry. Keeping that graph in the asset lets the
collection run again later against a narrower root set:

```sh
# Ship the asset as a relocatable object (keeps linking + reloc.*)
RUSTFLAGS="-C link-arg=--relocatable -C link-arg=--no-gc-sections" \
  CARGO_PROFILE_RELEASE_STRIP=none cargo build --release   # 4.05 MB

# Collect it against the exports one program actually reaches
rust-lld -flavor wasm --no-entry --gc-sections --strip-all \
  --export=cabi_realloc_wit_bindgen_0_58_0 --export=__wasm_call_ctors \
  --export='wado:icu/casemap@0.1.0#uppercase' ... \
  -o casemap.wasm wado_bundled_icu.wasm                    # 89 KB
```

Measured against the 4.05 MB relocatable object:

| roots kept         |      size | rebuild equivalent       |
| ------------------ | --------: | ------------------------ |
| locale             |     35 KB | —                        |
| locale + casemap   |     89 KB | ~92 KB                   |
| properties         |     67 KB | —                        |
| normalizer         |    134 KB | ~125 KB                  |
| collator           |   1.17 MB | ~1.12 MB                 |
| segmenter (all 4)  |   2.31 MB | ~2.35 MB                 |
| **graphemes only** | **23 KB** | not reachable by rebuild |
| every export       |   3.63 MB | ~3.7 MB                  |

Each slice lands on its from-source rebuild, so the graph carries everything the
collection needs, data included. And the root set can be finer than a rebuild
can express: `graphemes only` would need the WIT surface split first.

Correctness is checked, not inferred. `runtime-check` passes against the
all-exports re-link, and its `casemap-only` binary asserts Turkish, German and
en-US casing against the 93 KB component built from the re-linked slice
(`wit-casemap/` narrows the world) — locale-aware data that survived the
collection.

```sh
cd runtime-check && cargo run --release --bin casemap-only -- <component.wasm>
```

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

Result: a component whose `world` has only exports, no imports (see Size above).

### Alternative: wasm32-wasip2

Targeting `wasm32-wasip2` with std lets `cargo build` emit a component directly
(its linker runs `wasm-component-ld`), but std pulls in ~13 `wasi:cli`/`wasi:io`
imports. Kept here only as a note; the no_std path above is preferred for a
bundled asset.

## Runtime check

[`runtime-check/`](runtime-check/) instantiates the built component under
wasmtime and calls into it, proving the baked Unicode/CLDR data is live across
the component boundary — including the locale `resource` path (Turkish
`istanbul` → `İSTANBUL`, not ASCII toupper), NFC/NFD round-trips, grapheme
segmentation of a ZWJ family-emoji cluster, and character properties.

```sh
cd runtime-check && cargo run --release
```

## Key findings

- Full `icu` (all components, `compiled_data`) compiles to wasm cleanly.
- `wit-bindgen` marshals strings, resources, borrows and results across CM WIT.
- Rust LTO tree-shakes ICU by reachability: only what the WIT surface uses
  survives, so the WIT surface is the size knob (see Size above).
- no_std + wasm32-unknown-unknown yields a zero-import, fully self-contained
  component — the same self-contained model as `wado-bundled-libm`.

## Follow-up: data/code separation

This spike bakes data into the component via `compiled_data`. The alternative —
a **data-free feature component** that loads a postcard blob at runtime via
ICU4X's `BlobDataProvider`, with the blob supplied by a shared **data** component
composed in over the Component Model — is validated in [`bdp-spike/`](bdp-spike/).
