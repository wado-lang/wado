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
