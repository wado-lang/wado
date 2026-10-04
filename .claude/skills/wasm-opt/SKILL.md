---
name: wasm-opt
description: Run binaryen's wasm-opt over the core Wasm that Wado generates, and read what it changes. Use for any question about wasm-opt or binaryen, or to find where Wado's codegen leaves size or speed on the table.
---

# wasm-opt over Wado Output

wasm-opt reads a core module, so the component goes through jco first. What
wasm-opt then removes is what Wado's codegen could remove.

## Setup

binaryen is not in the mise registry, and mise's `github:` backend gets a 403 in
a cloud session:

```sh
mkdir -p /tmp/binaryen
curl -sSL https://github.com/WebAssembly/binaryen/releases/download/version_133/binaryen-version_133-x86_64-linux.tar.gz | tar xz -C /tmp/binaryen
```

Where the mise shim for wasm-tools reports no version, call it under
`~/.local/share/mise/installs/wasm-tools/latest/`.

## Pipeline

```sh
B=/tmp/binaryen/binaryen-version_133/bin
S=/tmp/wasm-opt && mkdir -p $S
FEATURES=(--mvp-features --enable-sign-ext --enable-mutable-globals
  --enable-nontrapping-float-to-int --enable-simd --enable-relaxed-simd
  --enable-bulk-memory --enable-bulk-memory-opt --enable-call-indirect-overlong
  --enable-exception-handling --enable-tail-call --enable-reference-types
  --enable-multivalue --enable-gc --enable-extended-const --enable-multimemory)
wado compile -Os -f no-wide-arithmetic prog.wado -o $S/prog.wasm
node scripts/jco/transpile-released.mjs $S/prog.wasm $S/jco   # → $S/jco/prog.core.wasm
$B/wasm-opt "${FEATURES[@]}" -O3 $S/jco/prog.core.wasm -o $S/O3.wasm
```

- `-Os` is the baseline: an `-O2` one counts names wasm-opt drops anyway.
- `-f no-wide-arithmetic` because V8 runs it (the `jco` skill).
- Never `-all`: it emits proposals V8 cannot load.
- A run with no pass only re-encodes, separating encoding waste from
  optimization. `-g` keeps names for a diff; measure size without it.

## Reading the Difference

- `wasm-tools objdump` for section sizes, `wasm-tools dump` for encoding, and
  per-function opcode histograms over `wasm-tools print` rather than a
  whole-module diff.
- `-O2 --skip-pass=NAME` measures one pass: the bytes `-O2` grows without it.
  The numbers overlap, and a pass run alone misleads for `-optimizing` variants.
- To run the result, replace `<name>.core.wasm` in a copy of the jco output and
  run it as the `jco` skill says, a median of at least 7 runs per variant.

## Open Codegen Targets

Measured on the zlib benchmark with binaryen 133, where wasm-opt `-O2` also made
decompression faster on Node while compression stayed flat. Each remains
something wasm-opt removes and Wado does not:

- [ ] Duplicate function types: `-O2` keeps about half, most of the rest going
  with the functions it inlines.
- [ ] Set sinking: `simplify-locals` moves a `local.set` into its one read.
- [ ] Control-flow shape: `-O2` turns `block`/`br_if`/`return` into `if`/`else`.
- [ ] Single-caller inlining, whatever the callee's size (`inflate_fast`).
- [ ] Identical functions at `-O2`: the outlined `$cold0` bounds check of
  `List<T>::index_value` / `index_assign` per element type, `i32`'s
  `Display::fmt` and `Inspect::inspect`, and `param_spec` clones whose
  bindings differ.
