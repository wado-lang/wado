# Gale Highlight Query — Customizable Syntax Highlighting

## Context

Gale's `highlight` generator option emits a `highlight(input) -> String`
function that renders source as HTML `<span class="...">` spans. Until now the
token → capture classification was a fixed heuristic in `highlight_gen.wado`:
`classify_lexer_rule` matched rule-name substrings (`COMMENT`, `STRING`, `K_`,
`KEYWORD`, …) and `classify_literal` matched literal text (all-alpha → keyword,
brackets, delimiters, `true`/`false`/`null` → `constant.builtin`). This is
implicit, grammar-name-dependent, and gives the grammar author no control: a
token named `ATOM` that is really a keyword got no highlight, and there was no
way to say "an identifier inside a `functionCall` is a function name."

The runtime (`runtime/highlight.wado`) already carried the right data model —
`HighlightMapping { defaults, rule_names, overrides }`, where `overrides`
expresses "inside rule X, token kind K → capture" — but the generator always
emitted `overrides = []`, so the context-sensitive tier was dead code.

The wider ecosystem converged on a decoupled model: the parser builds a tree,
and a **separate declarative query** maps tree nodes to capture names.
Tree-sitter uses `highlights.scm` (S-expression queries); Lezer uses
`styleTags` mapping node types to highlight tags, including contextual patterns
like `"CallExpression/VariableName"`. Both use a standard dot-separated capture
vocabulary (`function.builtin`, `punctuation.bracket`) — the exact vocabulary
Gale already emits.

## Decision

Replace the heuristic with a **declarative highlight query** supplied alongside
the grammar and compiled at generation time into the existing `defaults` /
`overrides` tables.

### Query format: a `highlights.scm` subset

The query is a strict subset of tree-sitter query syntax, shaped to exactly the
runtime's two-tier expressiveness — no more, no less:

```scheme
; line comment
(STRING_LITERAL) @string        ; default: a lexer token kind → capture
"select" @keyword               ; default: a literal token → capture
(functionCall (IDENTIFIER) @function)  ; override: within parser rule, token → capture
(functionCall "(" @punctuation.bracket)
```

- **Default** `(TOKEN) @cap` / `"lit" @cap` — maps a token kind (by lexer rule
  name, or by literal text) to a capture, filling `defaults[kind]`.
- **Override** `(rule (TOKEN) @cap)` / `(rule "lit" @cap)` — one level of
  nesting: while the parse is inside parser rule `rule` (i.e. `rule` is on the
  rule stack), `TOKEN` maps to `cap`, populating `overrides`. This matches
  Lezer's `CallExpression/VariableName`. Nesting semantics follow the runtime:
  the override fires anywhere within the rule's subtree, not only for a direct
  child.
- **Captures** use the tree-sitter standard dot-separated vocabulary; they
  become HTML classes (`foo.bar` → `class="foo bar"`).

Anything the runtime cannot express — nesting deeper than one rule context,
predicates, field selectors, anchors, wildcards — is a loud generation
diagnostic, never silently accepted. Unknown token/rule names are likewise
diagnostics. This keeps the accepted subset honest and leaves room to widen it
if the runtime ever grows deeper context.

Rationale for the subset over alternatives: JSON is verbose and comment-hostile
for what is a human-authored theme map; a bespoke flat DSL would be non-standard
and its growth path home-grown. The `.scm` subset is the ecosystem standard, is
forward-compatible (widen the accepted subset as the runtime grows), reuses the
capture vocabulary Gale already emits, and — Gale being a parser toolkit —
parsing S-expressions is cheap and on-brand.

### Delivery: a Kiln input, routed by extension

Kiln generators are pure `(inputs, options) → outputs` functions; they cannot
read ambient files. Every input must be statically enumerable and is pre-loaded
by the compiler by value into `req.inputs`. So the query file rides in on the
same channel as supplementary `.g4` grammars:

```wado
use hl from "./JSON.g4" with {
    generator: {
        module: "wado:gale",
        inputs: ["./JSON.highlights.scm"],
    },
};
```

The generator routes `req.inputs` by extension: `.g4` → grammar assembly,
`.scm` → highlight query. Because inputs are hashed into the Kiln cache key,
editing the query re-generates the parser with no extra wiring.

### Presence enables; the `highlight` option is removed

The old `highlight: bool` generator option is deleted. Highlighting is emitted
**iff a `.scm` query input is present**. This removes the "turned it on but got
nothing" failure mode and makes the enable signal a single fact. The highlight
runtime fragment and tables stay fully gated: a grammar with no query is
byte-identical to before.

### No heuristics

The `classify_*` heuristics are removed outright. To keep the on-ramp short, the
package README documents a starter `.scm` covering the common conventions the
old heuristic approximated (comments, strings, numbers, keyword tokens,
brackets, delimiters), for authors to adapt to their grammar's real token names.

## Consequences

- The dead `overrides` tier becomes the primary customization surface;
  context-sensitive highlighting is now expressible.
- Classification is static (compiled into `defaults`/`overrides` at generation
  time) — no runtime query interpreter, preserving Gale's "inline, minimal,
  byte-identical-when-off" codegen and its wasm-size budget. Re-theming stays
  dynamic where it belongs: capture names are CSS classes, styled without
  regeneration.
- Generated HTML classes align with the tree-sitter standard vocabulary, so
  existing themes apply.
- Every driver test that carried `options: { highlight: … }` is updated; the two
  highlight driver tests gain a `.scm` input. Existing generated goldens for
  non-highlight grammars are unchanged in content (only their Kiln metadata
  hashes move, from the generator source change).
- Compatibility with older Gale invocations is intentionally not preserved.
