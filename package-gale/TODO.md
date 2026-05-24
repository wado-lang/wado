# Gale TODO

Open work towards full ANTLR4 compatibility and the performance budget it implies. Read this together with:

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract, stage layering, descriptor pipeline, and triage workflow.
- [`AGENTS.md`](./AGENTS.md) — internal architecture (LL prediction design, soundness invariants) and failed approaches.

This file lists what is **not yet done**. Closed work belongs in commit history.

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

Grammars whose alt selection requires arbitrary-length lookahead through ambiguous prefixes cannot be decided by static FOLLOW + K-prefix. ANTLR4 handles them with a runtime ATN simulator (closure / predict / DFA cache) — see `vendor/antlr4/runtime/Java/src/org/antlr/v4/runtime/atn/ParserATNSimulator.java`. Gale's static path will always have edges.

Two complementary directions, neither scoped yet:

- **Runtime ATN simulator** in Gale. Large investment; matches ANTLR4 semantics one-for-one.
- **Stage B′ via the JVM ANTLR4 oracle.** Shell out to the vendored `antlr4` JVM tool (already available in the submodule plus `runtime-testsuite/`) to compute oracle parse trees for descriptors whose `[output]` is action-printed (`FullContextParsing/*`, composite descriptors, etc.) and would otherwise be auto-skipped by `normalize_output_for_stage_b`. Cheaper to land; gives us a measurement axis for any future runtime simulator.

## LL prediction — architecture cleanup

Reduces coupling between the codegen walk and the analysis layer; no behaviour change.

(no items currently — the previously listed `intern_follow_variant` → `lookup_follow_variant_id` switch in lower has landed. `register_follow_variants` runs as the first step of `lower_with_ctx`, populating the canonical `(rule, mask, k_prefix_mask)` triples for every parse / scan site lower visits. Lower's `RuleRef` arms consume the registry via `lookup_follow_variant_id`, which panics on a missing key so any future walker / lower drift surfaces immediately.)

## Stage B follow-on — composite descriptors (Stage C dependency)

All 17 `CompositeLexers` / `CompositeParsers` upstream descriptors auto-skip today. The bottleneck is _not_ multi-input plumbing (`extract_antlr4_descriptors.wado`'s `parsed.slave_grammars.len() > 0` short-circuit could be lifted; Kiln already supports multi-input). Every composite descriptor's `[output]` is a host-side artefact — `<writeln(...)>` action-body prints (`S.a`, `M.b`, `T.y`), `Token.toString` dumps (`[@0,0:2='abc',<1>,1:0]`), or empty `[output]`. None survive `normalize_output_for_stage_b`. Re-evaluate this entry once Stage C lands.

## Descriptor importer — infrastructure gaps

- **Composite (slave-grammar) descriptors.** 17 descriptors short-circuit on `parsed.slave_grammars.len() > 0`. Kiln's `use t from "<C>/<Name>.g4" with { ... }` directive needs to resolve `import S;` against sibling `<Name>.slaveN.g4` files. Once that lands the short-circuit comes out.
- **Captured action output (Stage C deliverable).** Parser descriptors whose `[output]` is purely action-print stdout (e.g. `<writeln("S.a")>`) get claim (b) but no `[output]` comparison. Needs Stage C action-body translation in `codegen.wado` plus a parser-side `accumulated_output: String` API.

## Gale bugs surfaced by Stage A drivers

Each is marked `[stage_a_todo]` in `status.toml` and lands a `#[TODO]` test. Roughly ordered by impact:

### Parser codegen

- **LR rule with `returns` + list-label combination.** `LeftRecursion/ReturnValueAndActionsList1_{2,4}`: parse stops at the first comma in the input list. Investigate the LR-alt-rewrite path's interaction with list-label storage.
- **LR operator-precedence chain.** `Performance/DropLoopEntryBranchInLRRule_4`: Gale picks the wrong precedence chain for an or-then-and expression.

### Lexer codegen

- **EOF-suffixed rule priority.** `LexerExec/EOFSuffixInFirstRule_2`: when two rules can match the same prefix (`A : 'a' EOF;` vs `B : 'a';`), ANTLR4 prefers the one that consumes the trailing EOF. Gale picks the lexically-later rule. Fix in the longest-match tiebreaker.
- **Recursive lexer rule with `.+?` / `.*?` wildcard.** `LexerExec/RecursiveLexerRuleRefWithWildcard{Plus,Star}_1`: nested `/* /*...*/ */` comments mistokenize because the recursive call doesn't re-enter under the non-greedy bound.
- **`-> more, mode(...)` chain across modes.** `LexerExec/ZeroLengthToken`: a token built via `-> more, pushMode(...)` followed by `-> more, mode(...)` should merge into a single token spanning all the `more`'d chars, but Gale emits the final piece only.

### Compile-blocking codegen failures (`[stage_a_skip]`)

These reject grammars wholesale — Gale codegen produces invalid Wado (or Gale's own validator rejects the grammar), so the descriptor's test file cannot even compile. Verified root causes (the historical `[stage_a_skip]` reasons in `status.toml` were partly stale and have been updated):

- ~~**User token name collides with Gale's internal sentinel** — `Performance/ExpressionGrammar_{1,2}`.~~ Fixed: `gen_token_constants` / `gen_token_kind_name_fn` (`lexer_gen.wado`) now suppress the sentinel global and its match-arm when a user lexer rule already owns the `TK_ERROR` identifier. The canonical `ERROR : .;` shape is semantically a catch-all that aligns with the sentinel's no-match role, so collapsing onto a single kind preserves behaviour across both code paths (user rule match + lexer no-match emit). `TK_EOF` is intentionally not collapsed — ANTLR4 reserves `EOF` as a built-in (`kind == TK_EOF` means end-of-stream in every generated parser) and the corpus contains no user `EOF` lexer rule; if one ever appears, the right place is a g4-parse-layer diagnostic. Regression tests in `src/lexer_gen_test.wado`.
- ~~**Parser rule references an implicit (lexer-undefined) token** — `LeftRecursion/WhitespaceInfluence_{1,2}`.~~ Fixed: a new `synthesize_implicit_tokens` pass in `ir.wado` walks the post-merge grammar's pending refs and pushes a `LexerRule::virtual_token` for every uppercase parser-side reference whose name has no defining rule (matching ANTLR4's silent implicit-token-type behaviour from `vendor/antlr4/doc/lexer-rules.md`). The pass runs in both call sites (`main.wado` for the CLI, `generator.wado` for kiln) right before `check_references`, so the unresolved-token check now sees the synthesized rules and the downstream `gen_token_constants` emits a `TK_<name>` global with the `virtual` trailing comment. Behaviour: the lexer cannot produce the token on its own (no body), but the parser can still expect it — matching ANTLR4. Regression tests in `src/ir_test.wado` (happy path, dedup, defined-tokens, EOF exclusion).
- ~~**List-label `b2+=b*` shadows the container with the per-iteration value** — `ParserExec/Labels`.~~ Fixed: the per-iter parse result and the list-label container now get distinct names by construction. Two distinct codegen paths handle the two surface shapes (`Repeat(Label)` vs bare `Label`), so the fix has two halves: `lower_repeat_op` (`lower.wado`) seeds its fresh `inner_counts` with `outer_field`, forcing the inner's `dedup_name` to fork on collision (`let b = _parse_b(p)?; b2.push(b);`); `gen_op_list_label` (`parser_gen.wado`) rebinds the leaf's emit field to `<container>_item` when it would clash with the container, keeping the surrounding name_counts and the visitor's parallel naming pipeline untouched. Verified via the new `tests/grammars/label_list_collision.g4` driver (`name+=X name+=X*` shape) plus the existing label_gaps suite. Closes both `ParserExec/Labels` and `ParserExec/ReservedWordsEscaping` since the latter is the same shape with an extra Wado-keyword rule rename on top.
- **Set list-label `val+=(INT | FLOAT)*` panics inside Gale** — `ParserExec/ListLabelsOnSet`. `gen_op_storable_group_inner` panics with `surface element is not Group` before any Wado is emitted (panic chain: `gen_op_storable_group_inner` ← `gen_op_inner_for_storable_repeat` ← `gen_op_repeat_star_plus_storable` ← `gen_op_repeat_star_plus_greedy` ← `gen_op_repeat_with_follow_override` ← `gen_op_repeat` ← `gen_alt_elements`). Fixture: existing `tests/antlr4-compat/grammars/ParserExec/ListLabelsOnSet.g4` (7 lines). Fix in `parser_gen.wado`: teach `gen_op_storable_group_inner` (or its caller) to accept the set-shape surface, or route set list-labels through a separate emitter.
- ~~**Non-greedy optional `??` routed through `*`-style codegen** — `ParserExec/IfIfElseNonGreedyBinding1`.~~ Partially fixed: emit shape is now `Option<T>` plus a single `if` check (matching the lower-side `FieldKind::Option` decl), so the test compiles. `lower_repeat_op` dispatches non-greedy `Optional` through the greedy strategy pipeline; `gen_op_repeat_with_follow_override` routes the same case to the greedy Optional emitter. Regression test in `src/codegen_test.wado` (`non-greedy optional (??) emits an Option<T> field, not Array<T>`). Remaining (`[stage_a_todo]`): the greedy emitter applies first-set prediction, not the prefer-skip semantics ANTLR4's `??` requires — the descriptor's expected parse tree binds the dangling `else` to the *outer* if, but Gale still binds it to the *inner* if. Closing this needs either a follow-guarded Optional dispatcher or runtime ATN simulation.
- ~~**Wado-reserved-word rule names + list-label collision** — `ParserExec/ReservedWordsEscaping`.~~ Fixed alongside `ParserExec/Labels` (same root cause, just compounded by the `if → if_` keyword rename that aligned both the label name and the inner TokenRef field name on `if_`).
- ~~**`type(N)` lexer command leaves `TK_<N>` undeclared** — `LexerExec/TokenType0xFFFF`.~~ Fixed: `gen_token_constants` (`lexer_gen.wado`) now walks every non-fragment rule's `type_override`, parses numeric targets, and emits one `global TK_<N>: i32 = N - 1;` per distinct value. The `N - 1` accounts for the runtime's `kind + 1` remap in `to_lexer_string` (runtime.wado) so the rendered ANTLR4 type id round-trips to exactly `N`. Regression tests live in `src/lexer_gen_test.wado` (numeric override, dedup, `type(0)` rejection, named override no-op).

These are Gale bugs end-to-end; none reproduce in `wado-compiler` as the compiler is correctly rejecting invalid input. Verified 2026-05-24 by running `wado run package-gale/src/main.wado gen <grammar.g4>` on each `.g4` and inspecting the generated Wado (or the gen-time stderr).

### Runtime gaps (test compiles, fails at runtime)

- **Non-default-channel tokens don't appear in `to_lexer_string`** — `LexerExec/ReservedWordsEscaping`. Fix: either land non-default-channel tokens in the main stream (with a channel attribute) and have the parser filter, or extend `to_lexer_string` to walk `Token.leading_trivia` in source order.

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
