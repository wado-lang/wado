# Resilient Parser — Target Design

How Gale's generated parsers should behave. The parser is **error-resilient**:
it never bails on a syntax error. It always returns a tree plus diagnostics, so
language front ends, LSP, and syntax highlighting all work on broken input.

## Principles

- **One uniform tree.** The parser builds a single untyped CST directly, as a
  flat columnar store (`CstStore`) — no typed-CST and no walk/convert step. The
  generic view consumers want (LSP, highlight) is the parser's only output, so
  it is free.
- **Infallible parsing.** Every rule function returns its node; it never returns
  `Result` and never propagates `?`. Errors are recovered locally and recorded.
- **Lossless error tokens.** Every recovery edit is representable in the tree, so
  the original input round-trips.
- **Diagnostics are the error currency.** A clean parse has an empty diagnostic
  list; a broken parse still yields a usable tree alongside the diagnostics.
- **Fast machine-generated highlight.** Highlight is one flat forward scan over
  the store (rule-kind stack → `(span, class)`), no intermediate tree. It always
  walks the partial tree — even on broken input — so rule-context overrides
  survive wherever the parser built structure (a fragment's interpolation, an
  unterminated construct); a follow-up pass default-classifies any token the
  walk did not reach, keeping default coloring and text intact across the rest.

## The tree

The CST is a flat pre-order event stream held in parallel `i32` columns — the
single source of truth, not a node object tree. A node is addressed by the row
index of its `E_OPEN` event (row 0 is the root):

```
CstStore { tag, a, b, alt, end, flags, next }   // parallel List<i32>
row tags: E_OPEN | E_CLOSE | E_TOK | E_MISS | E_SKIP
```

- Consumers read the store through `CstStore` cursor methods over a row index —
  `kind` / `span` / `alt` / `is_error` / `first_child` / `next_sibling` /
  `child_kind` / `find_child` / `to_string_tree` — threaded as unbundled scalars
  (`&CstStore`, `i32`), so traversal allocates nothing. The store holds no
  reference to the `TokenStream` (owned by `ParseResult`), so the result is a
  freely movable value; methods that need terminal text take a `&TokenStream`.
- `NodeKind` is an `i32` newtype (rule id; `K_ERROR` for a recovery region).
  Its `Display` renders the rule name and `Inspect` renders `name(id)`, so
  debugging shows names. The name table is grammar-specific, emitted by codegen.
- `flags`: `NODE_ERROR` (this node or a descendant was repaired; bubbles up),
  `NODE_INCOMPLETE` (a required terminal was inserted). `end` is `span.end` and
  `next` the row past a node's subtree — both derived by the `finish()` finalize
  pass so every query stays O(1).

### Error-token vocabulary (the full set)

The three recovery edits — insert, delete, region — are each first-class:

| Concern           | Store                 | Token-stream flag (`lex.wado`) |
| ----------------- | --------------------- | ------------------------------ |
| Inserted terminal | `E_MISS` row          | `TOK_SYNTHETIC` (zero-width)   |
| Deleted terminal  | `E_SKIP` row          | `TOK_SKIPPED`                  |
| Error region      | `E_OPEN` of `K_ERROR` | —                              |
| Lexer no-match    | `E_TOK` of `TK_ERROR` | `TOK_LEX_ERROR`                |

A `Missing` token keeps the _expected_ kind in the stream, so a `Missing` slot is
still "a STRING", just synthetic.

## Building: TreeBuilder + recovery

The parser drives a `TreeBuilder` (`start_node` / `token` / `missing` / `skip` /
`start_error` / `finish_node`), which appends a flat event stream into the
columns and finalizes them once (one linear pass in `finish()`).

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
ParseResult { cst: CstStore, tokens: TokenStream, diagnostics: List<Diagnostic> }
```

One entry point, behaviour tuned by a number: `max_errors` caps how many
diagnostics the parser collects before it stops recovering and folds the tree
closed (`<= 1` is effectively fail-fast, still returning a partial tree).
Defaulted, so the common call is just `parse(input)`. There is no generator
option for recovery on/off — recovery is always built in.

## Status

Everything above is built. The parser is fully **infallible**: no generated
function returns `Result` or propagates `?`. A match that cannot consume its
terminal recovers in place via a `recovering` flag rather than unwinding a
`Result`.

**Recovery — error-token edits (done).**

- `expect(kind, sync)` recovers locally: delete a spurious terminal
  (`<skip>`, `ExtraToken`), insert a missing one when the current token
  continues the rule (`<missing>`, `MissingToken`, `sync` = static
  FIRST-of-rest), or skip an unrecoverable run into a lossless `<error>`
  (`K_ERROR`) region and resync to a `sync` token. Only a no-sync mismatch
  unwinds.
- Scan-gated `*`/`+` loops over a RuleRef body enter a malformed element when
  its FIRST token is present, so the broken element lands in the tree with its
  repair edits.
- The no-viable-alt fallback records a `NoViableAlternative` diagnostic (the
  unwind/fold represents the error region).
- `max_errors` is threaded onto the parser: once reached, recovery stops and
  folds the tree closed.
- Fixtures in `tests/driver_cst_error_recovery_test.wado` assert the
  insert / delete / `<error>`-resync / `max_errors` trees;
  `tests/driver_cst_diagnostics_test.wado` asserts the `NoViableAlternative`
  code.

**Highlight (done): `NodeKind` `Display`/`Inspect`.** Codegen emits name-aware
impls next to `RULE_NAMES`, so `{node.kind}` prints the rule name and
`{node.kind:?}` prints `name(id)`.

### Deferred

- **Recovery re-entry (fragment structure).** A bare snippet whose tokens the
  start rule can't derive (e.g. a top-level statement under an `item*` start
  rule) is left unconsumed, so nested-only constructs build no subtree. Design
  for grammar-agnostic, backtrack-free re-entry: `recovery-reentry.md`.
- **No-viable `K_ERROR` _node_.** The no-viable fallback carries the
  `NoViableAlternative` code but does not open an explicit `K_ERROR` node:
  the diagnostic's `rule_stack` is built on unwind, which is incompatible
  with placing a node and continuing. The fold represents the error region.
- **`related`-note bracket hints (e.g. "'(' opened here").** Needs
  bracket-pair detection the IR does not support today — `LiteralOp` carries
  no literal text, and pairing openers/closers across nesting plus tracking
  the opener position at runtime is a feature in its own right. ANTLR4 does
  not generate these automatically either. Revisit if a consumer needs it.
