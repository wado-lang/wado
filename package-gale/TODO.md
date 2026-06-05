# Gale TODO

Open work towards full ANTLR4 compatibility and the performance budget it implies. Read this together with:

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract, stage layering, descriptor pipeline, and triage workflow.
- [`AGENTS.md`](./AGENTS.md) — internal architecture (LL prediction design, soundness invariants) and failed approaches.

This file lists what is **not yet done**. Closed work belongs in commit history.

## Diagnostics & introspection ([#1246](https://github.com/wado-lang/wado/issues/1246))

Grammar-authoring DX follow-ups left after parts 1–4 landed:

- **`OptionalScanGuardFallback` warning.** Lowering raises `OverlapTournament`
  today; the obvious next kind warns when an `e?` resolves to
  `OptionalScanGuard` (live case: `attribute` in `example/Wado.g4`). Deferred
  because it needs the enclosing rule name threaded through
  `pick_optional_specialised` (lower.wado); add the `DiagnosticKind` variant,
  the warn site, and a fixture together.
- **Structured diagnostic-to-rule identity.** `Diagnostic.rule` is the human
  label `build_overlap_dispatch` was called with (`rule 'r'`, `General group
  exprGroup0`, …). `dump.wado`'s `render_rule_diagnostics` re-associates a
  diagnostic to its rule by substring-matching `'<name>'`, so group-scoped
  warnings (no quoted rule name) never inline under their owning rule. Carry a
  structured owner (rule name/index) on `Diagnostic`, set it at the warn site,
  compare by equality, keep the label display-only. The same change lets
  `build_overlap_dispatch` take an explicit `is_scan_pass` flag instead of
  recovering the pass from the `" (scan)"` suffix via `ends_with_scan` (the
  warn-once invariant is currently backstopped by the `(rule, message)` dedup
  in `GenContext::warn`, but the suffix heuristic is fragile alone).

## LL prediction — remaining gaps

### Iter-body K-prefix for `Repeat` inner `RuleRef`s

The K-prefix follow-mask path closes the multi-token tail-greedy gap at the outer alt position, but a `RuleRef` inside a `Repeat` body still falls back to the 1-token mask path (the variant body's iter-entry gate fires; nested calls inside the iter body do not). The fixed-point "next iter | exit-to-caller" computation that would let it gate is straightforward but not yet plumbed. Few real grammars need it; revisit when a descriptor surfaces a regression.

### Multi-alt `RuleRef` expansion in `deep_position_first_sets_from`

The K-prefix caller-side mask analysis halts at a multi-alt `RuleRef` because the per-depth union of multi-alt prefixes would over-yield by matching cross-alt sequences no real alt admits. A per-alt sequence representation (`List<List<List<String>>>`) could extend the walk safely — useful when a caller's continuation passes through a multi-alt rule like `expr : literal | name`.

### Multi-alt variant dispatcher — K-prefix cascade relaxation

`tail_greedy_k_prefix_of_element`'s `RuleRef` arm still halts the K-prefix cascade through multi-alt rules out of conservatism. Now that the dispatcher in `parse_<rule>__follow_<id>` routes correctly to the variant's own `_bt_<n>` / `_scan_<n>` helpers (covered by `tests/grammars/ll_multi_alt_overlap.g4`), the `RuleRef` recursion gate can be relaxed so K-prefix flows through multi-alt rules cleanly. Requires its own regression fixture before the guard is loosened.

### ATN-class grammars

Grammars whose alt selection needs arbitrary-length lookahead through ambiguous prefixes cannot be decided by static FOLLOW + K-prefix. ANTLR4 handles them with a runtime ATN simulator (closure / predict / DFA cache; out of scope to inspect — see License hygiene in `AGENTS.md`). Gale's static path will always have edges.

Concrete ATN-class cases already on the board (each with a pinned fixture, see "Gale bugs surfaced by Stage A drivers" below): the LR operator-precedence chain (`DropLoopEntryBranchInLRRule_4`), non-greedy `??` binding (`IfIfElseNonGreedyBinding1/2`), and recursive lexer wildcard (`RecursiveLexerRuleRefWithWildcard{Plus,Star}_1`).

Two complementary directions, neither scoped yet:

- **Runtime ATN simulator** in Gale. Large investment; matches ANTLR4 one-for-one. Must stay clean-room (do not read ANTLR4's `ParserATNSimulator.java` etc.).
- **Lean on the Stage B′ JVM oracle as the measurement axis.** Already landed (see below); each ATN-class fix flips its pinned `[stage_b_oracle_todo]` test green.

`gale dump` renders the overlap-group prediction tree (the same `build_prediction` tree `gen_multi_alt_body_bt` emits), so ATN-class edges surface as `Ambiguous([alt N, alt M])` leaves under the relevant rule — e.g. `DropLoopEntryBranchInLRRule_4`'s `stat` resolves to `Ambiguous([alt 0, alt 1])` at depth 0 (`expr ';'` and `expr '.'` share their entire `expr` prefix). Before scoping a concrete fix, add the still-missing halt-reason / LR loop-entry / K-prefix per-site fields to the dump (Phase 1 deferred them).

## Stage A gaps — Gale bugs surfaced by descriptor drivers

Each is marked `[stage_a_todo]` (or `[stage_b_oracle_todo]`) in `status.toml` and ships a `#[TODO]` test. All three are ATN-class (see above) and Stage-C-independent.

- **LR operator-precedence chain.** `Performance/DropLoopEntryBranchInLRRule_4`: Gale picks the wrong precedence chain for an or-then-and expression. The inner `expr` of `'between' expr 'and' expr` is parsed at `min_prec=0` (matching ANTLR4); ANTLR4 only resolves the greedy binary-`and` capture via full-context adaptive prediction at the LR loop-entry (the "drop loop entry branch" optimisation the descriptor is named after). Gale's static `scan_expr_lr_*` sees `and X2` match and commits.
- **Non-greedy `??` prediction.** `ParserExec/IfIfElseNonGreedyBinding1/2`: emit shape is `Option<T>` (compile-blocker gone) but dispatch reuses the greedy first-set predictor, so the dangling `else` binds to the inner `if` instead of the outer one ANTLR4 picks. A global `rule_follow("ifStatement")` skip-set is over-broad (it also yields at the outermost call, breaking the parse). Fixture: `tests/grammars/ll_optional_non_greedy.g4` + `tests/driver_optional_non_greedy_test.wado` (pins Gale's current wrong tree; the oracle shape is a comment beside it). The Stage B′ `IfIfElseNonGreedyBinding1` test flips green simultaneously.
- **Recursive lexer rule with `.+?` / `.*?` wildcard.** `LexerExec/RecursiveLexerRuleRefWithWildcard{Plus,Star}_1`: nested `/* /*...*/ */` comments mistokenize because the recursive call doesn't re-enter under the non-greedy bound. Matching ANTLR4's NFA→DFA result requires bounding the recursive call against the non-greedy suffix without backtracking; the static single-pass emitter over-consumes.

## Stage B′ — JVM-oracle integration

First full-corpus run has landed: 78 Stage B′ tests across `FullContextParsing`, `LeftRecursion`, `ParserErrors`, `ParserExec`, `SemPredEvalParser`, and `Sets`, with ATN-class divergences pinned under `[stage_b_oracle_todo]`. Infrastructure (design in [`antlr4-compatibility.md`](./antlr4-compatibility.md); `scripts/antlr4-oracle.sh`, `scripts/strip-grammar.wado`, `scripts/extract_antlr4_descriptors.wado --finalize-stage-b-oracle`, `scripts/extract-antlr4-descriptors.sh`; `[stage_b_oracle_skip]` / `[stage_b_oracle_todo]` in `status.toml`) is complete and Java is needed only at extract time, not in CI.

Remaining:

- **Extend coverage to the remaining parser categories** (`ParseTrees`, `Listeners`) and re-triage `[stage_b_oracle_skip]` / `[stage_b_oracle_todo]` after each re-extract. A small set fail the oracle even after action stripping because they carry StringTemplate directives outside action bodies (e.g. `ParserExec/ReservedWordsEscaping`'s `returns [<IntArg("")> return_]`); record those in `[stage_b_oracle_skip]` with the javac error.

## Composite (slave-grammar) descriptors

All 17 `CompositeLexers` / `CompositeParsers` descriptors short-circuit on `parsed.slave_grammars.len() > 0`. Two independent blockers:

- **Importer multi-input plumbing.** Kiln's `use t from "<C>/<Name>.g4" with { ... }` directive must resolve `import S;` against sibling `<Name>.slaveN.g4` files. Kiln already supports multi-input; lift the short-circuit once resolution lands.
- **Host-side `[output]` (Stage C).** Every composite `[output]` is a host-side artefact — `<writeln(...)>` action prints (`S.a`, `M.b`), `Token.toString` dumps, or empty — so none survive `normalize_output_for_stage_b`. Re-evaluate once Stage C lands.

## Stage C — action / predicate execution

Gale **recognises** but **silently discards** the contents of `{ ... }` action blocks and `{ ... }?` semantic predicates. The g4 parser accepts them, so grammars containing them (`ANTLRv4Lexer`, `RustLexer`, `RustParser`, `TypeScriptLexer`, `TypeScriptParser`) load cleanly — but the generated lexer/parser behaves as if every predicate were `true` and every action a no-op. That is wrong for:

- `RustLexer.RAW_STRING_LITERAL` — the closing `#` count must match the opening `#` count, enforced by a predicate; without it Gale mistokenizes Rust raw strings.
- TypeScript's regex-vs-division disambiguation and other context-sensitive lexer (3) and parser (17) rules.

Stage C is a hard prerequisite for treating Gale as a drop-in ANTLR4 replacement, for any lexer-level optimization (a fast tokenizer is meaningless if it tokenizes incorrectly), and for `Grammar.options.superClass` / `tokenVocab`. It also unblocks composite-descriptor `[output]` comparison and parser descriptors whose `[output]` is purely action-print stdout.

Sketch:

- Extend the IR so `OptionValue::Action` and per-alt action / predicate elements carry a language-tagged source fragment instead of a placeholder string.
- Add a pluggable "action translator" interface; ship at minimum an identity translator for Wado-written action bodies.
- Generate a `SuperClass` trait (name from `superClass = Foo`); emit action bodies as method calls on `self` resolving through it. `tokenVocab` then falls out — another grammar's generated `TokenKind` enum is imported by name rather than merged at IR time.

Translating Java/Rust/Python action bodies to Wado is the hard part. A reasonable first cut requires callers to provide hand-written Wado equivalents (a sidecar mapping predicate / action ID → Wado snippet), with automatic translation later. See [`docs/wep-2026-03-02-gale.md`](../docs/wep-2026-03-02-gale.md).

## Performance — where the 5× gap lives

Investigation against `benchmark/sqlite_parse` (Wado/Gale at `-O2`, ~137 ms/iter on a 13 KB SQL fixture; Rust `sqlparser-rs` at debug for reference is ~6.7 ms/iter — release would be far less).

Profile (guest sampler, 5 ms interval) self-time top:

|   Pct | Symbol                                                   |
| ----: | -------------------------------------------------------- |
| 27.9% | `tokenize`                                               |
| 26.0% | `List<Token>::push` (per-token `struct.new Token`)       |
| 17.2% | `Parser::last_end` (4-step `Parser→List→Token→Span→end`) |
|  4.4% | `List<Token>::grow`                                      |

Token-stream construction (`tokenize` + `List<Token>::push` + `grow`) is 58% of self-time; token reads via `Parser` are next.

### What does not work

- **Inlining hot Parser methods / any per-method micro-opt.** Caching `Parser::last_end` as a field or forcing `#[inline]` removes the named function from the profile but does not move wall time — the cost is the actual loads (`Parser→List→Token→Span→end`), not call overhead, and inlining merely redistributes it into the callers. wasmtime + Cranelift handles small Wasm calls cheaply enough that inlinability is not the lever.

### What would actually move the needle

The dominant cost is **Wasm GC `(array (ref Token))` indirection plus per-token `struct.new Token` allocation**. A 5× improvement requires decomposing `List<Token>` into parallel primitive arrays (`kinds` / `starts` / `ends` as `List<i32>`) so that `peek_kind` becomes a single `array.get i32` (not `array.get (ref Token)` + `struct.get`) and per-token struct allocation disappears in the lex loop. Two non-overlapping paths:

1. **Gale-side**: redesign `Token` so hot fields are flat primitives, with an opaque sidecar (or removal) for `text` / `leading_trivia`. Keep the public `Token` API as a view handle if needed.
2. **Wado-side**: extend `container_sroa` to handle (a) struct fields (currently locals only), (b) inner structs with nested struct/reference fields, (c) cross-function rewrites for the `scan_*(&List<Token>, ...)` parameter pattern (1100+ sites in the SQLite parser pass `&p.tokens` as a bare reference, always escaping). Today the pass fires on zero candidates in Gale-generated parsers.

### Lexer dispatch (independent secondary lever)

Inside the 27.9% `tokenize` self-time, work splits into per-character branch dispatch and keyword classification. Candidate techniques — pick by what profiling on the predicate-correct lexer (after Stage C) says is hottest. None help in isolation; they multiply with the SoA win above, not replace it.

- **Table-driven DFA** for the whole lexer (NFA → DFA → transition table). Replaces both per-character dispatch and `classify_keyword`. `mode` blocks become a DFA per mode plus mode-switch on accept; lexer commands attach as accept-state attributes. Semantic predicates are the only DFA-blocker (need a hybrid prefix + predicate gate once Stage C makes them real).
- **Trie / nested-switch on bytes** for `classify_keyword` only. Branches share prefixes (`IN` → `INSERT` / `INSTEAD` / `INTERSECT` / `INTO`). Smaller code-size impact than a full DFA.
- **Compile-time perfect hash** (`gperf`-style) for `classify_keyword`. O(1) lookup, best when keyword count is large.
- **SIMD-based pre-scan** (Wasm `v128`) for token boundaries / character-class membership in bulk. Effective if per-byte work is tiny but the byte loop is the bound.
