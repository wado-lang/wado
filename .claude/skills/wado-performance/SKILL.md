---
name: wado-performance
description: Analyze and improve the runtime speed of a Wado program's compiled guest Wasm — profile hot functions, read the generated WIR for allocations and copies, reason about the WasmGC cost model, and A/B-measure a fix. Use for guest-side speed work (serde, parsers, numeric loops, hot stdlib). For host-side native compiler profiling see profiling-wado-compiler; for wrong-code in an optimizer pass see optimizer-debug.
---

# Wado performance

Speed of the **compiled guest Wasm** (what wasmtime runs), not the native
compiler — that is `profiling-wado-compiler`.

Loop: profile the hot function → read its WIR for what it allocates/copies per
iteration → change one thing → A/B same-machine best-of-three → keep or revert.

## 1. Profile

```sh
wado run --profile guest,profile.json,1 prog.wado   # interval 1 short runs, 0 exhaustive
node .claude/skills/wado-performance/scripts/analyze_guest_profile.ts profile.json [--top N]
```

The script reports self (leaf) and inclusive counts per function; names keep
monomorphization detail, so each instantiation is separate. Loop a one-shot hot
phase N times so it clears the fixed setup (aim ≥ ~200 samples). Firefox
Profiler (`profiler.firefox.com`) gives a flame graph; `perf` + `--profile
jitdump` gives instruction-level (store- vs compute-bound), see
`docs/jitdump-profiling.md`.

**Dev-profile inflation:** a `cargo run` `wado` JITs guest code near-release but
runs the wasmtime runtime / GC / allocator at dev speed, so profiles over-weight
allocation/GC frames — read percentages as relative and confirm a GC/alloc win
on a release build.

## 2. Read the WIR — allocations and copies first

```sh
wado dump -O2 prog.wado                 # final WIR
wado dump --tir-monomorphized prog.wado # how `?`, for-of, … desugar
```

Three villains, each a heap alloc or deep copy, and a bug when one lands **per
element** in a loop:

- **`struct.new` / `Box<…>`** — a heap object. `for x of &list` boxes every
  element (WasmGC has no interior references, so a by-ref iterator materializes
  `&T` as a box).
- **`array.new` / `array.new_default`** — a fresh GC array (`_default`
  zero-fills); watch for one per call where a buffer could be reused.
- **`$value_copy$T…`** — a value-semantics deep copy of a value-typed binding/arg
  unless the source is _fresh_ (a call / literal / variant result, or a fresh
  value's payload). `x?` desugars to `match f() {…}`, so freshness must see
  through the `match`; a missed copy shows up here and is removable.

Also: a `Trait::method(…)` call left in a hot loop (the inliner declined it), and
`array_set_u8` / `array_get` (bounds-checked; one per element is the store floor
for `Array<T>`-backed `String` / `List`).

## 3. WasmGC cost facts

- **Small-object churn is the GC cost**, not allocation — the `copying` collector
  re-traces live objects every cycle. Fix with fewer, flatter objects (a flat
  column store over a node tree). Measure the share with `--collector null` (no
  GC; it leaks, so drive a fixed iteration count) vs `--collector copying`.
- **GC-array access is bounds-checked, no unchecked variant.** A lookup table in
  a GC array adds a checked load per access — it lost to plain arithmetic.
- **Constant `/` and `%` are cheap** (Cranelift magic-multiply, `x/k` and `x%k`
  fused) — don't trade a divide for extra multiplies.
- **Write into the caller's buffer, not a temp.** `` `{v}` `` allocates a
  throwaway `String` and copies it in, per value; `buf.push_display(&v)` skips
  both. `reserve()` before a push burst pays one capacity check, not N.
- **`internal_raw_data()` / returning `Array<T>` by value is a copy API** — for a
  single read use `get_unchecked` / `set_byte_unchecked`.

## 4. Inlining is usually not the lever

wasmtime/Cranelift call small Wasm functions cheaply, so forcing inlining rarely
moves wall-time and raising the threshold bloats hot loops (measured slower). The
exception is a tight iteration-bound loop with a trivial body. When a hot leaf
has a rare heavy sub-case, split it: a tiny wrapper on the common path
(`if width > 0 { apply_padding_slow(…) }`) plus an out-of-line `#[inline(never)]`
helper — preferred over a `cold_path()` marker, which lies about branch
likelihood when the slow path is always taken once reached.

## 5. Measurement

Only relative numbers carry signal. A/B one change same-machine best-of-three,
back to back — never against another machine or session (a "regression" is often
a slower VM that hour; re-measure the unchanged reference to check). `vs best` in
`benchmark/README.md` is the metric; the `benchmark` skill + `pick.ts` do the
three passes. Isolate the phase — A/B a float-format change on `fts`, not on a
serialize benchmark that dilutes it.

## 6. Lessons

Reverted anti-wins, kept so they aren't retried: a GC-array digit table (checked
load per digit), two-digit arithmetic (the divides were already fused), forcing
inlining (loop bloat). Stop when the floor is the representation — a store-bound
loop on an `Array<T>`-backed `String` is near-optimal short of leaving GC arrays.

## See also

- `profiling-wado-compiler` — the native `wado` binary (host side).
- `benchmark` — run the suite / wasm-size report.
- `optimizer-debug` — a NIR/WIR pass producing _wrong_ code, not just slow.
