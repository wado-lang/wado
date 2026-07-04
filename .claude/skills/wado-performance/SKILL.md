---
name: wado-performance
description: Analyze and improve the runtime performance of a Wado program's compiled guest Wasm — profile hot functions, read the generated WIR/NIR, reason about the WasmGC cost model, and A/B-measure a fix. Use for guest-side speed work (serde, parsers, numeric loops, hot stdlib). For host-side native compiler profiling see profiling-wado-compiler; for wrong-code in an optimizer pass see optimizer-debug.
---

# Wado performance

Improve the runtime speed of the **compiled guest Wasm** a Wado program runs as.
This is guest-side work (the code wasmtime executes), not the native `wado`
compiler (that is `profiling-wado-compiler`).

The one rule that overrides everything below: **measure, never assume.** Wasm/GC
cost intuitions from native targets are frequently wrong here (see the cost
model). Every change is a hypothesis until an A/B says otherwise — this session
shipped a "2-digit lookup table" that *regressed* the very loop it targeted.

## The loop

1. **Profile** to find the hot function(s) — where the samples actually land.
2. **Read the generated code** (`wado dump`) for that function — what is it
   really doing per iteration (allocs, bounds-checked stores, copies, calls)?
3. **Form one hypothesis** about the dominant cost.
4. **A/B measure** it: build with and without the change, same machine,
   best-of-three, back to back. Keep only if it wins; revert if flat or worse.
5. Repeat on the next hot spot.

Do not batch several speculative changes into one build — you won't know which
helped and which regressed.

## 1. Profile (guest profiler)

```sh
wado run --profile guest,profile.json,1 prog.wado
# during dev: cargo run --release --bin wado -- run --profile guest,profile.json,1 prog.wado
```

Parameters: `guest[,path[,interval_ms]]`. Interval `1` for short runs, `10`
(default) for >2 s, `0` for exhaustive (~10–15× overhead, exact call counts).
Aim for ≥ ~200 samples: if a phase is one-shot (file read, one parse), loop the
hot phase N times in a scratch program so its samples dominate the fixed setup.

Parse the Firefox-Profiler JSON reporting **both self (leaf) and inclusive** —
self tells you the code actually burning cycles, inclusive tells you the caller
tree:

```sh
python3 -c "
import json,sys; from collections import Counter
t=json.load(open(sys.argv[1]))['threads'][0]
S=t['stringArray']; sm=t['samples']; st=t['stackTable']; ft=t['frameTable']; fn=t['funcTable']
inc=Counter(); slf=Counter()
def name(k): fr=st['frame'][k]; return S[fn['name'][ft['func'][fr]]]
for k in sm['stack']:
    if k is None: continue
    slf[name(k)]+=1; seen=set(); c=k
    while c is not None:
        nm=name(c)
        if nm not in seen: inc[nm]+=1; seen.add(nm)
        c=st['prefix'][c]
tot=sm['length']; print('total',tot)
print('--- SELF (leaf) ---')
for nm,c in slf.most_common(20): print(f'{c:6d} {100*c/tot:5.1f}%  {nm}')
print('--- INCLUSIVE ---')
for nm,c in inc.most_common(15): print(f'{c:6d} {100*c/tot:5.1f}%  {nm}')
" profile.json
```

Upload `profile.json` to <https://profiler.firefox.com/> for a flame graph.
Symbol names carry monomorphization detail
(`List<f64>^Serialize::serialize<core:json/JsonSerializer>`), so you can tell
exactly which instantiation is hot.

For **instruction-level** attribution (is it the divide or the store?), Linux
`perf` via `--profile jitdump` — see `docs/jitdump-profiling.md`. Function-level
sampling can't tell you which instruction inside a hot leaf dominates; when a
leaf is store-bound vs compute-bound is the question, go to jitdump.

## 2. Read the generated code (`wado dump`)

Look at what the function compiled to before theorizing:

```sh
wado dump -O2 prog.wado                 # final WIR (what codegen emits)
wado dump --nir -O2 prog.wado           # optimized NIR
wado dump --nir-lowered -O2 prog.wado   # NIR right after lowering (pre-optimize)
wado dump --tir-monomorphized prog.wado # TIR (see how `?`, for-of, etc. desugar)
```

Grep the hot function's body for the tell-tale costs:

- `$value_copy$T…(` — a defensive deep copy (value semantics). One per value-typed
  binding/argument that the analysis couldn't prove fresh.
- `struct.new` / `Box<…> { … }` / `array.new` — a GC allocation. Watch for one
  **per element** in a loop (e.g. `Box<f64>` minted by a by-reference iterator).
- `array_set_u8` / `array_get` / `array.set` / `array.get` — a **bounds-checked**
  GC-array access. One per element is the floor for GC-array-backed data.
- a `Foo^Trait::method(…)` **call** left in a hot loop — the inliner declined it
  (see §4). `_licm_…` locals mean LICM already hoisted a loop-invariant load.

Confirm what the optimizer *did*: e.g. `buf.repr` hoisted out of a write loop, a
method inlined (no call site left), a `?`-match value-copy elided.

## 3. Benchmark discipline

Cloud VMs are noisy and **absolute throughput is meaningless** — only relative
numbers carry signal:

- **Same-machine A/B for a single change.** Build binary A and binary B, measure
  both back-to-back best-of-three. Never compare against a number from another
  machine or another session (the whole baseline drifts; a "regression" is often
  just a slower VM that day — verify by re-measuring the unchanged reference).
- **`vs best` is the metric** in `benchmark/README.md` — fastest row over this
  row, computed within one run set. Rebuild every row from one consistent run
  when refreshing.
- **Best-of-three**, lowest `ms/iter` per row (throughput ∝ 1/ms for fixed
  work). The `benchmark` skill + `benchmark/pick.ts` automate this.
- Isolate the phase: to A/B `write_digits_at`, `fts` (a pure f64→string bench)
  is a cleaner signal than canada serialize, where float formatting is diluted
  by the sequence machinery.

## 4. WasmGC guest cost model

The intuitions that repeatedly bit in this session:

- **A GC-array access is bounds-checked.** `array.get`/`array.set` trap on OOB;
  there is no unchecked variant. `String` and `List<T>` are backed by `Array<T>`,
  so **writing N bytes costs N bounds-checked stores — that is the floor.** A
  lookup **table stored in a GC array makes things worse**: it adds a
  bounds-checked `array.get` per access. The 2-digit digit table regressed
  `write_digits_at` for exactly this reason; the plain per-digit loop won.
- **Integer `/` and `%` by a constant are cheap.** cranelift strength-reduces
  them to a magic-multiply and fuses `x/k` with `x%k` (one reciprocal, then
  `x - (x/k)*k`). So a digit loop is **not** division-bound — halving the divides
  (2 digits per step) added a multiply and *lost*. Don't "optimize" constant
  division.
- **Value semantics inserts deep copies.** A value-typed (`struct`/`List`/tuple,
  not a reference) binding or by-value argument is wrapped in `$value_copy$T`
  unless the source is provably *fresh* (an rvalue that aliases nothing — a call
  result, a literal, a variant construct, the payload of a fresh value). A copy
  the analysis misses shows up as `$value_copy$T…` in the WIR and is often
  removable. `x?` desugars to `let x = match f() {…}`; freshness must see through
  the `match`.
- **Prefer writing into the caller's buffer over a temp.** `` `{v}` `` (a
  template) allocates a throwaway `String`, formats into it, then copies it into
  the real buffer — per value. A direct writer (`buf.push_display(&v)`) skips the
  alloc and the copy.
- **`reserve()` before a burst.** `List::push` checks capacity every call; one
  `reserve(n)` + a burst of `push_within_capacity` pays one check instead of n.
- **`for x of &list` boxes every element.** WasmGC has no interior references, so
  a by-reference iterator materializes a `Box<T>` per element to yield `&T`. This
  is inherent to the representation; the per-element box survives even when the
  iterator is inlined. Iteration-bound loops pay it heavily; work-bound loops
  (a heavy body) barely notice.
- **`internal_raw_data()` / returning `Array<T>` by value is a copy API.** It is
  copy-free today only by grace of the freshness optimization. For a single read,
  use a per-element accessor (`get_unchecked`, `set_byte_unchecked`) so no
  backing array is ever materialized.

## 5. The guest inliner

Inlining a small function out of a hot loop removes the call, but the size
heuristic is conservative. Model in `wado-compiler/src/optimize/inline.rs`:

- **Threshold by `-O`:** O1 = 4, O2 = 13 (default), O3 = 32, `-Os` = 13. `#[inline]`
  multiplies the threshold ×5; `#[inline(never)]` pins a function out-of-line.
- **Size is an expression count** (`count_block_exprs`). Field accesses,
  `struct.new`/box literals, and `x += 1` each cost more than they look — a
  4-line `Iterator::next` measured **20**, and `SerializeSeq::element` measured
  **25–30**, both well over 13. So they need `#[inline]`, not a small threshold
  bump. (Get the real number by a temporary `eprintln!` in `is_inline_eligible`;
  don't hand-count.)
- **`cold_path()` zeroes the tail in the size count.** `count_block_exprs` stops
  at a `cold_path()` marker (and at a diverging `return`/`break`). So a
  fast-path wrapper whose rare branch is a *large* cold call becomes tiny and
  inlines with **no hint** — but only when the excluded tail is large. It shaved
  just 1 off `next()` (the excluded tail was a bare `return null`), and did *not*
  make it inline.
- **Fast-path / slow-path split** (the reusable pattern): keep the hot common
  path a couple of ops in a tiny wrapper and push the rare/large logic into an
  out-of-line helper. `apply_padding` became `if self.width > 0 {
  apply_padding_slow(…) }` + `#[inline(never)] fn apply_padding_slow`. Prefer
  `#[inline(never)]` on the slow fn over a `cold_path()` marker in the wrapper
  when the branch is genuinely taken every time it *is* reached (a width IS set)
  — `cold_path()` bakes a "this branch is unlikely" hint that would be a lie
  there; `#[inline(never)]` states only "keep it out-of-line."

## 6. Recurring wins & anti-wins (from the canada-serde work)

Wins (each A/B-confirmed):

- Direct-to-buffer numeric formatting via a `String` extension (`push_display`)
  instead of a `` `{v}` `` temp — removes an alloc + copy per number.
- `reserve()` + `push_within_capacity` for a fixed-width byte burst (CBOR f64).
- Value-copy elision for fresh `match`/`?` results — a compiler fix that also
  **shrank** parser-heavy Wasm (sqlite_highlight −23%) by shedding `$value_copy$T`
  helpers.
- Fast-path/cold-path split of a per-value formatter helper (`apply_padding`) —
  general `Display` win, `fts` +6%.
- Hot branch first: `if cond { hot } else { cold }` over `if !cond { cold } else
  { hot }` — readability, negligible runtime (cranelift reorders blocks anyway).

Anti-wins (measured worse, reverted):

- A GC-array 2-digit lookup table for `u64`→decimal — a bounds-checked load per
  digit beat the arithmetic it replaced.
- 2-digits-per-iteration arithmetic in the digit loop — the constant divides
  were already fused, so it only added ops.

The lasting lesson: `write_digits_at` (the hottest leaf, ~31% of canada JSON
serialize) is **store-bound** on the GC-array-backed `String`; short of moving
strings off GC arrays, the simple per-digit loop is near-optimal. Match the
effort to where the samples are, and stop when the floor is the representation.

## Cleanup

```sh
rm -f profile.json
```

## See also

- `profiling-wado-compiler` — profile the native `wado` binary (host side).
- `benchmark` — run the benchmark suite / wasm-size report and refresh the READMEs.
- `optimizer-debug` — when a NIR/WIR pass produces *wrong* code (not just slow).
- `jco` — run/benchmark the transpiled component on Node.
