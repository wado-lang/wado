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

#### Stage 2b — full corpus on the homogeneous sink ✅ (done)

**One emitter, sink-branched (not two emitters).** The homogeneous parser is
emitted by `parser_gen`'s **single** emission pipeline, branched on the node
sink (`GenContext.is_homogeneous`). The `Typed` sink binds CST fields and
assembles structs + `Result`/`?`; the `Homogeneous` sink drives a
`TreeBuilder` (the Parser's `expect`/`match_*` append the matched token, and a
rule-entry wrapper does `start_node`/`finish_node`), so **every prediction /
scan / ATN / LR / group / repeat decision is reused verbatim** — only the
structure-building sites differ:

- `gen_alt_body` returns `Ok(())` instead of assembling the alt struct.
- The repeat storables (`gen_op_repeat_star_plus_storable`,
  `gen_op_repeat_non_greedy_storable`) drop the `List<T>` accumulation and the
  per-iteration `.push`, keeping the loop / scan-guard / caller-FOLLOW gate.
- The greedy/shape-lookahead Optionals collapse the `Option<T>` if-expression
  to a present/absent statement.
- The group store ops (`gen_consume_*` / `gen_general_*` / the overlap loops)
  drop the `let var: Type = …` value binding and the variant construction; the
  dispatch (overlap grouping + scan tournament) and the chosen alt's appending
  body are shared. A standalone group that matches nothing appends nothing
  (matching the typed `_optional_op` path, which yields `null`).
- ATN-class LR uses the same `atn_lr_loop_decision` dispatch
  (`gen_lr_fn_homog_atn`) the typed `gen_lr_suffix_dispatch_atn` emits.

Because the structure-building branch lets the **typed** prediction code drive
the appending Parser, the ATN-class gaps close by reuse, not re-implementation:
the runtime ATN simulator (`atn_predict_with_stack`, `atn_lr_loop_decision`)
and the scan-tournament machinery (`gen_scan_winner_tournament`) emit the same
calls for both sinks; the homogeneous runtime inlines `atn.wado` when
`needs_atn`. `?`-propagation **is** the Stage-2 fail-soft: the first error
folds every open node on the way up and `parse` returns the partial tree plus
one diagnostic.

**Corpus status.** The whole parse/tree corpus runs on the homogeneous sink
and matches the typed trees one-node-per-rule:

- The non-ATN grammars (precedence climbing, labels, shared-prefix
  tournaments, caller-FOLLOW gates, set/consume groups, non-greedy spans,
  case-insensitive + lexer fixtures).
- The **ATN-class** grammars — the `ll_*` / `lr_*` prediction fixtures
  (`ll_basic`, `ll_multi_alt`, `ll_wildcard_alt`, `lr_between`,
  `lr_scan_caller`, `multiple_eof`, …) and the big real-world grammars
  (Rust, TypeScript, SQLite, CSS3, HTML, ANTLRv4) — parse homogeneously, with
  `driver_cst_*` driver tests pinning the trees / end-to-end parse.

The typed corpus stays byte-identical and green throughout (the `Typed` branch
is never touched). A pre-existing scan bug surfaced by the unified walker — a
non-greedy wildcard `.*?` / `.+?` scanned zero inner tokens (empty
`inner_first`) and undershot — was fixed: the scan now skips to the exit set
(`gen_scan_non_greedy_skip`, `ScanRepeatElement.non_greedy`).

**Remaining for "homogeneous the only path" (step 3).** Every feature now runs
on the homogeneous sink — parse/tree (this stage), error reporting (Stage 3
down-payment below), and highlight (Stage 4 down-payment below). The typed-CST
emitter (`gen_cst_types`, `visitor_gen`) is kept alive only by tests that still
drive the *typed* path: the typed-API `sexpression_ast`, the `trace` test, the
redundant `driver_<x>` duplicates of the `driver_cst_<x>` tests, and the
generated ANTLR4-compat **stage_b** corpus (83 parse tests — these flip to
homogeneous in one extractor change once the homogeneous parser exposes
per-rule entry points and sub-tree rendering; stage_a's 285 tokenize tests are
sink-independent and need no migration). Retiring the typed path is now a
mechanical cleanup (migrate/delete those tests, then drop `gen_cst_types` /
`visitor_gen` / the typed codegen branch), not a feature dependency. The
temporary `sink_v2` bring-up flag and its duplicate `driver_cst_v2_*` tests are
already removed.

### Stage 3 — Recovery (error reporting on the homogeneous sink ✅; error-token edits remaining)

The error-*reporting* bucket runs on the homogeneous sink: a scan-gated repeat's
speculative re-parse records the precise inner error (`TreeBuilder::truncate`
brackets it so the throwaway subtree is rolled back along with the cursor), the
parse entry picks the deepest error (`p.pending` vs the propagated `e`), and the
diagnostic carries the active rule chain. `error_recovery`, `tie_recovery`, and
`diagnostics` pass as `driver_cst_*` tests.

Still to do for full recovery: `expect_or_recover` (insert missing / delete
extra / sync to FOLLOW), `Missing`/`Skipped`/`K_ERROR` emission into the tree,
the no-viable-alt fallback, and the `max_errors` entry parameter, with fixtures
asserting error-token trees for broken input.

### Stage 4 — Highlight ✅ (+ polish remaining)

Highlight walks the homogeneous `CstNode` tree directly: the sink-independent
core (`HighlightVisitor` + mapping + `classify` + `highlight_html` with inherent
`hl_*` hooks) lives in `highlight.wado`, the typed-CST `Visitor` bridge in a thin
`highlight_visitor.wado` (typed path only), and the homogeneous walk
(`highlight_walk` over `CstNode`) in `highlight_homog.wado`. A clean parse walks
the tree (rule-context overrides apply); a broken parse falls back to a flat
default-class token pass. `json_highlight` / `sqlite_highlight` pass as
`driver_cst_*` tests.

Polish remaining: generate `NodeKind` `Display`/`Inspect` name impls and add
`related`-note hints (e.g. matching brackets).
