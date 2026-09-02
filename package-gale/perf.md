# Gale Performance Notes

Performance notes for the Gale code generator and its generated parsers, in
three parts:

- **Current state** — what the benchmarks measure, the latest numbers, and the
  representation and standing rules that shape them.
- **What's next** — candidate levers not yet taken.
- **Tried and didn't pan out** — measured dead-ends and non-levers, kept so we
  don't repeat them.

Wado-wide performance rules — the WasmGC cost model, what decides adoption, and
the measured dead-ends — live in the `wado-performance` skill; this file keeps
what is true of Gale specifically.

Read with [`AGENTS.md`](./AGENTS.md) (dev-cycle essentials) and
[`antlr4-compatibility.md`](./antlr4-compatibility.md) (prediction / codegen
design). Design of the flat CST lives in
[`resilient-parser.md`](./resilient-parser.md).

**Performance-related TODO items live here, not in `TODO.md`.**

## Current state

### Benchmarks

**`benchmark/sqlite_parse`** (parse only: build the CST, then `result.ok()`)
measures the build. **`benchmark/syntax_highlight`** measures highlighting, and
as of 2026-09-02 that no longer includes a build: `SQLite.highlights.scm` has no
rule-context capture, so nothing reads the tree and `highlight` does not parse.
Both run the same 13366-byte SQLite fixture, guest at `-O2`.

**TODO: no benchmark covers highlighting over a tree.** That path — parse, walk,
rule-context capture — is only covered functionally, by
`tests/driver_cst_json_highlight_test.wado`. Either give `SQLite.highlights.scm`
a rule-context capture to bring it back under measurement, or add a benchmark
that has one.

Dev-host numbers (`cargo run` `wado`; post-flat-CST, 2026-07). Read the split as
a ratio, not as absolutes: the same benchmark measured **1.467 ms/iter** on a dev
host on 2026-09-02, against 1.436–1.479 on a release one. The two hosts sit
within each other's spread now, because `Cargo.toml` raises `opt-level` on the
compiler's dependencies and wasmtime is one of them. Re-measure both columns
before sizing a GC lever off them.

| benchmark                         | `copying` (default) | `null` (no GC) |     GC part |
| --------------------------------- | ------------------: | -------------: | ----------: |
| `syntax_highlight` (build + walk) |        ~9.5 ms/iter |   ~5.8 ms/iter | ~4.1 (~42%) |
| `sqlite_parse` (build only)       |        ~4.4 ms/iter |              — |           — |

`syntax_highlight` is **no longer GC-bound**: collection was ~42% of wall-clock
at that snapshot, down from ~73% on the old node tree (see "The CST is a flat
store"). The release headline + comparison baselines (Gale vs `tree-sitter` for
highlight, vs `sqlparser-rs` for parse) come from `mise run syntax-highlight` /
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
`benchmark/README.md`) — the dev multiple is GC-inflated (the `wado-performance`
skill's §1). Still enough to move Gale from 4th to 2nd place, ahead of native
tree-sitter and Lezer.

The residual ~4.1 ms GC is highlight's own captures/HTML allocation (the profile
below), not the CST — `sqlite_parse` (build-then-discard,
no highlight) is ~4.4 ms/iter and did not regress. One known transient:
`TreeBuilder::finish` copies the `tag`/`a`/`b`/`alt` columns into the store
because Wado has no by-value `self` / move (methods are `&self`/`&mut self`);
each copy is exact-sized (a `List` value copy right-sizes to `used`) and the
builder's originals die with the parse.

**Column pre-size (landed).** `TreeBuilder::with_capacity` sizes the event columns
to `4 × tokens` (measured `rows ≈ 3.44 × tokens`) so they never grow from empty —
one right-sized `array.new_default` instead of `log2(rows)` doubling reallocs, at
~16% over-fill (comparable to `TokenStream`'s `chars/4`). Release `sqlite_parse`
2.52 → 2.36 ms/iter (~6%, build-heavy), `syntax_highlight` ~2% (build is a smaller
share); no over-fill regression.

### Dispatch reaches every lexer rule (landed, 2026-08)

Two emit-site defects had the tokenizer call rules that could not match: a
reference to a non-fragment lexer rule counted as unbounded (Rust: 325 `try_`
call sites → 161), and a `mode` grammar skipped first-char dispatch outright
(TypeScript 0 → 57 branches, 116 calls per character → 10 worst case).

Unmeasured here — both benchmarks run a modeless grammar whose first sets were
already exact, so size this on a keyword-dense or mode-bearing grammar.

### Standing rules (measured)

The four rules these benchmarks established — live set over allocation count,
module-lifetime GC data as a per-collection tax, benchmark on an idle host
against a same-window arm, and size a GC win by its release number — are Wado's,
not Gale's, and live in the `wado-performance` skill with the evidence from here.

What is Gale's: the decoded ATN is the module-lifetime data those rules are
about. `state_cont_*` is flat offset/count columns and the LR fixpoint is gated
behind `needs_scan_atn` for exactly that reason (#1475) — a change that puts one
`List` per ATN state back into a global will cost 3–6× on `sqlite_parse` however
fast its own code is.

### The highlight phase pays only for what a query asks for (landed, 2026-09-02)

Only a rule-context capture reads the parse tree. Everything below follows from
that, and together the levers take `syntax_highlight` from 1.551 to 0.508
ms/iter. The first two are worth **+8.0%** on a release host (1.551 → 1.436
ms/iter, alternating best of five per arm, arms' ranges disjoint):

- **No rule-context capture, no CST walk (+6.5%).** `highlight_walk` visits
  every CST row — `rows ≈ 3.44 × tokens` — only to maintain the visitor's rule
  stack, which `classify` reads inside the override loop and nowhere else. A
  query with no rule-context capture leaves that loop empty, so the walk is
  unobservable: `hl_cover_unvisited`'s token sweep reaches the same captures.
  `gen_highlight` now emits the call only when the query resolved an override.
  Skipping the walk **without** also gating `hl_cover_unvisited`'s sort is a
  _loss_ (1.553–1.615) — the sweep alone is already in start order, and sorting
  ~2900 captures costs more than the walk did.
- **Escape HTML by byte run (+1.8%).** `highlight_html` drove a `StrCharIter`
  and called `escape_html_char`, which called `String::push` — three calls per
  source character, ~40K per iteration. The escapable set is ASCII, so a byte
  walk is UTF-8-correct (only a lead byte advances the char position) and the
  stretch between two escapes is one `array.copy`.

Note what the second one is not: the 2026-07 run-batching attempt below, which
kept the char iterator and measured flat. Batching was never the lever; the
calls were.

`nir/drop_value` (`docs/optimizer.md`) took another 3.3% on top of the two, so
the three come to **1.515 → 1.351 ms/iter (+12.1%)** against `origin/main`,
three alternating pairs with the order swapped and the arms disjoint. That pass
is Wado-wide rather than Gale's, but this is the benchmark it showed on: the
parser and `TreeBuilder` discard a `pop()` per closed node, and each was
allocating the `Option` it threw away.

Then the same question one level up retired the rest. Skipping the walk left
`highlight` still calling `parse`, building a CST that nothing then read.
`gen_highlight` now routes a query without a rule-context capture down the path
lexer-only grammars have always used: tokenize, sweep, render. Best of three,
**9.80 → 26.21 MB/s (1.359 → 0.508 ms/iter, 2.7x)**, HTML byte-identical to
`origin/main`'s over the 13321-byte fixture. The profile predicted it — parse
was ~51% inclusive and `TreeBuilder::finish` 9.0% — and 0.851 ms of the 1.359
is 62.6%.

### Live profile (`syntax_highlight`, 2999 leaf samples @1 ms, 2026-09-02)

Taken after the two levers above, before `nir/drop_value` and before the token
sweep stopped marking `visited`. The dev profile is noisy per-frame; mid-size
frames swing ±several points across runs, so read the buckets, not the rows.

|  Pct | Symbol                             | bucket                        |
| ---: | ---------------------------------- | ----------------------------- |
| 5.8% | `TreeBuilder::push_row`            | CST column build              |
| 5.7% | `scan_any_name`                    | scan                          |
| 5.5% | `scan_expr`                        | scan                          |
| 5.5% | `try_IDENTIFIER`                   | lexer                         |
| 4.8% | `push_escaped_upto`                | highlight (HTML output)       |
| 4.8% | `TreeBuilder::finish`              | CST finalize + column copy    |
| 4.7% | `_kind_set_4`                      | kind-set membership           |
| 4.4% | `TokenStream::push_token_flagged`  | tokenize                      |
| 4.2% | `bubble_to_parent`                 | CST finalize                  |
| 3.2% | `HighlightVisitor::hl_visit_token` | highlight                     |
| 3.1% | `follow_yields`                    | scan/predict (LL FOLLOW gate) |
| 2.8% | `String::grow`                     | HTML output realloc           |
| 2.8% | `highlight_html`                   | highlight (HTML output)       |
| 2.7% | `tokenize`                         | lexer                         |

Inclusive buckets: **parse** ~51%, **tokenize** ~20%, **highlight** ~17%
(`highlight_html` 10.4% + the token sweep 6.8%), **CST finalize**
(`TreeBuilder::finish`) 9.0%. Highlight was ~23% before the two levers above.

## What's next

Pick the current top frame off the live profile above rather than a fixed recipe
here: the frames shift as levers land, and the mid-size ones are noisy, so
re-measure before committing. Candidates read off the profile above:

- **`TreeBuilder::finish` (9.0% inclusive, `bubble_to_parent` 4.2% of it).** It
  allocates three `List::filled(n, 0)` columns and value-copies `tag` / `a` /
  `b` / `alt` out of the builder, because Wado has no by-value `self` — seven
  ~10K-element arrays per parse, and the copies are _live_ for the whole walk,
  not transient. Building the parser's events straight into the store, with
  `finish` filling `end` / `flags` / `next` in place, removes all four copies.
- **First-char dispatch is linear in the ranges, not the rules.** The dispatch is an
  `if / else if` chain over the first-char sets, so a rule opening on a large set costs
  a comparison per range — a `[\p{L}]` rule is ~700. Coalescing branches with identical
  call lists keeps the emitted code small (Rust: 56 branches, 161 `try_` calls) but
  leaves ~2000 comparisons on the fall-through path. ASCII resolves early (single-char
  branches are sorted and come first), so this is a worst case rather than a
  benchmark-visible cost. A sorted interval table with a binary search would bound it.
- **Every scan alternative re-tests the token its dispatch selected it on.**
  `gen_scan_multi_alt` binds `alt_kind = pos < tokens.len() ? tokens[pos] :
  TK_EOF` and branches on it; each partition body then re-reads the same token —
  `if pos >= tokens.len() || tokens[pos] != TK_IDENTIFIER { break try_0; }` in
  one arm, a second `_kind_set_37(tokens[pos])` inside `scan_keyword` in the
  next. `scan_any_name` is 5.7% self, `_kind_set_4` 4.7% and `_kind_set_37`
  2.6%, and those frames are call-frequency-bound, so this is the class of calls
  to cut. The elision is sound when the partition's guard set excludes `TK_EOF`
  (so the branch implies `pos < tokens.len()`) and the alt's first scan element
  accepts every token in that guard — then the body is `pos += 1`. Both are
  static: the guard set is `ScanAltPlan::groups_tokens[g]`, the accepted set is
  `ScanBody::elements[0]`. A hoisted alt is emitted as its own `<base>_alt_<i>`
  helper, so it needs the checked entry point kept for any other caller.

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

- **Compute-side gen levers (2026-08, landed).** `benchmark/gale_gen` (Rust
  grammar, dev host) profiled four compute hot spots; fixing them (plus the
  review round: in-place FOLLOW propagation instead of per-contribution
  FollowBits copies, and the left-corner reachability walks precomputed in id
  space) took best-of-three from 728.8 to ~336 ms/iter (~2.2×), generated
  output byte-identical for the Rust and SQLite grammars:
  - `build_dispatch_groups` + first-char accumulation (~25% self): `add_range`
    and the distinct-range collection deduped by linear scan, each (char, call)
    probe walked the whole range list, and `ranges_meet_unclaimed` counted
    claimed chars by filtering the full list per range. Now packed-i64 set
    probes (insertion order kept), a sorted-starts/prefix-max-end index per
    call, and binary-search claimed counts.
  - `follow_env` fixed point (~11%): re-derived FIRST names and re-interned
    them through a String TreeMap every iteration. FIRST contributions are
    iteration-invariant, so each RuleRef site flattens once to (callee, static
    kind-id bits, inherits-caller-FOLLOW) and the loop is pure bitset
    propagation in the kind-id space.
  - Kind-set canonicalisation (~6%): the per-call String sort + name-joined
    registry key became an id sort by a lazily-rebuilt name-rank table with an
    id-joined key. Rank order == name order, so canonical order and helper ids
    are unchanged.
  - `check_left_recursion` (~4%): per-element visiting-set nullable recursion
    with a linear rule-name scan → one worklist nullable table + name→index
    map per grammar (same least fixed point).

  Remaining top self frames after these: `String^Eq::eq` + `String^Ord::cmp` +
  `TreeMap<String, i32>` probes (~14%, the `visiting` lists and rule-name maps
  of the SCC-memoized analyses), `$value_copy$` IR deep copies (~12%, diffuse
  across lower / prediction / parse), and prediction (`build_sll_node` ~14%
  inclusive).

  (A `type TokenId = i32` newtype for the id — so a `Display` can format token
  names later — currently trips a `$value_copy` codegen ICE when a
  newtype-valued `TreeMap` coexists with its `i32` base in a value-copied struct
  (minimal repro saved). A compiler P0; use plain `i32` until it is fixed.)

- **Borrow the IR the generator only reads (2026-08-25, landed).** A
  release-host guest profile of `benchmark/gale_gen` (4026 leaf samples) put
  **23%** of the run in `$value_copy$` helpers — `RuleRefElement` 5.2%,
  `TokenRefElement` 3.5%, `Alternative` 2.8% — prediction paying 10.1% of it and
  `lower` 7.0%. Two shapes:

  - `SllConfig.elements` / `SllReturn.elements` are now `&List<Element>`.
    Prediction rebuilds a config per token per alternative (`SllConfig { ..*c,
    pos }`) and deep-copied the element list — every `String` of every
    `RuleRefElement` — per rebuild. `push_return` / `build_prediction` declare
    the `stores[...]` the borrow needs.
  - Five by-value `for` bindings became `for … of &…` (`lexer_elem_refs_rule`,
    `collect_literal_tokens`, `merge_grammars`, `sll_advance`,
    `try_expand_opaque`). A by-value binding deep-copies each element inside
    `SliceValueIter<T>::next`; a by-ref one SROAs to a bare indexed load.

  Alternating A/B best-of-five, `-O2`: **222.6 → 294.5 KB/s** (154.5 → 116.3
  ms/iter, **+32%**), 5/5 rounds; the by-ref loops are ~5 points of it. Generated
  Rust parser byte-identical. Copies fall to 13.9% of the profile and
  by-value-`for` copies to 1.3%; the top frame is now string-keyed `TreeMap`
  probing (`String^Ord::cmp` 8.8%, `Eq::eq` 4.3%, `find_index` 3.1%,
  rebalancing ~5%) — the id-carrying lever above, applied to the name-keyed maps
  that are left.

  This borrows a list that is already a root, so it adds nothing to the live set
  — unlike sharing its _elements_, which cost 3× ([`dead-ends.md`](../.claude/skills/wado-performance/dead-ends.md)).

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

General findings — forcing inlining, raising the inline threshold, `br_table`
over a compare cascade, `String::grow` pre-sizing, index loops in place of
`for x of &List<i32>` — were all first measured here and now live in the
`wado-performance` skill and its `dead-ends.md`. What stays is bound to Gale's
own shapes:

- **Re-sizing `TokenStream::new`'s arrays** (~8.5% self-time). The cost is
  inherent WasmGC `array.new_default` zero-fill across 10 parallel `List<i32>`
  arrays, each pre-sized to `chars/4`. Measured fill for the benchmark input:
  2892 tokens + 2431 trivia vs a 3342 cap — only ~19% over-allocation; `chars/4`
  zero-fills ~3342/array vs ~8191 for grow-from-`[]` doubling (~2.4× better), so
  `[]` would be worse. Exact-sizing every array gained only ~2%, two-thirds
  within jitter. Leave the `chars/4` pre-size.
- **Kind-set membership** (`_kind_set_*`, ~3%). A `k matches { TK_… | … }` test
  over the large SQLite keyword set, called from scan dispatch and lookahead
  gates. Lowering it to a `br_table` left self-time unchanged, so kind-set is
  **call-frequency-bound, not dispatch-bound** — cut the calls, not the branch.
  The same answer came back for `classify_keyword`'s first-char dispatch.
- **Run-at-a-time HTML escaping** (2026-07). `highlight_html` escapes char by
  char; batching the stretches between escapable bytes into one
  `push_str_range_unchecked` measured **flat** (median 2.93 vs 2.90 ms/iter). The
  captures are dense — ~2900 over 13366 chars, so the mean unescaped stretch is
  ~4.6 chars and the per-run bookkeeping costs what the batching saves.
  Input-shape-bound: sparse captures would answer differently. Batching the
  _same_ runs off a byte walk instead of the char iterator did pay (+1.8%,
  2026-09-02) — what the loop dropped was three calls per character, not the
  copies.
- **Hoisting `HighlightVisitor::classify`'s default path** (2026-09-02) to get it
  under the inline budget: flat, and the WIR shows it still not inlined.
  `.claude/skills/wado-performance/dead-ends.md` has the numbers. Deleting the
  call's _caller_ is what paid.

## Correctness items with a performance flavor

These are ATN-class prediction gaps tracked for compatibility — Gale's static
predictor committing where ANTLR4 defers, or deferring where the lookahead
would have settled it — relevant when reasoning about the scan/predict hot
path. Full context in `TODO.md` ("Soundness and compatibility divergence") and
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
- **A `Repeat` config is walked as a single iteration, so a decision the
  lookahead could settle buys a scan instead — probably not worth acting on
  (2026-08).** `sll_advance` moves a `Repeat` config past the repeat and never
  emits the "still looping" reading, so `a : X+ Y | X Z` on `X X Y` has no
  branch for the second `X` and drops into the dispatch's fallback tournament,
  which scans each alt from the decision point
  (`tests/grammars/ll_repeat_alt_gap.g4`). The tree is right either way: the
  tournament picks the alt whose body parses, and the `else` is always there
  for a well-formed grammar — `dispatch_fallback_indices` withholds it only
  where the cascade provably claims every first token, or where an alt is not
  fully scannable, which needs a reference to a name that is not a parser rule.
  What makes it hard to price: the fallback tournament is emitted at 32 sites
  across TypeScript (20), css3 (7), Rust (3) and ANTLRv4 (2) and at **none** in
  `sqlite` or `json`, so nothing in the benchmark set pays for it, and those
  sites are not all this shape (`typeParameter : identifier constraint? | …` is
  an at-end gap, not a repeat one). Worth revisiting only if a profile puts one
  of them on top; the repair is not cheap either, since a second config per alt
  collides with the `alt_index` dedup key — the failed approach `AGENTS.md`
  names first — so it wants dedup keyed by position, which is walk work the
  runtime simulator is the complete form of.
