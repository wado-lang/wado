---
name: wado-performance
description: Analyze and improve the runtime speed of a Wado program's compiled guest Wasm — profile hot functions, read the generated WIR for allocations and copies, reason about the WasmGC cost model, and A/B-measure a fix. Use for guest-side speed work (serde, parsers, numeric loops, hot stdlib). For host-side native compiler profiling see profiling-wado-compiler; for wrong-code in an optimizer pass see optimizer-debug.
---

# Wado performance

Improve the runtime speed of the **compiled guest Wasm** a Wado program runs as
(the code wasmtime executes), not the native `wado` compiler — that is
`profiling-wado-compiler`.

Working loop: **profile** to find the hot function, **read its WIR** to see what
it allocates/copies/calls per iteration, change one thing, **A/B-measure**
same-machine best-of-three, keep or revert. One hypothesis per build — batching
several hides which helped and which regressed.

## 1. Profile

```sh
wado run --profile guest,profile.json,1 prog.wado
# dev: cargo run --release --bin wado -- run --profile guest,profile.json,1 prog.wado
```

`guest[,path[,interval_ms]]` — interval `1` for short runs, `0` for exhaustive
(exact call counts, ~10–15× overhead). Aim for ≥ ~200 samples; if the hot phase
is one-shot (a file read, one parse), loop it N times in a scratch program so it
dominates the fixed setup.

Aggregate it with the bundled script, which reports **both self (leaf) and
inclusive** — self is the code burning cycles, inclusive is the caller tree:

```sh
node .claude/skills/wado-performance/scripts/analyze_guest_profile.ts profile.json [--top N]
```

(The `.ts` runs directly on Node ≥ 23.6 via type stripping — no build step.)

Symbol names carry monomorphization detail
(`List<f64>^Serialize::serialize<core:json/JsonSerializer>`), so you see exactly
which instantiation is hot. Upload `profile.json` to
<https://profiler.firefox.com/> for a flame graph. For instruction-level
attribution (is the leaf store-bound or compute-bound?), Linux `perf` via
`--profile jitdump` — see `docs/jitdump-profiling.md`.

**Dev-profile inflation (read before trusting percentages).** With a `cargo run`
`wado`, `Cargo.toml` raises `opt-level` on `cranelift-codegen`, so JIT-compiled
**guest code is near-release**, but the wasmtime runtime, GC, and allocator run
at dev speed. The profile therefore **over-weights allocation/GC frames**
relative to pure compute. Read percentages as relative, and confirm a
GC/alloc-shaped win on a release build before believing its size.

## 2. Read the WIR — allocations and copies first

```sh
wado dump -O2 prog.wado                 # final WIR (what codegen emits)
wado dump --nir-lowered -O2 prog.wado   # NIR pre-optimize (what the optimizer started from)
wado dump --tir-monomorphized prog.wado # see how `?`, for-of, etc. desugar
```

Scan the hot function's body for the three usual guest-perf villains — each is a
GC allocation or a deep copy, and each is a bug when it lands **per element** in
a loop:

- **`struct.new` / `Box<…> { … }`** — a heap object. A `Box<T>` minted every
  iteration is the classic one: `for x of &list` boxes each element (WasmGC has
  no interior references, so a by-reference iterator materializes `&T` as a box).
- **`array.new` / `array.new_default`** — a fresh GC array; `array.new_default`
  also zero-fills. Watch for one per call where a buffer could be reused.
- **`$value_copy$T…(`** — a defensive deep copy inserted by value semantics on a
  value-typed (`struct`/`List`/tuple, not a reference) binding or by-value
  argument, unless the source is provably _fresh_ (aliases nothing: a call
  result, literal, variant construct, or the payload of a fresh value). `x?`
  desugars to `let x = match f() {…}`, so freshness must see through the `match`;
  a copy the analysis misses is removable and shows up here.

Also worth reading: `array_set_u8`/`array_get` (bounds-checked GC-array access —
one per element is the store floor for GC-array-backed `String`/`List`); a
`Foo^Trait::method(…)` **call** left in a hot loop; `_licm_…` locals (LICM
already hoisted a loop-invariant load). Confirm the optimizer did what you expect
before theorizing.

## 3. The WasmGC cost model

- **GC allocations are traced.** The default `copying` collector re-traces live
  objects every cycle, so **per-element object churn dominates** — thousands of
  small objects cost more in collection than in allocation. The structural fix is
  fewer, flatter objects (a flat column store beats a node tree), not a faster
  allocator. Measure the GC share directly:

  ```sh
  wado run --collector null    -O2 prog.wado   # no GC (leaks; use a fixed-iteration driver)
  wado run --collector copying -O2 prog.wado   # default
  ```

  `copying − null` is the collection cost. `null` leaks, so drive a fixed
  iteration count (the auto-tuned harness OOMs); never ship `drc` (pathological).

- **A GC-array access is bounds-checked** and has no unchecked variant. `String`
  and `List<T>` are `Array<T>`-backed, so writing N bytes is N bounds-checked
  stores — the floor. A lookup **table in a GC array makes it worse** (a bounds-
  checked load per access); it lost to plain arithmetic in the digit loop.

- **Constant `/` and `%` are cheap.** Cranelift strength-reduces them to a
  magic-multiply and fuses `x/k` with `x%k`. A digit loop is not division-bound;
  don't trade a fused divide for extra multiplies.

- **Write into the caller's buffer, not a temp.** A `` `{v}` `` template
  allocates a throwaway `String`, formats into it, then copies it in — per value.
  A direct writer (`buf.push_display(&v)`) skips both. `reserve()` before a burst
  of `push`es pays one capacity check instead of one per element.

- **`internal_raw_data()` / returning `Array<T>` by value is a copy API** (free
  only by grace of the freshness pass). For a single read use a per-element
  accessor (`get_unchecked`, `set_byte_unchecked`).

## 4. Inlining is usually not the lever

Understand the size heuristic
(`wado-compiler/src/optimize/inline.rs`: an expression-count threshold per `-O`
level, ×5 under `#[inline]`, pinned by `#[inline(never)]`), but **forcing
inlining of a small hot function rarely moves wall-time** — wasmtime + Cranelift
call small Wasm functions cheaply, and raising the inline threshold _bloats_ hot
loops and has measured _slower_. The real exception is a tight **iteration-bound**
loop whose body is trivial, where a per-element `next()` call is a large share; a
work-bound loop (heavy body) barely notices the same call. Measure before adding
a hint.

When a genuinely hot leaf has a rare heavy sub-case, split it: a tiny wrapper
keeps the common path (`if width > 0 { apply_padding_slow(…) }`) and the rare
logic goes to an out-of-line `#[inline(never)]` helper. `cold_path()` inside the
wrapper also drops the cold tail from the inline size count, but only pays off
when that tail is large, and it bakes a "branch unlikely" hint that lies if the
branch is taken whenever it is reached — prefer `#[inline(never)]` there.

## 5. Measurement discipline

Cloud VMs are noisy and absolute throughput is machine-dependent; only relative
numbers carry signal.

- **Same-machine A/B for one change.** Build binary A and B, measure both
  back-to-back best-of-three. Never compare against a number from another machine
  or session — the whole baseline drifts, and a "regression" is often just a
  slower VM that hour (re-measure the unchanged reference to check).
- **`vs best`** in `benchmark/README.md` is the metric (fastest row ÷ this row,
  within one run set). The `benchmark` skill + `benchmark/pick.ts` run three
  passes and pick the lowest `ms/iter` per row.
- **Isolate the phase.** To A/B a float-formatting change, `fts` (pure
  f64→string) is a cleaner signal than a serialize benchmark where formatting is
  diluted by the sequence machinery.

## 6. Wins and anti-wins seen so far

Wins (each A/B-confirmed): direct-to-buffer numeric formatting instead of a temp
`String`; `reserve()` + a capacity-skipping push for a fixed-width byte burst;
eliding the value-copy of a fresh `match`/`?` result (a compiler fix that also
_shrank_ parser-heavy Wasm by shedding `$value_copy$T` helpers); a
fast-path/cold-path split of a per-value formatter helper; retiring a per-node
object tree for a flat column store (turned a GC-bound parser compute-bound).

Anti-wins (measured worse, reverted): a GC-array digit table (bounds-checked load
per digit); two-digits-per-step arithmetic (the divides were already fused);
forcing inlining / raising the inline threshold (loop bloat). The recurring
lesson: match effort to where the samples land, and stop when the floor is the
representation (a store-bound loop on a GC-array-backed `String` is near-optimal
short of moving strings off GC arrays).

## See also

- `profiling-wado-compiler` — profile the native `wado` binary (host side).
- `benchmark` — run the suite / wasm-size report and refresh the READMEs.
- `optimizer-debug` — a NIR/WIR pass producing _wrong_ code, not just slow.
- `jco` — run/benchmark the transpiled component on Node.

Cleanup: `rm -f profile.json`.
