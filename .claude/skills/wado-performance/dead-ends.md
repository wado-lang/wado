# Performance Dead Ends

Optimizations that were measured and did **not** pay off. Read before starting
performance work; add an entry whenever one lands here.

An entry lands here when a change clears none of the three bars in the skill's
§5: the benchmark did not move, the WIR A/B diff shows no fewer instructions, and
it does not even win the qualitative-tie case on wasm size. A change that clears
one of them is kept, and belongs in its commit instead. Note what is _not_ a bar:
code quantity. Neither wasm bytes nor dump lines correlate with speed, so only a
WIR diff establishes "fewer instructions". Profile shares only rank
candidates — two profiles have different sample counts, so a shifted percentage
proves nothing.

```sh
wado run -O2 --profile guest,prof.json,1 benchmark/json_catalog/json_catalog.wado
wado dump -O2 benchmark/json_catalog/json_catalog.wado    # before/after: diff the hot function
for i in 1 2 3; do mise run json-catalog; done           # before and after
```

## Where json-canada's time actually is (2026-09-04)

Not a dead end — the map the entries below were measured against, since every
share here was established by ablation rather than read off a profile. Each row
is the whole benchmark minus one thing, dev build (which matches release on this
benchmark to under 2%, so the stdlib can be edited and re-run in 8 seconds with
no rebuild).

Serialize, 7.65 ms/iter:

| remove                                    | ms/iter | cost |
| ----------------------------------------- | ------- | ---- |
| — (baseline)                              | 7.65    | —    |
| every division in the digit loop          | 7.44    | 0.24 |
| the decimal-point `array_copy` shift      | 7.28    | 0.37 |
| `short()`, replaced by a constant         | 5.98    | 1.68 |
| the digit loop, replaced by `array_fill`  | 5.77    | 1.90 |
| all float formatting (19-byte `push_str`) | 3.09    | 4.56 |
| the float bytes entirely (traversal only) | 1.58    | 6.07 |

Deserialize, 10.96 ms/iter: `scanned_to_f64` → 0.0 costs 1.55 ms, and deleting
`digits = digits * 10 + d` on top of that is free — the scan is bound by the
bounds-checked `array.get` and its branches, not the arithmetic.

Two things follow. The serde traversal alone (1.58 ms, no float bytes written)
is already 65% of serde_json's entire 2.43 ms serialize, so the structural cost
is the standing gap, not the codec. And `short()` at 1.68 ms is ~15 ns per
conversion, which is ryu-class — there is no algorithmic slack left in it.

## Four ways to keep `short` small and still drop its `uscale` calls (2026-09-04)

Inlining `uscale` into `short` buys json-canada ser +2.4% and costs json-twitter
ser 2.3% — placement, on a hot path that never calls `short` (see the entry
below). So: get the win without growing `short`. Four shapes, all worse, and the
reason is the same each time.

| arm                                              | `short` | canada ser | twitter ser |
| ------------------------------------------------ | ------- | ---------- | ----------- |
| base                                             | 87      | 7.59       | 0.821       |
| inline `uscale` into `short` (what landed)       | 122     | **7.39**   | 0.837       |
| share the mask, keep the three calls             | 89      | 7.70       | 0.835       |
| one call returning all three scalings            | 86      | 7.81       | **0.814**   |
| one call returning the two bounds                | 88      | 7.88       | **0.818**   |
| pin the rare arm out of line (shrinks the above) | 113     | 7.46       | 0.833       |

Every arm but the first makes fewer calls and fewer mask computations than base,
and every one of them is **slower than base**. So "fewer instructions" does not
predict the outcome here at all. The arm that wins is the only one with _no call
left on the hot path_. What it saves is the call boundary itself — argument
setup, the multivalue return, the registers that cannot stay live across it — and
not the two mask computations, which are worth nothing. The three-scaling arm
also does a third wide multiply `short` does not need: a shortest representation
at full width has `dmin == dmax`, so canada only ever needs two.

Two traps worth naming. A call is charged the ABI edge unless the callee carries
a loop this pass will splice, and what the threshold judges is that price **less
the call site it replaces**, so a helper that reads as "a mask and three calls"
comes in under budget and is pulled into `short` together with all three bodies.
`#[inline(never)]` is the only way to hold such a helper out of line, and
reaching for it there is not a claim against the cost model.

A size-matched control does not falsify a placement story either. Padding `short`
with a dead branch grew the _wasm_ by 196 bytes against the real change's 205 and
left twitter at exactly 0.820 all three rounds, because a compare-and-jump is
nothing in machine code next to three inlined `uscale` bodies. Wasm bytes are not
the unit the effect is measured in.

Generalizes: canada's gain and twitter's loss are one phenomenon, not two. The
only lever that moves either is whether `short` carries the scaling inline, and
it moves them in opposite directions.

## Three ways to get `uscale` inlined (2026-09-04)

`uscale` grosses 21 against the -O2 budget of 16, and its four parameters make
the call site it replaces worth 6, so `net_cost` puts it at 15 and `short`
carries all three scalings inline. That buys json-canada ser 7%, the whole of the
-O3 gain on this row. Raising the threshold to 22 buys the same 7%, along with
the global bloat the 2026-08-27 entry rules out.

The three arms below were measured while `short` still paid three calls per
conversion, each recomputing the same `(1 << (s & 63)) - 1`. Each fails for a
reason that does not turn on how `uscale` is admitted:

- **`builtin::cold_path()` in `uscale`'s exactness arm.** Prices the function by
  its fast path and gets it inlined everywhere. json-canada ser +3.4%, de +2.7%
  — and **fts -4.5%**, three rounds, non-overlapping. `fixed_width_for_prec`
  scales once, so it gains nothing from the mask and only pays the growth; it is
  the same size-sensitive function the skill's `cold_outline` note names.
- **Merging `check_special`'s two zero tests** into `bits << 1 == 0` (cost 18 →
  under budget). json-canada de **-2.4%** on its own and nothing measurable on
  ser, on a function the de path never calls. Pure placement.
- **Returning the two bracketing scalings together** (`uscale_bounds`), to shrink
  `short` from three calls to two rather than grow it. The threshold admits it on
  its own price, and the growth budget, where a single-site candidate costs
  nothing, never gets to veto it. `short` grew from 87 to 113 dump lines
  regardless.

Generalizes: at this scale module layout outweighs the instruction saving, in
both directions. Growing `short` moved json-twitter serialize 2–5% on a hot path
that is byte-identical bar block-label numbering. So hash the wasm of every row
first and only measure the ones that differ. Three of the five float-touching
benchmarks came out byte-identical, which retires their readings outright and is
the only reason the surviving numbers mean anything.

## Two digits at a time in `write_digits_at` (2026-09-04)

`write_decimal` is 27.7% self on json-canada, whose shortest-round-trip
mantissas are 17 digits: `v % 10` / `v / 10` per digit is a 17-long serial
chain on `v`, which reads as latency-bound. Stepping `v / 100` and peeling the
pair's two digits off `r` halves the chain and moves those two divisions off it.

**5% slower**, three alternating pairs, ser 282.5/286.9/288.3 → 261.6/272.7/271.7
MB/s. The chain was never the cost. Two ablations on the same loop say so:

| `write_digits_at` variant                  | ser ms/iter |
| ------------------------------------------ | ----------- |
| today                                      | 7.65        |
| `v & 7` / `v >> 1` (every division gone)   | 7.44        |
| `array_fill` (whole loop gone, same bytes) | 5.77        |

Deleting **every** division buys 0.24 ms; deleting the per-byte `array.set`
loop buys 1.9 ms. The loop is store-bound at ~2.8 cycles/byte, and a bulk fill
of the same span is near-free because it lowers to a vectorized memset. So two
digits per step only added a third multiply to a loop the multiplies were never
pacing.

Generalizes: on a GC-array-backed `String` the digit loop's cost is the one
`array.set` per byte, which no digit-generation scheme removes. Ablate the
arithmetic before optimizing it. This is the same finding as the 2026-08-16 entry
below, reached from the opposite direction — a 17-digit chain rather than a short
one — so the digit count is not what makes it come out differently.

## i64 divisions in integer formatting (2026-08-16)

Decimal formatting cost three i64 divisions per digit: `count_digits_i64` ran
`t / 10` per digit just to count, `write_decimal_digits` ran both `% 10` and
`/ 10`. Cut to one (binary search over powers of ten; remainder recovered as
`temp - q * 10`).

json-catalog ser, best of 3: **1.532 ms → 1.530 ms**. Within noise — the spread
inside each group (before: 1.532–1.601) exceeded the difference between them.

Cranelift strength-reduces division by a constant into multiply-high + shift,
so no division was ever issued. The ~28% in these frames is the per-byte
`array.set` into the GC buffer and the surrounding buffer management
(`String::grow`, `push_str`), not arithmetic.

Reverted; the digit-boundary tests were kept. Generalizes to any "replace a
constant divide/modulo" idea in guest code.

## Sharing list elements instead of deep-copying them (2026-08-20)

Lifting `value_copy_demote`'s variant-deep-copy gate is a no-op: the candidate
set grows from 26 helpers to 44 and demotes the same 3, for byte-identical Wasm
(`WADO_TRACE=demote`). It retargets only a `let x = $value_copy$T(arg)` binding,
and gale's `List<Element>` copies — 9.6% of the profile — are struct fields
(`SllConfig { ..*c, pos }`).

Extending the pass to expression position is the obvious next step and the wrong
one. Forcing every element clone shallow is the upper bound of any sharing
scheme:

|                     | `copying` | `null` (no GC) |
| ------------------- | --------- | -------------- |
| deep copy (default) | 171 KB/s  | 208 KB/s       |
| shared elements     | 60 KB/s   | 260 KB/s       |

A deep-copied element dies in the nursery and costs a copying collector nothing;
a shared one is live from everything that borrowed it. GC goes from 18% of the
run to 77%.

Generalizes: price a "share instead of copy" idea against the live set it
creates, not the allocations it removes.

The motivating copy is gone as of 2026-08-25: `SllConfig.elements` is a
`&List<Element>`, which shares a list that is already a root rather than its
elements, and gale-gen gained 32% (`package-gale/perf.md`).

## Making an expression-position labeled block a break target (2026-08-20)

`Analyzer::walk_expr` pushes no exit entry for a value-producing labeled block,
so every `break` it holds resolves to "every local live". Pushing one is more
precise and does unlock moves — `build_sll_node`'s loop binding among them.

A precise live set also admits place moves the coarse one refused, and a place
move marks its root moved, retiring every immutable share of that root:
`Rebuild::rebuild` trades three shares of a member tuple for one element move.
gale-gen, best of four alternating pairs, **182.6 KB/s without it vs 168.0**.

Pricing the two elisions against each other needs the share analysis keyed on
liveness, a standing item in
[WEP: Ownership Analysis](../../../docs/wep-2026-05-21-resource-ownership.md).

## Raising the `-O2` inline budget (2026-08-27)

The 13-instruction budget leaves 313 functions out of line in gale-gen, and
raising it to 20 pulls them in: gale-gen 98.9 → 95.9 ms/iter, cbor-twitter
serialize +7.8%, syntax-highlight +3.7%, sqlite-parse +3.4%, sieve +2.2%.

It is still a loss. json-twitter **serialize drops 17%** (638 → 530 MB/s),
mandelbrot goes slightly backwards, and every program pays for it in bytes:
`-Os` output grows 41% on gale-gen (1.29 → 1.81 MB), 43% on json-twitter, 16%
on syntax-highlight.

So 20 is not a better setting — a serializer's hot loop is exactly what growing
it that far damages. What the sweep says instead is that the threshold is doing
two jobs, admitting a callee and pricing a loop body, and a gale-gen-shaped win
needs the second priced apart from the first. 16 did land later, on the same
rows; the budget this entry measures against is that, not the 13 above.

## A jump table in place of a compare cascade (gale, 2026-07/08)

Two independent rewrites, both reverted. `_kind_set_*` — a
`k matches { TK_… | … }` over the large SQLite keyword set, ~3% of the profile —
already lowers to a Wasm `br_table`, and its self-time was **unchanged** from the
compare cascade it replaced. `classify_keyword`'s per-length first-char dispatch,
rewritten from an `eq_ignore_ascii_case` else-if chain to a fold-once
`to_ascii_lowercase()` feeding a `match`, measured a consistent slight **loss**
(sqlite-parse 3.309 → 3.357 ms/iter, best of 3).

Cranelift lowers a short compare chain competitively and the chain predicts well,
while the jump table adds an indirect branch. Both frames were
call-frequency-bound, not dispatch-bound.

Generalizes: a hot `match`-vs-cascade frame is telling you how often it is
called. Cut the calls.

Both cascades here were short and already early-exit. A run of independent `if`s
that tests every key whatever matched is not that shape, and turning one into a
`br_table` gained 10.7% on cbor-twitter deserialize (`nir/if_chain_to_match`).

## Index loops in place of `for x of &List<i32>` (gale, 2026-07)

Iterating by reference boxes every element (WasmGC has no interior references),
so rewriting `follow_yields`'s membership scan and `classify`'s `rule_stack` scan
as index loops removes every box. It measured **within noise** — one of three
paired rounds.

The boxes die before the next collection, so the copying collector never traces
them. Same shape as a compiler pass that removed thousands of per-token
`Box<i32>` allocations for −0.7 ms/iter under `--collector null` and nothing at
all under `copying`.

Generalizes: an allocation you can prove short-lived is not a cost. Price a
de-allocation idea against the live set, not the allocation count.

## Pre-sizing a `String` past its one growth (gale, 2026-07)

`highlight_html`'s output grows once past its `source.len() * 5` reserve (HTML is
~6× source for keyword-dense SQL), and `String::grow` was 9% of the **dev**
profile. Bumping the reserve to `* 7` removes the growth; a release A/B
(best of 5) was **identical**, 3.90 vs 3.91 MB/s.

`String::grow` is allocation and zero-fill, which the dev host inflates ~4–5×;
its release share is ~1–2%, under the benchmark's noise. Contrast the CST column
pre-size, worth a clear ~6% because `sqlite_parse` is build-only and the arrays
are the work.

Generalizes: this is the dev-vs-release rule with a number on it. A grow-removal
sized from a dev profile is sized wrong.

## Working around `array.copy` (2026-08-28)

`array.copy` has a fast path that does not call out to the runtime, and it beats
anything hand-written. Two attempts to route around it, from opposite
directions, both lost:

- **Replacing a short copy with a byte loop.** `String::push_str` moves a dozen
  bytes at a time, so a length-gated loop looked like it would skip a call.
  json-twitter serialize, best of 3: **677 -> 479 MB/s** with the loop taken up
  to 32 bytes. A GC array has no unchecked accessor, so the loop pays a bounds
  check on both the `array.get` and the `array.set` of every byte.
- **Restructuring so the copy is not needed.** `fpfmt` inserts a float's decimal
  point by writing the digits flush and shifting the fraction one byte right,
  five sites over. Rewriting them into one right-to-left pass that writes both
  digit runs around the point removes every copy at the same division and store
  count — and measured flat: json-canada **198.60 -> 199.94 MB/s**, inside the
  baseline arm's own 195.2-198.6 spread, `fts` unchanged, `-Os` output 33-36
  bytes larger. Reverted.

Generalizes: reach for `array.copy` and leave it alone. It is not what your
profile is pointing at, so neither hand-rolling it nor contorting the algorithm
to avoid it is worth spending a measurement on.

## Widening `String::grow`'s growth factor (2026-08-28)

`String::grow` doubles, so a serializer reaching a megabyte reallocates ~14
times from `SERIALIZE_BUFFER_CAPACITY`'s 128 bytes. Widening the factor is worth
a lot on one benchmark and costs on several others, and no model of _why_ has
survived contact with the numbers. Serialize throughput, best of 3-4 alternating
on an idle host:

| factor                  | canada    | twitter | catalog | cbor-tw ser | fts   | twitter de |
| ----------------------- | --------- | ------- | ------- | ----------- | ----- | ---------- |
| 2 (today)               | 215.1     | 710.5   | 1.27 GB | 1.18 GB     | 19.73 | 161.3      |
| 4                       | **288.8** | 736.1   | 1.31 GB | 1.14 GB     | 19.25 | 154.3      |
| 16                      | 237.3     | 817.9   | 1.31 GB | —           | —     | —          |
| 2 below 64 KiB, 4 above | 251.1     | 632.3   | —       | 1.16 GB     | —     | —          |

Nothing explains the shape. It is not bytes zeroed: at factor 4 canada allocates
_more_ in total (~11.2 MB against ~8.4 MB, since the final capacity can reach
4x the content) and runs 34% faster. It is not the allocation count either:
factor 16 makes 5 allocations to factor 4's 9 and is **slower**. And the hybrid
is worse than both flat factors on twitter while making fewer allocations than
factor 2 and ending at the same capacity — twitter's response to the factor is
not even monotonic (+15.8% at 16, +3.6% at 4, -10.5% at the hybrid).

Reverted, all of it. The lead is real and unclaimed: **canada has ~30% sitting
in buffer growth policy**, and the live-set rule is the obvious suspect — its
parse tree stays reachable across the whole serialize loop, so every collection
re-traces 55K `List`s and anything changing collection frequency moves the
benchmark. Next attempt should instrument collections first, not guess a factor.
Measure a candidate on cbor-twitter, fts and a deserialize phase too; those are
where a wider factor takes its cost, and a factor tuned on canada alone looks
free.

## Three short ones (2026-07/08)

Each measured flat or negative, each with an obvious-sounding motivation:

- **A GC-array digit table.** Indexing a table of digit characters adds a
  bounds-checked load per digit; plain arithmetic already had none.
- **Two-digit-at-a-time formatting.** The divides it was meant to halve were
  already fused into magic multiplies, so it bought only extra multiplies.
- **Forcing inlining.** Raising the threshold bloats the hot loop; wasmtime
  calls a small Wasm function cheaply enough that the call was never the cost.

The generalization: stop when the floor is the representation. A store-bound
loop over an `Array<T>`-backed `String` is near-optimal short of leaving GC
arrays, and no scheme that keeps the array beats it.

## SROA-ing the derived deserializer's slot tuple (2026-09-01)

`ReflectStruct::empty_slots` returns `[Option<F_0>, …]` as a tuple literal, but
out of line, so the caller's `let slots = empty_slots()` is bound to a _call_ and
`sroa`'s direct-literal matcher never sees it: the slots stay one heap tuple,
allocated per struct and written field by field for the whole decode. `defaults`
already carries `InlineHint::Always` for exactly this reason, and giving
`empty_slots` the same hint works — `wado dump -O2` on cbor-twitter goes from 516
`slots.N` heap accesses to none.

It is **6.5% slower**. cbor-twitter de, three alternating pairs, 212.1/212.0/214.9
→ 198.3/197.2/201.4 MB/s; json-catalog, whose structs are 2–9 fields wide, is
flat.

Forty slots become forty `ref`-typed locals live across a loop full of calls, so
the function trades one bump allocation for forty stack slots it reloads at every
call boundary — plus a `ref.null` init apiece at entry. Reverted.

Generalizes: SROA is priced by the aggregate's _width_, not by the allocation it
removes. Past the register file, decomposing a wide aggregate whose live range
spans calls is a pessimization, and "the allocation is gone" says nothing about
which side won.

## Marking the CBOR scalar decoders' slow paths cold (2026-09-01)

`deserialize_bool` / `deserialize_string` are a two-byte fast path followed by a
tolerant `loop` for tag-wrapped and indefinite encodings. `inline_cost` discounts
what a `cold_path()` marker opens, so a marker before the loop should price each
function by its fast path and get it inlined at every field.

It does not: the WIR A/B is byte-identical on the hot path — the same inlined
`peek_byte`, the same two compares — and the call sites still call. The marker
only stops `is_passthrough_tag` from being inlined into the arm nobody runs. The
fast path alone is still over `-O2`'s 16-instruction budget, so the discount
changes nothing to admit.

Generalizes: the cold discount decides admission, not size. Marking a slow path
cold is worth doing when the hot path _would_ fit under the threshold without it;
when it would not, the marker buys only cold-side bytes.

## Splitting `encode_char` at the ASCII boundary (2026-08-30)

`encode_char` carries `#[inline]`, so its four-way UTF-8 width dispatch and nine
stores land at every write site. Keeping the one-byte case and calling out for
the rest — what the manual-split advice prescribes — lost 2-4% on five serde
rows and gained nothing, including on json-catalog ser, the row it was aimed at.

The split adds a call in the middle of a byte-writing loop, taking `&mut
Array<u8>`, so the loop reloads the array and the position across it. What it
removed was three compares the branch predictor gets right on ASCII text.

Generalizes to: the manual-split advice is for a sub-case whose _body_ is heavy
(an allocation, a grow, a formatter), not for one that is merely branchy. Ask
what the caller copies instead — if the answer is "a compare", there was nothing
to move.

## Outlining a `cold_path()` at a function's top level (2026-08-31)

`nir/cold_outline` collects only the arms of a visited node, so a function's
root block is never a region and a top-level marker is missed. Teaching the
traversal about the root reads like closing a gap.

The benchmark does not move: `sieve` swaps which arm wins across three
alternating pairs, `json-catalog` de differs by under 0.2%. The WIR A/B says
why — the whole diff is one new `__initialize_modules$cold0`, the
`__initialize_module` it swallowed, and TypeId renumbering. **The parse and
serialize loops are identical.** What a top-level marker reaches is every
module's init guard: one call site, behind a branch, run once. Only then does
size decide, and it decides against: every program grew (hello_world +5,
pi_approx +5, zlib +3, sqlite_highlight +9 bytes) across 1883 golden fixtures.

Generalizes to: the pass pays off on a hot leaf whose rare arm is copied at
every call site, and a region reached once behind a branch has nothing to give
back. "The traversal has a blind spot" is a claim about the code, not about what
closing it is worth — and bytes alone would have retired this for the wrong
reason, since they do not track speed.

## Hoisting `HighlightVisitor::classify`'s common path to get it inlined (2026-09-02)

`classify` is one call per token and per trivia, ~5300 on syntax-highlight, and
`package-gale/perf.md` named it a lever: it walks the override list before the
`default_ids[kind]` lookup, and SQLite has no overrides at all. Splitting the
scan into `classify_override` leaves a fast path of two compares and one indexed
load, which reads like it should fit `-O2`'s 16-instruction budget.

It does not fit. `wado dump -O2` still shows both call sites, and the benchmark
is flat to slightly negative: 1.537–1.561 ms/iter against 1.525–1.536. Writing
the guard as `overrides.len() > 0` rather than `!is_empty()` changed neither,
though it did remove a non-inlined `List<ResolvedOverride>::len` call the WIR
had kept.

What paid on the same frame was deleting the caller. With no override nothing
reads the rule stack, so the CST walk that maintains it is unobservable, and
`gen_highlight` stops emitting it (+6.5%).

Generalizes: a fast path you shrink is still a call until it is under the
budget, and "under the budget" is a WIR question, not an eyeball one. Ask what
makes the call unnecessary before asking what makes it cheap.

## `TreeBuilder` handing its store over instead of copying it out (2026-09-03)

`TreeBuilder::finish(&self) -> CstStore` deep-copied `tag`/`a`/`b`/`alt` — four
~10K-element `List`s per `sqlite_parse` — because the builder held its own flat
columns and `finish` handed a fresh `CstStore` back by value. Making the
builder hold `store: CstStore` directly (`finish(&mut self)` fills the derived
`end`/`flags`/`next` columns in place, caller reads `p.b.store`) removes all
four `array_new` + `array_copy` pairs; confirmed in the WIR.

It is a **regression**: isolated from the scan-guard elision that landed
alongside it (`package-gale/perf.md`), three alternating pairs on
`sqlite_parse`, best-of-three — base 1.150 ms/iter, this change alone 1.162
ms/iter, **slower in all 3 rounds**. Combined with the scan elision it also
loses to the scan elision alone in all 3 rounds (1.131–1.141 vs 1.120–1.124).

`TreeBuilder::push_row` was 6.4% self-time in the profile (`perf.md`'s
"Current state" table) — called once per CST row, several times per token.
Nesting the four columns one level deeper (`self.tag` → `self.store.tag`)
adds a `struct.get` on every field access in that function and in `finish`,
paid every row; the four removed copies happen once per parse. Reverted.

Generalizes: an allocation removed once per call is not free against an
indirection paid on every access inside the function that removes it. Price
a copy-elision idea against the loop it would run inside, not just the
allocation it deletes.
