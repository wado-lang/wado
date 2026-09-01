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

**A sample lands at the next epoch check, not where the time went.** The guest
profiler samples on an epoch deadline and wasmtime checks the epoch at function
entries and loop headers, so straight-line code is charged to whichever it
reaches next. A derived deserializer's field-dispatch chain reported as **73%
self in `deserialize_i32`** on an 80-field struct, and 19% in `deserialize_bool`
on cbor-twitter — neither function is more than a bounds check and two compares.
A hot small leaf is telling you how often it is entered, so go read its caller.
The profile ranks candidates; it does not locate them.

**A low self-percentage retires a dataset, not the idea.** `FieldSchema::lookup`
read 0.71% on json-catalog, whose widest struct is 16 fields; rewriting the same
function cut 44% off cbor-twitter's decode, whose `User` has 40. Rank a per-item
cost on the input whose items are widest, not on whichever is at hand.

**Rule out a super-linear pass before blaming GC** — that same inflation makes an
algorithmic blow-up read as GC-bound; sweep input size to tell them apart.
Faster-than-linear growth is a hypothesis and not a verdict, since a live set can
grow that way too, so the WIR is what settles which.

Sweeping a _shape_ dimension is sharper than size, and the one to vary is the one
the suspect is indexed by. Decoding 1000 CBOR records, holding that count fixed
so no per-record term is left for the growth to be:

| `i32` fields per record | 5  | 10 | 20 | 40  | 80  |
| ----------------------- | -- | -- | -- | --- | --- |
| ns per field            | 85 | 80 | 87 | 129 | 221 |

Hold everything but the dimension under test — a sweep that varies two answers
about neither.

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
- **SROA is priced by the aggregate's width, not by the allocation it removes.**
  Splitting a 40-slot tuple into locals deletes one `struct.new` per struct and
  costs 6.5% on cbor-twitter: past the register file, forty `ref` locals live
  across a call-heavy loop are forty spill slots reloaded at every call boundary,
  plus a `ref.null` init apiece at entry. "The allocation is gone" says nothing
  about which side won (`dead-ends.md`).
- **`array.copy` is fast; leave it alone.** It has a fast path that does not call
  out to the runtime, and it beats a hand-written loop from a couple of bytes on
  — the loop pays the bounds check above on both the get and the set of every
  byte. Neither hand-roll it nor contort an algorithm to avoid it
  (`dead-ends.md`).
- **Constant `/` and `%` are cheap** (Cranelift magic-multiply, `x/k` and `x%k`
  fused) — don't trade a divide for extra multiplies.
- **A short compare cascade is not a dispatch problem.** Cranelift lowers a short
  `else if` chain competitively, and a `match` over it (a `br_table`) adds an
  indirect branch: two separate rewrites to jump tables measured flat and
  slightly slower. Such a frame is usually call-frequency-bound, not
  dispatch-bound — cut the calls, not the branch. What does answer to dispatch is
  a cascade long enough to pay for that branch, or one that is not a cascade at
  all: independent `if`s no arm leaves test every key whatever matched, which
  `nir/if_chain_to_match` is what fixes.
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
its own, so the leaf inlines at its hot-path size. Its region runs to the end of
the enclosing block, so a marker mid-loop-body is one it cannot take (see that
pass's module doc) and that shape still needs the split written out. Split by
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
ranking. Check `ps` and `free` as well as `uptime` — a load average lags a
session that just started and says nothing about memory, and another agent's on
this box put the same test target at 145 s and at 2499 s before OOM-killing the
command after it. Nothing in a number says whether its host was idle, so
`benchmark/README.md` is a sanity check on the arm you just built, never the
control for it — even on the machine that produced it; a HEAD build has measured
615 MB/s against its own recorded 656 in the same afternoon. Isolate the phase —
A/B a float-format change on `fts`, not on a serialize benchmark that dilutes it.

### A/B-ing a compiler change

A change to the compiler needs two compilers. `benchmark-baseline` builds
`origin/main`'s once and caches it under that commit; `WADO_BIN` then runs it
through _this_ tree's harness, so only the compiler differs — the baseline's own
`benchmark/` would put the branch's harness changes inside the comparison too.

```sh
base=$(mise run benchmark-baseline)   # ~5 min the first time, 2 s after
# alternate, so neither arm always goes second
WADO_BIN=$base mise run benchmark-all > b1.log 2>&1
mise run benchmark-all > h1.log 2>&1  # …and so on, 3 each
node benchmark/ab.ts --base b1.log b2.log b3.log --head h1.log h2.log h3.log
```

`ab.ts` decides each row by whether the arms' `[min, max]` overlap, not by the
delta: on a 5 ms benchmark a 6% gap between bests sits inside one arm's own
spread. **Read the reference rows first** — C, Rust and JavaScript run the same
binary in both arms, so a `SLOWER` among them is the host drifting and no Wado
row can be read either.

Confirm a surviving row before believing it: the whole-suite arms are minutes
apart, and the reference rows only catch drift big enough to cross a range. Loop
that one benchmark back to back and check the ranking holds pair by pair.

```sh
for i in 1 2 3 4 5; do
  "$base" run -O2 benchmark/sieve/sieve.wado
  target/release/wado run -O2 benchmark/sieve/sieve.wado
done
```

`WADO_SKIP_PASS=<pass>` is a third arm off the same binary, which is how a
regression is attributed to one pass without a third build. `WADO_BENCH_FLAGS`
sweeps a knob the same way, but the harness spends it on `wado run`, so a knob
`compile` alone accepts is one no sweep reaches.

**Give a threshold a temporary env override and sweep it, rather than
rebuilding per value** — and reach for it the moment a change looks like it only
pays above some size, because that shape usually means two rewrites are riding
one knob. `if_chain_to_match` appeared to need a 12-arm floor; overriding its
threshold and `match_to_switch`'s separately showed the fusion was never the
cost at any width and the `br_table` past it was the whole of it on the row that
regressed, turning 3.6% down on cbor-catalog into 2.1% up. Delete the overrides before
committing: read per node visit, `std::env::var` is itself a compile-time cost.

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
