# Gale TODO

Open work towards full ANTLR4 compatibility and the performance budget it implies. Read this together with:

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract, stage layering, descriptor pipeline, and triage workflow.
- [`AGENTS.md`](./AGENTS.md) — internal architecture (LL prediction design, soundness invariants) and failed approaches.
- [`perf.md`](./perf.md) — runtime performance: benchmark state, live profile, what would move the needle, and measured perf dead-ends.

This file lists what is **not yet done**. Closed work belongs in commit history.

## Diagnostics & introspection ([#1246](https://github.com/wado-lang/wado/issues/1246))

Grammar-authoring DX follow-ups:

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

The lexer side of this is done — recursive non-greedy wildcard rules run on a runtime lexer simulator (`src/runtime/latn.wado`); see `antlr4-compatibility.md`. What remains is the parser side, below.

#### Decided design — hybrid runtime ATN simulator

Pre-release, so no API-compat constraints; the two axes that matter are **ANTLR4 behavioural compatibility** and **runtime speed** (perf.md). Design locked in after the trade-off discussion:

- **Hybrid, not full-ATN.** Keep the static compiled fast path for every decision static prediction already resolves (the decidable majority). Replace **only** the `Ambiguous` decision sites — today's longest-match scan tournament — with a call to a runtime ATN simulator. This is a strict superset of both extremes: complete (no static edges) like full-ATN, yet leaves the hot path (`scan_*`, `follow_yields`, leaf rules) untouched, so it sidesteps the measured data-driven-VM-scan dead-end in perf.md (that regressed +24% by interpreting the _hot_ leaf scanners; the simulator runs only at _cold_, currently-broken sites).
- **Clean-room.** Implement from the published ALL(\*) algorithm (closure / SLL predict / DFA cache / full-context fallback). Do **not** read ANTLR4's `ParserATNSimulator.java`, `ATNConfig.java`, `LL1Analyzer.java`, etc. (License hygiene, `AGENTS.md`).
- **ATN embedding: serialized `i32` blob → SoA.** Emit the reachable-whole-grammar ATN as one compact `i32` array `global`, decoded once at parser construction into flat parallel `List<i32>` arrays (state kind/rule/decision, transition from/to/kind/label). Smallest artifact, cheapest init, `array.get i32` walk on cache-miss — aligns with perf.md's SoA direction. Readability is covered by `gale dump --atn` plus a serialize/deserialize round-trip test. (Rejected: typed-struct globals — thousands of `struct.new` at init; per-decision specialised code — cannot express recursive closure / DFA caching.)
- **DFA cache: per-`Parser` instance.** Cache lives on the `Parser` struct, fresh per `parse()` call. Within a single parse the same decision (e.g. a deep `expr` recursion's loop-entry) is hit repeatedly and hits the cache, capturing most of the benefit with no statefulness/mutable-global concerns. A persistent cross-parse global cache is a later perf lever, not the first cut.
- **Conflict resolution: ANTLR4-compatible.** On a genuine prediction conflict pick the minimum alt index; non-greedy subrules (`??`/`*?`/`+?`) prefer the exit transition. The ATN is built so this falls out of transition ordering.
- **Trigger.** The simulator is invoked exactly where `build_prediction` returns a tree containing `Ambiguous`; everything else keeps emitting the existing compiled path.

The static parser/non-greedy `??`/LR loop-entry pieces of this design are in place (`src/atn.wado`, `src/runtime/atn.wado`, `atn_predict_with_stack` / `atn_lr_loop_decision`), as is the compile-time decision-number correspondence (`assign_atn_decisions` → `Atn.decision_sites`, read via `GenContext::atn_decision_site`). Both ATN-routed parser sites are wired:

- Every non-greedy `??` routes through `atn_predict_with_stack(sim, decision, …)`, each keyed to its own decision number.
- A multi-alt `AtEndConflict` site — an alt returns to the caller while another continues, the one ambiguity the longest-match scan tournament resolves unsoundly (its longest pick can steal a token the caller needs) — routes through the simulator too, keyed on the rule body decision (`rule_body_decision` → `GenContext::atn_rule_body_decision`); `grammar_has_at_end_conflict` (run before `build_atn`) triggers the ATN build. Regression fixtures: `tests/grammars/ll_longest_vs_context.g4` + `tests/driver_cst_ll_longest_vs_context_test.wado`.

Every other ambiguity reason (`OpaqueRuleRef`, `MaxDepth`, `ConfigExplosion`, `MultiAtEnd`, `NoViableAlt`) stays on the sound scan tournament — the tournament's longest-match agrees with ANTLR4 for those on the whole corpus. Route one through the simulator only if a descriptor ever shows the tournament resolving it wrongly, per the static-edge policy above.

The Stage B′ JVM oracle (see below) is the measurement axis: each ATN-class fix flips its pinned `[stage_b_oracle_todo]` / `[stage_a_todo]` test green.

#### Lexer ATN follow-ups (perf, optional)

The lexer simulator is correct and shipped; these are speed/size levers only:

- The lexer ATN reuses the parser blob format + `atn_decode`, so a lexer-only ATN grammar inlines the (otherwise unused) parser simulator as dead code. Split the shared decode / `AtnSim` into its own fragment if generated size matters there.
- `latn_match` interns nothing — stacks are value-copied `List<i32>` and the per-step visited set is string-keyed. Fine at the cold lexer sites; intern stacks (like the parser `CtxArena`) if a hot recursive-comment workload shows up.

To triage which static edge a concrete fix must close, `gale dump` surfaces each unresolvable decision as `Ambiguous([alt N, alt M]) — <reason>` (`AmbiguityReason` in `prediction.wado`), with the per-site LR `loop-entry:` dispatch (`conflict-min` + suffix-first overlap groups) and follow-variant `k-prefix=` masks alongside.

## Stage B′ — JVM-oracle integration

The Stage B′ pipeline covers 78 tests across `FullContextParsing`, `LeftRecursion`, `ParserErrors`, `ParserExec`, `SemPredEvalParser`, and `Sets`, with ATN-class divergences pinned under `[stage_b_oracle_todo]`. Infrastructure (design in [`antlr4-compatibility.md`](./antlr4-compatibility.md); `scripts/antlr4-oracle.sh`, `scripts/strip_grammar.wado`, `scripts/extract_antlr4_descriptors.wado --finalize-stage-b-oracle`, `scripts/extract-antlr4-descriptors.sh`; `[stage_b_oracle_skip]` / `[stage_b_oracle_todo]` in `status.toml`) is in place; Java is needed only at extract time, not in CI.

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

## Code-health bugs

Line numbers will drift; add a failing test before fixing.

### Soundness and compatibility divergence

These are the highest-risk bugs: a static-prediction edge or a parse/scan
asymmetry that can mis-parse valid input. Several need their own focused
PR with full-corpus validation rather than a quick patch (the AGENTS.md
"LL Prediction" notes the static path "will always have edges").

- [ ] SLL prediction under-approximates and emits incomplete Dispatch trees (valid input rejected, since codegen emits Dispatch with no else-fallback):
  - `sll_advance` collapses `+`/`*` repeats to "consumes exactly one token" (`a : X+ Y | X Z` mispredicts on `X X Y`). `src/prediction.wado:522-539`
  - `try_expand_opaque` lacks the at-end-config handling its template `build_sll_node` has, dropping at-end alts from the returned Dispatch. `src/prediction.wado:753-901` vs `:712-745`
- [ ] Scan/parse EOF asymmetry: parse-side `expect`/`match_set` match `TK_EOF` without advancing (matchedEOF), scan side emits `pos += 1` unconditionally — scan over-counts by 1 whenever EOF is the last matched element, which can flip a tournament tie. `src/parser_gen.wado:1311-1314`, `:1388`, `:626` vs `:312-316`, `:413-416`
- [ ] Tournament/scan-gate call sites never forward the runtime `follow` argument (helpers run with `&EMPTY_FOLLOW`) while the corresponding parse calls forward it — violating the documented scan/parse lockstep invariant on FOLLOW-gated grammars. `src/parser_gen.wado:5856`, `:5905`, `:5913`, `:5447`, `:5455`
- [ ] SimpleCst group scan lowering threads `outer_follow` while the parse-op path threads `empty_follow`, and the two comments contradict each other about which is sound. Decide once, fix the other side, pin with a fixture. `src/lower.wado:2783-2796` vs `:2820-2841`
- [ ] A label on a Transparent group (`x=(ID)`) silently drops the binding: `rebind_group_shape`'s Transparent arm returns unchanged and the promised caller recursion does not exist; the inner field was also deduped against a throwaway scope. `src/lower.wado:3962-3972`, `:3638-3683`
- [ ] `\P{...}` (negated Unicode property) is parsed as literal chars `P { L }` (only lowercase `p` is detected); unknown `\p{...}` properties expand to an empty set silently. At minimum warn. Full handling needs Unicode complement ranges (Gale's `\p` support is already a hand-rolled approximation). `src/g4/parser.wado:1517`, `:1546-1616`
- [ ] GIR-level multi-alt dispatch has no wildcard-alt awareness (soundness invariant 4 is only applied on parser_gen's surface-IR paths); a wildcard alt gets an empty-token branch in a `Direct` dispatch. Also `alt_is_wildcard_led` does not unwrap labels, so `w=.` escapes the wildcard machinery entirely. `src/lower.wado:1450-1494`, `src/alt_grouping.wado:31-41`
- [ ] Overlapping-but-unequal first-char ranges in the lexer dispatch shadow later rules: groups are keyed by exact guard string and emitted as `if/else if`, so a char in the intersection only tries the first range group; the wildcard fallback containing all range calls is unreachable for it. `src/lexer_gen.wado:1629-1666`, `:1702-1728`
- [ ] Surrogate / astral handling in char ranges: `CharRange` endpoints are Wado `char` (Unicode scalars), so a surrogate code point (`[\uD800-\uDBFF]`, legal in ANTLR4 for matching UTF-16 code units) cannot be represented — the escape resolvers fall back to U+FFFD instead of trapping, but a surrogate _range_ collapses to a single replacement char. Full support needs a wider char-range representation (i32 code-point endpoints). `src/g4/parser.wado` `resolve_unicode_escape`, `src/ir.wado` `CharRange`.

### Pipeline and tooling correctness

- [ ] `action_strip`'s `[...]` now ends at the first unescaped `]` (correct for char sets, the corpus case). This loses the depth tracking that handled a rule-argument action whose host type contains `[]` (`r[int[] arr]`): such an action ends early and its remainder leaks into the grammar text (`catch [...]` is already handled separately via `find_balanced_close_bracket`). No corpus grammar exercises this (all nested-`[` cases are char sets), but a context-aware stripper (distinguish set vs arg-action by lexer/parser position) would handle both. `src/g4/action_strip.wado:38-61`

### Diagnostics and minor

- [ ] `gen_error_fallback` puts internal constant names (`TK_IDENT`) in user-facing "expected" lists while the `expect` path uses `token_kind_name` — two error paths, two vocabularies. `src/parser_gen.wado:6290-6313`
- [ ] Error-token text is a message, so diagnostics read `unexpected token "unterminated string"`. `src/g4/lexer.wado:110`, `src/g4/parser.wado:1107`
- [ ] `ParseError.expected` is populated everywhere but rendered by nothing (the Display impl omits it). `src/runtime/lex.wado:166`, `:207-214`
- [ ] Empty lookahead `sig` is guarded on the scan side but not the parse side, where `gen_lookahead_condition` would emit syntactically broken code (`if` / `&& ()`); either the guard is dead or the parse side is missing it. `src/parser_gen.wado:1679-1681` vs `:3178`, `:3240`
- [ ] Diagnostic-to-rule association is by substring on a free-form label; `Diagnostic.rule` carries labels like `"SimpleCst group"` with no rule name, so the `(rule, message)` dedup can collapse diagnostics from different rules. Already tracked under "Structured diagnostic-to-rule identity". `src/gir.wado:106-109`, `src/dump.wado:505-517`
- [ ] List-label leaf path double-bumps the inner name counter (lower bakes one bump, codegen applies two), and the Group arm lacks the collision rebind the leaf arm has — both in the dedup bug class `codegen_label_collision_test.wado` exists for. Also the non-greedy transparent first iteration dedups outer-scope bindings against a fresh counter table. `src/parser_gen.wado:3502-3530`, `:3480-3493`, `:3835-3838`

### Unchecked-argument quality nits (non-crash)

- [ ] Malformed lexer command _arguments_ are still unchecked (the paren panics are fixed): `pushMode(42)` interns a mode literally named `42`, `-> ;` yields the odd "unknown lexer command ;". Validate the argument is an identifier. `src/g4/parser.wado:1232-1290`
