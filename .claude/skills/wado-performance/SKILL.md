---
name: wado-performance
description: Analyze and improve the runtime speed of a Wado program's compiled guest Wasm — profile hot functions, read the generated WIR for allocations and copies, reason about the WasmGC cost model, and A/B-measure a fix. Use for any guest-side speed question, whatever the program does. For host-side native compiler profiling see profiling-wado-compiler; for wrong code out of an optimizer pass see optimizer-debug.
---

# Wado Performance

The speed of the compiled guest Wasm. The loop: profile, read the hot
function's WIR, change one thing, A/B it with the WIR diff, keep or drop by §5.
Read `dead-ends.md` first, and add to it whenever an A/B comes back flat or
negative.

## 1. Profile

```sh
wado run --profile guest,profile.json,1 prog.wado   # interval 1 short runs, 0 exhaustive
wado test --profile guest,profile.json,1 file.wado
node .claude/skills/wado-performance/scripts/analyze_guest_profile.ts profile.json [--top N]
```

Self and inclusive counts per monomorphized function; loop a short phase until
it has a couple of hundred samples. `profiler.firefox.com` draws the flame graph;
`--profile jitdump` with `perf` goes to instructions (`docs/jitdump-profiling.md`).

- A dev-build `wado` runs wasmtime's GC and allocator several times slower, so
  its profiles over-weight allocation. Size a GC or allocation win on release;
  compute wins carry over.
- A sample lands at the next epoch check (a function entry or loop header), so
  a small hot leaf reports how often it is entered: read its caller. The profile
  ranks candidates; it does not locate them.
- A low share rules out one dataset, not the function: profile a per-item cost
  on the input whose items are widest.
- Tell a super-linear pass from GC by sweeping one dimension with the rest held,
  preferably the shape the suspect is indexed by (fields per record, not record
  count). The WIR settles it.

## 2. Read the WIR

```sh
wado dump -O2 prog.wado                 # final WIR
wado dump --tir-monomorphized prog.wado # how `?`, for-of, … desugar
```

Each of these is a bug when it lands per element of a loop:

- `struct.new` / `Box<…>`. `for x of &list` boxes every element, since WasmGC
  has no interior reference. A tuple is a GC struct too. Multivalue (a return the
  caller destructures) and SROA (a literal split into locals) remove one; a merge
  point defeats both, which is a pass to fix.
- `array.new` / `array.new_default`, where a buffer could be reused.
- `$value_copy$T…`, a deep copy of a non-fresh value. Count them:
  `wado compile -O2 --log-level info … 2>&1 | grep -c 'remark: a copy of'`. A cut
  count is a result even when the benchmark is flat. Moving a copy into a
  callee's call sites multiplies it.

Also a `Trait::method(…)` call the inliner left in a loop, and bounds-checked
`array_set_u8` / `array_get_value`, one per element being the floor for
`Array<T>`-backed types.

A shape you are about to rewrite by hand in the stdlib is usually one a pass
exists for: name the pass, read its precondition, and fix it there. Known
preconditions that have bitten: `sroa` and `multi_value_return` want a direct
literal binding at every site, and `cold_outline` refuses a region containing a
`return`.

## 3. WasmGC Cost Facts

- The live set is the cost, not the allocation count: the copying collector
  traces only survivors, so cutting transient allocations moves nothing.
- Module-lifetime data taxes every collection. Prefer flat columns to nested
  lists, and build nothing nobody reads. `--collector null` against `copying`
  measures the GC share (null leaks, so use a fixed iteration count).
- The heap size is part of the measurement. `wado` starts the guest at 256 MiB
  (`--gc-heap-initial`); raw `wasmtime` starts at zero and traces at every
  doubling, which can flip a ranking.
- `List::with_capacity` zero-fills (`array.new_default`). Size it about right;
  growing by doubling zero-fills more.
- Every `array.get` is bounds-checked, with no unchecked form. A lone one costs
  about 20 machine instructions; up to four in one block share the check, at
  about 8 each. Wider blocks add nothing (`dead-ends.md`). So read a run of
  several bytes per check, as `peek_after_whitespace_run` in `core:json` does,
  when runs are long. `array.set` shares nothing; only `array.copy` /
  `array.fill` amortise writes. `wasmtime explore -W gc,function-references`
  shows the sequence.
- `array.copy` beats a loop from a couple of bytes on. Don't avoid it.
- SROA is priced by width: past the register file, split locals spill at every
  call.
- Constant `/` and `%` are cheap.
- A short compare cascade beats a `br_table`; such a frame is call-bound, so cut
  calls. Independent `if`s that all run are `nir/if_chain_to_match`'s. A set
  membership test reads best as `k matches { A | B | 'x'..='z' }`, which
  `match_to_bitset` makes branch-free for a narrow scrutinee and a span of 256.
- Write into the caller's buffer (`buf.push_display(&v)`), not a template
  temporary. Adjacent pushes fuse into one capacity check by themselves.
- `internal_raw_data()` and an `Array<T>` returned by value copy; a single read
  wants `get_unchecked`.
- An `assert` of a precondition the callers establish is free; the same test as
  a guard is not. Measure the two as separate arms.

## 4. Inlining

Fix the inliner, never the code. The stdlib carries no `#[inline]`: a hint that
helps marks a case the optimizer misses. When it declines what it should splice,
find the price with `WADO_TRACE=inline` and fix it in `optimize/inline.rs`.
Never bend source to the current prices (collapsing `let`s, choosing a width,
splitting to get under the threshold). Raising the threshold wholesale measured
slower. A rare heavy branch needs only `builtin::cold_path()`;
`nir/cold_outline` moves it out. A slow path that is not rare is not cold.

## 5. Measurement

Only relative numbers carry signal.

- A/B both arms in one session on an idle host (check `ps` and `free`, not just
  `uptime`), alternating, with the order swapped once: a session's first run
  reads high. A README figure is a sanity check, never the control.
- Isolate the phase: a float-format change on `fts`, not a serializer.
- `core:json` inputs pull opposite ways: `citm_catalog.json` is mostly spaces,
  `canada.json` minified floats. Measure both, and json-twitter for strings.
- A dev-build A/B flips verdicts on allocation-heavy rows (deserializers, CST
  builds). Iterate on dev; settle those on release.
- A CI "Performance Alert" flagging rows the diff cannot reach is the runner.
- While the user is iterating, 4–5 alternating pairs on the target row and the
  wasm hash are the reading; the whole suite is for the end.

### A Compiler Change

Run every benchmark at `-O0` through `-O3` before trusting an optimizer change:
the suites miss shapes only large bodies like `gale_gen` produce, and a trap
there reads `ERROR task failed`.

```sh
base=$(mise run benchmark-baseline)   # origin/main's compiler, cached per commit
WADO_BIN=$base mise run benchmark-all > b1.log 2>&1
mise run benchmark-all > h1.log 2>&1  # alternate, 3 each
node benchmark/ab.ts --base b1.log b2.log b3.log --head h1.log h2.log h3.log
```

Time Wado rows alone with `mise run all-wado [names…]` from `benchmark/`; the
reference arms run the same binary either way. Keep `sieve` as the in-band
control.

Before timing, hash every benchmark's wasm under both compilers. A row whose
bytes are identical has not moved. Gate on each compile's exit status, since a
failed one leaves the last round's file behind:

```sh
for f in benchmark/*/*.wado; do
  case "$f" in *_schema.wado) continue ;; esac
  world=
  case "$f" in */http_routing/*) world=--world=wasi:http/service ;; esac
  "$base" compile -O2 ${world:+"$world"} -o /tmp/b.wasm "$f" > /tmp/cc.log 2>&1 \
    || { echo "FAILED  $f"; cat /tmp/cc.log; continue; }
  target/release/wado compile -O2 ${world:+"$world"} -o /tmp/h.wasm "$f" > /tmp/cc.log 2>&1 \
    || { echo "FAILED  $f"; cat /tmp/cc.log; continue; }
  cmp -s /tmp/b.wasm /tmp/h.wasm || echo "DIFFERS $f"
done
```

Do the same for the `wasm-size` programs at `-Os`. Then diff the differing rows
function by function (`wado dump --wir -O2`), which names what moved; a
correctness fix is held to byte-identical output. Time only the rows whose hot
path moved.

`ab.ts` calls a row by whether the arms' ranges overlap. Read the reference rows
first: a `SLOWER` among them is drift, and no Wado row is readable. Confirm a
survivor with back-to-back pairs of that one benchmark.

### A Stdlib Change

A release build embeds the stdlib, so build one binary per arm, replacing the
whole of `lib/` each time, then run rounds against them:

```sh
for arm in base head; do
  rm -rf wado-compiler/lib
  git checkout $arm -- wado-compiler/lib
  cargo build --release --bin wado --quiet
  cp target/release/wado /tmp/ab/wado-$arm
done
git checkout HEAD -- wado-compiler/lib
for r in 1 2 3; do for arm in base head; do
  WADO_BIN=/tmp/ab/wado-$arm mise run benchmark-json-catalog
done; done
```

`WADO_SKIP_PASS=<pass>` makes a third arm from one binary. To sweep a threshold,
give it a temporary env override and delete it before committing; a change that
only pays above some size usually has two rewrites riding one knob.

### What Decides Adoption

1. The benchmark moves: keep it.
2. The WIR diff shows fewer instructions on the hot path: keep it, flat or not.
3. Flat, and the diff only different: keep the smaller wasm.
4. Otherwise drop it, into `dead-ends.md`.

Neither wasm size nor dump size predicts speed. Size is a budget of its own and
a tiebreaker at rank 3; trading size for speed takes the speed. A fold of an
idiom wasmtime already matches (shift-or into `rotl`) buys size only.

A runtime setting one benchmark wants becomes a CLI option with a conservative
default, and benchmarks opt in through `gc_heap_flags` in `benchmark/wado.sh`.
Sweep a candidate both ways from the default.

Stop when the floor is the representation: a store-bound loop on an
`Array<T>`-backed `String` is near optimal short of leaving GC arrays.
