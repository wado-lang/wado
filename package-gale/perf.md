# Gale Performance Notes

Standing performance findings for the Gale code generator and its
generated parsers: the current benchmark state, the live profile, the
directions that would move the needle, and the measured dead-ends. Read
with [`AGENTS.md`](./AGENTS.md) (architecture, LL-prediction design,
failed approaches) and [`antlr4-compatibility.md`](./antlr4-compatibility.md).

**Performance-related TODO items live here, not in `TODO.md`.**

## Benchmark state (measured 2026-06, dev host)

The basis here is **`benchmark/syntax_highlight`**: it builds the full CST
_and_ walks it (highlighting), so it exercises the realistic consumer path —
build + traverse — not just build. `benchmark/sqlite_parse` (parse only: build
the CST, then `result.ok()`) is kept as a build-isolation companion. Both run
the same 13366-byte SQLite fixture, guest at `-O2`.

Dev-host Gale numbers (`cargo run` `wado`; see the measurement note):

| benchmark                         |    per-iter | throughput |
| --------------------------------- | ----------: | ---------: |
| `syntax_highlight` (build + walk) | ~28 ms/iter |  ~470 KB/s |
| `sqlite_parse` (build only)       |  ~5 ms/iter |  ~2.5 MB/s |

The release headline + comparison baselines (Gale vs `tree-sitter` for
highlight, vs `sqlparser-rs` for parse) come from `mise run syntax-highlight` /
`mise run sqlite-parse` on a release host; not reproduced here (dev-only).

> **Measurement note (read before trusting the percentages).** Profiles are
> captured with the **dev-profile `wado`** (`cargo run`, per the inner-dev-loop
> guidance in the root `CLAUDE.md`). `Cargo.toml` raises `opt-level` on
> `cranelift-codegen`, so JIT-compiled **guest code is near-release quality**,
> but the wasmtime runtime, GC, and allocator run at dev speed — ~4–5× slower
> per iter, with the slack in **allocation/GC**. So the profile inflates
> allocation-bound frames (CST build, `String`/`List` growth) relative to
> pure-compute ones (`scan_*`, kind-set). Read the percentages as relative.

Reproduce:

```sh
cd benchmark
# Gale alone, dev host, with a guest profile (self-time sampling):
wado run --no-cache --profile guest,/tmp/p.json,1 -O2 syntax_highlight/syntax_highlight.wado
```

(`wado` = `cargo run --bin wado --`. Analyze `p.json` with the
`profiling-wado` skill's script — count **leaf** frames for self-time — or
upload to profiler.firefox.com. The table below merges **5 dev-host runs @1 ms
= 3104 samples** to damp per-run sampling noise.)

## Live profile (syntax_highlight, guest sampler, 3 runs merged, 2125 samples @1 ms)

Post-`matches`-lexer shape (~28 ms/iter). Build-and-walk dominates: walking the
CST then building it is over half of self-time. `List<char>::grow` is gone (the
§2 fix); `HighlightVisitor::classify` — 10.9% in an earlier snapshot — collapsed
to ~1%. The lexer now emits `chars[pos] matches { 'a'..='z' | … }` (br_table for
dense classes) instead of `||`/`<=` disjunctions: profile-neutral (kind-set held
at ~3%, lexer-compute ~9%), a modest wall-clock move (~29.7 → ~28 ms) and a
code-size/readability win — confirming br_table ≈ cascade for membership (§3).

|   Pct | Symbol                     | role                                                  |
| ----: | -------------------------- | ----------------------------------------------------- |
| 29.9% | `highlight_walk`           | recursive walk over the `CstNode` tree (traversal)    |
| 12.1% | `List<CstChild>::push`     | per-node child-list build                             |
|  8.4% | `TokenStream::new`         | SoA token-array alloc (WasmGC zero-fill; not a lever) |
|  8.1% | `tree_build_node`          | CST materialization (event log → tree)                |
|  3.8% | `String::push`             | HTML output build                                     |
|  2.4% | `List<i32>::push`          | `rule_stack` / trivia push                            |
|  2.4% | `List<BuildEvent>::push`   | CST event-log build                                   |
|  2.3% | `push_class`               | HTML class emit                                       |
|  2.2% | `scan_any_name`            | scan (prediction)                                     |
|  2.0% | `char::to_ascii_lowercase` | case-insensitive keyword match                        |
|  2.0% | `_kind_set_8`              | membership over the big keyword set                   |

(Frames under 2% self-time omitted.)

Rough buckets (self-time): **CST walk** (`highlight_walk` + `hl_visit_token`) ≈
**32%**; **CST build** (`CstChild::push` + `tree_build_node` + `BuildEvent`
push/grow + `List<i32>::push`) ≈ **25%**; **token-array alloc**
(`TokenStream::new`) ≈ **8%**; **HTML render** (`String::push` + `push_class` +
`highlight_html`) ≈ **7%**; `scan_*` ≈ **5%**; kind-set (`_kind_set_*`) ≈ **3%**.

So the CST — walk it, build it — is the overwhelming majority at **~57%**, with
`highlight_walk` alone the single largest frame at ~29%. After the §2 lexer fix
and the classify collapse, the standing prize is squarely the CST representation.

## Permanently live GC data taxes every collection (2026-07)

The at-end-conflict routing added for ATN-class grammars (#1475) regressed
`sqlite_parse` 2.4 → 4.3 ms/iter (release host) **without the simulator ever
being hot**: the parse-loop ATN work was ~4 predictions/iter (~0.04 ms) and
the per-rule `atn_stack` push/pop measured free. The regression was the
decoded `ATN_SIM` global itself — `state_cont_first(_neg)` held one inner
`List<i32>` per state (2 × 3,677 for SQLite, ~7.4K permanently live objects)
— which the copying collector re-traces on **every** GC cycle, and the parse
loop's allocation rate makes cycles frequent. Identical wasm for the hot
functions (`tree_build_node` etc.) ran ~3–6× slower in self-time purely from
that resident graph.

Controlled measurements (release host, per-parse steady state, 13366-byte
fixture; baseline 2.40 ms):

| resident dummy graph              | ms/parse |
| --------------------------------- | -------: |
| none                              |     2.40 |
| 1 flat `List<i32>`, 160 KB        |     3.30 |
| 7,400 lists × ~55 i32 (ATN shape) |     4.79 |

Fixes shipped: (1) an `AtEndConflict` routes to the simulator only when a
divergence token intersects the rule's whole-grammar FOLLOW
(`GenContext::at_end_conflict_routes`) — FOLLOW-disjoint conflicts (SQLite's
`UNION` vs `UNION ALL`) keep the tournament and the grammar carries no ATN;
(2) `state_cont_*` flattened to offset/count columns and the whole LR
fixpoint gated behind `atn_decode(…, lr_tables)` = `needs_scan_atn`, so a
`??` / at-end-only grammar decodes no LR tables at all. Post-fix,
`sqlite_parse` is back to 2.4 ms/iter with the generated parser
byte-identical to pre-#1475.

**Standing rule:** treat module-lifetime GC-reachable data (globals holding
lists-of-lists, big caches) as a per-collection tax on every allocating hot
loop, not as free one-time cost — prefer flat columns over nested lists, and
don't decode/build what the grammar never reads. The residual cost of even
flat resident data (~+0.9 ms/parse per 160 KB) is a Wado-runtime GC
characteristic, tracked outside Gale.

## `syntax_highlight` is GC-bound: ~77% of wall-clock is collection (2026-07)

Measure GC time directly by swapping the collector (`wado run --collector
<null|copying|drc>`): `null` never collects, so `copying − null` is the
collection cost. `null` leaks, so use a fixed-iteration driver (40×
`highlight`), not the auto-tuned harness (which OOMs the GC heap under `null`).

| collector           | ms/iter | note                          |
| ------------------- | ------: | ----------------------------- |
| `null` (no GC)      |    ~5.3 | pure compute + bump-allocate  |
| `copying` (default) |   ~23.0 | **+17.7 ms = ~77% is GC**     |
| `drc`               |    ~234 | pathological here — never use |

So the benchmark is GC-bound, and the copying collector's cost is the **live
set it traces/copies each cycle** — the whole CST during build-and-walk — not
the allocation _count_. This explains why cutting transient allocations does
not move wall-clock: a `Box`/node that dies before the next collection is never
copied, so it is free under `copying` regardless of how many there are. The
lever is the live CST footprint (§1), not allocation volume.

Corollary — measured on the compiler-side `wir/elide_adjacent_box_locals` pass
(elides the `Box<i32>` a `&primitive` payload binding lowers to, e.g.
`highlight_walk`'s `Token(i) => hl_visit_token(*i)`): it removes thousands of
per-token box allocs and shaves ~0.7 ms/iter off the `null` (bump-alloc) path,
but is **within noise under `copying`** — the boxes never entered the live set.
A correct, general optimizer win; not a lever for this GC-bound benchmark.

## Flat event-stream CST (SSOT) — LANDED, ~3× wall / ~5× GC on syntax_highlight (2026-07)

**Landed.** The per-node `CstNode` / `CstChild` tree (and the `List<BuildEvent>`
it was built from) is retired; the parser records a flat i32-column event stream
that _is_ the tree (SSOT), finalized in one linear pass, and every consumer reads
it through a `CstStore` cursor method over an `i32` row index (`s.kind(i)`,
`s.first_child`/`s.next_sibling`, `s.child_kind`, …). All 1845 package-gale tests
pass; goldens regenerated. The design and the retired-tree rationale are captured
in the "Design" subsection below plus `resilient-parser.md`.

Measured on `benchmark-syntax-highlight` (dev host, `-O2` guest, auto-tuned
harness), tree baseline → flat:

| collector           | tree | flat | change         |
| ------------------- | ---: | ---: | -------------- |
| `copying` (default) | 29.5 |  9.9 | **~3.0× wall** |
| `null` (no GC)      |  8.0 |  5.8 | ~1.4×          |
| GC portion          | 21.5 |  4.1 | **~5.2× less** |

GC share fell 73% → ~42%: no longer GC-bound. The residual ~4.1 ms GC is
highlight's own captures/HTML allocation (§4), not the CST — `sqlite_parse`
(build-then-discard, no highlight) is ~4.4 ms/iter copying, no regression vs the
~5 ms dev-host baseline. One known transient: `TreeBuilder::finish` copies the
`tag`/`a`/`alt` columns into the store because Wado has no by-value `self` /
move (methods are `&self`/`&mut self`), so the columns cannot be moved out of
the builder; the copy is exact-sized, dies immediately, and is free under
`copying`. The original prototype note (40× fixed driver, `copying` 20.3 → 3.9)
follows.

Prototyped and measured (40× fixed driver, byte-identical output). The whole CST
— `CstNode` tree + per-node `List<CstChild>` + `CstChild` variants, plus the
`List<BuildEvent>` it is built from — is thousands of small GC objects, and
that live-object count is the copying-collector tax the section above measures.
Replacing it with a **flat i32-column event stream as the single source of
truth** collapses that to a handful of arrays:

| path                       | `null` | `copying` | GC part |
| -------------------------- | -----: | --------: | ------: |
| tree (current)             |   5.18 |     20.33 |    15.2 |
| flat event-stream + cursor |   3.32 |      3.92 |     0.6 |

`copying` 20.3 → 3.9 ms (**~5.2×**); GC 15.2 → 0.6 ms (**GC essentially gone** —
no longer GC-bound). `null` also drops (no tree build, no walk-time iterator
box). This is the failed "flat green-tree + cursor" spike (below) done right:
the two prior losses were **value-cursor re-boxing** (a `Cst`/`CstChild` per
visited node — every struct is a WasmGC object) and **`array.new_default`
zero-fill of an over-sized second arena**. Both are avoided here: the parser's
event stream _is_ the store (no second arena), and traversal threads
`(&columns, index: i32)` as unbundled scalars (columns by ref, index an i32) —
no node/cursor struct is ever constructed, so the walk allocates nothing.

Design:

- `TreeBuilder` stores three parallel `List<i32>` columns (`tag` / `a` / `b`)
  instead of `List<BuildEvent>`. Method signatures are unchanged, so the
  generated parser is byte-for-byte the same driver. `Open` carries
  `end` / `flags` / `alt` columns too, patched at `finish_node` / `set_alt`
  (bubble `NODE_ERROR` up the open-stack on close), so `is_error` / `span.end` /
  `alt` stay O(1) — **no information and no O(1) query is lost** vs the node
  tree.
- Every consumer is a function over `(&columns, index)`. Linear consumers
  (`highlight`, `to_string_tree`, rendering) walk the columns forward — no
  index. Random-access consumers (`find_child`, Nth-child, typed
  `<rule>_alt`) skip-scan by depth, or use a child-offset index built once on
  demand. Only semantic change: a subtree is a **view** (cursor) into the
  shared store, not an owned deep copy — strictly better for the read-only
  consumers that exist today.

Execution: do the whole migration in one pass — retire `CstNode` / `CstChild` /
`tree_build_node`, move all consumers to the `(&columns, index)` cursor, no
`highlight`-only bypass and no build-gate. A parallel "old tree + new flat"
bridge is more total code and more risk than a single cutover; one representation
is the lowest-work, highest-ROI path. Land it with driver tests over
error-recovery input (K_ERROR / Miss / Skip columns) plus the existing
`to_string_tree` goldens, and re-confirm with `--collector null,copying` on both
`syntax_highlight` and `sqlite_parse` (build-then-discard pays column zero-fill
for less GC benefit — verify it does not regress).

## What would move the needle

Ordered by profile self-time. None are mutually exclusive.

### 1. CST build + walk (~57%) — dominant, but the obvious rewrite failed

`highlight_walk` (~29%, the single largest frame) traverses the materialized
`CstNode` tree; building it (`CstChild` push + `tree_build_node` + event-log,
~26%) allocates a `List<CstChild>` per node.

**Landed (cheap, −24%):** `tree_build_node` initialised each node's child list
as `[]`, which allocates a cap-0 list and then `grow`s on the first `push`
(empty alloc + a grow call + GC churn, per node). Pre-sizing to
`List::with_capacity(4)` (the grow-minimum, covering the common 3–4-child
fan-out) does one right-sized allocation — syntax-highlight **39.4 → 30 ms**.
The sweet spot is small: `with_capacity(8)`/`64` _regress_ (over-zero-fill via
`array.new_default`, the same trap the cursor SoA hit), so this is right-sizing,
not "reserve big". It only shows up under a live heap (build-and-walk); parse-
only is ~neutral.

The deeper lever — a flat SoA arena + cursor, no per-node list at all — was
implemented and **lost on both benchmarks**: see "Failed approaches". The walk
loss is `children()` re-boxing per visited node; the retry lever recorded there
is scalar child accessors that walk allocation-free. Until a representation
cheap to _both_ build and walk lands, the per-node-list build + traversal is the
standing open problem and the largest remaining prize.

### 2. Lexer source-char buffer — `List<char>::grow` — LANDED

The lexer collects the input into a `List<char>` (`Lexer::new`) that the
`TokenStream` then borrows. The old `input.chars().collect()` grew that buffer
from empty, reallocating `log2(n)` times over the whole source (~8% here, the
same `[]`-then-grow pattern §1 fixed for child lists). Replaced with
`String::to_chars` (`string.wado`): one `with_capacity(self.len())` pre-size
then a straight loop over the shared `decode_utf8_scalar`, no per-char `Option`
or iterator dispatch. `Lexer::new` now calls `input.to_chars()`. Re-profiled:
`List<char>::grow` is gone from the hot list (`List<char>::push` 0.8% +
`to_chars` 0.9%). Wall-clock is within noise of baseline — the buffer was never
the bound on the build-and-walk path; the `grow` self-time was dev-host GC
inflation. Kept as the better-reading, no-slower primitive (also reused by Gale).

### 3. Kind-set membership — `_kind_set_*` (~3%) — not a lever

`_kind_set_8` alone is ~2%: a `k matches { TK_… | TK_… | … }` membership test
over the large SQLite keyword set (~125 kinds), called from scan dispatch and
lookahead gates. These now lower to a Wasm `br_table` (the const-global→literal
fix unblocked `match_to_switch`), yet the self-time is unchanged from the old
compare-cascade — converting the densest, most-converted helper to a pure
`br_table` (zero comparisons) did not move it. So kind-set is **call-frequency-
bound, not dispatch-bound**: a perfect-hash / bitset would not help either
(Cranelift already lowers the cascade competitively). Left as a measured
non-lever. A pure-compute frame the dev host does _not_ inflate, so its release
share is a touch higher.

### 4. HTML render output (~8%, syntax-highlight only)

`String::push` building the HTML output, plus `push_class` (per-capture class
string, splitting `.` → space char-by-char). Levers: emit class names without
the per-char `push_class` loop, and append larger runs of unescaped source
instead of char-at-a-time. Lives in `highlight_html` / `push_class`
(`highlight.wado`).

## What does not work

- **Inlining hot methods / any per-method micro-opt.** Measured **no wall-time
  change** from forcing inlining of small hot functions — wasmtime + Cranelift
  handle small Wasm calls cheaply, so inlinability is not the lever. Confirmed
  again by the cursor spike, where raising the inline threshold _worsened_
  runtime (it bloats hot loops); see "Failed approaches".
- **Re-sizing `TokenStream::new`'s arrays** (profiles at ~8.5% self-time). The
  cost is inherent WasmGC `array.new_default` zero-fill across its 10 parallel
  `List<i32>` arrays, each pre-sized to `chars/4`. Measured the actual fill for
  the benchmark input (13366 chars): 2892 tokens, 2431 trivia vs a 3342 cap —
  only ~19% over-allocation. The current `chars/4` pre-size is already correct:
  it zero-fills ~3342/array vs ~8191/array for grow-from-`[]` doubling (~2.4×
  better), so `[]` would be worse, not better. An A/B sizing every array to its
  _exact_ used count (the unreachable ceiling of any cap-tuning) gained only ~2%,
  two-thirds of it within run jitter — sub-1% on release after dev-host alloc
  inflation. Not a lever; leave the `chars/4` pre-size.
- **Data-driven / bytecode-VM scan** (see below).

## Failed approaches (do not repeat)

### Flat green-tree + cursor CST — NO-GO (2026-06)

Replaced the per-node `CstNode` value tree with a flat SoA `CstArena` + `Cst`
cursor (rowan-style), built in one pass over the `BuildEvent` log. In isolation
the SoA build is ~2× faster, but `sqlite-parse` (which builds then discards the
tree) regressed: 4.9 ms (old) vs 18 ms cursor. A loose `List::with_capacity(n)`
zero-fills via `array.new_default`, so nine over-sized arrays dominated
(131 ms); exact-sizing the arrays cut it to 18 ms but no further. The residual
~3.7× is intrinsic to the array-heavy build on WasmGC — raising the inline
threshold only worsens it (sharp 14→15 cliff: 18 → 55 ms). Not `value_copy`
(generated NIR is clean). The walk-heavy `syntax-highlight` (build **and** walk
the whole CST) was the obvious place to win, but cursor lost there too — 64.7 ms
vs 39.4 ms — because `children()` boxes a `CstChild` + `Cst` per visited node,
moving allocation from build-time to walk-time instead of removing it. Reverted
in `37d6597`; the spike is preserved at `9b92e249` / `e48cef13` for a retry.
Retry lever: scalar child accessors (`child_kind(i)` / `child_node(i)`, no
`CstChild` box) so the walk allocates nothing.

### Data-driven (bytecode VM) scan — NO-GO (2026-06)

**Goal.** Replace the per-rule compiled `scan_*` functions with a single
bytecode interpreter over the already-clean scan IR
(`ScanBody`/`ScanElement` in `gir.wado`) + per-rule op tables, to shrink
the generated artifact (scan is ~21% of the compiled module / ~27% of
`wado compile -O2` time) and so speed up the full-build-to-test cycle.
Tempting because scan is only ~8–12% of parse self-time.

**Spike.** In a copy of the generated SQLite parser, the three hottest
_leaf_ scanners (`scan_keyword`, `scan_literal_value`, `scan_any_name`)
were rewritten to delegate to a faithful flat-`List<i32>` bytecode VM
(every op — `TOK`, `KINDSET`, `CALL`, `DISPATCH`, `OK`, `FAIL` — runs
through the interpreter loop with bounds-checked `SCAN_PROG[ip]`
fetches). Parse output was identical, so the timing is valid.

**Result (end-to-end, SQLite, `queries.sql`, 600 iters):**

| build                       |  per-parse | vs baseline |
| --------------------------- | ---------: | ----------: |
| baseline (compiled scan)    |  ~9,275 µs |           — |
| VM for 3 leaf scanners only | ~11,500 µs |    **+24%** |

Converting ~4.9% of self-time cost +24% wall time ⇒ implied slowdown
factor **K ≈ 5.9**. A full conversion projects to **+30% to +60%** parse
time.

**Why K is structurally high in Wado (not tunable away):**

1. **No `unsafe` / no raw pointers** — every bytecode fetch is a
   bounds-checked list index; the check cannot be dropped.
2. **Loss of inlining** — a 3-line `scan_keyword` is inlined into its
   caller at `-O2`; a VM call never is, and the hottest scanners are
   exactly the tiny ones that benefit most from inlining.
3. GC + value semantics add per-call overhead the compiled path avoids.

**Decision.** Keep the compiled scanner. A hybrid (some rules compiled,
some interpreted) was rejected on maintainability grounds — two
scanner-codegen backends to keep in lockstep is not worth it. The
artifact-size lever and runtime speed are in direct tension here, so the
compiled scanner stays and dev-cycle wins should come from the build
pipeline (subset/lower-opt inner-loop builds), not from interpreting
scan.

### `core:cbor`/`core:serde` for the ATN blob codec — NO-GO (2026-06)

**Goal.** Replace the hand-written ATN wire-format codec (`atn_blob_bytes`
in `atn.wado` + `atn_decode` in `runtime/atn.wado`) with derived
`Serialize`/`Deserialize` + `core:cbor` `to_bytes`/`from_bytes`, so the
field layout is generated from the struct (maximal DRY — no hand codec,
no shared constants) instead of two hand-kept-in-lockstep functions.

**Spike.** Measured the wasm-size cost in isolation (the codec linked for
a `List<i32>`-heavy struct), -O2:

| variant                                   |     wasm |
| ----------------------------------------- | -------: |
| hand `i32`-array decode (isolated)        |  7,740 B |
| `cbor` encode+decode + `serde` (isolated) | 20,622 B |

⇒ the cbor+serde codec adds **~+12.9 KB** (decode-only is less,
est. +6–9 KB net per parser). For reference the whole Binding2
parser+driver is **~33.6 KB** at -O2, so this is a **+18–40%** size hit
on every ATN-using parser. `report-wasm-size` is a tracked budget.

**Other costs.** (1) Decode runs once per `parse()` (per-`Parser`); a
generic `Deserializer` + base64/byte parse is structurally slower than a
single linear `array.get i32` walk — the wrong direction for parse speed.
(2) the runtime is inlined verbatim into every generated parser, so a
`core:serde`/`core:cbor`-based codec would carry those imports plus the
derived impls into every ATN-using parser — the size hit measured above —
where the hand codec stays dependency-free.

**Decision.** Keep the hand-written codec. After the wire-format DRY
refactor it is one writer (`atn_blob_bytes`) + one reader (`atn_decode`)

- one shared constant set in `runtime/atn.wado` — fast, zero-dependency, and
  round-trip-tested (`atn_test.wado` drives `atn_blob_bytes` → `atn_decode`
  field-for-field). The codec/reader pair is the irreducible minimum;
  serde would trade ~95 LOC for a heavyweight per-parser dependency.
  (Separable, still open: embedding the blob as base64/`#data` rather than
  an `i32`-array literal — a source-size/compile-time lever, codec-agnostic,
  worth revisiting only once a _large_ grammar's ATN literal is a measured
  problem. Today only small grammars carry an ATN.)

## Correctness items with a performance flavor

These are ATN-class prediction gaps tracked for compatibility, but each
is "Gale's static predictor commits where ANTLR4 defers" — relevant when
reasoning about the scan/predict hot path. Full context in `TODO.md`
("Stage A gaps") and `AGENTS.md` (soundness invariants).

- **LR operator-precedence chain** (`DropLoopEntryBranchInLRRule_4`):
  `scan_expr_lr_*` sees `and X` match and commits where ANTLR4 resolves
  the precedence via full-context prediction at the LR loop entry.
- **Recursive lexer rule with `.+?` / `.*?`**
  (`RecursiveLexerRuleRefWithWildcard{Plus,Star}_1`): the static
  single-pass emitter over-consumes nested `/* … */` comments.
