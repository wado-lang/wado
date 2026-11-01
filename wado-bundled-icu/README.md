# wado-bundled-icu

Bundles [ICU4X](https://github.com/unicode-org/icu4x) into a self-contained
**Wasm Component Model** component, the way `wado-bundled-libm` bundles libm.
See [WEP: `core:icu`](../docs/wep-2026-08-09-core-icu.md).

It is its own Cargo workspace root so its heavy dependency tree (icu +
wit-bindgen) never perturbs the main workspace's pinned wasm-tools generation.

The crate builds one of two worlds:

| build                        | world            | WIT               | what it is                                   |
| ---------------------------- | ---------------- | ----------------- | -------------------------------------------- |
| `cargo build --release`      | `icu-properties` | `wit-properties/` | the shippable `core:icu` surface             |
| `--release --features spike` | `icu`            | `wit/`            | the wider technical-validation surface below |

## The shippable surface

It exports four interfaces: character properties, collation, case mapping, and
grapheme segmentation. A locale crosses as its language, script and region
subtags, each packed into a `u32`, so nothing on this side parses a tag:
`core:icu` checks it once, in Wado.

### Character properties

Each property has three exports of its own: its ranges, whether it holds one
character, and the parser of its value names, strict or loose per UAX #44. A
value crosses as its position in the property's list, a `u16`. Since a call
reaches one property's exports, the toolchain's collection keeps that
property's data alone.

The property interface is generated, with the Rust tables behind it and the
Wado enums `core:icu` names the values with. `gen/` holds the generator, and
`mise run update-bundled-icu` runs it before the build. It covers 63 binary
properties, 15 enumerated ones, and the General_Category groups (`L` as well as
`Lu`).

Three things it deliberately does not answer:

- **`Block`.** ICU4X ships no Block data.
- **The UCD's contributory `Other_*` properties.** UAX #44 defines them as
  inputs to derived properties rather than as properties to query, and ICU4X
  follows that.
- **`Decomposition_Type`.** ICU4X ships no data for it.

Each name is refused rather than answered with a close-enough set. A caller complementing an
approximate set would admit real members of the property instead of missing
some.

None of the three occurs in a live rule anywhere in
[antlr/grammars-v4](https://github.com/antlr/grammars-v4) (1,150 grammars).
`Block` and `Decomposition_Type` never appear at all. Every `Other_*`
occurrence sits in a comment: Python's and Rust's lexers spell the code points
out by hand instead.

### Collation

`collator` is a resource with a destructor: `create` takes a locale and
`compare` orders two strings the way that locale sorts them. Collation data is
baked for every locale, 1.34 MB of the core module. A program that never
constructs a collator collects it away.

### Case mapping and graphemes

`casemap` maps and folds by the language subtag alone, the only one that selects
a tailoring. `fold` picks ICU4X's Turkic folding for `tr` and `az`.
`segmenter` returns where each grapheme cluster ends, or only how many there
are. The core module holding all four interfaces is 1.68 MB, the size of
`target/wasm32-unknown-unknown/release/wado_bundled_icu.wasm` after
`cargo build --release`.

## What the spike surface exposes

See [`wit/world.wit`](wit/world.wit). It is the **string-oriented** slice of
ICU4X plus per-character properties:

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

## Size of the spike surface

Import-free component, **~3.7 MB** with all six interfaces. Because Rust LTO
slices ICU by reachability, the WIT surface is the size knob when rebuilding.
Per-interface attribution, measured by building with interfaces removed:

| interface          | added size | notes                                       |
| ------------------ | ---------: | ------------------------------------------- |
| segmenter (`auto`) |   ~2.35 MB | LSTM + CJK/SE-Asian **dictionary** data     |
| collator           |   ~1.12 MB | root UCA collation table                    |
| normalizer         |    ~125 KB | NFC/NFD/NFKC/NFKD tables                    |
| locale + casemap   |     ~92 KB | baseline                                    |
| properties         |     ~44 KB | the gc/script tries + binary sets are cheap |

So segmenter+collator are ~93% of the bytes. Dropping word/line segmentation
(the `auto` dictionary) or collation shrinks the bundle dramatically.

The 44 KB row answers seven properties by name in the WIT. The shippable
surface answers every one, and the toolchain collects it down to the lifted
functions a program imports. The WEP measures what a program carries.

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
(`wit-casemap/` narrows the world). That is locale-aware data surviving the
collection.

```sh
cd runtime-check && cargo run --release --bin casemap-only -- <component.wasm>
```

## Build: no_std, zero-import, self-contained, the libm model

The crate is `#![no_std]`, supplies its own global allocator (dlmalloc), and
maps `panic` straight to a Wasm trap (Wado has no exceptions). It builds for
`wasm32-unknown-unknown` (no WASI, no std) so the module imports nothing.

```sh
# 1. Compile the no_std core module (target set in .cargo/config.toml).
#    Add `--features spike` for the wider surface the sections above measure.
#    The relocations are what the data-reference map is resolved from.
RUSTFLAGS="-C link-arg=--emit-relocs" CARGO_PROFILE_RELEASE_STRIP=none cargo build --release

# 2. Wrap it into a validated component using the component-type section
#    wit-bindgen embedded, printed as WAT. No WASI adapter is needed because
#    there are no imports.
../scripts/dev-bin.sh wado-dev-tools bundled-asset --component \
  target/wasm32-unknown-unknown/release/wado_bundled_icu.wasm \
  > target/wado_bundled_icu.component.wat
```

Result: a component whose `world` has only exports, no imports.
`mise run update-bundled-icu` runs both steps into `wado-compiler/lib/core/icu.wat`.

### Alternative: wasm32-wasip2

Targeting `wasm32-wasip2` with std lets `cargo build` emit a component directly
(its linker runs `wasm-component-ld`), but std pulls in ~13 `wasi:cli`/`wasi:io`
imports. Kept here only as a note; the no_std path above is preferred for a
bundled asset.

## Runtime check

[`runtime-check/`](runtime-check/) instantiates the spike component under
wasmtime and calls into it, proving the baked Unicode/CLDR data is live across
the component boundary. It covers the locale `resource` path (Turkish
`istanbul` → `İSTANBUL`, not ASCII toupper), NFC/NFD round-trips, grapheme
segmentation of a ZWJ family-emoji cluster, and character properties. It binds
`wit/`, so build the asset with `--features spike` first.

```sh
cd runtime-check && cargo run --release
```

## Key findings

- Full `icu` (all components, `compiled_data`) compiles to wasm cleanly.
- `wit-bindgen` marshals strings, resources, borrows and results across CM WIT.
- Rust LTO tree-shakes ICU by reachability: only what the WIT surface uses
  survives, so the WIT surface is the size knob.
- no_std + wasm32-unknown-unknown yields a zero-import, fully self-contained
  component, the same model as `wado-bundled-libm`.

## Follow-up: data/code separation

Both worlds bake their data in via `compiled_data`. The alternative is a
**data-free feature component** that loads a postcard blob at runtime via
ICU4X's `BlobDataProvider`, with the blob supplied by a shared **data**
component composed in over the Component Model. `bdp-spike/` validated it and
was then removed. At e52a46286, `bdp-spike/build.sh` reproduces its
measurements.
