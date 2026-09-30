---
name: wasm-opt
description: Run binaryen's wasm-opt over the core Wasm that Wado generates, and read what it changes. Use for any question about wasm-opt or binaryen, or to find where Wado's codegen leaves size or speed on the table.
---

# wasm-opt over Wado output

wasm-opt reads a core module, not a component. jco splits a Wado component and
hands back its core module, so the pipeline runs through jco. What wasm-opt then
removes shows what Wado's own codegen could remove.

## Setup

binaryen is not in the mise registry, and mise's `github:` backend gets a 403
from the GitHub API in a cloud session. Download the release tarball instead:

```sh
S=<scratchpad>
curl -sSL https://github.com/WebAssembly/binaryen/releases/download/version_133/binaryen-version_133-x86_64-linux.tar.gz | tar xz -C $S
B=$S/binaryen-version_133/bin
```

wasm-tools comes from mise. Where its shim reports no version set, call the
binary under `~/.local/share/mise/installs/wasm-tools/latest/` directly.

## Pipeline

```sh
wado compile -Os -f no-wide-arithmetic prog.wado -o $S/prog.wasm
node scripts/jco/transpile-released.mjs $S/prog.wasm $S/jco   # → $S/jco/prog.core.wasm
$B/wasm-opt $FEATURES -O3 $S/jco/prog.core.wasm -o $S/O3.wasm
```

- Make `-Os` the baseline. It is `-O2` with the name section stripped and
  assert messages dropped. An `-O2` baseline counts about 9KB of names that
  wasm-opt drops by default.
- `-f no-wide-arithmetic` is required, since the module is run on V8 (see the
  `jco` skill).
- Name the features explicitly. Never pass `-all`: it lets wasm-opt emit
  proposals V8 cannot load, compact imports among them. Wado needs this set:

  ```sh
  FEATURES="--mvp-features --enable-sign-ext --enable-mutable-globals \
    --enable-nontrapping-float-to-int --enable-simd --enable-relaxed-simd \
    --enable-bulk-memory --enable-bulk-memory-opt --enable-call-indirect-overlong \
    --enable-exception-handling --enable-tail-call --enable-reference-types \
    --enable-multivalue --enable-gc --enable-extended-const --enable-multimemory"
  ```

- Run with no pass (`$B/wasm-opt $FEATURES in.wasm -o rt.wasm`) as well. This
  re-encodes the module and nothing more, so it separates encoding waste from
  optimization.
- Add `-g` to keep function names for a diff. Measure size without it.

## Reading the difference

- `wasm-tools objdump` gives the size and count of each section.
- `wasm-tools dump` annotates every byte. Use it for encoding questions, such
  as how the local declarations are grouped.
- `wasm-tools print` gives WAT to diff. A whole-module diff is too large to
  read, so compare per-function instruction counts and opcode histograms with a
  small Node script instead.

To run the result, copy the jco output directory and replace its
`<name>.core.wasm`. Then run it as the `jco` skill describes. Keep a median of
at least 7 runs per variant, since the spread on a cloud VM is ±15%.

## Findings: zlib benchmark (binaryen 133, 2026-09)

Sizes of `zlib_bench.core.wasm`:

| Variant           | Bytes  |
| ----------------- | ------ |
| Wado `-Os`        | 90,877 |
| wasm-opt, no pass | 84,979 |
| wasm-opt `-O2`    | 73,013 |
| wasm-opt `-O3`    | 71,274 |
| wasm-opt `-Oz`    | 70,883 |

On Node 26, decompression ran faster under wasm-opt `-O2` and `-Os`: the median
rose 10–40% across two sessions. Compression, and everything under `-O3`,
stayed within the noise.

What wasm-opt removes, as open codegen targets. The counts after the first
come from Wado's `-O2` build:

- [ ] Local declarations: `codegen/emit.rs` writes one entry per local,
  `(1, ty)`, without merging neighbours of one type. That is 2,942 entries
  and 7.1KB, against 346 entries and 1.0KB once grouped by type.
- [ ] Duplicate function types: 167 types stand outside the rec group, and only
  104 of them differ.
- [ ] Nullable locals: every reference local is `ref null`, so each read of one
  carries a `ref.as_non_null`. wasm-opt makes the locals non-nullable,
  which takes the count from 1,869 to 113.
- [ ] Set-then-get: a `local.set X` followed at once by `local.get X` occurs
  1,191 times. wasm-opt turns it into a `local.tee` or removes it.
- [ ] Identical functions: `deflate_raw` equals `deflate_raw$spec0` and
  `zlib_wrap` equals `zlib_wrap$spec0`. The outlined `$cold0` bounds-check
  paths of `List<T>::index_value` and `index_assign` repeat once per element
  type with the same body.
