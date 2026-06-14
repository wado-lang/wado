# Gale Code Health

Tech-debt tracker for `package-gale/`, seeded by a full-source audit
(2026-06-10). This file tracks **cleanup** work only — duplicated logic
and test-coverage gaps. Correctness bugs from the audit now live under
"Code-health bugs" in [`TODO.md`](./TODO.md); feature/compatibility work
is the rest of `TODO.md`.

The dead code, stale/contradictory comments, naming drift, and
structure/policy findings from the original audit have been resolved (see
commit history); what remains below is the higher-effort structural work.

How to read:

- This file lists only what is **not yet done**. Closed items are removed; the fix lives in commit history.
- Line numbers are as of the audit commit and will drift.

## Logic duplication

The biggest single lever: most twin-path bugs exist because the second copy missed a fix.

- [ ] Two escape resolvers (`read_string_escape` vs `resolve_char` — both fixed in place but still separate, should be unified), four balanced-delimiter scanners with conflicting failure conventions (-1 vs end-of-input), four comment-skipping implementations, duplicated postfix/alternatives parsing (`parse_postfix`/`parse_lexer_postfix`, `parse_alternatives`/`parse_lexer_alternatives`), duplicated `{ ID (, ID)* }` block scanners (tokens/channels — the hang fix landed in one, now both).
- [ ] `best_*` state-reset strings rendered in four places. `src/lexer_gen.wado:1352-1423`, `:1846-1900` (token numbering is now unified via `token_slot_order`).
- [ ] `generator.wado` still copies the open → read → parse → merge → synthesize → check pipeline that `main.wado` now folds into `load_and_merge`. `src/generator.wado:55-88`
- [ ] `try_expand_opaque` re-implements `build_sll_node`'s dispatch construction (where the at-end handling got lost); `dump.wado::render_prediction` hand-mirrors `gen_multi_alt_body_alt`'s grouping+sort+`build_prediction(…, MAX_LOOKAHEAD_DEPTH, …)` pipeline.
- [ ] Test helpers copy-pasted per file: `assert_tree` ×33, `assert_parses` ×8 (+2 near-clones), `unparse_xml` ×3, hand-rolled `str_contains` ×5 (stdlib `String::contains` exists and is used elsewhere). Create a shared test-support module.
- [ ] Extractor emitter boilerplate ×6 (~80% identical bodies). `scripts/extract_antlr4_descriptors.wado:659-998`, `:1841-1857`
- [ ] Name-dedup machinery re-implemented in `visitor_gen.wado` (`get_name_count`/`increment_name_count` + inline `dedup_name`) instead of importing `gen_util`. `src/visitor_gen.wado:236-240`, `:392-410`
- [ ] Architectural duplication: `codegen.wado` reads pre-classified GIR shapes, but `visitor_gen.wado` re-derives the same field names from the surface IR with its own group-counter logic — numbering drift produces non-compiling field names. `src/codegen.wado:347-369` vs `src/visitor_gen.wado`

## Test coverage gaps

- [ ] `codegen_test.wado` asserts only on generated-source substrings, so dispatch-shape bugs are invisible to it; coverage relies entirely on driver/descriptor layers.
- [ ] Triage parsing covers 3 of 4 axes: no `"oracle"` arm in `lookup_for_axis` (unknown axes silently map to stage_b), and zero tests for `[stage_b_oracle_skip]`/`[stage_b_oracle_todo]` parsing. `scripts/extract_antlr4_descriptors.wado:2051-2057`
- [ ] `gen_tokenize_fn` / `build_dispatch_groups` / keyword classifier (largest lexer_gen code) have no unit tests; deep-nullable FIRST through rule refs is a `gen_context_test` blind spot.
