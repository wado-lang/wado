---
name: wado-performance
description: Analyze and improve the runtime speed of a Wado program's compiled guest Wasm — profile hot functions, read the generated WIR for allocations and copies, reason about the WasmGC cost model, and A/B-measure a fix. Use for any guest-side speed question, whatever the program does. For host-side native compiler profiling see profiling-wado-compiler; for wrong code out of an optimizer pass see optimizer-debug.
---

# Wado Performance

Speed of the **compiled guest Wasm** (what wasmtime runs), not the native
compiler — that is `profiling-wado-compiler`.

Loop: profile the hot function → read its WIR for what it allocates/copies per
iteration → change one thing → A/B both arms in one session, plus the WIR diff of
the hot function → keep or revert (§5 says which evidence decides).

Re-profile before choosing a target: the percentages a WEP or an older note
quotes predate whatever has landed since.

**A speedup lands in the compiler, not in Wado source.** Editing `.wado` files,
the stdlib included, is fine as an experiment: an ablation that prices a piece,
or a hand-written shape that shows what the optimizer should emit. Shipping
such an edit as the speedup is forbidden, unless the user approves it or asks
for it. A fast iteration loop on the stdlib is not a reason to make an
exception. §2 and §4 say where the fix goes instead.

## 1. Profile

```sh
wado run --profile guest,profile.json,1 prog.wado   # interval 1 short runs, 0 exhaustive
wado test --profile guest,profile.json,1 file.wado  # same, over one file's test blocks
node .claude/skills/wado-performance/scripts/analyze_guest_profile.ts profile.json [--top N]
```

The script reports self (leaf) and inclusive counts per function; names keep
monomorphization detail, so each instantiation is separate. Loop a one-shot hot
phase N times so it clears the fixed setup (aim ≥ ~200 samples). Firefox
Profiler (`profiler.firefox.com`) gives a flame graph; `perf` + `--profile jitdump` gives instruction-level (store- vs compute-bound), see
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

**A low self-percentage rules out one dataset, not the function.**
`FieldSchema::lookup` read 0.71% on json-catalog, whose widest struct is 16
fields. Rewriting that same function cut 44% off cbor-twitter's decode, where
`User` has 40. Profile a per-item cost on the input whose items are widest.

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
  `&T` as a box). A tuple is a GC struct too, so retyping a two-field struct as
  `[u64, u64]` allocates the same. Two things remove the allocation: multivalue
  on a return the caller destructures, and SROA on a literal whose fields the
  optimizer can split into locals. A merge point defeats both preconditions,
  which is a pass to fix rather than a call site to rewrite (below).
- **`array.new` / `array.new_default`** — a fresh GC array (`_default`
  zero-fills); watch for one per call where a buffer could be reused.
- **`$value_copy$T…`** — a value-semantics deep copy of a value-typed binding/arg
  unless the source is _fresh_ (a call / literal / variant result, or a fresh
  value's payload). `x?` desugars to `match f() {…}`, so freshness must see
  through the `match`; a missed copy shows up here and is removable.

Count the copies, not only the time: a cut count is a result even when the
benchmark is flat. This counts the entry package's; remarks skip the stdlib, so
count a stdlib copy in `wado dump -O2`:

```sh
wado compile -O2 --log-level info prog.wado 2>&1 | grep -c 'remark: a copy of'
```

Moving a copy from a callee into its call sites multiplies it by the number of
sites.

Also: a `Trait::method(…)` call left in a hot loop (the inliner declined it), and
`array_set_u8` / `array_get_value` (bounds-checked; one per element is the store floor
for `Array<T>`-backed `String` / `List`).

### A stdlib workaround is a bug report about a pass

An optimizer fix reaches every Wado program. The stdlib edit that routes around
one reaches a single call site, and hides the gap that produced it. So when the
shape you are about to rewrite by hand is one a pass exists for, name the pass
and read its precondition first. That is where the fix belongs.

Three turned up this way while cutting `fts`. All three are also live in
`short`, the path every `${x}` on a float takes, which is why fixing the first
paid on a benchmark `fts` never touched.

- **`sroa` matches only a direct literal binding.** An inlined `get_pow10` left
  `let pm = <block with two exits>`, one building the struct and one calling out
  for it, with `pm.hi` / `pm.lo` the only uses. The `PmHiLo` was a heap object
  per conversion. Fixed by extending `slot_temp_sroa`, which already scalarized
  the `[tag, slots…]` shape, to a struct literal and to an exit that hands over
  the aggregate: **json-canada de +12.7%, ser +7.2%**, and `uscale_pow10`
  deleted.
- **`multi_value_return` is all-or-nothing per callee.** It wants _every_ call
  site to be `let $tmp = Call(f)` whose only uses are field accesses.
  `mul_pow10` had seven sites and six were exactly that; the one yielding the
  call as a block value disqualified all seven. Fixing the first gap retired
  this one, since the offending site is now a `let`. The precondition is
  unchanged, so the next callee to hit it pays the same way.
- **`cold_outline` refuses a region containing a `return`** (`control_escapes`),
  which is every rare slow path there is. Leaving `fixed_width_for_prec`'s
  out-of-range tail inline cost json-canada ser 6.5% against a byte-identical
  serialize path: growing a hot function moves everything downstream of it in
  the module. Hand-splitting it into a function restored the row, and
  `fixed_width_out_of_range` in `fpfmt.wado` is that split.

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
- **The GC heap's size is part of the measurement.** `wado` starts every guest
  with 256 MiB (`--gc-heap-initial`), which the copying collector splits in two,
  so a benchmark allocates through half a heap rather than climbing to one. A
  raw `wasmtime` invocation starts at zero and doubles its way up to the working
  set, paying a full trace at every rung — and there the ranking of two
  compilers flips with the heap size rather than with the code.
- **`with_capacity` zero-fills.** `List::with_capacity(n)` is an
  `array.new_default`, so an over-sized arena pays for every slot it never uses —
  once badly enough to turn a 2× faster build into a 4× slower one. Growing from
  `[]` by doubling is not the fix either: it zero-fills ~2.4× more than a
  reasonable pre-size. Size it about right, or grow.
- **GC-array access is bounds-checked, no unchecked variant.** A lookup table in
  a GC array adds a checked load per access — it lost to plain arithmetic.
- **A lone `array.get` costs ~20 machine instructions; gets sharing a block cost
  ~8.** wasmtime re-derives the object's null check, its length load and the
  overflow-checked element address per get, and neither hoists them out of a
  loop nor shares them across blocks — only across gets in one block. Read the
  actual sequence with `wasmtime explore -W gc,function-references f.wat`; a
  byte-at-a-time loop is 22 instructions and 6 branches per byte, four gets in
  one block are 18 + 4×8. So a scan reads several bytes per bounds check and
  then tests them: `peek_after_whitespace_run` in `core:json` is that shape,
  worth 12.6% on json-catalog deserialize. It pays in proportion to the run it
  covers, against the one partial block it always wastes — under ~16 bytes per
  run it is a loss. **Four is where the sharing stops**: a wider block only adds
  lone gets, and measures worse the wider it gets (`dead-ends.md`).
  **`array.set` shares nothing**: a store may write the header as far as
  Cranelift knows, so four adjacent sets reload the length four times. Only
  `array.copy` / `array.fill` amortise a write.
- **SROA is priced by the aggregate's width, not by the allocation it removes.**
  Splitting a 40-slot tuple into locals deletes one `struct.new` per struct and
  costs 6.5% on cbor-twitter: past the register file, forty `ref` locals live
  across a call-heavy loop are forty spill slots reloaded at every call boundary,
  plus a `ref.null` init apiece at entry. "The allocation is gone" says nothing
  about which side won (`dead-ends.md`).
- **`array.copy` is fast; leave it alone.** It beats a hand-written loop from a
  couple of bytes on — the loop pays the bounds check above on both the get and
  the set of every byte. Neither hand-roll it nor contort an algorithm to avoid
  it (`dead-ends.md`). Its length decides the cost: a constant one compiles to
  inline loads and stores, a run-time one calls `wasmtime_builtin_memory_copy`.
  So a small copy repeated per item pays to reach the copy with a constant
  length, which is what `param_spec` cloning on `name.used` gave the JSON key
  writer.
- **Constant `/` and `%` are cheap** (Cranelift magic-multiply, `x/k` and `x%k`
  fused) — don't trade a divide for extra multiplies.
- **A short compare cascade is not a dispatch problem.** Cranelift lowers a short
  `else if` chain competitively, and a `match` over it (a `br_table`) adds an
  indirect branch: two separate rewrites to jump tables measured flat and
  slightly slower. Such a frame is usually call-frequency-bound, not
  dispatch-bound — cut the calls, not the branch. What does answer to dispatch is
  a cascade long enough to pay for that branch, or one that is not a cascade at
  all: independent `if`s no arm leaves test every key whatever matched, which
  `nir/if_chain_to_match` is what fixes. A set _membership_ test,
  `k matches { A | B | 'x'..='z' | … }`, is neither. `match_to_bitset` lowers
  it to a branch-free mask test when the scrutinee is 32 bits or narrower and
  the members span at most 256 values, so write it as the set rather than
  hand-rolling a range compare or a table. Past that span it is a `br_table`.
- **Write into the caller's buffer, not a temp.** `` `{v}` `` allocates a
  throwaway `String` and copies it in, per value; `buf.push_display(&v)` skips
  both. A run of adjacent `push` / `push_str` calls is fused into one capacity
  check by `nir/string_push`, so write the appends plainly and let it batch them.
- **`internal_raw_data()` / returning `Array<T>` by value is a copy API** — for a
  single read use `get_unchecked` / `set_byte_unchecked`.
- **An `assert` of a caller-guaranteed precondition is free; the same test as a
  guard is not.** `is_json_ws` in `core:json` needs `b < 64` (Wasm masks a shift
  count mod 64) and every call site short-circuits on `b > b' '` first. Writing
  the precondition as `assert b < 64` measures flat on json-catalog deserialize;
  writing it as `b < 64 &&` in the returned expression costs 6%, and dropping
  the bitset for four compares costs 19%. So a hot leaf whose precondition the
  callers establish keeps both the assert and the fast body. Measure the assert
  and the guard as separate arms: folded into one they read as a single cost,
  and the assert takes the blame for what the guard spent.

## 4. Inlining: fix the inliner, never the code

An inline hint or a hand-inlined body is almost never worth it: it reaches one
call site, and it silently outlives the measurement that justified it. The
stdlib carries no `#[inline]` / `#[inline(never)]`, because a hint that makes
code faster marks a case the optimizer misses.

Improving the inliner is what pays, and it keeps being improved. A better price
reaches every program at once. When it declines a callee that should be spliced,
that is a cost-model bug: find the price with `WADO_TRACE=inline` and fix it in
`optimize/inline.rs`.

Never bend source to the inliner's current prices. Collapsing `let`s into one
expression, choosing an operation width, or splitting a function only to get
under the threshold are all banned. It is §2's rule applied to the inliner: the
contortion reaches one call site and hides the gap from every other. zlib's
`read_u32_le` is the case that set this rule. Widening each byte to `u64`
priced it one instruction over the threshold, and doing the arithmetic in `u32`
with one widening at the end got it inlined. But the widening costs nothing:
Cranelift folds an `i64.extend_i32_u` into the load or arithmetic producing it. The
price was wrong, and `is_zero_extension` in `optimize/inline.rs` is its fix.

Raising the threshold wholesale is not the lever: it bloats hot loops and
measured slower. The lever is a price that matches what the spliced code costs.

A split that is right on its own stays. A slow path that is not rare is one: a
`width > 0` branch runs every time a width is set, so it is hot when taken and
no `cold_path()` marker should claim otherwise. A rare heavy sub-case needs no
hand-split at all: `nir/cold_outline` moves what the marker opens into a
function of its own, so the leaf inlines at its hot-path size. A marker
`cold_outline` cannot take (mid-loop-body, see that pass's module doc) is a gap
in that pass, per §2.

## 5. Measurement

**Report an improvement as +%, a regression as −%.** Compute it as speedup,
`base / head − 1` on ms/iter (or `head / base − 1` on throughput), so faster is
always positive. A report that writes one faster row as "+5%" and another as
"−5% ms" leaves the reader to work out which way each number points.

Only relative numbers carry signal. **A/B both arms in the same session**, best of
three or four, alternating and with the order swapped once — the first run of a
session reads high, so a fixed order silently taxes whichever arm goes second.
Run on an **idle** host, nothing else building: an A/B taken beside a compiling
test suite has put both arms inside each other's spread and flipped their
ranking. Check `ps` and `free` as well as `uptime` — a load average lags a
session that just started and says nothing about memory, and another agent's on
this box put the same test target at 17× its idle time before OOM-killing the
command after it. Nothing in a number says whether its host was idle, so
`benchmark/README.md` is a sanity check on the arm you just built, never the
control for it — even on the machine that produced it; a HEAD build has measured
615 MB/s against its own recorded 656 in the same afternoon. Isolate the phase —
A/B a float-format change on `fts`, not on a serialize benchmark that dilutes it.

`core:json`'s inputs pull opposite ways: `citm_catalog.json` is mostly
pretty-printing whitespace, `canada.json` minified floats. A scan win on one is
no evidence about the other, so measure both, and json-twitter for strings.

A CI "Performance Alert" on rows the diff cannot reach is the shared runner.
Confirm it locally before treating it as a regression.

While the user is iterating, the reading is 4–5 back-to-back pairs on the target
row plus the wasm hash; the whole-suite A/B below is for the wrap-up.

**A dev-build A/B is only valid where the dev build is.** The inflation §1
describes flips A/B verdicts, not only profile weights. Dev runs the wasmtime
runtime, GC and allocator at dev speed, so a row bound by allocation reads a
different winner. json-canada is store- and compute-bound, so it matched release
to under 2% and made a fast stdlib loop possible. On the same change dev
called syntax-highlight -1.2% where release said **+1.4%**, and cbor-canada and
cbor-twitter deserialize -2.9% and -1.2% where release said +0.3%. Every row that
moved is a deserialize or a CST build, which is what allocates. Iterate on
dev, then settle any row whose work is building an object graph on release.

Scratch files below go in `scratchpad/`. A path handed to the harness, which
runs from `benchmark/`, must be absolute.

### A/B-ing a compiler change

Build the head arm first: every command below compares against
`target/release/wado`, and a stale one compares main with itself.

```sh
cargo build --release --bin wado
```

Run every benchmark at `-O0` through `-O3` before trusting an optimizer change:
both suites have passed a miscompile only the large bodies `gale_gen` produces
reach, where it reads as `ERROR task failed`. The last `-O` flag wins, so
`WADO_BENCH_FLAGS=-O1 mise run benchmark-all` runs the suite at `-O1`.

A change to the compiler needs two compilers. `benchmark-baseline` builds
`origin/main`'s once and caches it under that commit; `WADO_BIN` then runs it
through _this_ tree's harness, so only the compiler differs — the baseline's own
`benchmark/` would put the branch's harness changes inside the comparison too.
The task fetches `origin/main` each time it runs and deletes the baseline of an
older main, so copy it out once and compare against the copy: a moved main is
another compiler.

```sh
cp "$(mise run benchmark-baseline)" scratchpad/wado-main   # slow the first time
```

```sh
# alternate, so neither arm always goes second
WADO_BIN="$PWD/scratchpad/wado-main" mise run benchmark-all > scratchpad/b1.log 2>&1
mise run benchmark-all > scratchpad/h1.log 2>&1  # …and so on, 3 each
node benchmark/ab.ts --base scratchpad/b{1,2,3}.log --head scratchpad/h{1,2,3}.log
```

**Time the Wado rows alone, with `mise run all-wado`.** The reference arms (C,
Rust, JavaScript, the Java ones) run the same binary whatever the compiler does,
so re-timing them buys nothing and stretches a round many times over.
That is the gap the host drifts across: a three-arm `benchmark-all` comparison
came back with `ANTLR4 (Java)` at -2.2%, `count-prime / JavaScript` at +1.4% and
a prime sieve 4.3% "faster" from a string-append change, all unreadable. The
same arms over `all-wado`, six rounds back to back, settled every row. Keep
`sieve` in the selection as the in-band control, and run `benchmark-all` once at
the end for the record.

It is a task of `benchmark/mise.toml`, so it runs from `benchmark/` only.

```sh
cd benchmark
mise run all-wado                                # every Wado row
mise run all-wado json_catalog sieve             # those, by name
```

Its log feeds `ab.ts` and `pick.ts` like any other, one row per benchmark file.

**Hash the wasm before you time anything.** Compile every benchmark under both
compilers and compare. A row whose bytes are identical cannot have moved, so
whatever the suite says about it is the host. That is a stronger check than
timing, and it leaves only the few rows that differ to measure. A
`field_scalarize` fix came out byte-identical on all but three benchmarks. The
suite had meanwhile called `fts` 6.9% SLOWER with non-overlapping ranges; the
identical SHA-256 retired that reading outright.

Gate the compare on each compiler's exit status, not on its output file. A
compile that failed leaves the previous round's file in place, and comparing
those reads as "identical". That is the one answer this check must never give by
accident. Drop the schema modules, which are no world entry point, and give
`http_routing` the world it targets. Every remaining benchmark must compile, so
a `FAILED` row is one to go and read, and it carries the diagnostic explaining
it. `wado compile` reports on stderr even when it succeeds, so its output is
held back and printed with the failure, which keeps the sweep's own lines
readable.

Keep the world one `--world=…` token: zsh does not word-split an expansion, so
two words in a variable reach `wado` as a single flag it rejects.

```sh
for f in benchmark/*/*.wado; do
  case "$f" in *_schema.wado) continue ;; esac
  world=
  case "$f" in */http_routing/*) world=--world=wasi:http/service ;; esac
  scratchpad/wado-main compile -O2 ${world:+"$world"} -o scratchpad/b.wasm "$f" > scratchpad/cc.log 2>&1 \
    || { echo "FAILED  $f"; cat scratchpad/cc.log; continue; }
  target/release/wado compile -O2 ${world:+"$world"} -o scratchpad/h.wasm "$f" > scratchpad/cc.log 2>&1 \
    || { echo "FAILED  $f"; cat scratchpad/cc.log; continue; }
  cmp -s scratchpad/b.wasm scratchpad/h.wasm || echo "DIFFERS $f"
done
```

Give the four `wasm-size` programs the same pass at `-Os`: no benchmark covers
`sqlite_highlight`, the largest generated program in the tree.

For each row that differs, diff the two `wado dump --wir -O2` outputs function by
function, which names what moved. A correctness fix is held to byte-identical
output. Then time only the rows whose hot path moved, back to back, and read the
rest as unmoved.

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
  scratchpad/wado-main run -O2 benchmark/sieve/sieve.wado
  target/release/wado run -O2 benchmark/sieve/sieve.wado
done
```

### A/B-ing a stdlib change

A change to `lib/core/*.wado` needs two compilers as well, which the source tree
hides: a release build embeds the stdlib where a dev build reads it from disk.
`benchmark/wado.sh` falls back to `cargo run --release` whenever `WADO_BIN` is
unset, so swapping an arm's `.wado` files into the tree invalidates
`wado-compiler` and rebuilds it under `lto=thin` / `codegen-units=1`. That is
two full rebuilds per alternating round, and they are the wall clock rather than
the benchmark.

Build one binary per arm first, from the same compiler source with `lib/` at the
branch's fork point and at `HEAD`, so `lib/` is all that differs. `git restore`
removes a file the source tree lacks, so neither binary embeds a stdlib
belonging to neither arm. It overwrites uncommitted edits under `lib/`, so
commit them first.

```sh
set -e  # a failed build would leave an earlier A/B's binary as its arm
rm -f scratchpad/wado-base scratchpad/wado-head
fork=$(git merge-base origin/main HEAD)
# however the block ends, `lib/` goes back to HEAD rather than stay reverted
trap 'git restore --source=HEAD --worktree -- wado-compiler/lib' EXIT
git restore --source="$fork" --worktree -- wado-compiler/lib
cargo build --release --bin wado --quiet
cp target/release/wado scratchpad/wado-base
git restore --source=HEAD --worktree -- wado-compiler/lib
cargo build --release --bin wado --quiet
cp target/release/wado scratchpad/wado-head
for r in 1 2 3; do
  for arm in base head; do
    WADO_BIN="$PWD/scratchpad/wado-$arm" mise run benchmark-json-catalog
  done
done
```

The tree's sources stop mattering once the binaries exist, so a round costs what
the benchmark costs. Rounds are cheap enough then to run six or ten of them,
which is what it takes to resolve a delta near 1% out of this row's spread.

`WADO_SKIP_PASS=<pass>` is a third arm off the same binary, which is how a
regression is attributed to one pass without a third build. `WADO_BENCH_FLAGS`
sweeps a knob the same way; the harness appends it to every `wado compile` and
`wado run` it issues, so only a knob both accept can be swept.

**Give a threshold a temporary env override and sweep it, rather than
rebuilding per value** — and reach for it the moment a change looks like it only
pays above some size, because that shape usually means two rewrites are riding
one knob. `if_chain_to_match` appeared to need a 12-arm floor; overriding its
threshold and `match_to_switch`'s separately showed the fusion was never the
cost at any width and the `br_table` past it was the whole of it on the row that
regressed, turning 3.6% down on cbor-catalog into 2.1% up. Delete the overrides before
committing: read per node visit, `std::env::var` is itself a compile-time cost.

A runtime setting one benchmark wants becomes a CLI option with a conservative
default, and that benchmark opts in through `gc_heap_flags` in
`benchmark/wado.sh`. Sweep a candidate below the default as well as above it.

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
tiebreaker at rank 3. Where size and speed trade, take the speed and state the
size cost. A fold of an idiom wasmtime already matches (shift-or into `rotl`)
buys size only, so judge it by bytes.

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
