# Gale Development Guide

Dev-cycle essentials for working on Gale, a Wado-native ANTLR4-compatible parser generator. Design and progress live in companion docs:

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract, prediction / codegen design, soundness invariants, descriptor pipeline, and triage.
- [`action.md`](./action.md) — action / predicate execution design and progress, plus the java2wado (Java → Wado) design.
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
- Gale is a superset: it may accept grammars ANTLR4 rejects only when the meaning is uniquely determined by Gale's language model — never an invented behavior. When accepting would require guessing, reject loudly.
- The one skip exception is action bodies: `{...}` actions, `{...}?` predicates, `catch`/`finally`, `@init`/`@after`. The parser recognizes them and keeps their presence and position in the IR; only the host-language code inside the braces is discarded (until Stage C runs it — see `action.md`).
- TDD every g4 change with a unit test in `src/g4/{lexer,parser}_test.wado`. If an existing test encodes a wrong expectation, fix the test — the spec wins; confirm against the published jar as a black-box oracle.

Full contract, stages, and the EOF rationale: [`antlr4-compatibility.md`](./antlr4-compatibility.md).

## License hygiene — reading `vendor/antlr4/`

ANTLR4 is BSD-3; copying or paraphrasing its implementation risks making Gale a derivative work. So:

- Do NOT read ANTLR4 implementation source: `vendor/antlr4/tool/**/*.{java,g}` and `vendor/antlr4/runtime/**/*.java` (e.g. `ParserATNSimulator.java`, `LL1Analyzer.java`, the bootstrap `.g` grammars). Algorithmic ideas inferred from that source belong to ANTLR4, not Gale.
- OK to read: `.g4` files anywhere under `vendor/antlr4/`; `runtime-testsuite/**/*.txt` descriptors; `vendor/antlr4/doc/*.md` (spec-like prose — the canonical `.g4` semantics reference; a curated index is in `antlr4-compatibility.md`).
- OK to run: the published `antlr-4.13.2-complete.jar` as a black-box oracle (clean-room measurement).

## Standing codegen rules

- No backtracking, ever — parser or lexer. Disambiguate with static k-token lookahead; a decision static prediction cannot resolve in depth 5 routes to the runtime ATN simulator, never a try-fail-retry loop. Mechanics, soundness invariants, and ATN escalation: [`antlr4-compatibility.md`](./antlr4-compatibility.md) (Prediction & codegen design).
- Keep generated code byte-identical for grammars that do not use a feature (actions, FOLLOW gates, ATN) — gate every emit site on the feature.
- A compiler bug is P0 (top-level `CLAUDE.md`): write a minimal `wado-compiler/tests/fixtures/` repro first, then fix.

## Debugging tools

`gale dump` prints a static per-rule prediction report — rule shape, first sets, the prediction tree, and ATN-class `Ambiguous(...)` decisions with the reason the static path halted. It reflects what the emitter sees, not the raw IR.

```sh
wado run package-gale dump path/to/Grammar.g4
```

The `trace` generator option logs a runtime event stream to stderr (enter / ok / FAIL per rule, per-alt scan lengths, the committed `pick`); its `alt#N` indices match `gale dump`. Strictly opt-in — off is byte-identical output.

```sh
gale gen --trace Grammar.g4
```

or `options: { trace: true }` in a Kiln `with { generator: ... }` block.

## Running tests

```sh
wado test package-gale/**/*.wado               # all package tests
wado test package-gale/src/codegen_test.wado   # one file
```

Three test layers, all driven by `.g4` in `tests/grammars/` plus the descriptor corpus:

1. g4 parse tests (`src/g4/integration_test.wado`) — real `.g4` files parse into `Grammar` IR.
2. Driver tests (`tests/driver_*_test.wado`) — invoke the generator at compile time via `use ... with { generator: ... }`, parse input, and assert `to_string_tree()` output (EOF omitted; `normalize_tree()` lets you write indented expected trees).
3. ANTLR4 descriptor compatibility (`tests/antlr4-compat/`) — the extracted corpus as a long-lived regression suite; see [`antlr4-compatibility.md`](./antlr4-compatibility.md).

To add an e2e grammar: drop the `.g4` in `tests/grammars/` (with `// Source:` / `// License:` headers), add a parse test in `src/g4/integration_test.wado`, and a driver test that imports it via the generator.

## Inlined runtime

The generated parser inlines the runtime fragments in `src/runtime/*.wado` (`lex`, `diag`, `tree`, `tools` always; `follow` / `highlight` / `atn` / `latn` gated per-feature), assembled by `gen_runtime` in `codegen.wado`. Each fragment is also a real module for dev / test.

## Failed approaches (do not repeat)

Prediction dead-ends — the static path always has edges (a decidability limit); the complete answer is the runtime ATN simulator (see `antlr4-compatibility.md`):

- RuleRef expansion via a return stack (2026-03): expanding multi-token RuleRefs during SLL prediction to cut backtracking. Tokens from inside an expanded sub-rule can't be used at the decision point without an ATN-grade depth mapping, and dedup-by-alt merges alts that share a sub-rule. Left as zero-overhead scaffolding on `SllConfig`.
- LL(\*) static variant emit (2026-05), three over-broad attempts at per-(rule, follow-mask) variants. Static analysis can't distinguish "tail-greedy that should yield to the caller" from "one that legitimately re-enters" — each over-broad guard silently broke a real grammar (`htmlContent`, CSS `selector`). Superseded by the runtime FOLLOW gate; pair any LL repair with a rejection-case fixture, not just a hit-case one.

Performance dead-ends (e.g. data-driven scan) live in [`perf.md`](./perf.md).
