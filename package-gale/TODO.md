# Gale TODO

Known gaps to close on the road to **full ANTLR4 compatibility**. The
compatibility principle is documented in [AGENTS.md](./AGENTS.md): the g4
parser must accept any well-formed grammar that upstream `antlr4` accepts,
with the single exception that host-language action bodies are skipped.

The g4 parser already accepts the full ANTLR4 surface syntax (with that
exception). The remaining work is mostly about **propagating** parsed
information into the IR and **using** it in the code generator so that
generated parsers are semantically correct, not just syntactically accepted.

## A. Parser accepts but IR drops information

Constructs the parser recognizes correctly, but whose payload is currently
discarded so downstream stages cannot see them. Each item should grow an IR
field plus targeted parser tests.

- **Grammar / rule / element / block options** (`options { caseInsensitive=true; ... }`,
  `<assoc=right, fail='msg'>`). Every option block is currently consumed and
  thrown away. Need at minimum a `Map<String, String>` on `Grammar`,
  `ParserRule`, `LexerRule`, and on the relevant elements. ANTLR4 spec:
  `vendor/antlr4/doc/options.md`.
- **Grammar-level options** like `superClass`, `tokenVocab`, `language`,
  `caseInsensitive`. Same as above — currently invisible to the rest of
  the pipeline.
- **Rule visibility modifiers** (`public`, `private`, `protected`).
  Accepted by `parse_grammar` but not stored on `ParserRule`.
- **`channels { ERROR, USER_CHAN }` block.** Names are accepted but
  discarded, so `channel(USER_CHAN)` cannot be resolved at codegen time.
  Add `Grammar.channels: Array<String>` and resolve named channel
  references in `parse_lexer_actions`.
- **`tokens { VIRTUAL_TOK }` block.** Virtual lexer rules are created,
  but the fact that a token came from a `tokens{}` block (vs. a real
  rule) is lost. Tag those entries so generators can emit the right
  token-id constants.
- **Named actions** (`@header { ... }`, `@parser::members { ... }`).
  Action bodies are intentionally skipped per the compatibility
  principle, **but their _presence_ and _position_ should still be
  recorded** in the IR. AGENTS.md states this explicitly ("preserve their
  _presence_ and _position_"); the implementation does not yet match.

## C. IR has the field but the code generator ignores it

These constructs are parsed and stored on the IR, but the parser /
lexer code generator never reads them. As a result the parser-level
"compat" is misleading: the grammar parses, but the generated parser
behaves incorrectly. **This is the most user-visible gap.**

| IR field                               | Generator status                                                                          |
| -------------------------------------- | ----------------------------------------------------------------------------------------- |
| `Element::Wildcard` (`.`)              | Not handled in `parser_gen.wado` / `gen_util.wado`. Likely produces wrong code or panics. |
| `Element::Not` (parser-side `~`)       | Only `LexerNotElement` (lexer-side) is wired. Parser-side `~ TOK` is unhandled.           |
| `LabelElement.list` (`+=`)             | Treated identically to single `=`. Should append to an `Array<T>` field.                  |
| `LexerRule.set_mode` (`mode(X)`)       | Unread. Generated lexer never switches the current mode.                                  |
| `LexerRule.more`                       | Unread. `more` semantics (collect more text without emitting) is not implemented.         |
| `LexerRule.type_override` (`type(X)`)  | Unread. Token type rewriting is not applied.                                              |
| `LexerRule.channel` (integer or named) | Unread. Tokens always go to channel 0 (or are skipped, in the HIDDEN approximation).      |

Each row needs:

1. A unit test in `lexer_gen_test.wado` / `parser_gen_test.wado` that
   exercises a minimal grammar using the construct and asserts the
   generated code does the right thing.
2. Codegen support in the relevant `*_gen.wado` file.
3. A driver test under `tests/grammars/` (or extending one of the
   existing real-world grammars) that proves end-to-end parsing of
   real input works after the change.

## D. Semantic approximations to revisit

- **`channel(HIDDEN)` is treated as `skip`.** ANTLR4 actually emits
  HIDDEN tokens on a separate channel; they are not discarded. This is
  a pre-existing approximation that the new lexer-command parser
  preserves. To remove the approximation, the runtime needs proper
  channel routing (`Token.channel`, channel-aware parser lookups).
- **HIDDEN channel id is hard-coded to 1.** Matches ANTLR4
  (`Token.HIDDEN_CHANNEL = 1`) but the value should come from a named
  constant once channels become first class.

## E. Test density / regression safety

- **Integration tests for most real grammars are loose.** Most tests in
  `g4/integration_test.wado` only assert `parser_rules.len() > N` and
  similar shape checks. After Batch 5 the HTMLLexer / TypeScriptLexer
  / css3Lexer tests verify specific lexer-command fields, but the
  Rust, SQLite, ANTLRv4, and HTMLParser tests still need similar
  tightening.
- **No negative test cases.** No fixtures for malformed `.g4` input
  (syntax errors, missing rules, duplicate rule names) — robustness
  regressions are easy to miss.
- **Golden test timeouts were bumped, not fixed.** `generate_typescript_golden`,
  `generate_rust_golden`, and `generate_css3_golden` are right at the
  edge of their (now 120 s) timeouts. The root cause is slow code
  generation for the largest grammars. Investigate which generator
  pass is the bottleneck (likely SLL prediction or string building)
  and fix it instead of widening the timeout further.

## F. Edge cases not yet exercised

These are corners of ANTLR4 syntax that no current test grammar uses, so
the parser may or may not handle them correctly. Each should get a
dedicated unit test before the next compatibility-related change.

- `~` applied to a block, not just a single token: `~( 'a' | 'b' )`.
- Block label syntax: `lbl=( a | b )` and `lbl+=( a | b )`.
- ARG_ACTION with only whitespace: `returns []`.
- ANTLR3-style numbered token assignment: `tokens { A=1, B=2 }`.

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
