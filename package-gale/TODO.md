# Gale TODO

Open work towards full ANTLR4 compatibility and the performance budget it implies. Read this together with:

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract, stage layering, descriptor pipeline, and triage workflow.
- [`AGENTS.md`](./AGENTS.md) — internal architecture (LL prediction design, soundness invariants) and failed approaches.

This file lists what is **not yet done**. Closed work belongs in commit history.

## Diagnostics & introspection ([#1246](https://github.com/wado-lang/wado/issues/1246))

Grammar-authoring DX gaps surfaced while writing a new `.g4` against Gale.
Parts 1–4 have landed; the items below are follow-ups left on the table.

- **More prediction-strategy warnings (part 3 follow-up).** Lowering raises
  `OverlapTournament` today. The obvious next kind is an
  `OptionalScanGuardFallback` — warn when an `e?` resolves to
  `OptionalScanGuard` (a live case: `attribute` in `example/Wado.g4`). It
  needs the enclosing rule name threaded through `pick_optional_specialised`
  (lower.wado), which is why it was deferred; add the `DiagnosticKind`
  variant back together with the warn site and a fixture.
- **Structured diagnostic-to-rule identity (part 3/4 follow-up).** A
  `Diagnostic.rule` is the human label `build_overlap_dispatch` was called
  with (`rule 'r'`, `LR rule 'expr' atom`, `General group exprGroup0`,
  `SimpleCst group`). `dump.wado`'s `render_rule_diagnostics` re-associates a
  diagnostic with its rule by substring-matching `'<name>'`, so group-scoped
  warnings (no quoted rule name) never inline under their owning rule — they
  appear only in the summary with a label that can't be mapped back to a .g4
  rule. Carry a structured owner (rule name/index) on `Diagnostic`, set it at
  the warn site, and compare by equality; keep the label display-only. The
  same change lets `build_overlap_dispatch` take an explicit `is_scan_pass`
  flag instead of recovering the pass from the `" (scan)"` suffix via
  `ends_with_scan` (today the warn-once invariant is backstopped by the
  `(rule, message)` dedup in `GenContext::warn`, but the suffix heuristic is
  fragile on its own).
- **Single lowering on `gale gen` (part 3 follow-up).** `cmd_gen` lowers once
  to read diagnostics for stderr, then `generate` lowers again internally and
  discards its `LoweredGrammar.diagnostics`. Have `generate` return (or expose)
  the diagnostics so `gale gen` lowers once. Same site should run
  `normalize_caches` before the diagnostic lower to match `generate` /
  `cmd_dump` (benign for parser-built IR today, but inconsistent).

## LL prediction — remaining gaps

### Iter-body K-prefix for `Repeat` inner `RuleRef`s

The K-prefix follow-mask path closes the multi-token tail-greedy gap at the outer alt position, but a `RuleRef` sitting **inside** a `Repeat` body still falls back to the 1-token mask path (the variant body's iter-entry gate fires correctly; nested calls inside the iter body do not). The fixed-point "next iter | exit-to-caller" computation that would let it gate is straightforward but not yet plumbed. Few real grammars need it; revisit when a descriptor surfaces a regression.

### Multi-alt `RuleRef` expansion in `deep_position_first_sets_from`

The K-prefix caller-side mask analysis halts at a multi-alt `RuleRef` because the per-depth union of multi-alt prefixes would over-yield by matching cross-alt sequences that no real alt admits. A per-alt sequence representation (`Array<Array<Array<String>>>`) could extend the walk safely — useful when a caller's continuation passes through a multi-alt rule like `expr : literal | name`.

### Multi-alt variant dispatcher emit — K-prefix cascade relaxation

The dispatcher in `parse_<rule>__follow_<id>` for multi-alt rules
with overlapping FIRST sets now routes to the variant's own
`parse_<rule>__follow_<id>_bt_<n>` / `_scan_<n>` helpers (was
falsely routing to the non-variant `parse_<rule>_bt_<n>` /
`_scan_<n>`; covered by `tests/grammars/ll_multi_alt_overlap.g4`).
The remaining follow-up: `tail_greedy_k_prefix_of_element`'s
`RuleRef` arm still halts the K-prefix cascade through multi-alt
rules out of conservatism. With the dispatcher fixed, the
`RuleRef` recursion gate can now be relaxed so K-prefix flows
through multi-alt rules cleanly. Requires its own regression
fixture before the guard is loosened.

### ATN-class grammars

Grammars whose alt selection requires arbitrary-length lookahead through ambiguous prefixes cannot be decided by static FOLLOW + K-prefix. ANTLR4 handles them with a runtime ATN simulator (closure / predict / DFA cache; out of scope to inspect, see License hygiene in `AGENTS.md`). Gale's static path will always have edges.

Two complementary directions, neither scoped yet:

- **Runtime ATN simulator** in Gale. Large investment; matches ANTLR4 semantics one-for-one.
- **Stage B′ via the JVM ANTLR4 oracle.** Shell out to the vendored `antlr4` JVM tool (already available in the submodule plus `runtime-testsuite/`) to compute oracle parse trees for descriptors whose `[output]` is action-printed (`FullContextParsing/*`, composite descriptors, etc.) and would otherwise be auto-skipped by `normalize_output_for_stage_b`. Cheaper to land; gives us a measurement axis for any future runtime simulator.

Before either direction is scoped, `gale dump` now renders the
overlap-group prediction tree (the same `build_prediction` tree
`gen_multi_alt_body_bt` emits in `parser_gen.wado`) so the
ATN-class edges surface as `Ambiguous([alt N, alt M])` leaves
under the relevant rule. Example: dumping the Performance
`DropLoopEntryBranchInLRRule_4` grammar shows `stat`'s overlap
group resolving to `Ambiguous([alt 0, alt 1])` at depth 0 — both
`expr ';'` and `expr '.'` share their entire `expr` prefix. The
remaining halt-reason / LR loop-entry / K-prefix per-site fields
are still missing from the dump (Phase 1 deferred them); add them
when a concrete ATN-class fix needs them.

## LL prediction — architecture cleanup

Reduces coupling between the codegen walk and the analysis layer; no behaviour change.

(no items currently — the previously listed `intern_follow_variant` → `lookup_follow_variant_id` switch in lower has landed. `register_follow_variants` runs as the first step of `lower_with_ctx`, populating the canonical `(rule, mask, k_prefix_mask)` triples for every parse / scan site lower visits. Lower's `RuleRef` arms consume the registry via `lookup_follow_variant_id`, which panics on a missing key so any future walker / lower drift surfaces immediately.)

## Stage B follow-on — composite descriptors (Stage C dependency)

All 17 `CompositeLexers` / `CompositeParsers` upstream descriptors auto-skip today. The bottleneck is _not_ multi-input plumbing (`extract_antlr4_descriptors.wado`'s `parsed.slave_grammars.len() > 0` short-circuit could be lifted; Kiln already supports multi-input). Every composite descriptor's `[output]` is a host-side artefact — `<writeln(...)>` action-body prints (`S.a`, `M.b`, `T.y`), `Token.toString` dumps (`[@0,0:2='abc',<1>,1:0]`), or empty `[output]`. None survive `normalize_output_for_stage_b`. Re-evaluate this entry once Stage C lands.

## Stage B′ — JVM-oracle integration (in progress)

Design and infrastructure landed in [`antlr4-compatibility.md`](./antlr4-compatibility.md) "Stage B′ — JVM-oracle-derived expected trees". Remaining tasks to wire it up end-to-end:

- **Extract manifest emission.** Teach `extract_antlr4_descriptors.wado` to emit `package-gale/tests/antlr4-compat/oracle-pending.tsv` listing each Stage B′ candidate (descriptor whose `[output]` would auto-skip from Stage B, single-grammar, parser-type, not in `[stage_b_oracle_skip]`). Tab-separated fields: `category`, `name`, `start_rule`, `input` (newlines escaped as `\\n`). Wire the existing `load_triage_maps` to surface `[stage_b_oracle_skip]` and `[stage_b_oracle_force]`.
- **Shell wrapper oracle loop.** `extract-antlr4-descriptors.sh` reads `oracle-pending.tsv`, calls `scripts/strip-grammar.wado` → `scripts/antlr4-oracle.sh` for each entry, and writes the resulting `(rule …)` tree to `tests/antlr4-compat/stage_b_oracle/<C>/<N>_test.wado` via a heredoc template. Oracle failures emit a stderr warning naming the descriptor so the maintainer can add it to `[stage_b_oracle_skip]` (silent skip is forbidden by design).
- **status.toml schema.** Add empty `[stage_b_oracle_skip]` and `[stage_b_oracle_force]` sections under the existing `[stage_b_skip]`. Update the header doc-comment to point at the new sections.
- **Re-extract + commit artefacts.** Re-run `mise run extract-antlr4-descriptors`. The first time it lands, `FullContextParsing/*`, `Performance/*`, `ParserExec/IfIfElse*`, and similar candidates will populate `stage_b_oracle/`. Triage any oracle failures into `[stage_b_oracle_skip]`, any tree-shape mismatches into `[stage_b_oracle_todo]` (TODO: add this state to `status.toml` for parity with `[stage_b_todo]`).

The `IfIfElseNonGreedyBinding1` Stage B′ test (once the loop lands) will sit beside the existing pinned-wrong-shape fixture under `tests/grammars/ll_optional_non_greedy.g4`. Both should flip green simultaneously when a non-greedy `??` fix lands.

## Descriptor importer — infrastructure gaps

- **Composite (slave-grammar) descriptors.** 17 descriptors short-circuit on `parsed.slave_grammars.len() > 0`. Kiln's `use t from "<C>/<Name>.g4" with { ... }` directive needs to resolve `import S;` against sibling `<Name>.slaveN.g4` files. Once that lands the short-circuit comes out.
- **Captured action output (Stage C deliverable).** Parser descriptors whose `[output]` is purely action-print stdout (e.g. `<writeln("S.a")>`) get claim (b) but no `[output]` comparison. Needs Stage C action-body translation in `codegen.wado` plus a parser-side `accumulated_output: String` API.

## Gale bugs surfaced by Stage A drivers

Each is marked `[stage_a_todo]` in `status.toml` and lands a `#[TODO]` test. Roughly ordered by impact:

### Parser codegen

- **LR operator-precedence chain.** `Performance/DropLoopEntryBranchInLRRule_4`: Gale picks the wrong precedence chain for an or-then-and expression. This is ATN-class: the inner `expr` of `'between' expr 'and' expr` is parsed at `min_prec=0` (matching ANTLR4, which gives middle refs precedence 0), and ANTLR4 only resolves the greedy binary-`and` capture via full-context adaptive prediction at the LR loop-entry (the "drop loop entry branch" optimisation the descriptor is named after). Gale's static `scan_expr_lr_*` sees `and X2` matches and commits. See the **ATN-class grammars** section above.
- **Non-greedy `??` prediction layer.** `ParserExec/IfIfElseNonGreedyBinding1`: the emit shape is `Option<T>` (compile-blocker gone) but the dispatch reuses the greedy first-set predictor, so the dangling `else` binds to the inner `if` instead of the outer one ANTLR4 picks. Investigated and deferred: the proper fix requires either runtime ATN simulation or per-call-site follow-variant infrastructure for `??`. The latter sits in the same territory as Failed Approaches LL(\*) variant emit attempts 2–3 (multi-token-inner Repeat in `tail_greedy_first`; suffix-nullable RuleRef sites), so its risk profile is high; a global `rule_follow("ifStatement")`-based skip-set is over-broad — it also yields at the outermost call, breaking the parse. Regression fixture: `tests/grammars/ll_optional_non_greedy.g4` + `tests/driver_optional_non_greedy_test.wado` (pins Gale's _current_ wrong tree shape so the assertion flips the moment a fix lands; the correct ANTLR4-oracle shape is recorded as a comment beside the pinned shape). Tracked here, no design picked yet.

### Lexer codegen

- **Recursive lexer rule with `.+?` / `.*?` wildcard.** `LexerExec/RecursiveLexerRuleRefWithWildcard{Plus,Star}_1`: nested `/* /*...*/ */` comments mistokenize because the recursive call doesn't re-enter under the non-greedy bound. This is ATN-class: matching ANTLR4's NFA→DFA result requires bounding the recursive call against the non-greedy suffix without backtracking; the static single-pass emitter over-consumes (the recursive `try_<rule>` greedily eats a whole sibling comment, so the outer rule can no longer find its closing delimiter). See the **ATN-class grammars** section above.

## Stage C — action / predicate execution

Gale currently **recognises** but **silently discards** the contents of `{ ... }` action blocks and `{ ... }?` semantic predicates. The g4 parser accepts them, so grammars that contain them (`ANTLRv4Lexer`, `RustLexer`, `RustParser`, `TypeScriptLexer`, `TypeScriptParser`) load cleanly — but the generated lexer/parser behaves as if every predicate were `true` and every action were a no-op. That is wrong for:

- `RustLexer.RAW_STRING_LITERAL` — the closing `#` count must match the opening `#` count, enforced by a predicate; without it Gale mistokenizes Rust raw strings.
- TypeScript's regex-vs-division disambiguation and other context-sensitive lexer rules (3 predicates) and parser rules (17).

Stage C is a hard prerequisite for several things:

- Treating Gale as a drop-in ANTLR4 replacement (the stated principle in [`AGENTS.md`](./AGENTS.md)).
- Any lexer-level optimization work — claiming a tokenizer is fast is meaningless if it tokenizes incorrectly.
- `Grammar.options.superClass` and `tokenVocab`, which become wireable once action bodies are real.

Sketch:

- Extend the IR so `OptionValue::Action` and per-alt action / predicate elements carry a language-tagged source fragment instead of being a placeholder string.
- Add a pluggable "action translator" interface; ship at minimum an identity translator for Wado-written action bodies (so Wado-authored grammars work natively).
- Generate a `SuperClass` trait (name derived from `superClass = Foo`) and require callers to `impl` it; emit action bodies as method calls on `self` that resolve through that trait.
- `tokenVocab` falls out at that point — another grammar's generated `TokenKind` enum can be imported by name rather than merged at IR time.

Translating Java/Rust/Python action bodies to Wado is the obvious hard part. A reasonable first cut is to require callers to provide hand-written Wado equivalents for non-Wado grammars (a sidecar file mapping predicate / action ID → Wado snippet), with a migration path to automatic translation later. See [`docs/wep-2026-03-02-gale.md`](../docs/wep-2026-03-02-gale.md) for the design direction.

## Performance — where the 5× gap actually lives

Investigation against `benchmark/sqlite_parse` (Wado/Gale at `-O2`, ~137 ms/iter on a 13 KB SQL fixture; Rust `sqlparser-rs` at debug for reference is ~6.7 ms/iter — release would be far less).

Profile (guest sampler, 5 ms interval) self-time top:

|   Pct | Symbol                                                    |
| ----: | --------------------------------------------------------- |
| 27.9% | `tokenize`                                                |
| 26.0% | `Array<Token>::push` (per-token `struct.new Token`)       |
| 17.2% | `Parser::last_end` (4-step `Parser→Array→Token→Span→end`) |
|  4.4% | `Array<Token>::grow`                                      |

Combined: token-stream construction (`tokenize`, `Array<Token>::push`, and `Array<Token>::grow`) is 58% of self-time. Token reads via `Parser` are next.

### What does not work

- **Inlining hot Parser methods.** `Parser::last_end` accounted for 17% self-time; both caching it as a field and forcing `#[inline]` eliminate the named function from the profile but do not move wall time. The cost was the actual loads (`Parser→Array→Token→Span→end`), not call overhead — inlining merely redistributes it into the callers (`parse_expr`, `Parser::expect`, …). wasmtime + Cranelift handles small Wasm function calls cheaply enough that hunting for inlinability is not a productive lever here.
- **Any micro-optimization on individual Parser methods.** Same reason: the bytes loaded are unchanged, so the work is unchanged.

### What would actually move the needle

The dominant cost is **Wasm GC `(array (ref Token))` indirection plus per-token `struct.new Token` allocation**. A 5× improvement requires decomposing `Array<Token>` into parallel primitive arrays (`kinds`/`starts`/`ends` as `Array<i32>`, packed in Wasm GC) so that:

- `peek_kind` / `tokens[i].kind` becomes a single `array.get i32` instead of `array.get (ref Token)` + `struct.get`.
- Per-token struct allocation disappears in the lex loop.

Two non-overlapping paths to get there:

1. **Gale-side**: redesign `Token` so the hot fields are flat primitives, with an opaque sidecar (or removal) for `text` / `leading_trivia`. Keep the public `Token` API as a view handle if needed for compatibility.
2. **Wado-side**: extend `container_sroa` to handle (a) struct fields (currently locals only — see `wado-compiler/src/optimize/container_sroa.rs` "Future directions"), (b) inner structs with nested struct or reference fields, (c) cross-function rewrites for the `scan_*(&Array<Token>, ...)` parameter pattern (1100+ sites in the SQLite parser pass `&p.tokens` as a bare reference, currently always escaping). Today the pass fires on zero candidates in Gale-generated parsers.

### Lexer dispatch (independent secondary lever)

Inside the 27.9% `tokenize` self-time, the work splits roughly into per-character branch dispatch and keyword classification. Several techniques can replace the current hand-rolled cascade — pick by what profiling on the predicate-correct lexer (after Stage C) says is hottest:

- **Table-driven DFA** for the whole lexer (NFA → DFA subset construction → state-transition table). Replaces both per-character dispatch and `classify_keyword`. ANTLR4 `mode` blocks become a DFA per mode plus mode-switch on accept; lexer commands (`skip`, `more`, `type(N)`, `channel(HIDDEN)`) attach as accept-state attributes. Semantic predicates are the only DFA-blocker — once predicates are real (Stage C), predicate-bearing rules need a hybrid (DFA-friendly prefix + predicate gate) or a per-rule fallback.
- **Trie / nested-switch on bytes** for `classify_keyword` only. Targets the keyword cascade (~140 SQL keywords today, length-bucketed nested `if`-chain). Branches share prefixes (`IN` → `INSERT` / `INSTEAD` / `INTERSECT` / `INTO`), so a trie is a clean fit. Smaller code-size impact than a full DFA.
- **Compile-time perfect hash** for `classify_keyword`. `gperf`-style build-time generated hash from identifier bytes → `TokenKind`. O(1) lookup with no comparisons after hashing. Best when keyword count is large enough that linear / trie lookup is the bottleneck.
- **SIMD-based pre-scan** (Wasm `v128`) for finding token boundaries and character-class membership in bulk. Effective if the per-byte work is tiny but the byte loop is the bound.

The choice depends on which sub-cost in `tokenize` dominates after the SoA + correctness work above is done. None of these are useful in isolation — they multiply with the SoA win, not replace it.
