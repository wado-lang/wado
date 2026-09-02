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
