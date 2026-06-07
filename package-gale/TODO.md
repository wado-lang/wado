# Gale TODO

Open work towards full ANTLR4 compatibility and the performance budget it implies. Read this together with:

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract, stage layering, descriptor pipeline, and triage workflow.
- [`AGENTS.md`](./AGENTS.md) — internal architecture (LL prediction design, soundness invariants) and failed approaches.
- [`perf.md`](./perf.md) — runtime performance: benchmark state, live profile, what would move the needle, and measured perf dead-ends.

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

### ATN-class grammars

Grammars whose alt selection needs arbitrary-length lookahead through ambiguous prefixes cannot be decided by static FOLLOW + K-prefix. The static path will always have edges — this is not a tuning gap but a decidability one: the lookahead language of a recursive ambiguous prefix is non-regular, so the per-decision lookahead DFA built by subset construction over the ATN does not converge to a finite machine. ANTLR3's static LL(\*) failed here; ANTLR4 replaced it with a **runtime** simulator that builds the decision DFA lazily, driven by the actual input (ALL(\*)). Runtime-ness is what makes the answer complete.

Concrete ATN-class cases already on the board (each with a pinned fixture, see "Gale bugs surfaced by Stage A drivers" below): the LR operator-precedence chain (`DropLoopEntryBranchInLRRule_4`), non-greedy `??` binding (`IfIfElseNonGreedyBinding1/2`), and recursive lexer wildcard (`RecursiveLexerRuleRefWithWildcard{Plus,Star}_1`).

#### Decided design — hybrid runtime ATN simulator (2026-06)

Pre-release, so no API-compat constraints; the two axes that matter are **ANTLR4 behavioural compatibility** and **runtime speed** (perf.md). Design locked in after the trade-off discussion:

- **Hybrid, not full-ATN.** Keep the static compiled fast path for every decision static prediction already resolves (the decidable majority). Replace **only** the `Ambiguous` decision sites — today's longest-match scan tournament — with a call to a runtime ATN simulator. This is a strict superset of both extremes: complete (no static edges) like full-ATN, yet leaves the hot path (`scan_*`, `follow_yields`, leaf rules) untouched, so it sidesteps the measured data-driven-VM-scan dead-end in perf.md (that regressed +24% by interpreting the _hot_ leaf scanners; the simulator runs only at _cold_, currently-broken sites).
- **Clean-room.** Implement from the published ALL(\*) algorithm (closure / SLL predict / DFA cache / full-context fallback). Do **not** read ANTLR4's `ParserATNSimulator.java`, `ATNConfig.java`, `LL1Analyzer.java`, etc. (License hygiene, `AGENTS.md`).
- **ATN embedding: serialized `i32` blob → SoA.** Emit the reachable-whole-grammar ATN as one compact `i32` array `global`, decoded once at parser construction into flat parallel `List<i32>` arrays (state kind/rule/decision, transition from/to/kind/label). Smallest artifact, cheapest init, `array.get i32` walk on cache-miss — aligns with perf.md's SoA direction. Readability is covered by `gale dump --atn` plus a serialize/deserialize round-trip test. (Rejected: typed-struct globals — thousands of `struct.new` at init; per-decision specialised code — cannot express recursive closure / DFA caching.)
- **DFA cache: per-`Parser` instance.** Cache lives on the `Parser` struct, fresh per `parse()` call. Within a single parse the same decision (e.g. a deep `expr` recursion's loop-entry) is hit repeatedly and hits the cache, capturing most of the benefit with no statefulness/mutable-global concerns. A persistent cross-parse global cache is a later perf lever, not the first cut.
- **Conflict resolution: ANTLR4-compatible.** On a genuine prediction conflict pick the minimum alt index; non-greedy subrules (`??`/`*?`/`+?`) prefer the exit transition. The ATN is built so this falls out of transition ordering.
- **Trigger.** The simulator is invoked exactly where `build_prediction` returns a tree containing `Ambiguous`; everything else keeps emitting the existing compiled path.

#### Phased plan

- **P1** — _(done)_ ATN data model + builder (parser side) + `i32` serialize/deserialize + `gale dump --atn`. Behaviour-unchanged; round-trip + structural unit tests in `src/atn_test.wado`. (`src/atn.wado`, dump wiring in `main.wado`.)
- **P2** — _(done)_ runtime simulator core: `atn_decode` (blob → SoA `AtnSim`), `atn_closure` / `atn_move`, `atn_predict_with_stack` / `atn_adaptive_predict` (SLL: simulate to single alt; conflict → min-alt with non-greedy exit-first ordering). In `runtime.wado`, unit-tested in `src/atn_sim_test.wado` (two-token resolution + greedy/non-greedy tie-break). DFA cache not yet added (a P3 perf add, not needed for correctness). `atn_predict_with_stack` already accepts a caller stack for full context.
- **P3a** — _(done)_ full-context prediction **engine**. Upgraded the simulator to take the caller's **rule-invocation stack** (rule indices) instead of needing a per-call-site GIR↔ATN map: on rule exit a config branches into every ATN return-state where the caller rule invokes the callee (rule-context precision); a config returning past a real stack bottom accepts only EOF (`ATN_ACCEPT_EOF`). **Why the rule-index stack:** the real follow is what makes IfIfElse work — with an empty (SLL) stack the exit-config matches any follow (wildcard) so non-greedy skips `else` even at the OUTER `ifStatement` and the parse fails. Proven in `src/atn_sim_test.wado`: the SAME `??` decision resolves EXIT for the inner `ifStatement` (else→outer) and ENTER for the outer (consumes else) given stacks `[0,1,2,1,2]` vs `[0,1,2]`. Engine-only; no codegen path calls it yet.
- **P3b** — wire the engine into codegen so non-greedy `??` actually flips green. Blast radius is contained: gate everything on a new `GenContext.needs_atn` (set by lowering on a non-greedy repeat, mirroring `emit_follow`), so only non-greedy grammars change; all others stay byte-identical.
  1. `GenContext.needs_atn` + `mark_needs_atn()`; set in `lower.wado` where `rep.non_greedy` Optional is lowered (`lower.wado:~2934`).
  2. In `codegen.wado::generate`, when `needs_atn`, `build_atn(&grammar)` + `serialize_atn` and emit `global GALE_ATN_DATA: List<i32> = [...]` (mirror `gen_follow_mask_globals`, `parser_gen.wado:~5420`).
  3. Parser struct (`gen_parser_struct`, `parser_gen.wado:170`): when `needs_atn`, add `atn: AtnSim` (decode `GALE_ATN_DATA` once in `_gale_new_parser`, `parser_gen.wado:~5315`) and `atn_stack: List<i32>`.
  4. Rule-entry wrapper (`gen_rule_entry_wrapper`, `parser_gen.wado:~1687`): when `needs_atn`, `p.atn_stack.push(<rule_index>)` on enter, `pop` on exit. Rule index = position in `grammar.parser_rules` (matches `build_atn`'s indexing). One i32/frame, non-greedy grammars only.
  5. At the non-greedy Optional emit site (`gen_op_repeat_optional_*`), replace the greedy first-set check with `atn_predict_with_stack(&p.atn, <decision>, &p.atn_stack, &p.tokens, p.pos, TK_EOF)`; enter iff the returned alt is the ENTER edge.
  6. **Decision-number mapping (the crux risk).** `build_atn` numbers decisions in surface-traversal order; the emitter works from GIR. For the non-greedy slice, map by `(rule_index, ordinal of non-greedy Optional within the rule in surface order)` — have `build_atn` record `non_greedy_optional_decisions[rule] = [decnum, …]` and the emitter track the same per-rule ordinal. Assert/fallback to the static greedy path if the ordinal can't be resolved, so a mapping miss degrades gracefully instead of mis-predicting. Flips `[stage_b_oracle_todo] IfIfElseNonGreedyBinding1/2`.
  7. Add the per-`Parser` DFA cache once correct (keyed by decision + token-prefix).
- **P4** — full-context conflict handling for left-recursive precedence; flip `DropLoopEntryBranchInLRRule_4` (`[stage_a_todo]`). Needs precedence predicates on the LR self-ref rule transitions (the `prec` field already exists on `AtnTransition`) and graph-merged stacks (the current `List<i32>` stack + config cap will blow up on deep left recursion — replace with a shared-stack/graph representation).
- **P5** — lexer ATN + non-greedy wildcard; flip `RecursiveLexerRuleRefWithWildcard{Plus,Star}_1` (`[stage_a_todo]`).
- **P4** — full-context conflict handling for left-recursive precedence; flip `DropLoopEntryBranchInLRRule_4` (`[stage_a_todo]`). Needs precedence predicates on the LR self-ref rule transitions (the `prec` field already exists on `AtnTransition`) and graph-merged stacks (the current `List<i32>` stack + config cap will blow up on deep left recursion — replace with a shared-stack/graph representation).
- **P5** — lexer ATN + non-greedy wildcard; flip `RecursiveLexerRuleRefWithWildcard{Plus,Star}_1` (`[stage_a_todo]`).

The Stage B′ JVM oracle (already landed, see below) is the measurement axis: each ATN-class fix flips its pinned `[stage_b_oracle_todo]` / `[stage_a_todo]` test green.

To triage which static edge a concrete fix must close, `gale dump` surfaces each unresolvable decision as `Ambiguous([alt N, alt M]) — <reason>` (`AmbiguityReason` in `prediction.wado`), with the per-site LR `loop-entry:` dispatch (`conflict-min` + suffix-first overlap groups) and follow-variant `k-prefix=` masks alongside. Example: `DropLoopEntryBranchInLRRule_4`'s `stat` → `Ambiguous([alt 0, alt 1]) — opaque rule-ref prefix` (its two alts share the entire `expr` prefix).

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

## Performance

Runtime performance — the benchmark state, the live profile, the
directions that would move the needle, and measured dead-ends (e.g.
data-driven scan) — lives in [`perf.md`](./perf.md).
