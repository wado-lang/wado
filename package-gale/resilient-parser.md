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
  `tests/driver_cst_lr_test.wado`).

Tournament dispatch, overlapping LR suffixes, and non-greedy raise a
codegen-time panic (still out of scope).

#### Stage 2b — broaden coverage, retire the old path (remaining)

Cover Tournament dispatch / non-greedy / overlapping LR suffixes (reuse
`parser_gen`'s scan & prediction), migrate the full driver + ANTLR4-compat
corpus to the homogeneous parser, then delete the typed-CST emitter
(`gen_cst_types`, `visitor_gen`) and make `homogeneous` the only path.

### Stage 3 — Recovery

Add `expect_or_recover`, missing/skipped/`K_ERROR` emission, the no-viable-alt
fallback, sync sets from FIRST/FOLLOW, and the `max_errors` entry parameter.
New fixtures assert diagnostics and error-token trees for broken input.

### Stage 4 — Highlight + polish

Rewrite `highlight.wado` to walk the homogeneous tree directly (drop the
Visitor-conversion path). Generate `NodeKind` `Display`/`Inspect` name impls.
Add `related`-note hints (e.g. matching brackets). Refresh docs and the ANTLR4
compatibility suite.
