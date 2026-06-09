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
- **P2** — _(done)_ runtime simulator core: `atn_decode` (blob → SoA `AtnSim`), `atn_closure` / `atn_move`, `atn_predict_with_stack` / `atn_adaptive_predict` (SLL: simulate to single alt; conflict → min-alt with non-greedy exit-first ordering). In `runtime/atn.wado`, unit-tested in `src/atn_sim_test.wado` (two-token resolution + greedy/non-greedy tie-break). DFA cache not yet added (a P3 perf add, not needed for correctness). `atn_predict_with_stack` already accepts a caller stack for full context.
- **P3a** — _(done)_ full-context prediction **engine**. Upgraded the simulator to take the caller's **rule-invocation stack** (rule indices) instead of needing a per-call-site GIR↔ATN map: on rule exit a config branches into every ATN return-state where the caller rule invokes the callee (rule-context precision); a config returning past a real stack bottom accepts only EOF (`ATN_ACCEPT_EOF`). **Why the rule-index stack:** the real follow is what makes IfIfElse work — with an empty (SLL) stack the exit-config matches any follow (wildcard) so non-greedy skips `else` even at the OUTER `ifStatement` and the parse fails. Proven in `src/atn_sim_test.wado`: the SAME `??` decision resolves EXIT for the inner `ifStatement` (else→outer) and ENTER for the outer (consumes else) given stacks `[0,1,2,1,2]` vs `[0,1,2]`. Engine-only; no codegen path calls it yet.
- **P3b** — _(done for non-greedy `??`)_ wired the engine into codegen. Gated on `GenContext.needs_atn` (set by lowering on a non-greedy Optional), so only non-greedy grammars change; all others stay byte-identical. `codegen.wado` emits `global GALE_ATN_DATA` (the serialized ATN); the `Parser` gains `atn: AtnSim` (decoded once) + `atn_stack: List<i32>`; the rule-entry wrapper push/pops the rule index; and `gen_op_{storable,transparent}_optional` route a non-greedy `??` through `atn_ng_optional_enter` (which identifies the rule's unique exit-first BlockStart at runtime from `atn_stack`'s top — no compile-time decision number needed, and falls back to greedy if the decision isn't unique). **Flipped green:** `ParserExec/IfIfElseNonGreedyBinding1` (`[stage_b_oracle_todo]` removed) and `tests/driver_optional_non_greedy_test.wado` (now asserts the ANTLR4-correct outer-binding tree). **Remaining P3b follow-ups:**
  - **Empty-alt groups.** _(done)_ `( | X)` / `(X | )` two-alt groups with exactly one empty alt are a true EBNF equivalence of `(X)??` / `(X)?`. `canonicalize_empty_alt_groups` (`ir.wado`, run at the top of `generate()` before lowering and `build_atn`) folds them onto the Optional path: empty-first ⇒ non-greedy, empty-last ⇒ greedy. So `IfIfElseNonGreedyBinding2`'s `( | 'else' statement)` reuses the proven Binding1 ATN-simulator path with no new prediction machinery. **Flipped green:** `ParserExec/IfIfElseNonGreedyBinding2` (`[stage_b_oracle_todo]` removed) + `tests/driver_empty_alt_group_test.wado` (outer-binding tree). Full O2 suite: 1661 passed / 0 failed (no regression across grammars using either empty-alt form).
  - **General multi-alt `Ambiguous` group / multiple `??` per rule (deferred to P4).** A genuinely multi-alt Ambiguous group (3+ alts, overlapping first-sets) and `>1` exit-first decision per rule still need a compile-time decision-number correspondence between `build_atn` and the emitter (stamp the ATN decision number on the surface group/repeat node, thread it onto the GIR op, emit `atn_predict_with_stack(sim, decision, …)`). This retires the `atn_ng_optional_enter` "unique exit-first BlockStart" heuristic. Its only real-corpus consumer beyond the empty-alt case is P4 (LR loop-entry), which additionally needs graph-merged stacks + precedence predicates — so the decision-number foundation is co-designed there rather than built speculatively now.
  - **Per-`Parser` DFA cache** (perf), once more decisions route through the simulator.
  - ~~The descriptor extractor (`scripts/extract_antlr4_descriptors.wado`) currently panics the wado compiler (`wir_build/translate.rs:1938`).~~ **Fixed** (PR #1329, on main): the ICE was a use→def edge clobber from method-signature resolution mis-attributing a foreign module's `AstId` to the consumer; `lookup_method_info` now suppresses reference recording. The extractor compiles cleanly again, so re-extraction is unblocked.
- **P4** — _(done)_ full-context handling for left-recursive precedence; `Performance/DropLoopEntryBranchInLRRule_4` flipped green via the dedicated O(n) LR loop-entry decision (parse + scan sides). See the P4 design/phasing below.
- **P5** — lexer ATN + non-greedy wildcard; flip `RecursiveLexerRuleRefWithWildcard{Plus,Star}_1` (`[stage_a_todo]`).

#### P4 design — LR loop-entry adaptive prediction

**The concrete bug.** `expr : … | expr 'and' expr | … | 'between' expr 'and' expr ;`. On `between X1 and X2 and X3 ;` Gale errors `expected 'and', got ';'`: the `between` atom's **first** `expr` is parsed greedily and consumes `X1 and X2 and X3` as binary-ands, so `between`'s mandatory literal `'and'` has nothing left. ANTLR4 _drops the loop-entry branch_ there — at the LR loop-entry after `X1` it predicts (full context, arbitrary lookahead) that continuing the `and`-loop makes the enclosing `between` unparseable, so it exits the loop early, leaving `and X2` for `between`. Precedence alone does **not** fix this: between's operands are `expr[0]`, so the `{prec≥0}?` loop predicate is satisfied; the decision is genuinely adaptive, not precedence-static.

**Why it's the biggest piece — four gaps, in dependency order.**

1. **LR ATN construction (`atn.wado`).** _(done)_ `build_atn` previously built the _pre-LR-rewrite_ surface alts, i.e. naive left-recursive `expr → RuleRef(expr) …` cycles the simulator cannot walk (infinite left-recursion). It now builds LR rules in the ANTLR4 rewritten form (`vendor/antlr4/doc/left-recursion.md`, license-OK):
   `expr[pr] : (atom alts) ( {prec_i ≥ pr}? op_i expr[next_i] )*` —
   a `RULE_START → atom block → STAR_LOOP_ENTRY`, where the loop-entry has one edge per LR alt carrying a **precedence predicate** (a new `ATN_TR_PRECEDENCE` transition kind whose `prec` is the alt's `own_prec`) plus the suffix, and a trailing exit edge; each recursive operand `RuleRef` carries the alt's `conflict_min` floor. Reuses lowering's existing LR analysis: `build_atn_with` takes the `LoweredGrammar` and builds LR rules from `LeftRecursiveRule`/`LrAlt` (`build_lr_body` / `build_lr_suffix`), non-LR rules from the surface alts as before; `gen_atn_data_global` passes the lowered grammar codegen already computed. Blob version bumped 1 → 2; `atn_closure` walks the precedence edge like epsilon for now (evaluation is P4.3). Behaviour-unchanged: no grammar routes LR decisions through the simulator yet. Unit-tested in `atn_test.wado` (rewritten loop-entry shape + precedence floors) and `atn_sim_test.wado` (the simulator terminates on the LR ATN instead of left-recursing).

2. **Exact prediction context (return-state stack), not rule indices.** The decision hinges on the _exact_ enclosing return state ("this `expr` is `between`'s first operand, after which the caller expects literal `'and'`"). The simulator's during-prediction `AtnConfig.stack` is already exact return states; the gap is the **caller** stack `p.atn_stack`, which the rule-entry wrapper fills with rule **indices** (`atn_call_return_states` then over-approximates by matching _every_ call site of the callee). That can't distinguish between's-first-operand from a loop operand. Upgrade: the generated parser pushes the **ATN return-state id** at each call site (not the rule index at rule entry), so the simulator pops to the precise caller continuation and sees between's pending `'and'`.
   - **P4.2a — correspondence foundation. _(done)_** A stable per-call-site id ties each surface `RuleRef` to its ATN return state: `assign_atn_call_sites` (`ir.wado`, run in `generate()` after canonicalization) stamps `RuleRefElement.atn_call_site`; `build_atn` records `call_site → return_state` in `Atn.call_site_returns` (the generic atom path and the LR suffix self-ref; the stripped leading LR self-ref correctly gets none). The map is codegen-only — NOT in the serialized blob. Behaviour-neutral (nothing reads it at runtime yet; generated parsers byte-identical). Proven on a `between`-shaped grammar (`atn_test.wado`): between's first operand, the LR loop operand, and between's second operand all map to distinct return states.
   - **P4.2b-i — GIR threading. _(done)_** `RuleRefElement.atn_call_site` is carried onto the lowered `RuleCallOp` (surface-origin sites + every rebuilding step; default -1). Behaviour-neutral; tested in `lower_test.wado`. Lets codegen resolve each call's return state from `Atn.call_site_returns`.
   - **P4.2b-ii — runtime rewire. _(done)_** The runtime caller stack now holds exact ATN return states. `generate()` builds the ATN after lowering and stashes `call_site_returns` on `GenContext`; `Parser.atn_ret_pending` is set by each caller before `_parse_X` (`emit_atn_ret_pending` at every parse-side call emit) and pushed by `gen_rule_entry_wrapper` onto `p.atn_stack`, popped on exit (error-safe across `?`). `atn_closure` pops to the exact `rstk[rs]` (start-rule -1 = EOF, intermediate -1 = SLL-accept); `atn_call_return_states` retired. `atn_ng_optional_enter` takes the current rule as a compile-time constant (`GenContext.current_rule_index`). Validated: non-greedy + empty-alt driver trees green (correct outer-`else` binding), `atn_sim_test` full-context stacks migrated to return states (inner `??` EXITs, outer `??` ENTERs); 401 driver + g4 tests, 206 codegen/lower/etc., 0 failed. Only the two ATN-using grammars' parsers change.

3. **Precedence-predicate evaluation in the simulator (`runtime/atn.wado`). _(done — P4.3 part 1)_** `atn_closure` prunes a loop-entry precedence edge `{prec_i ≥ pr}?` when `tr_prec < pr`. `AtnConfig` carries `pr` + a `pr_stack` (saved per during-prediction return frame); a recursive operand's precedence-carrying `RuleRef` raises `pr` to its baked floor (`expr[next_i]`) and restores it on rule-stop. `atn_predict_with_stack` gains a defaulted `min_prec` (0); seeds prune the predicted decision's edges by it. Unit-tested in `atn_sim_test.wado` (left-assoc `'+'` pruned at `min_prec` 2). The remaining **part 2** — codegen loop-entry wiring — is below.

4. **Scaling — graph-structured contexts. _(done — exponential killed; O(n) optimization still open)_** The exact per-config `List<i32>` stacks blew up exponentially under deep left recursion (measured ×7 per +2 depth; past depth ~10 the `ATN_MAX_CONFIGS` cap truncated and **mispredicted**). Replaced by a hash-consed `PredictionContext` DAG (`CtxArena`): each node is a set of `[return_state, restore_pr, parent]` edges, merged so the closure keeps **one config per `(state, alt, rs, pr)`** (`CfgSet`). Now polynomial (measured `lr_between.g4` depth 8/16/32 = 1.3s / 7s / 48s — no blow-up, no cap misprediction). The during-prediction recursion is input-driven (an LR operand is reached only after its operator token), so context depth is input-bounded — no cyclic-context machinery needed; generous `ATN_CLOSURE_GUARD`/`ATN_ARENA_CAP` are pathological-only. All 16 ATN unit + driver tests green.

   **O(n²) → O(n): dedicated loop-entry decision. _(done — parse side)_** The general ALL(*) simulation is the wrong tool for an LR loop-entry: each iteration's `predict` re-walks the exit alt's O(depth) enclosing-loop caller chain → O(n²) (deep descriptor ≈ 48s). The loop-entry needs no arbitrary lookahead — it is **precedence + yield-to-caller-mandatory**. `atn_lr_loop_decision` (`runtime/atn.wado`) computes it directly: (1) if the current token is mandatory in the caller continuation (walk the caller return-state stack, pop nullable levels, test the first non-nullable FIRST) → EXIT (`between`'s `'and'`); (2) else the lowest-index precedence-allowed enter edge whose suffix starts with the token → ENTER (left-assoc falls out of the prune); (3) else EXIT. FIRST/nullable per rule is a one-time fixpoint over the decoded ATN (`atn_compute_first_nullable`). `gen_lr_suffix_dispatch_atn` calls it instead of the simulator. Measured: `between X1 and X2 and X3` ~1ms; `lr_between.g4` depth 32/256 = 37ms/1.19s. 474 driver + LeftRecursion tests green, no regression. (Measured dead-ends, do not retry: graph-merged-context full simulation — correct but O(n²) here; per-`Parser` closure memo — full-context contexts encode the whole nesting depth so config sets never recur, memo never hits.)

   **Scan side. _(done — descriptor FLIPPED GREEN)_** `DropLoopEntryBranchInLRRule_4`'s `stat : expr ';' | expr '.'` runs a scan-tournament over `expr`, and `scan_expr` (a pure `scan_X(tokens, pos)` with no `Parser`) mis-scanned `between`. Mirrored the decision to the scan path *without* param threading: two module-level globals (gated on `needs_scan_atn`, an ATN-class LR rule) — `GALE_ATN_SIM` (`atn_decode` once) and a mutable `GALE_SCAN_STACK`. `gen_scan_op_element` brackets each self-ref operand scan with `GALE_SCAN_STACK.push/pop` of the call's return state (`ScanRuleCallElem.atn_call_site` → `call_site_returns`, popped before the fail check); `gen_scan_lr_suffix_dispatch_atn` routes the scan loop through `atn_lr_loop_decision(&GALE_ATN_SIM, &GALE_SCAN_STACK, …)`. `Performance/DropLoopEntryBranchInLRRule_4` parses in ~117ms and is out of `[stage_a_todo]`. 546 driver + LeftRecursion + ATN tests green, no regression; non-greedy parsers byte-identical (globals gated). _Note:_ the greedy operand makes a very long chain right-recurse deep (depth ~1024 overflows the wasm stack); the descriptor is shallow (≈ depth 30) so unaffected — a left-assoc loop form would be more robust if a deeper corpus needs it.

**Codegen wiring (P4.3 part 2 — _done_).** The LR loop-entry of an ATN-class rule is now routed through the simulator. `lr_rule_is_atn_class` (`lower.wado`) detects the shape (atom alt's self-ref followed by a fixed token that is an LR suffix-first), sets `LeftRecursiveRule.atn_class` + `needs_atn`; `lr_loop_entry_decision` (`atn.wado`) recovers the `STAR_LOOP_ENTRY` decision number, stashed per rule on `GenContext`; `gen_lr_suffix_dispatch_atn` (`parser_gen.wado`) emits `atn_predict_with_stack(decision, &p.atn_stack, …, min_prec)` and dispatches the edge index onto the suffix helpers (progress-guard termination, no static scan gate). Every non-ATN-class LR rule keeps its static `scan_X_lr_*` fast path byte-identical. Validated: `tests/grammars/lr_between.g4` + `driver_lr_between_test.wado` green (`between X1 and X2 and X3` parses; binary chains unregressed); 465 driver + `LeftRecursion` tests, 0 failed.

**Phasing — P4 COMPLETE.** P4.1 LR ATN build _(done)_ → P4.2a call-site↔return-state correspondence _(done)_ → P4.2b runtime rewire _(done)_ → P4.3 precedence eval + loop-entry wiring _(done; `lr_between.g4` green)_ → P4.4 _(done)_: GSS killed the exponential, then the dedicated O(n) `atn_lr_loop_decision` (precedence + yield-to-caller-mandatory) replaced the simulation on both the parse and scan sides, flipping `Performance/DropLoopEntryBranchInLRRule_4` green (~117ms). The graph-structured `PredictionContext` (`CtxArena`) remains as the merged-context core of the general simulator (still used by non-greedy `??`). Open follow-up (not P4): a left-associative LR loop form so very long chains don't right-recurse deep (only a pathological >~1000-deep input, beyond any real grammar, hits the wasm stack today).

**Open design questions to resolve before P4.3.**

- _min-prec threading into the simulator._ The parse side already carries `min_prec`; confirm the same value is the `pr` the loop-entry predicates compare against, and that `expr[next_i]` (the recursive operand's raised precedence) is encoded on the suffix RuleRef so nested decisions see the right `pr`.
- _Decision identity._ The loop-entry decision number must agree between `build_atn` and the emitter — reuse the P3b decision-number stamping (stamp on the rule / loop-entry node), now actually built here.
- _Interaction with the existing static LR dispatch._ Keep the static `scan_X_lr_*` fast path for LR rules with **no** ATN-class loop-entry (the common case stays byte-identical); only ATN-class LR rules route through the simulator.

The Stage B′ JVM oracle (already landed, see below) is the measurement axis: each ATN-class fix flips its pinned `[stage_b_oracle_todo]` / `[stage_a_todo]` test green.

To triage which static edge a concrete fix must close, `gale dump` surfaces each unresolvable decision as `Ambiguous([alt N, alt M]) — <reason>` (`AmbiguityReason` in `prediction.wado`), with the per-site LR `loop-entry:` dispatch (`conflict-min` + suffix-first overlap groups) and follow-variant `k-prefix=` masks alongside. Example: `DropLoopEntryBranchInLRRule_4`'s `stat` → `Ambiguous([alt 0, alt 1]) — opaque rule-ref prefix` (its two alts share the entire `expr` prefix).

## Stage A gaps — Gale bugs surfaced by descriptor drivers

Marked `[stage_a_todo]` (or `[stage_b_oracle_todo]`) in `status.toml`, each shipping a `#[TODO]` test. All are ATN-class (see above) and Stage-C-independent.

- **LR operator-precedence chain. _(done — flipped green)_** `Performance/DropLoopEntryBranchInLRRule_4`: the inner `expr` of `'between' expr 'and' expr` is parsed at `min_prec=0`; Gale's static `scan_expr_lr_*` saw `and X2` match and committed, starving `between`'s mandatory `'and'`. Resolved by the dedicated O(n) `atn_lr_loop_decision` (precedence + yield-to-caller-mandatory) on both the parse (`gen_lr_suffix_dispatch_atn`) and scan (`gen_scan_lr_suffix_dispatch_atn` + `GALE_SCAN_STACK`) sides — the "drop loop entry branch" semantics, clean-room. Parses in ~117ms; out of `[stage_a_todo]`.
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
