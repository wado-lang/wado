# Gale TODO

Known gaps to close on the road to **full ANTLR4 compatibility**. The
compatibility principle is documented in [AGENTS.md](./AGENTS.md): the g4
parser must accept any well-formed grammar that upstream `antlr4` accepts,
with the single exception that host-language action bodies are skipped.

The g4 parser already accepts the full ANTLR4 surface syntax (with that
exception). The remaining work is mostly about **propagating** parsed
information into the IR and **using** it in the code generator so that
generated parsers are semantically correct, not just syntactically accepted.

## Performance: sqlite-parse Benchmark

### Next Up: Remove the LR Gate in `is_rule_scannable`

See `AGENTS.md` "Failed Approaches: Removing the LR Gate ... 2026-04"
for the two attempts that did not land. The remaining `bt_try` blocks
in `sqlite.wado` (`parse_sql_stmt` — WITH/SELECT/DELETE/... and CREATE
groups) and their mirrors in `sqlite_highlight.wado` all sit under
Stage C's LR bail-out. Closing this requires one of the three
approaches below, in increasing order of implementation cost:

1. **Per-site ordered-lazy mode for `gen_rule_ref_group_scan_dispatch`.**
   The `sql_stmt` WITH partition is RuleRef-only and its sorted alts are
   mutually disjoint after a 1–2 token prefix, so first-success-wins on
   `sort_group_by_element_count` is correctness-preserving there. The
   2026-04 attempt got this right; what blocked it was applying the same
   change to `emit_scan_partition_body` and the two general-group
   dispatch sites, where alts include strict-prefix shapes like
   `column_ref` vs `function_call` that need true longest-match. The
   minimal fix is to flip first-success-wins on **only** in
   `gen_rule_ref_group_scan_dispatch` (statement-rule RuleRef alts), and
   keep longest-match everywhere else. This unlocks the LR gate without
   regressing `expr`-atom dispatch.
   - Note: the gate is still tripped by the LR check in
     `is_rule_scannable`. Either drop the gate just for the
     RuleRef-only path, or detect "all alts of this rule are scannable
     in the RuleRef-only sense" at the call site.

2. **k-lookahead candidate pruning before the tournament.** Use the
   existing `alt_position_first_sets(alt, max_k)` to partition group
   alts by their depth-0..k fingerprints. Scan only the subset whose
   fingerprint matches the upcoming k tokens. For `sql_stmt`: the alts
   diverge by k=2 or k=3 (e.g. `WITH`-prefixed vs bare `SELECT` vs
   `SELECT`-after-CTE vs `DELETE FROM` vs `INSERT INTO`/`REPLACE INTO`
   vs `UPDATE`/`UPDATE OR`). This shrinks the tournament from 9 to
   typically 1–2 candidates before any full LR scan runs.

3. **ATN-style adaptive prediction (ALL(\*)).** The textbook answer.
   Out of scope for the short term; listed for completeness.

**Verification gates** (must hold throughout):

- All Wado tests green, including `mise run test-wado` (the CI gate
  that broke on the 2026-04 second attempt — `benchmark/sqlite_parse`
  exercises COALESCE/COUNT/SUM-style `function_call` shapes that the
  small driver tests in `package-gale/tests/` do not).
- `driver_sqlite_test`, `driver_sqlite_create_table_test`,
  `driver_sqlite_minimal_test`, `driver_sqlite_highlight_test`,
  `sqlite_regression_test`, `sqlite_case_when_test`.
- `sqlite_parse` per-iteration ≤ today's 11,826 µs on the dev machine.
- `typescript.wado` / `rust.wado` `bt_try` counts must not increase
  (they are semantic-predicate-gated, not LR-gated, and out of scope
  per WEP-2026-03-02).

## Generated Parser Bugs

(none currently)

## Future Work: Actions and `superClass` (low priority)

Gale currently skips the contents of `{ ... }` action blocks and semantic
predicates — the g4 parser recognizes them (so real-world grammars parse
cleanly) but the code generator discards the host-language source. This is
intentional for the near term: emitting Wado from opaque Java/Rust/Python
snippets requires a cross-language translator we do not have.

Once action-body support is designed, `Grammar.options.superClass` becomes
meaningful and can be wired through as a trait bound on the generated
parser/lexer struct (something like `impl SuperClass for GeneratedParser`,
with action bodies able to call `self.helper(...)`). Until then the option
is surfaced only as a metadata comment.

Rough sketch, for when this is picked up:

- Extend the IR so `OptionValue::Action` and per-alt action elements carry
  a language-tagged source fragment instead of being a placeholder string.
- Add a pluggable "action translator" interface; ship at minimum an
  identity translator for Wado-written action bodies.
- Generate a `SuperClass` trait (name derived from `superClass = Foo`) and
  require callers to `impl` it; emit action bodies as method calls on
  `self` that resolve through that trait.
- `tokenVocab` falls out naturally at that point — another grammar's
  generated `TokenKind` enum can be imported by name rather than merged at
  IR time.

No work here blocks any current Gale user. Re-prioritize only when a real
grammar outside the `clean` set (ANTLR4, Rust, TypeScript lexers) needs its
action semantics reproduced, not just skipped.
