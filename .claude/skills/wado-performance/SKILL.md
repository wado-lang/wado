---
name: wado-performance
description: Analyze and improve the runtime speed of a Wado program's compiled guest Wasm — profile hot functions, read the generated WIR for allocations and copies, reason about the WasmGC cost model, and A/B-measure a fix. Use for guest-side speed work (serde, parsers, numeric loops, hot stdlib). For host-side native compiler profiling see profiling-wado-compiler; for wrong-code in an optimizer pass see optimizer-debug.
---

# Wado performance

Speed of the **compiled guest Wasm** (what wasmtime runs), not the native
compiler — that is `profiling-wado-compiler`.

Loop: profile the hot function → read its WIR for what it allocates/copies per
iteration → change one thing → A/B both arms in one session, plus the WIR diff of
the hot function → keep or revert (§5 says which evidence decides).

## 1. Profile

```sh
wado run --profile guest,profile.json,1 prog.wado   # interval 1 short runs, 0 exhaustive
wado test --profile guest,profile.json,1 file.wado  # same, over one file's test blocks
node .claude/skills/wado-performance/scripts/analyze_guest_profile.ts profile.json [--top N]
```

The script reports self (leaf) and inclusive counts per function; names keep
monomorphization detail, so each instantiation is separate. Loop a one-shot hot
phase N times so it clears the fixed setup (aim ≥ ~200 samples). Firefox
Profiler (`profiler.firefox.com`) gives a flame graph; `perf` + `--profile
jitdump` gives instruction-level (store- vs compute-bound), see
`docs/jitdump-profiling.md`.

**Dev-profile inflation:** a `cargo run` `wado` JITs guest code near-release but
runs the wasmtime runtime / GC / allocator at dev speed (~4–5× slower), so
profiles over-weight allocation/GC frames — read percentages as relative and
**size any GC or allocation win by its release number, not the dev multiple**. A
flat-CST rewrite that cut a benchmark ~3× on dev gained ~1.47× on release,
because release GC was only ~⅓ of wall-clock to begin with. Pure compute does not
inflate, so a compute-bound win carries over intact.

**Rule out a super-linear pass before blaming GC** — that same inflation makes an
algorithmic blow-up read as GC-bound; sweep input size (faster-than-linear growth
⇒ the fix is the algorithm, not allocation) to tell them apart.

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
`array_set_u8` / `array_get_value` (bounds-checked; one per element is the store floor
for `Array<T>`-backed `String` / `List`).

## 3. WasmGC cost facts

- **The live set is the cost, not the allocation count.** The `copying` collector
  traces what survives a cycle; an object that dies before the next one is never
  copied, however many there were. Cutting _transient_ allocations therefore moves
  nothing — a compiler pass that removed thousands of per-token `Box<i32>` allocs
  measured within noise under `copying` (and −0.7 ms/iter under `null`). Chase the
  footprint, not the volume. The same rule retires "iterate by index to stop
  `for x of &list` boxing": the boxes die immediately.
- **Module-lifetime GC data is a tax on every collection.** A decoded table held
  in a global as one `List<i32>` per state (~7.4K permanently live objects) made
  _identical_ hot-function wasm run 3–6× slower purely from the resident graph;
  flattening it to offset/count columns fixed it. A resident 160 KB flat
  `List<i32>` costs ~+0.9 ms/parse, the 7,400-list shape ~+2.4 ms. **Prefer flat
  columns over nested lists, and don't build what nothing reads.** Measure the GC
  share with `--collector null` (it leaks, so drive a fixed iteration count) vs
  `--collector copying`.
- **`with_capacity` zero-fills.** `List::with_capacity(n)` is an
  `array.new_default`, so an over-sized arena pays for every slot it never uses —
  once badly enough to turn a 2× faster build into a 4× slower one. Growing from
  `[]` by doubling is not the fix either: it zero-fills ~2.4× more than a
  reasonable pre-size. Size it about right, or grow.
- **GC-array access is bounds-checked, no unchecked variant.** A lookup table in
  a GC array adds a checked load per access — it lost to plain arithmetic.
- **`array.copy` is fast; leave it alone.** It has a fast path that does not call
  out to the runtime, and it beats a hand-written loop from a couple of bytes on
  — the loop pays the bounds check above on both the get and the set of every
  byte. Neither hand-roll it nor contort an algorithm to avoid it
  (`dead-ends.md`).
- **Constant `/` and `%` are cheap** (Cranelift magic-multiply, `x/k` and `x%k`
  fused) — don't trade a divide for extra multiplies.
- **A compare cascade is not a dispatch problem.** Cranelift lowers a short
  `else if` chain competitively, and a `match` over it (a `br_table`) adds an
  indirect branch: two separate rewrites to jump tables measured flat and
  slightly slower. Such a frame is usually call-frequency-bound, not
  dispatch-bound — cut the calls, not the branch.
- **Write into the caller's buffer, not a temp.** `` `{v}` `` allocates a
  throwaway `String` and copies it in, per value; `buf.push_display(&v)` skips
  both. A run of adjacent `push` / `push_str` calls is fused into one capacity
  check by `nir/string_push`, so write the appends plainly and let it batch them.
- **`internal_raw_data()` / returning `Array<T>` by value is a copy API** — for a
  single read use `get_unchecked` / `set_byte_unchecked`.

## 4. Inlining is usually not the lever

wasmtime/Cranelift call small Wasm functions cheaply, so forcing inlining rarely
moves wall-time and raising the threshold bloats hot loops (measured slower). The
exception is a tight iteration-bound loop with a trivial body.

A rare heavy sub-case behind a `cold_path()` marker usually needs no
hand-splitting: `nir/cold_outline` moves what the marker opens into a function of
its own, so the leaf inlines at its hot-path size. Its region runs from the
marker to the end of the enclosing block, so a marker in the middle of a loop
body — with the loop's own bookkeeping after it — is one it cannot take
(`docs/optimizer.md`); that shape still needs the split written out. Split by
hand also when the slow path is not rare: a `width > 0` branch that runs every
time a width is set is hot when taken, which no marker should claim otherwise.

**Write the stdlib to be fast without inline hints.** A `#[inline]` /
`#[inline(never)]` in `wado-compiler/lib/` is a claim the cost model got it
wrong, and it silently outlives whatever measurement justified it — the split
that `#[inline(never)]` was added for is usually one the inliner already
declines on size. Prefer changing the shape (a separate function, a smaller hot
path) and leave the decision to the threshold; reach for a hint only after
measuring that the shape alone does not get there, and say in a comment what it
buys.

## 5. Measurement

Only relative numbers carry signal. **A/B both arms in the same session**, best of
three or four, alternating and with the order swapped once — the first run of a
session reads high, so a fixed order silently taxes whichever arm goes second.
Run on an **idle** host, nothing else building: an A/B taken beside a compiling
test suite has put both arms inside each other's spread and flipped their
ranking. Nothing in a number says whether its host was idle, so
`benchmark/README.md` is a sanity check on the arm you just built, never the
control for it — even on the machine that produced it; a HEAD build has measured
615 MB/s against its own recorded 656 in the same afternoon. Isolate the phase —
A/B a float-format change on `fts`, not on a serialize benchmark that dilutes it.

**What decides adoption**, in priority order:

1. **The benchmark moves** → keep it.
2. **The WIR A/B diff shows fewer instructions** → keep it, benchmark flat or not.
   The benchmark simply does not reach them.
3. **The benchmark is flat and the diff is qualitative** — a different sequence,
   with no reading of it that says which is faster → keep whichever emits the
   **smaller wasm**. This is the only question wasm size answers.
4. **Anything else** → drop it, and write it up in `dead-ends.md`.

Only a WIR diff decides case 2. Diff the two `wado dump -O2` outputs and read
what the hot function issues per iteration — a run of N capacity checks collapsed
to one, a call gone from a loop body. Nothing else establishes "fewer
instructions": not the dump's line count, and not the wasm byte count.

**Neither wasm size nor dump size correlates with speed.** Smaller output is
routinely slower and larger output routinely faster — the bytes are mostly code
that never runs, and what does run is priced by what the loop executes. The three
quantities move independently: the append fusion grew `wado dump -O2` on
syntax-highlight 8.3% (a fused write unparses its offset as an expression) and
shrank the `-Os` binary 1.5%, while the thing that justified it was a +8%
benchmark and a diff showing one less capacity check per key. Size is its own
budget (`mise run report-wasm-size`); as evidence about speed it is only the
tiebreaker at rank 3.

## 6. Lessons

`dead-ends.md` (next to this file) is the record: every optimization measured
and dropped, with the A/B that killed it and what it generalizes to. **Read it
before starting**, and add an entry whenever an A/B comes back flat or negative
— a dead end nobody wrote down is one somebody re-measures.

Stop when the floor is the representation — a store-bound loop on an
`Array<T>`-backed `String` is near-optimal short of leaving GC arrays.

## See also

- `dead-ends.md` — what has already been measured and dropped.
- `profiling-wado-compiler` — the native `wado` binary (host side).
- `benchmark` — run the suite / wasm-size report.
- `optimizer-debug` — a NIR/WIR pass producing _wrong_ code, not just slow.
