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

- [ ] The remaining alternatives twins (`parse_alternatives`/`parse_lexer_alternatives`) are still separate: a trivial `|`-separated loop differing only in element type. Left alone deliberately — a generic/closure unification costs more than the 5-line duplication removes. (The balanced-delimiter scanners, comment-skipping, `parse_postfix`/`parse_lexer_postfix`, and the `tokens`/`channels` block scanners are now unified via `src/g4/scan.wado`, `parse_repeat_op`, and `parse_id_list_block`.)
- [ ] `try_expand_opaque` re-implements `build_sll_node`'s dispatch construction (where the at-end handling got lost); `dump.wado::render_prediction` hand-mirrors `gen_multi_alt_body_alt`'s grouping+sort+`build_prediction(…, MAX_LOOKAHEAD_DEPTH, …)` pipeline.
- [ ] Test helpers copy-pasted per file: `assert_tree` ×33, `assert_parses` ×8 (+2 near-clones), `unparse_xml` ×3. These are each bound to their grammar's generated namespace (`g::parse` / `g::to_tree` / `g::token_kind_name` / `normalize_tree`), so a shared module can only host a grammar-independent tail (normalize+compare+assert); the per-grammar calls stay. (The hand-rolled `str_contains` copies are gone — replaced by stdlib `String::contains`.)
- [ ] Extractor emitter boilerplate ×6 (~80% identical bodies). `scripts/extract_antlr4_descriptors.wado:659-998`, `:1841-1857`
- [ ] Architectural duplication: `codegen.wado` reads pre-classified GIR shapes, but `visitor_gen.wado` re-derives the same field names from the surface IR with its own group-counter logic — numbering drift produces non-compiling field names. `src/codegen.wado:347-369` vs `src/visitor_gen.wado`

## Test coverage gaps

- [ ] `codegen_test.wado` asserts only on generated-source substrings, so dispatch-shape bugs are invisible to it; coverage relies entirely on driver/descriptor layers.
- [ ] Triage parsing covers 3 of 4 axes: no `"oracle"` arm in `lookup_for_axis` (unknown axes silently map to stage_b), and zero tests for `[stage_b_oracle_skip]`/`[stage_b_oracle_todo]` parsing. `scripts/extract_antlr4_descriptors.wado:2051-2057`
- [ ] `gen_tokenize_fn` / `build_dispatch_groups` / keyword classifier (largest lexer_gen code) have no unit tests; deep-nullable FIRST through rule refs is a `gen_context_test` blind spot.
