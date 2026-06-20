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

## Test coverage gaps

- [ ] `codegen_test.wado` asserts only on generated-source substrings, so dispatch-shape bugs are invisible to it; coverage relies entirely on driver/descriptor layers.
- [ ] Triage parsing covers 3 of 4 axes: no `"oracle"` arm in `lookup_for_axis` (unknown axes silently map to stage_b), and zero tests for `[stage_b_oracle_skip]`/`[stage_b_oracle_todo]` parsing. `scripts/extract_antlr4_descriptors.wado:2051-2057`
- [ ] `gen_tokenize_fn` / `build_dispatch_groups` / keyword classifier (largest lexer_gen code) have no unit tests; deep-nullable FIRST through rule refs is a `gen_context_test` blind spot.
