# Resilient Parser — Target Design

How Gale's generated parsers should behave. The parser is **error-resilient**:
it never bails on a syntax error. It always returns a tree plus diagnostics, so
language front ends, LSP, and syntax highlighting all work on broken input.

## Principles

- **One homogeneous tree.** The parser builds a single untyped `CstNode` tree
  directly — no typed-CST and no walk/convert step. The generic view consumers
  want (LSP, highlight) is the parser's only output, so it is free.
- **Infallible parsing.** Every rule function returns its node; it never returns
  `Result` and never propagates `?`. Errors are recovered locally and recorded.
- **Lossless error tokens.** Every recovery edit is representable in the tree, so
  the original input round-trips.
- **Diagnostics are the error currency.** A clean parse has an empty diagnostic
  list; a broken parse still yields a usable tree alongside the diagnostics.
- **Fast machine-generated highlight.** Highlight is one direct walk over the
  homogeneous tree (rule-kind stack → `(span, class)`), no intermediate tree.

## The tree

```
CstNode  { kind: NodeKind, span, children: List<CstChild>, flags }
CstChild = Token(i32) | Missing(i32) | Skipped(i32) | Node(CstNode)
```

- `CstNode` is a pure value tree: terminals are `i32` indices into a
  `TokenStream` held by `ParseResult`, and the node stores no reference to it,
  so the result is a freely movable value. Rendering methods (`to_string_tree`)
  take the `&TokenStream`. (A `&TokenStream` field on the node tripped a current
  WIR-lowering ICE when the result was returned by value, and the value tree is
  the cleaner design anyway — composable and cacheable.)
- `NodeKind` is an `i32` newtype (rule id; `K_ERROR` for a recovery region).
  Its `Display` renders the rule name and `Inspect` renders `name(id)`, so
  debugging shows names. The name table is grammar-specific, emitted by codegen.
- `flags`: `NODE_ERROR` (this node or a descendant was repaired; bubbles up),
  `NODE_INCOMPLETE` (a required terminal was inserted).

### Error-token vocabulary (the full set)

The three recovery edits — insert, delete, region — are each first-class:

| Concern           | Tree                  | Token-stream flag (`lex.wado`) |
| ----------------- | --------------------- | ------------------------------ |
| Inserted terminal | `CstChild::Missing`   | `TOK_SYNTHETIC` (zero-width)   |
| Deleted terminal  | `CstChild::Skipped`   | `TOK_SKIPPED`                  |
| Error region      | `Node` of `K_ERROR`   | —                              |
| Lexer no-match    | `Token` of `TK_ERROR` | `TOK_LEX_ERROR`                |

A `Missing` token keeps the _expected_ kind in the stream, so a `Missing` slot is
still "a STRING", just synthetic.

## Building: TreeBuilder + recovery

The parser drives a `TreeBuilder` (`start_node` / `token` / `missing` / `skip` /
`start_error` / `finish_node`), which records a flat event stream and
materialises each node once.

Recovery replaces `expect(k)` with `expect_or_recover(k, sync)`:

1. **match** — consume.
2. **delete** — if `peek(1) == k`, the current token is spurious: `skip` it, then
   consume `k` (`ExtraToken`).
3. **insert** — if the current token continues the rule (in FOLLOW), synthesise a
   zero-width `missing` `k`, do not advance (`MissingToken`).
4. **sync** — otherwise skip tokens into a `K_ERROR` region until a token in
   `FOLLOW(rule) ∪ FIRST(rest) ∪ anchors`; at EOF, fill remaining required
   terminals with `missing` (`UnterminatedConstruct`).

Alternative dispatch with no viable alternative produces a `K_ERROR` node and a
`NoViableAlternative` diagnostic. Sync sets reuse Gale's existing FIRST/FOLLOW
analysis.

## Diagnostics

```
Diagnostic { severity, code, message, span, line, col,
             expected: List<i32>, found: i32, rule_stack, recovery, related }
```

`expected`/`found` are token-kind ids (tooling reuses the grammar's name tables).
`code` is machine-switchable (`MissingToken`, `ExtraToken`, `NoViableAlternative`,
`UnterminatedConstruct`, `LexError`, `UnexpectedToken`); `recovery` names the edit
applied; `related` carries secondary notes (e.g. "'(' opened here").

## Public API

```
parse(input: &String, max_errors: i32 = i32::MAX) -> ParseResult
ParseResult { root: CstNode, tokens: TokenStream, diagnostics: List<Diagnostic> }
```

One entry point, behaviour tuned by a number: `max_errors` caps how many
diagnostics the parser collects before it stops recovering and folds the tree
closed (`<= 1` is effectively fail-fast, still returning a partial tree).
Defaulted, so the common call is just `parse(input)`. There is no generator
option for recovery on/off — recovery is always built in.

## Stages

Each stage is a separate PR; the package stays green at every stage.

### Stage 1 — Runtime core ✅ (done)

`tree.wado` (homogeneous `CstNode` + `TreeBuilder` + `NodeKind`), `diag.wado`
(`Diagnostic`), and `TokenStream` recovery flags in `lex.wado`. Unit-tested in
isolation; no codegen change yet.

### Stage 2 — Builder-based codegen, no recovery

A new emitter (`cst_gen.wado`) builds the homogeneous tree via `TreeBuilder` and
is infallible on _valid_ input; on the first error it fails soft (one
diagnostic, close the tree). It reuses the shared lexer / token / lowering
pipeline and consumes the lowered GIR (`Direct` dispatch + `AltBody.ops`),
ignoring the typed-struct machinery entirely — there are no typed structs,
`walk_*`, `to_tree`, or `Result`/`?`. `parse` returns a `ParseResult`.

#### Stage 2a — new emitter behind a flag ✅ (done)

Gated by `GenerateOptions.homogeneous` (Kiln `options: { homogeneous: true }`),
so the old typed path stays the default and the corpus stays green. Runtime is
`lex` + `diag` + `tree`. Covered shapes, each proven end-to-end:

- **LL(1) `Direct` dispatch** — tokens, literals, rule refs, sequences,
  `*`/`+`/`?`, transparent/token-only groups, wildcard, set complement
  (`tests/grammars/calc_ll.g4`, `tests/driver_cst_calc_test.wado`).
- **Left recursion** — precedence climbing using the builder's
  `checkpoint`/`start_node_at` left-associative wrap; self-ref suffixes recurse
  at their baked `min_prec` (`tests/grammars/arith_lr.g4`,
  `tests/driver_cst_lr_test.wado`). Suffixes that share a first token form an
  **overlap group**, disambiguated by a second-token sub-dispatch reusing the
  typed emitter's `compute_lr_second_token` projection
  (`tests/grammars/lr_overlap.g4`, `tests/driver_cst_lr_overlap_test.wado`).
- **Tournament dispatch** — alternatives that share a lookahead prefix are
  disambiguated by a longest-match scan, reusing `parser_gen`'s scan functions
  verbatim (emitted only when a tournament exists; gated with their kind-set
  dependencies) (`tests/grammars/amb_tour.g4`,
  `tests/driver_cst_tour_test.wado`).
- **Caller-FOLLOW gates** — a tail-greedy `Repeat` yields to the caller's
  continuation via `follow_yields`, threading the defaulted `follow` parameter
  and per-call-site `FollowArg` exactly as lowering computed them (so the
  soundness invariants are reused, not re-derived) (`tests/grammars/follow_gate.g4`,
  `tests/driver_cst_follow_test.wado`).
- **Non-greedy `*?` / `+?`** — the loop runs only while the lowered static exit
  condition holds (`compute_non_greedy_condition` over the per-position follow
  sets, the same gate the typed emitter computes), so the inner yields the
  closing delimiter to the continuation (`tests/grammars/non_greedy.g4`,
  `tests/driver_cst_non_greedy_test.wado`).

Grammars whose prediction needs the runtime ATN simulator — a non-greedy `??`
(lowered as `Plain` + `mark_needs_atn`) or any ATN-class LR / scan decision —
are rejected up front by a `needs_atn` boundary check, so the scope edge is a
loud `panic` rather than a silently-wrong static dispatch.

#### Stage 2b — broaden coverage, retire the old path (remaining)

**Corpus migration status.** Every driver grammar that does **not** need the
runtime ATN simulator is migrated to the homogeneous emitter — 29 homogeneous
driver tests (107 cases), each with the same one-node-per-rule tree the typed
emitter produces (so migrating a driver test is just swapping the API, not the
expectations). The migrated set spans precedence climbing with left/right
associativity (`calculator`, `right_assoc_gaps`), labels (`label_gaps`,
`label_list_collision`), shared-prefix tournaments (`at_end_alt_gaps`,
`overlap_tournament`, `alt_shared_ident_prefix`), caller-FOLLOW gates
(`ll_multi_token_tail`, `ll_k_prefix_cascade`), set/consume groups
(`parser_set_group_dispatch`, `ll_consume_group`, `parser_gaps`), non-greedy
spans (`non_greedy_gaps`), case-insensitive lexing (`ci_sql`, `ci_rule_override`),
and the lexer fixtures (`recursive_lexer`, `unicode_props`, `lexer_command_gaps`,
`lexer_greedy_suffix`, `mode_gaps`, `sexpression`).

The driver grammars that remain on the typed path fall into exactly three
buckets, none migratable by hand: **(a) ATN-class** prediction (the big
real-world grammars — Rust / TypeScript / SQLite / CSS3 / HTML — and the
`ll_*` prediction fixtures and the `lr_*` overlap fixtures), which hit the
`needs_atn` boundary and need the prediction reuse below; **(b) typed-API
tests** (`sexpression_ast` reads typed CST struct fields — these are deleted in
step 3, not migrated); **(c) later stages** (recovery: `error_recovery` /
`tie_recovery` / `diagnostics` → Stage 3; highlight: `*_highlight` → Stage 4).

**Architectural limit of the GIR-only emitter (proven).** `cst_gen` consumes
the lowered GIR, whose `MultiAltDispatch` has only `Direct` (disjoint first
sets) and `Tournament` (all alts fully scannable). It has **no representation
for ATN-class multi-alt rules** — alts that overlap *and* are not all fully
scannable. The typed emitter resolves those through its surface
`PredictionNode` path (`build_prediction` → `gen_prediction_code`:
scan-tournament → hybrid save-rewind → runtime ATN simulator), which is the
bulk of `parser_gen`. So a GIR-only `cst_gen` cannot reach full corpus parity,
and the `needs_atn` boundary check in `gen_cst_parser` rejects those grammars
up front rather than mis-emitting a static dispatch. A second, smaller GIR-only
gap is **group / atom-level tournaments** (e.g. `('x' | 'x' 'y')` in
`multiple_eof.g4`): `cst_gen` emits a rule-level tournament (`emit_tournament`)
but `emit_alt_dispatch` panics on a `Tournament` group dispatch, which the
typed emitter handles via the same surface group-prediction path. Both gaps
close together under the sink refactor below, not by extending `cst_gen`.

**The realization — swap the Parser backend, not the emission.** The key
insight that makes this tractable: a typed rule emits `let f = p.expect(K)?` /
`let c = _parse_Y(p)?` / `return Ok(XType{…})`. If the **Parser's methods append
to the `TreeBuilder` and still return `Result`** (same signatures), then every
line of prediction / scan / ATN / LR / group / repeat emission is **byte-for-byte
identical** between typed and homogeneous — because it only ever calls the
Parser API. The sink difference collapses to exactly three places:

1. **Parser runtime** — `expect` / `match_any` / `match_not` append the matched
   token to `b: TreeBuilder` (and `_parse_Y` appends its subtree); the struct is
   the typed Parser + `b`, keeping `kinds` / `atn_stack` / scan helpers so the
   shared emission and the ATN runtime calls work unchanged.
2. **Rule entry wrapper** (`gen_rule_entry_wrapper`) — `start_node(RK_X)` before
   the captured inner call, `finish_node` after, return type `Result<()>`. The
   wrapper captures `let r = inner(p)` (no `?`), so on an inner `Err` the
   `finish_node` still runs and the open node folds as the error propagates up —
   the per-alt `return _alt_N(p)?` inside the inner needs no change.
3. **`gen_alt_body` exit** — `return Ok(())` instead of assembling the struct
   (the leaf ops already appended via the Parser methods).

`?`-propagation **is** Stage 2 fail-soft: the first error folds every open node
on the way up and the public `parse` returns the partial tree plus one
diagnostic. **ATN works for free** — the prediction emission (incl.
`atn_predict_with_stack`) is untouched and drives the same `atn_stack`-carrying
Parser. Incremental + testable: the 29 homogeneous driver tests pin the trees,
so each grammar flips to the new path (behind a temporary route) and is verified
against its existing expectation; once every non-ATN grammar matches, `cst_gen`
retires and the ATN grammars pass by reuse.

This supersedes the heavier "sink-branch every emit function" framing below
(kept for context); only a small set of structure-building sites actually
differ.

**Bring-up status (proven).** The realization is wired end-to-end behind a
temporary `sink_v2` generator option (`codegen` routes the homogeneous parser
through `parser_gen`'s sink-aware path + `cst_gen`'s node-kind / `to_string_tree`
scaffolding). It already **generates a runnable parser** for `calc_ll`: the
homogeneous `Parser` (builder-appending `expect`/`match_*`, `Result` kept), the
rule wrapper (`start_node`/`finish_node`, `Result<()>`), the public `parse`
(returns `ParseResult`, `Err` → one diagnostic), `gen_alt_body`'s `Ok(())`, and
the leaf line (identical for both sinks — `gen_op_leaf` is typed-only again,
the Parser appends) are all in and green. The remaining work to reach tree
parity is to give the **typed structure-building sites** a sink branch that
skips field assembly and just lets the body append: `gen_op_repeat` (drop the
`List<ItemX>` accumulation — loop the body), `gen_op_group` (drop the group
struct — dispatch + emit the chosen alt's elements), and the single-token /
LR-assembly fns. Each is mechanical and verified against the 29 existing
homogeneous trees, one grammar at a time, until `sink_v2` reaches parity and
becomes the only path.

**Decision: one emitter, pluggable node sink (not two emitters).** Reaching
"homogeneous is the only path" by porting the prediction/ATN core into `cst_gen`
would clone `parser_gen`'s most intricate logic into a second body kept in
lockstep — the exact anti-pattern soundness invariant #3 retired. Instead,
refactor `parser_gen`'s single emission pipeline over a node **sink**
(`Typed` builds structs + `Result`/`?`; `Homogeneous` drives the `TreeBuilder`,
infallible), reusing every prediction decision (incl. ATN) unchanged:

1. Thread the sink through `parser_gen`'s emit functions; `Typed` stays the
   default and the corpus stays byte-identical and green. **Done:** the sink
   flag (`GenContext.is_homogeneous`) and the first leaf layer — `gen_op_leaf`
   now branches on the sink (typed binds a CST field with `?`; homogeneous
   drives the `TreeBuilder`), and `cst_gen.emit_op` delegates its leaf ops to
   it. Both corpora byte-identical and green.
2. Wire the `Homogeneous` sink at the leaf / rule-entry / dispatch layers
   (the ATN return-stack plumbing — `emit_atn_enter`, `gen_atn_ret_pending`,
   `atn_*_decision` — is reused verbatim). Migrate the driver + ANTLR4-compat
   corpus behind the `homogeneous` flag, batch by batch, each proven green.
3. Delete the `Typed` sink, `gen_cst_types`, `visitor_gen`, and `cst_gen`;
   `homogeneous` becomes the only path.

**Mechanics established while starting the refactor.** Two facts shape the
remaining steps:

- **Surface-element threading is the shared prerequisite.** `cst_gen` walks the
  lowered GIR `Op`s; every reuse of `parser_gen`'s prediction (group dispatch,
  ATN) needs the *surface* `Element`s (`build_prediction` consumes them). So the
  next foundational step is to walk `Element`s and `Op`s in parallel (as
  `gen_alt_elements` already does) so the homogeneous emission has surface
  access at each op.
- **`build_prediction` is a pure decision oracle** (`prediction.wado` — returns
  the `PredictionNode` tree, no emission). The ATN-class / group-tournament gaps
  close by walking that same tree emitting `TreeBuilder` actions, so the
  prediction *logic* is reused unchanged (not cloned) — only the emission walk
  differs by sink, which cannot diverge on the decision because both walks read
  the one `PredictionNode`.

Because the homogeneous op-emission and the prediction it calls are mutually
recursive (a group's prediction emits ops; an op may be a group needing
prediction), they must live in one module: the leaf/repeat/group/dispatch
emission consolidates into `parser_gen`'s sink-branched emitters (it cannot
import `cst_gen`), shrinking `cst_gen` to scaffolding (runtime, node kinds,
`to_string_tree`) until step 3 absorbs it.

### Stage 3 — Recovery

Add `expect_or_recover`, missing/skipped/`K_ERROR` emission, the no-viable-alt
fallback, sync sets from FIRST/FOLLOW, and the `max_errors` entry parameter.
New fixtures assert diagnostics and error-token trees for broken input.

### Stage 4 — Highlight + polish

Rewrite `highlight.wado` to walk the homogeneous tree directly (drop the
Visitor-conversion path). Generate `NodeKind` `Display`/`Inspect` name impls.
Add `related`-note hints (e.g. matching brackets). Refresh docs and the ANTLR4
compatibility suite.
