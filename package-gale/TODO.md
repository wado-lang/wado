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

- ~~`Grammar.options.caseInsensitive = true` — generated lexer is still
  case-sensitive.~~ **Done** (this branch). Grammar-level and rule-level
  `caseInsensitive` now fold ASCII letters: literals emit
  `chars[pos] != 'x' && chars[pos] != 'X'`, char ranges fold `[a-z]` to
  include `[A-Z]` (and vice versa), keyword classifier honors
  `char_ci[i]`. Rule-level `caseInsensitive = false` overrides a
  grammar-level `true`. Driver coverage lives in
  `tests/grammars/ci_sql.g4` + `tests/driver_ci_sql_test.wado`.
- ~~`Alternative.options.assoc = right` on a left-recursive alt had no
  effect on generated parser behaviour.~~ **Done** (this branch). The
  LR precedence-climbing code now consults
  `is_right_associative(alt)` in `compute_lr_conflict_min_prec`:
  left-assoc alts recurse on the right operand at `own_prec + 1`
  (same-level rejected, outer loop handles left-association);
  right-assoc alts recurse at `own_prec` so same-level operators nest
  rightward. Conflict widening only applies to strictly higher-prec
  conflicting alts to avoid defeating right-assoc via self-conflict.
  Scan-side mirror updated in lockstep. Driver coverage: `2 ** 3 ** 4`
  CST assertion in `driver_typescript_test.wado`.
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

## C. Representation quality of stored options

- ~~`GrammarOption.value` is a lossy raw String.~~ **Done** (this
  branch). `GrammarOption.value` is now the typed variant
  `OptionValue { Ident(String), Qualified(Array<String>), Str(String),
  Int(i64), Action(String) }`. `Str` drops the surrounding single
  quotes, `Qualified` keeps the dotted components separate, and
  `Action` round-trips the raw host-language body between braces —
  previously collapsed to the placeholder `"{}"`. Both production
  consumers (`parser_gen.is_right_associative`,
  `lexer_gen.lookup_ci_option`) pattern-match on the variant.

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

### Next Up: Remove the LR Gate in `is_rule_scannable`

See the matching entry in `AGENTS.md` "Failed Approaches" for why the
naive unlock regressed `sqlite_parse` 10× and was reverted. The
remaining nine `bt_try` blocks in `sqlite.wado` (`parse_sql_stmt` —
WITH/SELECT/DELETE/... and CREATE groups) and their mirrors in
`sqlite_highlight.wado` all sit under Stage C's LR bail-out. Closing
this requires one of the three approaches below, in increasing order of
implementation cost:

1. **Ordered-lazy (first-success-wins) mode for Stage C.** When a
   group's alt first-sets are disjoint — or more generally, when the
   group is already disambiguated by some small prefix — Stage C should
   behave like `bt_try`: try alt 1's full scan, commit on success, stop.
   No exhaustive tournament. This is pure code-generation work in
   `emit_scan_partition_body` / `gen_scan_multi_alt`; no changes to
   runtime.
   - Note: `sql_stmt`'s problem group is NOT disjoint on first token,
     so this alone will not close the remaining nine sites. It is still
     worth doing because it unlocks LR for all disjoint-first groups
     currently gated, and simplifies reasoning about tournament cost.

2. **k-lookahead candidate pruning before the tournament.** Use the
   existing `alt_position_first_sets(alt, max_k)` (already sound post
   this branch's rewrite) to partition group alts by their depth-0..k
   fingerprints. Scan only the subset whose fingerprint matches the
   upcoming k tokens. For `sql_stmt`: the nine alts diverge by k=2 or
   k=3 (e.g. `WITH`-prefixed vs bare `SELECT` vs `SELECT`-after-CTE
   vs `DELETE FROM` vs `INSERT INTO`/`REPLACE INTO` vs `UPDATE`/
   `UPDATE OR`). This shrinks the tournament from 9 to typically 1–2
   candidates before any full LR scan runs.
   - `gen_context.wado` already computes sound positional first-sets.
     The work is in Stage C emission: mirror the parse-side
     `build_lookahead_condition` / `find_needed_depth` logic to pick a
     minimum-depth discriminating prefix per alt, then guard each
     scan-attempt with it.

3. **ATN-style adaptive prediction (ALL(\*)).** The textbook answer.
   Out of scope for the short term; listed for completeness.

**Target after (1) + (2):** `sqlite.wado` / `sqlite_highlight.wado`
`bt_try` 46 → 0. Scan cost upper-bounded by the dominant alt (1–2 LR
scans per sql_stmt entry, not 9). Expect another perf improvement on
`sqlite_parse`; worst case, parity with today (11,826 µs).

**Verification gates** (must hold throughout):

- All 1746 Wado tests green.
- `driver_sqlite_test`, `driver_sqlite_create_table_test`,
  `driver_sqlite_minimal_test`, `driver_sqlite_highlight_test`,
  `sqlite_regression_test`, `sqlite_case_when_test` — these cover the
  statement-list repetition and case-when paths that the previous naive
  unlock broke.
- `sqlite_parse` per-iteration ≤ today's 11,826 µs on the dev machine.
- `typescript.wado` / `rust.wado` `bt_try` counts must not increase
  (they are semantic-predicate-gated, not LR-gated, and out of scope
  for this work per WEP-2026-03-02).

## Code Quality

### `parser_gen.wado`

- **Duplicated branch merge logic**: SLL prediction tree building has
  similar merge/dedup patterns in multiple places. Could be consolidated.

## Generated Parser Bugs

(none currently)
