# Gale Performance Notes

Performance notes for the Gale code generator and its generated parsers, in
three parts:

- **Current state** — what the benchmarks measure, the latest numbers, and the
  representation and standing rules that shape them.
- **What's next** — candidate levers not yet taken.
- **Tried and didn't pan out** — measured dead-ends and non-levers, kept so we
  don't repeat them.

Read with [`AGENTS.md`](./AGENTS.md) (dev-cycle essentials) and
[`antlr4-compatibility.md`](./antlr4-compatibility.md) (prediction / codegen
design). Design of the flat CST lives in
[`resilient-parser.md`](./resilient-parser.md).

**Performance-related TODO items live here, not in `TODO.md`.**

## Current state

### Benchmarks

The basis is **`benchmark/syntax_highlight`**: it builds the full CST _and_
walks it (highlighting), so it exercises the realistic consumer path — build +
traverse. `benchmark/sqlite_parse` (parse only: build the CST, then
`result.ok()`) is the build-isolation companion. Both run the same 13366-byte
SQLite fixture, guest at `-O2`.

Dev-host numbers (`cargo run` `wado`; post-flat-CST, 2026-07):

| benchmark                         | `copying` (default) | `null` (no GC) |     GC part |
| --------------------------------- | ------------------: | -------------: | ----------: |
| `syntax_highlight` (build + walk) |        ~9.5 ms/iter |   ~5.8 ms/iter | ~4.1 (~42%) |
| `sqlite_parse` (build only)       |        ~4.4 ms/iter |              — |           — |

`syntax_highlight` is **no longer GC-bound** (~42% of wall-clock is collection,
down from ~73% on the old node tree — see "The CST is a flat store"). The
release headline + comparison baselines (Gale vs `tree-sitter` for highlight, vs
`sqlparser-rs` for parse) come from `mise run syntax-highlight` /
`mise run sqlite-parse` on a release host; not reproduced here (dev-only).

> **Measurement note.** These are dev-profile `wado` profiles, which over-weight
> allocation/GC frames; read the percentages as relative and size any GC win by
> its release number. Mechanism: the `wado-performance` skill.

Reproduce (guest self-time profile, and the collector split for GC cost):

```sh
cd benchmark
wado run --no-cache --profile guest,/tmp/p.json,1 -O2 syntax_highlight/syntax_highlight.wado
wado run --collector null   -O2 syntax_highlight/syntax_highlight.wado   # no GC
wado run --collector copying -O2 syntax_highlight/syntax_highlight.wado   # default
```

(`wado` = `cargo run --bin wado --`. Analyze `p.json` with the `wado-performance`
skill's script — count **leaf** frames for self-time — or upload to
profiler.firefox.com. `copying − null` is the collection cost; `drc` is
pathological here (~234 ms/iter) — never use it. `null` leaks, so use a
fixed-iteration driver, not the auto-tuned harness, which OOMs the heap under
`null`.)

### The CST is a flat store (SSOT)

The parser records a flat i32-column event stream that _is_ the tree (single
source of truth), finalized in one linear pass; consumers read it through
`CstStore` cursor methods over an `i32` row index (`s.kind(i)`,
`s.first_child`/`s.next_sibling`, `s.child_kind`, …). This retired the per-node
`CstNode` / `CstChild` object tree (and the `List<BuildEvent>` it was built
from) — thousands of small WasmGC objects the copying collector re-traced every
cycle, which is what made `syntax_highlight` GC-bound. Design in
`resilient-parser.md`.

Result vs the old node tree (dev host, `benchmark-syntax-highlight`):

| collector           | node tree | flat store |         change |
| ------------------- | --------: | ---------: | -------------: |
| `copying` (default) |      29.5 |        9.5 | **~3.0× wall** |
| `null` (no GC)      |       8.0 |        5.8 |          ~1.4× |
| GC portion          |      21.5 |        4.1 | **~5.2× less** |

On the **release** host the same change is only **~1.47×** (5.06 → 3.45 ms/iter,
`benchmark/README.md`) — the dev multiple is GC-inflated (see the standing rule
below). Still enough to move Gale from 4th to 2nd place, ahead of native
tree-sitter and Lezer.

The residual ~4.1 ms GC is highlight's own captures/HTML allocation (the profile
below), not the CST — `sqlite_parse` (build-then-discard,
no highlight) is ~4.4 ms/iter and did not regress. One known transient:
`TreeBuilder::finish` copies the `tag`/`a`/`alt` columns into the store because
Wado has no by-value `self` / move (methods are `&self`/`&mut self`); the copy
is exact-sized, dies immediately, and is free under `copying`.

**Column pre-size (landed).** `TreeBuilder::with_capacity` sizes the event columns
to `4 × tokens` (measured `rows ≈ 3.44 × tokens`) so they never grow from empty —
one right-sized `array.new_default` instead of `log2(rows)` doubling reallocs, at
~16% over-fill (comparable to `TokenStream`'s `chars/4`). Release `sqlite_parse`
2.52 → 2.36 ms/iter (~6%, build-heavy), `syntax_highlight` ~2% (build is a smaller
share); no over-fill regression.

### Standing rules (measured)

- **Live-set is the cost, not allocation count.** Under the copying collector,
  cost = the live set traced/copied each cycle; an object that dies before the
  next collection is never copied, so it is free regardless of how many there
  are. So cutting _transient_ allocations does not move wall-clock — confirmed on
  the compiler-side `wir/elide_adjacent_box_locals` pass, which removed thousands
  of per-token `Box<i32>` allocs (−0.7 ms/iter under `null`) but was **within
  noise under `copying`**. The lever is the live footprint, not allocation volume.
- **Module-lifetime GC-reachable data is a per-collection tax.** The decoded
  `ATN_SIM` global once held one inner `List<i32>` per state (~7.4K permanently
  live objects for SQLite); the copying collector re-traced them on **every**
  cycle, and identical wasm for the hot functions ran ~3–6× slower purely from
  that resident graph (`sqlite_parse` 2.4 → 4.3 ms/iter, #1475). Controlled: a
  resident 160 KB flat `List<i32>` costs ~+0.9 ms/parse; the 7,400-list ATN shape
  cost ~+2.4 ms. Fix shipped by flattening `state_cont_*` to offset/count columns
  and gating the LR fixpoint behind `needs_scan_atn`. **Prefer flat columns over
  nested lists, and don't decode/build what the grammar never reads.** The
  residual cost of even flat resident data is a Wado-runtime GC characteristic,
  tracked outside Gale.
- **Benchmark on an idle host, and only against an arm measured in the same
  window.** An A/B of the continuation probe run beside a compiling test suite
  put both arms at 4.65–4.97 MB/s; idle, the same two commits measured 4.83–5.36
  and their ranking flipped. Worse, one commit measured `sqlite_parse` at 2.302
  and 2.918 ms/iter a few hours apart on the same idle host — a 27% swing for
  identical code, while the `sqlparser-rs` reference row stayed at its baseline,
  so the reference does not certify the window either. A number in this file is
  evidence about the commits it was taken beside, not an absolute to compare a
  later run against; re-measure both arms rather than one.
- **A GC-bound win measured on the dev host overstates the release win.** The
  dev-profile runtime / GC / allocator run unoptimized (~4–5× slower, the
  measurement note above), so GC is a far larger share of dev wall-clock than
  release, and eliminating it looks correspondingly bigger. Concrete: the flat
  CST cut `syntax_highlight` ~3× on dev (GC ~5×) but only ~1.47× on release,
  because release GC was only ~⅓ of wall-clock to begin with. **Size any
  GC-focused optimization by its release number, not the dev multiple** — and
  conversely, a compute-bound win (e.g. the highlight `classify` / render levers)
  carries over to release largely intact, since the dev host does not inflate
  pure compute.

### Live profile (`syntax_highlight`, 1928 leaf samples @1 ms)

`List<i32>::push` is the top frame. The dev profile is noisy per-frame (see the
measurement note); mid-size frames swing ±several points across runs, so read the
buckets, not the individual rows.

|   Pct | Symbol                             | bucket                                   |
| ----: | ---------------------------------- | ---------------------------------------- |
| 14.5% | `List<i32>::push`                  | CST column build (+ `rule_stack`/trivia) |
|  6.2% | `TreeBuilder::finish`              | CST finalize + column copy               |
|  6.2% | `HighlightVisitor::classify`       | highlight                                |
|  5.0% | `_kind_set_8`                      | kind-set membership                      |
|  4.4% | `char::to_ascii_lowercase`         | case-insensitive keyword match           |
|  4.3% | `scan_any_name`                    | scan                                     |
|  3.4% | `scan_expr`                        | scan                                     |
|  3.3% | `highlight_html`                   | highlight (HTML output)                  |
|  3.1% | `try_IDENTIFIER`                   | lexer                                    |
|  2.7% | `List<i32>::pop`                   | CST/rule-stack                           |
|  2.6% | `String::internal_reserve_uninit`  | HTML output realloc                      |
|  2.4% | `follow_yields`                    | scan/predict (LL FOLLOW gate)            |
|  2.4% | `TokenStream::new`                 | SoA token-array alloc (WasmGC zero-fill) |
|  2.4% | `HighlightVisitor::hl_visit_token` | highlight                                |

Rough buckets: **CST build + walk** (`List<i32>` push/pop + `finish` +
`highlight_walk`) ≈ **~25%**; **highlight** (`classify` + `highlight_html` +
`escape_html_char` + `hl_visit_token`) ≈ **~14%**; **scan/predict**
(`follow_yields` + `scan_*` + kind-set) ≈ **~18%**; **lexer**
(`to_ascii_lowercase` + `try_*` + `classify_keyword` + `to_chars`) ≈ **~11%**.
CST column build is the largest bucket.

## What's next

Pick the current top frame off the live profile above rather than a fixed recipe
here: the frames shift as levers land, and the mid-size ones are noisy, so
re-measure before committing. Three candidates read off the profile above:

- **`List<i32>::push` (14.5%, top frame).** The columns are already pre-sized, yet
  every element still pays `push`'s `used >= repr.len()` grow check — ~10 columns per
  token (`TokenStream`) plus 4 per CST row (`TreeBuilder::push_row`, `rows ≈ 3.44 ×
  tokens`). `push_within_capacity` exists for exactly this ("a burst of appends after
  one `reserve` pays a single capacity check instead of one per element"), but both
  pre-sizes are heuristics (`4 × tokens`, `chars/4`) that a different input can exceed,
  and `push_within_capacity` leaves an over-run to the array bounds check, i.e. a trap.
  So the shape is **one capacity check per row/token with a grow fallback**, then
  unchecked appends across the columns — 10 checks → 1, not 10 → 0.
- **`char::to_ascii_lowercase` (4.4%).** `gen_keyword_check` (`lexer_gen.wado`) emits the
  guard for every char from `kc = 0`, so each keyword arm re-tests the first char that the
  enclosing dispatch already established; and that dispatch is a linear `else if` chain of
  `eq_ignore_ascii_case` calls over the distinct first letters (~6–20 deep per length
  bucket). Lowercase the first char once and `match` on it (br_table), and drop the
  redundant `kc = 0` guard. Pure compute, so it carries to release intact.
- **`HighlightVisitor::classify` (6.2%).** A non-inlined call per token that walks the
  override list before the `default_ids[kind]` lookup, even when — as for SQLite — there
  is exactly one override. Hoisting the common `default_ids` path to the call site
  (overrides only when the kind is in an override's kind set) would cut it.

### Generation-time cost: the generator itself (2026-07)

Distinct from the _generated parser_: how long `gale gen` takes to emit a large
grammar. Keyword-heavy grammars (SQLite, TypeScript, Rust) are slow; the Kiln
build path (copying collector + fuel) makes TypeScript/Rust take on the order of
an hour. Measured findings, `wado run … gen` (`cargo run` host):

- **The dominant cost was not GC — it was an exponential analysis (2026-07,
  fixed).** `gale gen` on TypeScript (30 min+) and Rust was ~99% inside
  `GenContext::is_rule_scannable_at`, the recursive scannability check the
  optional-scan-guard lowering runs per optional/repeat element. Profiling the
  exploding run (guest profiler with a `WADO_PROFILE_MAX_SECS` bounded flush —
  the profile only writes on clean exit, so an unbounded 30-min run is unusable)
  put `is_element/alt/rule_scannable_at` at **98.8% inclusive**; it barely
  registered on SQLite (57 s). Root cause: the SCC-aware memoization (`80ab9ed7e`)
  caches a rule only once its SCC has closed (`local_min >= p`); in a large
  mutual-recursion SCC (TS's expression / type / statement web) nearly every node
  stays provisional and is recomputed on every path → exponential (a synthetic
  dense-recursion grammar scaled ≈`1.4^R`; R=20 → 5.6 s, R=24 blew past minutes).
  Scannability is actually pure reachability — a rule is non-scannable iff it can
  reach an _undefined_-rule reference, the only `false` source in
  `is_element_scannable_at` — so `precompute_scannability` computes it once in
  O(V+E) and seeds `scannable_cache`, making the recursion O(1). **Byte-identical
  output** (css3 / SQLite / follow_gate md5 unchanged); **TypeScript 30 min+ →
  ~15 s gen** (63 s wall incl. the ~48 s generator recompile), **Rust similar**;
  the synthetic repro 5.6 s → 0.10 s. Lesson: profile keyword-heavy grammars
  end-to-end — a super-linear analysis hides behind the GC number, and micro-
  grammars miss it (the trigger is deep _mutual recursion_, not size/alt-count).
  The four other SCC-memoized analyses — `first_of_rule_at`, `rule_is_nullable_at`,
  `rule_is_single_token_at`, `tail_greedy_first_of_rule_at` — share the same
  SCC-root-only caching (`local_min >= p`) and are latent suspects on other
  grammars (they did _not_ dominate the TS/Rust profiles, so not yet active).
  Unlike scannability (pure reachability, precomputable), these compute real
  fixpoint values, so the fix is a shared SCC-complete cache (Tarjan: fix all
  members once the SCC closes), not a reachability precompute.

- **Two levers only: compute, and GC.** Isolate GC with `--collector null` (no
  GC; leaks, so only for a one-shot gen) vs the default `copying`:

  | grammar | output  | first-sets      | null (compute) | copying | GC       |
  | ------- | ------- | --------------- | -------------- | ------- | -------- |
  | css3    | 1.76 MB | small           | 39.1s          | 41.5s   | **2.4s** |
  | SQLite  | 786 KB  | large (150+ kw) | 38.5s          | 61–65s  | **~24s** |

- **GC scales with distinct-token count, not output size.** css3 emits a _bigger_
  file with ~10× _less_ GC — its first-sets are tiny. So the copying collector's
  cost is re-tracing the thousands of `String` token objects held in the
  long-lived first / kind / FOLLOW caches, and it explodes on keyword grammars
  (TypeScript `null`-gen OOMs — it accumulates that many token Strings).
  `follow_env`'s bitset FOLLOW fixed point (2026-07) removed one such holder;
  the FIRST-set caches and kind-set registry are the remaining ones.

- **Collector switch is a non-lever.** DRC (cost ∝ garbage) is _worse_ on small
  grammars — SQLite DRC 118s vs copying 61s — because its per-alloc overhead
  dwarfs the small live set; it only wins where copying thrashes a huge live set
  (TypeScript ~7min vs ~1h). A blanket kiln→DRC switch would regress the common
  case. Reduce allocation instead.

- **The lever (landing): the token's identity _is_ a dense integer, carried on
  the IR, not a name we cache.** `token_slot_order` already numbers every token
  (slot index == the emitted `TK_*` value); `token_kinds.wado`'s `TokenKinds`
  table (id ↔ name) is built once and `resolve_kind_ids` stamps each
  `Element::kind_id`. FIRST/kind/FOLLOW sets are then `List<i32>`; names are
  recovered only at emit (`token_names_of` / `kind_check_str`).

  **Do NOT re-cache via a lazy `String→i32` intern in the hot path** — measured a
  net loss (SQLite copying 61s → 80s, null 38.5s → 48s): the per-call intern is a
  String tree-lookup costing more than the compare it replaced, and it still
  rebuilds `TK_{name}`. The fast path is `elem.kind_id` (resolved once); the only
  fallback intern is for unresolved IR in unit tests.

  Byte-identity rules: FIRST-set order = insertion order of ids (== today's name
  order, 1:1); kind sets still canonicalise **by name** at `intern_kind_set`
  (sort names, then map to ids) so emit order/dedup are unchanged; emit writes
  the name, not the number. Measure the win as the `copying` − `null` GC delta.

  Progress (SQLite `wado run gen`, `copying` − `null` GC): baseline 24s.
  - P0 (table + `kind_id` + resolution pass, inert) — byte-identical.
  - P1+P2 (FIRST sets + prediction carry ids) — GC 21s, copying 59.3s, compute
    back to baseline (null 38.1s).
  - P3 (kind/sync/follow-mask registries store ids) — GC 17.5s, copying 56.5s.
    Remaining P4 (the `lower`/`parser_gen` FIRST-set boundary, `rule_follow_kinds`)
    is smaller and coupled on SQLite; the keyword-dense TS/Rust grammars (GC-bound)
    are where the accumulated cut should matter most.

  (A `type TokenId = i32` newtype for the id — so a `Display` can format token
  names later — currently trips a `$value_copy` codegen ICE when a
  newtype-valued `TreeMap` coexists with its `i32` base in a value-copied struct
  (minimal repro saved). A compiler P0; use plain `i32` until it is fixed.)

## Tried and didn't pan out

### Flat green-tree + cursor CST — NO-GO (2026-06), later done right

The **first** attempt at a flat CST: a SoA `CstArena` + `Cst` cursor
(rowan-style), built in one pass over the `BuildEvent` log. In isolation the SoA
build was ~2× faster, but `sqlite-parse` (build then discard) regressed 4.9 →
18 ms: a loose `List::with_capacity(n)` zero-fills via `array.new_default`, so
nine over-sized arrays dominated (131 ms); exact-sizing cut it to 18 ms but no
further. `syntax-highlight` also lost — 64.7 vs 39.4 ms — because `children()`
boxed a `CstChild` + `Cst` per visited node, moving allocation from build-time to
walk-time instead of removing it. Reverted in `37d6597`.

**Done right (2026-07, now the current state).** The retry lever recorded here —
scalar child accessors so the walk allocates nothing, and the parser's event
stream _as_ the store (no second arena, columns grown by `push` not
`with_capacity`) — is exactly what shipped: `copying` 29.5 → 9.5 ms. The two
losses to avoid, confirmed: **value-cursor re-boxing** (a struct per visited node
— every Wado struct is a WasmGC object) and **`array.new_default` zero-fill of an
over-sized arena**.

### Data-driven (bytecode VM) scan — NO-GO (2026-06)

**Goal.** Replace the per-rule compiled `scan_*` functions with a single
bytecode interpreter over the scan IR (`ScanBody`/`ScanElement`) + per-rule op
tables, to shrink the generated artifact (scan is ~21% of the compiled module /
~27% of `wado compile -O2` time) and speed up the build-to-test cycle.

**Spike.** The three hottest _leaf_ scanners were rewritten to delegate to a
faithful flat-`List<i32>` bytecode VM (bounds-checked `SCAN_PROG[ip]` fetches).
Output was identical, so the timing is valid.

| build (SQLite, 600 iters)   |  per-parse | vs baseline |
| --------------------------- | ---------: | ----------: |
| baseline (compiled scan)    |  ~9,275 µs |           — |
| VM for 3 leaf scanners only | ~11,500 µs |    **+24%** |

Converting ~4.9% of self-time cost +24% wall ⇒ implied slowdown **K ≈ 5.9**; a
full conversion projects **+30–60%** parse time.

**Why K is structurally high in Wado (not tunable away):** (1) no `unsafe` — every
bytecode fetch is a bounds-checked list index; (2) loss of inlining — a 3-line
`scan_keyword` inlines at `-O2`, a VM call never does, and the tiniest scanners
benefit most from inlining; (3) GC + value semantics add per-call overhead the
compiled path avoids.

**Decision.** Keep the compiled scanner. A hybrid was rejected on maintainability
(two scanner backends in lockstep). Dev-cycle wins should come from the build
pipeline (subset/lower-opt inner-loop builds), not from interpreting scan.

### `core:cbor` / `core:serde` for the ATN blob codec — NO-GO (2026-06)

**Goal.** Replace the hand-written ATN wire-format codec (`atn_blob_bytes` +
`atn_decode`) with derived `Serialize`/`Deserialize` + `core:cbor`, so the field
layout is generated from the struct.

**Spike (isolated wasm size, -O2):** hand `i32`-array decode = 7,740 B; cbor +
serde = 20,622 B ⇒ **~+12.9 KB** (decode-only est. +6–9 KB net per parser). The
whole Binding2 parser+driver is ~33.6 KB at -O2, so **+18–40%** on every ATN-using
parser; `report-wasm-size` is a tracked budget. Also: decode runs once per
`parse()`, and a generic `Deserializer` is structurally slower than a single
linear `array.get i32` walk — wrong direction for parse speed.

**Decision.** Keep the hand-written codec — one writer + one reader + one shared
constant set, round-trip-tested (`atn_test.wado`). (Separable, still open:
embedding the blob as base64/`#data` rather than an `i32`-array literal — a
source-size/compile-time lever, worth revisiting only once a _large_ grammar's
ATN literal is a measured problem.)

### Measured non-levers

- **Inlining hot methods / per-method micro-opt.** No wall-time change from
  forcing inlining of small hot functions; the cursor spike went further —
  raising the inline threshold _worsened_ runtime (bloats hot loops: sharp
  14→15 cliff, 18 → 55 ms). (Why small-call inlining rarely helps on wasmtime:
  the `wado-performance` skill.)
- **Re-sizing `TokenStream::new`'s arrays** (~8.5% self-time). The cost is
  inherent WasmGC `array.new_default` zero-fill across 10 parallel `List<i32>`
  arrays, each pre-sized to `chars/4`. Measured fill for the benchmark input:
  2892 tokens + 2431 trivia vs a 3342 cap — only ~19% over-allocation; `chars/4`
  zero-fills ~3342/array vs ~8191 for grow-from-`[]` doubling (~2.4× better), so
  `[]` would be worse. Exact-sizing every array gained only ~2%, two-thirds
  within jitter. Leave the `chars/4` pre-size.
- **Kind-set membership** (`_kind_set_*`, ~3%). A `k matches { TK_… | … }` test
  over the large SQLite keyword set, called from scan dispatch and lookahead
  gates. These lower to a Wasm `br_table`, yet the self-time is unchanged from
  the old compare-cascade — so kind-set is **call-frequency-bound, not
  dispatch-bound**; a perfect-hash/bitset would not help (Cranelift already
  lowers the cascade competitively). A pure-compute frame the dev host does _not_
  inflate, so its release share is a touch higher.
- **HTML-output `String` pre-size** (`String::grow`, 9% dev self-time). The
  `highlight_html` output grows once past its `source.len() * 5` reserve (HTML is
  ~6× source for keyword-dense SQL). Bumping to `* 7` removes the grow but a
  release A/B (best-of-5) was **identical** (3.90 vs 3.91 MB/s): `String::grow` is
  an allocation/zero-fill cost the dev host inflates, so its release share is
  ~1–2%, below the `syntax_highlight` benchmark's noise. A live example of the
  dev-vs-release standing rule — unlike the CST-column pre-size, which lands a
  clear ~6% because `sqlite_parse` is build-only. Left at `* 5`.

- **Run-at-a-time HTML escaping** (2026-07). `highlight_html` escapes char by char;
  batching the stretches between escapable bytes into one `push_str_range_unchecked`
  measured **flat** (median 2.93 vs 2.90 ms/iter). The captures are dense — ~2900 over
  13366 chars, so the mean unescaped stretch is ~4.6 chars and the per-run bookkeeping
  costs what the batching saves. Input-shape-bound: sparse captures would answer
  differently.

- **Index loops instead of `for x of &List<i32>`** (2026-07). Iterating by reference
  boxes every element; rewriting `follow_yields`'s membership scan and `classify`'s
  `rule_stack` scan as index loops removes every box and measured **within noise** (won
  1 of 3 paired rounds). Another instance of the live-set standing rule — the boxes die
  immediately, so the collector never traces them.

## Correctness items with a performance flavor

These are ATN-class prediction gaps tracked for compatibility, but each is
"Gale's static predictor commits where ANTLR4 defers" — relevant when reasoning
about the scan/predict hot path. Full context in `TODO.md` ("Soundness and compatibility divergence") and
`antlr4-compatibility.md` (prediction design, soundness invariants).

**A memoised ATN / lookahead DFA is a last resort.** It was the named lever for
the two entries below before each closed on the compiled scan instead. It is
unmeasured in Gale, but ANTLR4's lookahead DFA _is_ that cache and still parses
this grammar and input at 216.991 ms/iter against Gale's 2.535
(`benchmark/README.md`). Reach for the scan and the runtime FOLLOW gate first.

- **LR operator-precedence chain** (`DropLoopEntryBranchInLRRule_4`):
  `scan_expr_lr_*` sees `and X` match and commits where ANTLR4 resolves the
  precedence via full-context prediction at the LR loop entry. The mid-operand
  half (`expr BETWEEN expr AND expr` against `expr AND expr`) is **closed on the
  scan, not the simulator (2026-07)**: an LR self-reference that competes with
  its own alternative's later delimiter drops to `min_prec = 0` and carries the
  suffix continuation as a mask, so each loop entry scans the operator's suffix
  and then checks the continuation still stands. The gate rides the static LR
  dispatch, so a rule already routed to the simulator keeps its precedence and
  still diverges on the climbing cases — `lr_atn_mid_operand.g4` pins that half,
  two cases `#[TODO]`. The simulator answer had been priced out for the static
  half — on the dev profile over 40 statements it took `SELECT … BETWEEN 1 AND
  10 AND y = 2` from 41 ms to 2.3 s.
- **`lr_between.g4` is still ATN-class and may not need to be.** Its shared-delimiter
  competition sits in an _atom_ alternative (`'between' expr 'and' expr` — no leading
  self-reference), so the continuation gate above does not reach it. The question it
  asks is the same one, so the same gate may apply; if it does, the simulator comes
  out of grammars that embed it today. Untried.
- **Ambiguous greedy `rule?` and non-greedy `*?` / `+?` min-match — closed on
  the scan, not the simulator (2026-07).** The simulator answer was implemented
  and reverted at release `sqlite_parse` **2.604 → 402.372 ms/iter (155×)**:
  one prediction is a full closure over the grammar, and neither bounding the
  lookahead nor gating it brought that down. What shipped decides the ambiguity
  with the compiled scan instead — a greedy `?` scans its body then the
  continuation before entering, a non-greedy loop scans the continuation before
  exiting. Where the scan runs out — the rule's tail — the verdict conjoins
  the rule's classical FOLLOW, which cost one bug fix in `follow_env` (an
  optional's callee was receiving the inner's own FIRST) rather than a second
  runtime argument. That last conjunct is why a probe may only be stamped where
  the walk really reaches the rule's tail (soundness invariant 10). Release
  `sqlite_parse` measured unchanged at every step, each arm's own spread moving
  further than any gap between the arms.
- **Recursive lexer rule with `.+?` / `.*?`**
  (`RecursiveLexerRuleRefWithWildcard{Plus,Star}_1`): the static single-pass
  emitter over-consumes nested `/* … */` comments.
