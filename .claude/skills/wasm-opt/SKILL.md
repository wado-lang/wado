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
S=$(mktemp -d)
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
- `-O2 --skip-pass=NAME` measures what one pass is worth: the bytes `-O2` grows
  by without it. Passes overlap, so these do not add up to the whole gap.
  Running a pass alone misleads for the `-optimizing` variants, which rerun the
  whole function pipeline on what they touch.

To run the result, copy the jco output directory and replace its
`<name>.core.wasm`. Then run it as the `jco` skill describes. Keep a median of
at least 7 runs per variant, since the spread on a cloud VM is ±15%.

## Findings: zlib benchmark (binaryen 133, 2026-10)

Sizes of the zlib benchmark's core module:

| Variant           | Bytes  |
| ----------------- | ------ |
| Wado `-Os`        | 76,476 |
| wasm-opt, no pass | 76,943 |
| wasm-opt `-O2`    | 70,073 |
| wasm-opt `-O3`    | 67,979 |
| wasm-opt `-Oz`    | 67,853 |

Re-encoding alone makes the module larger, so Wado's encoding wastes nothing.

In 2026-09, on Node 26, decompression ran faster under wasm-opt `-O2` and
`-Os`: the median rose 10–40% across two sessions. Compression, and everything
under `-O3`, stayed within the noise.

What each `-O2` pass is worth, by `--skip-pass`:

| Pass                     | Bytes | What it does to Wado's output                    |
| ------------------------ | ----- | ------------------------------------------------ |
| `coalesce-locals`        | 2,230 | Mostly cleans up after inlining: 337 run alone   |
| `inlining-optimizing`    | 1,799 | Takes 114 functions to 53                        |
| `optimize-instructions`  | 1,164 | Peepholes, `ref.as_non_null` among them          |
| `remove-unused-brs`      | 1,062 | Turns `block`/`br_if`/`return` into `if`/`else`  |
| `precompute-propagate`   | 782   | Constant propagation                             |
| `dae-optimizing`         | 758   | Drops unused and always-constant parameters      |
| `heap2local`             | 664   | Turns non-escaping structs into locals           |
| `vacuum`, `merge-blocks` | 824   | Removes the blocks the passes above leave behind |

What wasm-opt removes, as open codegen targets:

- [x] Local declarations: the emitter wrote one entry per local, 2,942 entries
  and 7.1KB in all. Grouped by type, they take 346 entries and 1.0KB.
  `wir_optimize/local_layout.rs` groups them, and puts the most used first.
- [ ] Duplicate function types: 136 types, of which `-O2` keeps 69. Most of the
  rest go with the functions it inlines.
- [ ] `ref.as_non_null`: `-O2` takes the count from 388 to 16, and
  `optimize-instructions` alone to 49. About 320 wrap a `global.get` whose
  value goes straight to an `array.get`, `array.set` or `struct.get`, which
  traps on null by itself.
- [x] Set-then-get: a `local.set X` followed at once by `local.get X` occurred
  1,191 times. wasm-opt turns it into a `local.tee` or removes it. The last WIR
  pass fuses each pair into a tee (`fuse_remaining_local_tees`), and copy
  propagation removes a copy of a tee'd local. The `-Os` build keeps 39 pairs.
- [ ] Set sinking: `simplify-locals` moves a `local.set` into the one read of
  it, so the value stays on the stack. `-O2` removes 565 `local.set` and 725
  `local.get`, and adds 188 `local.tee`.
- [x] Local coalescing: wasm-opt merges locals whose live ranges never overlap,
  as a register allocator would. `wir_optimize/local_coalesce.rs` does the
  same per Wasm type. On the `-Os` build it takes 2,752 locals to 1,020.
- [ ] Control-flow shape: `-O2` takes `block` from 229 to 57, `br_if` from 191
  to 54, and `return` from 184 to 44, while `if` grows from 1,278 to 1,385.
- [ ] Single-caller inlining: `-O2` inlines functions with one caller whatever
  their size, `inflate_fast` among them.
- [ ] Identical functions: the `-Os` build has none. The `-O2` build keeps 9
  copies of the outlined `$cold0` bounds-check path of `List<T>::index_value`
  and `index_assign`, one per element type, and `i32`'s `Display::fmt` equals
  its `Inspect::inspect`. `--duplicate-function-elimination` saves 4,722 bytes
  there. Two `param_spec` clones whose bindings differ can fold to one body
  (`fmt_float_special$spec0` and `$spec1` in json_twitter).
- [x] Empty functions: a module whose globals all become constants keeps an
  empty `$initialize_module`. `wir_optimize/empty_work.rs` removes the calls
  to it and the once-flag that guarded them, and DCE drops both.
