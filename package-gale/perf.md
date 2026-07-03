# Gale Performance Notes

Performance notes for the Gale code generator and its generated parsers, in
three parts:

- **Current state** — what the benchmarks measure, the latest numbers, and the
  representation and standing rules that shape them.
- **What's next** — candidate levers not yet taken.
- **Tried and didn't pan out** — measured dead-ends and non-levers, kept so we
  don't repeat them.

Read with [`AGENTS.md`](./AGENTS.md) (architecture, LL-prediction design) and
[`antlr4-compatibility.md`](./antlr4-compatibility.md). Design of the flat CST
lives in [`resilient-parser.md`](./resilient-parser.md).

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

> **Measurement note (read before trusting percentages).** Profiles are captured
> with the **dev-profile `wado`** (`cargo run`, per the inner-dev-loop guidance
> in the root `CLAUDE.md`). `Cargo.toml` raises `opt-level` on
> `cranelift-codegen`, so JIT-compiled **guest code is near-release quality**,
> but the wasmtime runtime, GC, and allocator run at dev speed — ~4–5× slower
> per iter, with the slack in **allocation/GC**. So the profile inflates
> allocation-bound frames relative to pure-compute ones (`scan_*`, kind-set).
> Read the percentages as relative.

Reproduce (guest self-time profile, and the collector split for GC cost):

```sh
cd benchmark
wado run --no-cache --profile guest,/tmp/p.json,1 -O2 syntax_highlight/syntax_highlight.wado
wado run --collector null   -O2 syntax_highlight/syntax_highlight.wado   # no GC
wado run --collector copying -O2 syntax_highlight/syntax_highlight.wado   # default
```

(`wado` = `cargo run --bin wado --`. Analyze `p.json` with the `profiling-wado`
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
below and "What's next: Highlight"), not the CST — `sqlite_parse` (build-then-discard,
no highlight) is ~4.4 ms/iter and did not regress. One known transient:
`TreeBuilder::finish` copies the `tag`/`a`/`alt` columns into the store because
Wado has no by-value `self` / move (methods are `&self`/`&mut self`); the copy
is exact-sized, dies immediately, and is free under `copying`.

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

### Live profile (post-flat-CST, `syntax_highlight`, 3 runs merged, 4123 leaf samples @1 ms)

`highlight_walk` no longer appears (the flat scan is cheap), and `tree_build_node`
/ `List<CstChild>::push` are retired. Top self-time frames now:

|   Pct | Symbol                         | bucket                                   |
| ----: | ------------------------------ | ---------------------------------------- |
| 10.5% | `List<i32>::push`              | CST column build (+ `rule_stack`/trivia) |
|  8.2% | `HighlightVisitor::classify`   | highlight                                |
|  7.1% | `follow_yields`                | scan/predict (LL FOLLOW gate)            |
|  6.2% | `push_class`                   | highlight (HTML class emit)              |
|  6.1% | `List<i32>::grow`              | CST column realloc (grow-from-empty)     |
|  5.1% | `highlight_html`               | highlight (HTML render)                  |
|  4.9% | `TreeBuilder::finish`          | CST finalize + column copy               |
|  4.5% | `List<HighlightCapture>::push` | highlight (captures)                     |
|  4.2% | `String::grow`                 | HTML output realloc                      |
|  4.1% | `String::push`                 | HTML output build                        |
|  3.0% | `char::to_ascii_lowercase`     | case-insensitive keyword match           |
|  2.6% | `scan_any_name`                | scan                                     |
|  2.5% | `_kind_set_8`                  | kind-set membership                      |

Rough buckets: **highlight** (`classify` + `push_class` + `highlight_html` +
captures + `String` grow/push + `to_ascii_lowercase` + `hl_visit_token`) ≈
**~39%**; **CST build** (flat columns push/grow/pop + `finish` +
`bubble_to_parent`) ≈ **~24%**; **scan/predict** (`follow_yields` + `scan_*` +
kind-set) ≈ **~15%**. The walk is gone; **highlight is now the single dominant
cost**, and the CST is moderate to _build_ (column pushes + a grow-from-empty
realloc tax) but cheap to _walk_.

## What's next

Candidate levers from the post-flat-CST profile, ordered by bucket, none
mutually exclusive.

### 1. Highlight (~39%) — now the dominant cost

The whole highlight path is the biggest bucket; several independent levers, all
in `highlight.wado`:

- **`classify` (8.2%)** — per token it linear-scans the resolved overrides, then
  looks up `defaults[kind]`. Called once per terminal (and once per trivia). The
  override scan is O(overrides) per token; a kind-indexed override table (or a
  short-circuit when a rule has no overrides) would cut it.
- **HTML render (`push_class` 6.2% + `highlight_html` 5.1% + `String`
  grow/push 8.3%)** — `push_class` splits `.` → space char-by-char; `highlight_html`
  appends escaped source char-at-a-time. Emit class names without the per-char
  loop, append larger unescaped runs, and pre-size the output `String`.
- **Captures (`List<HighlightCapture>::push` 4.5%)** — the captures list grows
  from empty (see the column-presize lever below), and each `HighlightCapture` is
  a GC object — this list is the ~4.1 ms residual GC the current-state section
  flags. Pre-size it, or render directly without materialising the list.
- **`to_ascii_lowercase` (3.0%)** — case-insensitive keyword matching in the
  lexer; a lower-on-read or a case-folded scan table would remove it.

### 2. CST column grow-from-empty (`List<i32>::grow` ~6%)

The `TreeBuilder` columns (`tag`/`a`/`b`/`alt`) grow from `[]`, reallocating
`log2(rows)` times over the parse — the same grow-from-empty tax the old lexer
buffer and child-list presizes fixed. Pre-size them to a fraction of the token or
char count (`TokenStream` uses `chars/4`). **Watch the `array.new_default`
over-zero-fill trap** (a loose `with_capacity` regressed the failed cursor spike):
right-size, don't reserve big, and re-check `sqlite_parse` (build-then-discard
pays the fill for less benefit).

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
  forcing inlining of small hot functions — wasmtime + Cranelift handle small
  Wasm calls cheaply. Confirmed again by the cursor spike, where raising the
  inline threshold _worsened_ runtime (it bloats hot loops: sharp 14→15 cliff,
  18 → 55 ms).
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

## Correctness items with a performance flavor

These are ATN-class prediction gaps tracked for compatibility, but each is
"Gale's static predictor commits where ANTLR4 defers" — relevant when reasoning
about the scan/predict hot path. Full context in `TODO.md` ("Stage A gaps") and
`AGENTS.md` (soundness invariants).

- **LR operator-precedence chain** (`DropLoopEntryBranchInLRRule_4`):
  `scan_expr_lr_*` sees `and X` match and commits where ANTLR4 resolves the
  precedence via full-context prediction at the LR loop entry.
- **Recursive lexer rule with `.+?` / `.*?`**
  (`RecursiveLexerRuleRefWithWildcard{Plus,Star}_1`): the static single-pass
  emitter over-consumes nested `/* … */` comments.
