# Gale TODO

Known gaps to close on the road to **full ANTLR4 compatibility**. The
compatibility principle is documented in [AGENTS.md](./AGENTS.md): the g4
parser must accept any well-formed grammar that upstream `antlr4` accepts,
with the single exception that host-language action bodies are skipped.

The g4 parser already accepts the full ANTLR4 surface syntax (with that
exception). The remaining work is mostly about **propagating** parsed
information into the IR and **using** it in the code generator so that
generated parsers are semantically correct, not just syntactically accepted.

## A. Remaining IR gaps (partial from the previous sweep)

The previous TODO cleanup wired grammar-level and rule-level options into
the IR, but two sub-kinds of the same feature were left unhandled:

- **Element-level options** (`ID<assoc=right, fail='msg'>`, `'lit'<p=3>`).
  Currently consumed and dropped by `skip_angle_block`. `assoc=right`
  affects left-recursion handling semantically, so this cannot stay
  invisible. Needs a field on `Element` / `LabelElement` / `TokenRef` or a
  wrapper variant.
- **Block-level options** (`( options { assoc = right; } : a | b )`).
  Currently consumed by `skip_block_prequel`. Same story as above —
  need a dedicated slot on `Element::Group` (or a wrapping struct).

## B. IR → pipeline wiring (preserve what's stored)

The previous sweep populated the IR but the code generator still ignores
most of the new fields. "Parsed and stored" is not "used":

- `Grammar.options.caseInsensitive = true` — generated lexer is still
  case-sensitive. Needs a pass over lexer-gen to fold `tolower` into
  literal matching (or switch to case-insensitive char comparisons).
- `Grammar.options.superClass` / `tokenVocab` / `language` — completely
  ignored. `tokenVocab` in particular is non-trivial: it implies loading
  another grammar's token ids.
- `Grammar.named_actions` — stored for presence/position but not
  surfaced anywhere. At minimum, generated output could include a
  comment marker so downstream tooling can see where actions used to be.
- `ParserRule.visibility` / `LexerRule.visibility` — stored but unused.
  ANTLR4 itself ignores these at codegen, so matching ANTLR4's behavior
  is fine, but a lint or doc comment in the generated output would make
  the information observable.
- `LexerRule.is_virtual` — stored but not emitted differently from real
  lexer rules. Generated lexers happily produce try_<name> functions for
  virtual tokens that should never match. Should be gated.

## C. Representation quality of stored options

- **`GrammarOption.value` is a lossy raw String.** Option values in
  ANTLR4 can be identifier / qualified.name / string literal / integer
  literal / **action block (`{ ... }`)**. The current implementation
  stores identifiers and integers verbatim, wraps string literals in
  single quotes, and collapses action-block values to the placeholder
  `"{}"`. Round-trip from IR to surface syntax is therefore impossible
  for action-block values.
- Consider switching to a `variant OptionValue { Ident(String),
  Qualified(Array<String>), Str(String), Int(i64), Action(String) }`
  so consumers can branch without re-parsing the string.

## D. Driver-level verification (gaps from the previous sweep)

The previous sweep added unit tests and golden tests for new features,
but several semantic changes have no runtime verification in the driver
tests (`package-gale/tests/driver_*_test.wado`). Each of the following
should grow a dedicated driver assertion:

- **HIDDEN channel routing.** `HTMLLexer.g4` declares
  `TAG_WHITESPACE -> channel(HIDDEN)`. The driver test should tokenize
  a tag like `<p class="x">`, assert that whitespace tokens do **not**
  appear in the `tokens` array, and assert that they do appear in the
  following token's `leading_trivia` with `channel == 1`.
- **Parser-side `~TOK` / `~(block)`.** Find a driver test whose grammar
  exercises a parser-level complement and assert the generated parser
  accepts a representative input and rejects the complemented token.
- **List labels `+=`.** SQLite and several other grammars collect
  expression lists with `+=`. A driver test should parse a compound
  input and assert the resulting `Array<_>` field has the expected
  length and element values.
- **`mode(X)` semantics (set_mode).** The Rust lexer uses `mode()`
  alongside `pushMode` / `popMode`. Driver test should feed input that
  exercises `mode()`-style transitions and assert the token stream
  matches expectations.
- **`more` semantics.** HTMLLexer / TypeScriptLexer use `more` to
  concatenate multi-part tokens. Driver test should feed input that
  would split without `more` and assert a single combined token.
- **`type(X)` override.** Find a grammar that uses `type(X)` to rewrite
  the emitted kind and assert the driver output reflects the overridden
  kind, not the source rule name.

## E. Negative tests

The parser test suite is overwhelmingly positive — it verifies that
well-formed input parses. There are almost no negative tests that pin
down the parser's **rejection** behavior. Add fixtures for:

- Duplicate rule names (`foo : ID ; foo : NUM ;`).
- References to undefined rules.
- `mode X;` inside a parser grammar (already covered by one test — use
  it as a template for the rest).
- Malformed `channels { }` / `tokens { }` blocks (unclosed brace,
  trailing junk, etc.).
- `~` applied to something that isn't a set (ANTLR4 rejects
  `~ruleref`, only tokens / lexer atoms / blocks are allowed).
- Left-recursion with `assoc` conflicts.
- Lexer commands with unknown names (`foo : 'x' -> totallymadeup ;`).

Each fixture should assert `parse(input) matches { Err(_) }` with a
useful error message (not just "unexpected token"). This guards against
regressions where the parser silently accepts garbage.

## Performance: sqlite-parse Benchmark

### Remaining Bottleneck: `Parser::last_end` (11.7%)

| Function           | Self-time |
| ------------------ | --------- |
| `Parser::last_end` | 11.7%     |

Called after every token consumption to compute node spans via
`Span::new(start, p.last_end())`. The function itself is trivial (array
index), but at millions of calls the overhead accumulates.

Possible improvement: cache `last_end` in a field on `Parser` updated by
`advance()`, avoiding repeated array indexing.

### Resolved

- **Backtracking (~44%)**: Eliminated by scan-then-parse optimization.
  Lightweight scan functions check token kinds to pick the correct
  alternative before calling the real parse function once.
- **`Parser::expect` error path (~22%)**: Largely eliminated — scan
  functions avoid speculative parse failures that triggered error
  construction.

## Code Quality

### `parser_gen.wado`

- **Duplicated branch merge logic**: SLL prediction tree building has
  similar merge/dedup patterns in multiple places. Could be consolidated.

## Generated Parser Bugs

(none currently)
