# Gale Development Guide

Dev-cycle essentials for working on Gale, a Wado-native ANTLR4-compatible parser generator. Design and progress live in companion docs:

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract, prediction / codegen design, soundness invariants, descriptor pipeline, and triage.
- [`resilient-parser.md`](./resilient-parser.md) — error-resilient parsing and the flat CST.
- [`perf.md`](./perf.md) — performance notes: budget, levers, and measured perf dead-ends.
- [`TODO.md`](./TODO.md) — open work.
- [`README.md`](./README.md) and [WEP: Gale](../docs/wep-2026-03-02-gale.md) — overall design context.

(Each `wado` command below is `cargo run --bin wado`.)

## First: initialize the ANTLR4 submodule

The descriptor corpus, `.g4` semantics, and test regeneration all depend on it:

```sh
git submodule update --init --recommend-shallow vendor/antlr4
```

Files headed `// Do not edit by hand` are generated. To change one, edit its source (e.g. `status.toml`) and regenerate via `scripts/extract-antlr4-descriptors.sh` (needs the submodule).

## Compatibility principle

Gale targets full compatibility with the ANTLR4 `.g4` syntax. The g4 parser must accept any well-formed grammar upstream `antlr4` accepts; a real-world `.g4` that ANTLR4 accepts but Gale rejects is a Gale bug.

- Compatibility is a capability contract, not byte-for-byte output. Parse trees, tokens, and semantics must match; incidental rendering differences that carry no structure may diverge (e.g. the `<EOF>` marker in `toStringTree()`).
- Gale is a superset: it may accept grammars ANTLR4 rejects only when the meaning is uniquely determined by Gale's language model — never an invented behavior. When accepting would require guessing, reject loudly. Examples: `.`/`~X`-led left-recursive suffixes, and a lexer `mode` inside a combined `grammar` (ANTLR4 allows modes only in a `lexer grammar`; a combined grammar already bundles a lexer, so it desugars unambiguously — still rejected in a `parser grammar`).
- TDD every g4 change with a unit test in `src/g4/{lexer,parser}_test.wado`. If an existing test encodes a wrong expectation, fix the test — the spec wins; confirm against the published jar as a black-box oracle.

Full contract, stages, and the EOF rationale: [`antlr4-compatibility.md`](./antlr4-compatibility.md).

## License hygiene — reading `vendor/antlr4/`

ANTLR4 is BSD-3; copying or paraphrasing its implementation risks making Gale a derivative work. So:

- Do NOT read ANTLR4 implementation source: `vendor/antlr4/tool/**/*.{java,g}` and `vendor/antlr4/runtime/**/*.java` (e.g. `ParserATNSimulator.java`, `LL1Analyzer.java`, the bootstrap `.g` grammars). Algorithmic ideas inferred from that source belong to ANTLR4, not Gale.
- OK to read: `.g4` files anywhere under `vendor/antlr4/`; `runtime-testsuite/**/*.txt` descriptors; `vendor/antlr4/doc/*.md` (spec-like prose — the canonical `.g4` semantics reference; a curated index is in `antlr4-compatibility.md`).
- OK to run: the published `antlr-4.13.2-complete.jar` as a black-box oracle (clean-room measurement).

The first rule is enforced: `permissions.deny` covers the Read tool, `.claude/hooks/antlr4-license-guard.sh` covers Bash.

## Standing codegen rules

- No backtracking on the accept path — parser or lexer. Disambiguate with static k-token lookahead; a decision static prediction cannot resolve in depth 5 routes to the runtime ATN simulator, never a try-fail-retry loop. The one exception decides nothing: the repeat-exit probe re-parses a failed element under `speculating` to record where the error is, and rolls back all but the message. Mechanics, soundness invariants, and ATN escalation: [`antlr4-compatibility.md`](./antlr4-compatibility.md) (Prediction & codegen design).
- Keep generated code byte-identical for grammars that do not use a feature (actions, FOLLOW gates, ATN) — gate every emit site on the feature.
- A compiler bug is P0 (top-level `CLAUDE.md`): write a minimal `wado-compiler/tests/fixtures/` repro first, then fix.

## Debugging tools

`gale dump` prints a static per-rule prediction report — rule shape, first sets, the prediction tree, and ATN-class `Ambiguous(...)` decisions with the reason the static path halted. It reflects what the emitter sees, not the raw IR.

```sh
wado run package-gale dump path/to/Grammar.g4
```

`gale dump --lexer` is the same for the lexer: per rule, the matcher covering its text (own `try_`, the keyword classifier and its carrier, the shared literal matcher, an inlined fragment, `latn_match`), then each emit decision inside it with the reason a cheaper strategy was not available — plain vs lookahead-aware repeat, first-match vs arm scoring, first-match vs maximal munch. A trailing summary tallies them, so "did my change flip a strategy" is a diff rather than a regenerate-and-grep loop.

```sh
wado run package-gale dump --lexer path/to/Grammar.g4
```

Which matcher covers a rule is `lexer_rule_route`, and the emit reads it rather than re-deciding: both "does this rule get its own `try_`" and "does the dispatch call it" are derived from that one answer, so a shortcut added to the route is one every emit site already knows about. Asking the shortcuts separately is what let a rule keep its actions past the keyword classifier and still lose them to the shared literal matcher.

The emit _decisions_ below the route — plain vs lookahead-aware repeat, first-match vs arm scoring, maximal munch, suffix cutting, fragment inlining — are `lexer_rule_plan`: one tree per rule that `gen_lexer` emits from and the dump renders. Neither decides for itself, so neither can reach a construct the other does not, and tail position is a property of the plan rather than a parameter each function re-derives. A new strategy is a new plan node with two consumers; adding a branch to only one does not compile.

The plan never holds a second copy of the same elements. A scored alternation only peeks what follows it, so that suffix stays a step of the enclosing sequence and `gen_lexer_alt_seq` re-emits those steps from a `from` index; planning it apart would let the peek and the commit choose differently. Only a non-greedy repeat's exit try is cut out, since it alone lowers what follows outside the sequence's tail position.

`lexer_dump_test.wado` counts the strategies the dump reports against the locals the emitter mints for them (`alts_best_`, `la_win_`, `accept_`, `ng_saved_`), over shapes that force each one. The grammars are action-free on purpose: an action-carrying rule emits its body twice.

For a grammar outside the repo, `wado run --dir <dir> package-gale dump Grammar.g4` — see `--dir` in the root [`AGENTS.md`](../AGENTS.md).

The `trace` generator option logs a runtime event stream to stderr (enter / ok / FAIL per rule, per-alt scan lengths, the committed `pick`); its `alt#N` indices match `gale dump`. Strictly opt-in — off is byte-identical output.

```sh
gale gen --trace Grammar.g4
```

or `options: { trace: true }` in a Kiln `with { generator: ... }` block.

## Running tests

```sh
wado test package-gale                         # the whole package
wado test package-gale/src/codegen_test.wado   # one file
```

Pass the package directory and let the CLI discover the files. A hand-written glob is the thing that goes wrong: the descriptor corpus sits one directory deeper (`tests/antlr4-compat/stage_{a,b,b_oracle,c}/<Category>/`), so a flat `tests/antlr4-compat/*.wado` reaches about a third of the suite, passes, and says nothing about the rest — including the corpus that exists to catch compatibility regressions. The fixtures it never reaches also keep whatever the generator emitted the last time something did run them, so the committed corpus drifts behind the generator with every green run.

Test layers, all driven by `.g4` in `tests/grammars/` plus the descriptor corpus:

1. g4 parse tests (`src/g4/integration_test.wado`) — real `.g4` files parse into `Grammar` IR.
2. Driver tests (`tests/driver_*_test.wado`) — invoke the generator at compile time via `use ... with { generator: ... }`, parse input, and assert `to_string_tree()` output (EOF omitted; `normalize_tree()` from `tests/support/tree_compare.wado` lets you write indented expected trees).
3. ANTLR4 descriptor compatibility (`tests/antlr4-compat/`) — the extracted corpus as a long-lived regression suite; see [`antlr4-compatibility.md`](./antlr4-compatibility.md).

Real-world grammars can also be oracle-pinned (Stage B′ over the published jar, not hand-written trees): `scripts/regen-oracle.sh <key>` regenerates `tests/driver_cst_<key>_oracle_test.wado` from `tests/oracle/<key>/cases.*`, marking cases Gale currently parses differently `#[TODO]`. Java runs only at regen time; the committed trees keep CI Java-free. `sqlite` and `json` are pinned this way. Adding a grammar is config + cases, but only for a clean single combined `WS -> skip` grammar — split and whitespace-token grammars (Rust, TypeScript, css3) are out of scope; see "Stage B′ for real-world grammars" in [`antlr4-compatibility.md`](./antlr4-compatibility.md).

A `superClass` grammar has no behaviour without its hand-written base class, so `antlr4-oracle.sh` refuses to guess one. Pass `--super tests/grammars/java/<Base>.java`, once per base — a lexer and its sibling parser declare their own. Each is the Java twin of a Wado `impl` in the matching driver test, and keeping the pair in sync is what makes the comparison mean anything. `--probe-super` only reports what an input does against a synthesized base — it never yields pinnable output, for the reason in "Oracling a `superClass` grammar" in [`antlr4-compatibility.md`](./antlr4-compatibility.md). `scripts/antlr4-oracle-selftest.sh` pins both paths (needs java; run it after touching the oracle).

The `\p{...}` tables get an exact whole-space diff instead of pinned samples: `scripts/check-unicode-properties.sh` compares every property against the jar. The jar's Unicode snapshot is frozen at its build (4.13.2 is 15.0.0), so regenerate to match first:

```sh
scripts/regen-unicode-tables.sh 15.0.0   # match the jar
scripts/check-unicode-properties.sh
scripts/regen-unicode-tables.sh          # back to latest
```

To add an e2e grammar: drop the `.g4` in `tests/grammars/` (with `// Source:` / `// License:` headers), add a parse test in `src/g4/integration_test.wado`, and a driver test that imports it via the generator.

## Inlined runtime

The generated parser inlines the runtime fragments in `src/runtime/*.wado` (`lex`, `diag`, `tree`, `tools` always; `follow` / `highlight` / `atn` / `latn` gated per-feature). Each fragment is also a real module for dev / test.

Two rules follow from every byte of these files landing in every generated parser:

- **No comments.** They would be copied into hundreds of generated files, and the Kiln cache key is comment-blind (`is_wado_source` in `wado-cli/src/kiln_provider.rs` routes `.wado` through the canonical token stream), so editing one silently desynchronises the committed corpus from its generator. State intent through names, decomposition, and asserts.
- **Nothing test-only.** String-level comparison helpers live in `tests/support/tree_compare.wado`; only a helper taking a generated type (`to_lexer_string`, over the generated `TokenStream`) has to stay.

To force regeneration after editing a fragment, delete the invocation cache (`find tests/generated -name '*.kiln.json' -delete`) or pass `wado test --no-cache`. A plain `mise run test-wado` does not notice a comment-only edit.

## Failed approaches (do not repeat)

Prediction dead-ends — the static path always has edges (a decidability limit); the complete answer is the runtime ATN simulator (see `antlr4-compatibility.md`):

- RuleRef expansion via a return stack (2026-03): expanding multi-token RuleRefs during SLL prediction to cut backtracking. Tokens from inside an expanded sub-rule can't be used at the decision point without an ATN-grade depth mapping, and dedup-by-alt merges alts that share a sub-rule. Left as zero-overhead scaffolding.
- LL(\*) static variant emit (2026-05), three over-broad attempts at per-(rule, follow-mask) variants. Static analysis can't distinguish "tail-greedy that should yield to the caller" from "one that legitimately re-enters" — each over-broad guard silently broke a real grammar (`htmlContent`, CSS `selector`). Superseded by the runtime FOLLOW gate; pair any LL repair with a rejection-case fixture, not just a hit-case one.

Performance dead-ends (e.g. data-driven scan) live in [`perf.md`](./perf.md).
