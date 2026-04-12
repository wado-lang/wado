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

## D. Driver-level verification (remaining gaps)

The previous sweep added unit tests, golden tests, and driver tests for
the most critical semantic changes. The following driver coverage was
added in this branch:

- **HIDDEN channel routing** — `driver_html_test.wado` tokenizes
  `<p class="x">`, asserts TAG_WHITESPACE is absent from the main token
  stream, and asserts it appears as leading trivia on the next TAG_NAME
  with `channel == 1`. An additional end-to-end assertion confirms the
  parser successfully accepts whitespace-separated attributes.
- **HIDDEN channel (TypeScript)** — `driver_typescript_test.wado`
  verifies `WhiteSpaces` routes to trivia with `channel == 1` around
  `let x`.
- **User-defined channel routing** — `driver_antlr4_test.wado` exercises
  `channel(COMMENT)` (the second user channel beyond DEFAULT/HIDDEN),
  asserting both `//` LINE_COMMENT and `/* */` BLOCK_COMMENT appear as
  trivia with `channel == 3`.
- **`type(X)` override** — `driver_typescript_test.wado` tokenizes
  `` `hi` `` and asserts both backticks emit as `TK_BackTick`, never
  `TK_BackTickInside`, confirming the type-override rewrite.

Still missing driver-level coverage (deferred because no existing test
grammar exercises the feature end-to-end):

- **Parser-side `~TOK` / `~(block)`.** No driver-test grammar currently
  uses a parser-level complement. Either add a minimal new grammar, or
  extend `sexpression.g4` to exercise `~TOK`.
- **List labels `+=`.** None of the existing test grammars use list
  labels in the label sense — all `+=` matches are literal `'+='`
  operator tokens. Add a minimal new grammar or extend an existing one
  (a `list : items += item (',' items += item)*` pattern).
- **`mode(X)` semantics (set_mode).** No existing test grammar uses the
  bare `mode(X)` command — everything uses `pushMode` / `popMode`. Need
  a new fixture that actually calls `mode(X)`.
- **`more` semantics.** `ANTLRv4Lexer.LexerCharSet` mode uses `more`,
  but that mode is only entered from an action block (which Gale
  skips), so the rule is never actually triggered end-to-end. Need a
  fixture that enters a `more`-bearing mode via a lexer-command-only
  `pushMode`.
- **Wildcard `.` at parser level.** Only lexer-level `.` is exercised
  today. A parser rule like `any : . ;` has no driver-level test.

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
