# Gale Development Guide

Dev-cycle essentials for working on Gale, a Wado-native ANTLR4-compatible parser generator. Design and progress live in companion docs:

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract, prediction / codegen design, soundness invariants, descriptor pipeline, and triage.
- [`resilient-parser.md`](./resilient-parser.md) — error-resilient parsing and the flat CST.
- [`import.md`](./import.md) — grammar composition: how `import S;` resolves and what a delegate contributes.
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

Gale targets full compatibility with the ANTLR4 `.g4` syntax. The g4 parser must accept any well-formed grammar upstream `antlr4` accepts; a real-world `.g4` that ANTLR4 accepts but Gale rejects is a Gale bug. The one exception is `import Foo = Bar;`; claim (a) in [`antlr4-compatibility.md`](./antlr4-compatibility.md) carves it out.

- A bug in ANTLR4 itself is not reproduced: a test pins the corrected answer and names the upstream report. See "Bugs in ANTLR4 itself" in [`antlr4-compatibility.md`](./antlr4-compatibility.md).
- Compatibility is a capability contract, not byte-for-byte output. Parse trees, tokens, and semantics must match; incidental rendering differences that carry no structure may diverge (e.g. the `<EOF>` marker in `toStringTree()`).
- Gale is a superset: it may accept grammars ANTLR4 rejects only when the meaning is uniquely determined by Gale's language model — never an invented behavior. When accepting would require guessing, reject loudly. Each such grammar is listed, with the meaning Gale gives it, under "Grammars Gale accepts and ANTLR4 rejects" in [`antlr4-compatibility.md`](./antlr4-compatibility.md); a new one goes there too, confirmed against the jar.
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
- Keep the ATN off any shape the static path already lexes or predicts as the jar does: an ATN-class rule inlines about a thousand lines of runtime into the generated parser. A change to what routes to the ATN, or to a static prediction trigger, runs the shapes it newly includes and newly excludes through `scripts/antlr4-oracle.sh <grammar.g4> <start_rule> < input` (`--tokens` for the lexer) before it is reported. The script caches the jar; it needs only `java`.

## Debugging tools

`gale dump` prints a static per-rule prediction report — rule shape, first sets, the prediction tree, and ATN-class `Ambiguous(...)` decisions with the reason the static path halted. It reflects what the emitter sees, not the raw IR.

```sh
wado run package-gale dump path/to/Grammar.g4
```

`gale dump --lexer` is the same for the lexer: per rule, the matcher covering its text (own `try_`, the keyword classifier and its carrier, the shared literal matcher, the earlier rules that subsume it, a fragment's `frag_` matchers or its inlining, `latn_match`), then each emit decision inside it with the reason a cheaper strategy was not available — plain vs lookahead-aware repeat, first-match vs arm scoring, first-match vs maximal munch. A trailing summary tallies them, so "did my change flip a strategy" is a diff rather than a regenerate-and-grep loop.

```sh
wado run package-gale dump --lexer path/to/Grammar.g4
```

The route covering a rule (`lexer_rule_routes`) and the emit decisions below it
(`lexer_rule_plan`) are each decided once, and both the emitter and the dump read
them. A new strategy is a plan node with those two consumers, never a branch in
one of them.

The `trace` generator option logs a runtime event stream (enter / ok / FAIL per rule, per-alt scan lengths, the committed `pick`); its `alt#N` indices match `gale dump`. Strictly opt-in — off is byte-identical output.

It goes through `core:log` at `Trace` under the target `gale.trace`, so four gates stand between a decision and a line: the codegen switch, then `core:log`'s three tiers. `Trace` is below what `core:log` admits with no sink, so a trace build exports `trace_to_stderr(|| …)`, which installs one. Under `wado test` tier 1 is compiled down to `warn`, so add `-D log.level=trace` there as well.

```sh
gale gen --trace Grammar.g4
```

or `options: { trace: true }` in a Kiln `with { generator: ... }` block.

`tools/rust_corpus_check.wado` parses Rust files with the generated
`RustParser.g4` parser, one `ok` / `ng` line each with the first diagnostic's
rule stack, which names the failing rule where the message cannot. Every `.rs`
the repository tracks parses clean, so any `ng` is a regression.

```sh
git ls-files '*.rs' > target/rs-corpus.txt
wado run package-gale/tools/rust_corpus_check.wado -- --paths-from target/rs-corpus.txt
```

`tools/rust_inline_paths.wado` uses the same parser for `../AGENTS.md`'s rule on
inline `crate::` paths, driven by `../scripts/check-rust-paths.sh`.

## Running tests

```sh
wado test package-gale                         # the whole package
wado test package-gale/src/codegen_test.wado   # one file
```

`wado test package-gale` is the whole check for Gale work. A compiler change
needs no separate Gale run either: locally, `mise run test-wado` walks this
package, which catches miscompiles the e2e fixtures miss. `test-gale-o2` is
CI's.

Pass a directory, never a glob: the descriptor corpus sits one level deeper
(`tests/antlr4-compat/stage_*/<Category>/`), and a flat glob passes over most
of it.

A test that reaches `generate` compiles the whole generator, so such tests live
in `src/codegen_test.wado` (and the entry points' in `src/main_test.wado`),
paying that once.

A codegen test asserts what the generated parser does, not what its source says. `parses`, `lexes`, `printed` and `traced` run it through [`core:eval`](../docs/stdlib-core-eval.md) and return the trees, tokens, action output or trace lines; take the expected trees from the jar. Each run costs a few seconds of compile, cached across runs. Match the source text only for what running cannot show: that a feature left unused emits nothing, or which strategy the emitter picked.

Each corpus file carries up to `DESCRIPTORS_PER_FILE` descriptors, each importing its grammar as `t_<Name>`. Grouping is what bounds the corpus's compile time: every entry module is a whole-program `-O3` build, so the shared Gale runtime is compiled once per file rather than once per descriptor.

Test layers, all driven by `.g4` in `tests/grammars/` plus the descriptor corpus:

1. g4 parse tests (`src/g4/integration_test.wado`) — real `.g4` files parse into `Grammar` IR.
2. Driver tests (`tests/driver_*_test.wado`) — invoke the generator at compile time via `use ... with { generator: ... }`, parse input, and assert `to_string_tree()` output (EOF omitted; `normalize_tree()` from `tests/support/tree_compare.wado` lets you write indented expected trees).
3. ANTLR4 descriptor compatibility (`tests/antlr4-compat/`) — the extracted corpus as a long-lived regression suite; see [`antlr4-compatibility.md`](./antlr4-compatibility.md).

Real-world grammars can also be oracle-pinned (Stage B′ over the published jar, not hand-written trees): `scripts/regen-oracle.sh <key>` regenerates `tests/driver_cst_<key>_oracle_test.wado` from `tests/oracle/<key>/cases.*`, marking cases Gale currently parses differently `#[TODO]`. Java runs only at regen time; the committed trees keep CI Java-free. `sqlite` and `json` are pinned this way. Adding a grammar is config + cases, but only for a clean single combined `WS -> skip` grammar — split and whitespace-token grammars (Rust, TypeScript, css3) are out of scope; see "Stage B′ for real-world grammars" in [`antlr4-compatibility.md`](./antlr4-compatibility.md).

A `superClass` grammar has no behaviour without its hand-written base class, so `antlr4-oracle.sh` refuses to guess one. Pass `--super tests/grammars/java/<Base>.java`, once per base — a lexer and its sibling parser declare their own. Each is the Java twin of a Wado `impl` in the matching driver test, and keeping the pair in sync is what makes the comparison mean anything. `--probe-super` only reports what an input does against a synthesized base — it never yields pinnable output, for the reason in "Oracling a `superClass` grammar" in [`antlr4-compatibility.md`](./antlr4-compatibility.md). `scripts/antlr4-oracle-selftest.sh` pins both paths (needs java; run it after touching the oracle).

`\p{...}` takes its properties from [`core:icu`](../docs/stdlib-core-icu.md), which is their single source of truth: the jar is no longer consulted about them, and divergence that traces to ICU is accepted. What Gale can still get wrong is the layer above the data — the bare-name resolution order, the two `General_Category` aliases behind `cntrl` and `digit`, `EmojiPresentation=`, and the complement — and that is pinned character-by-character in `src/g4/parser_test.wado`.

To add an e2e grammar: drop the `.g4` in `tests/grammars/`, add a parse test in `src/g4/integration_test.wado`, and a driver test that imports it via the generator. Open the file with a comment saying which shape it pins and why that shape is hard.

A grammar taken from elsewhere also carries `// Source:` (its URL) and
`// License:`; one written here carries neither.

## Inlined runtime

The generated parser inlines the runtime fragments in `src/runtime/*.wado` (`lex`, `diag`, `tree`, `tools` always; `follow` / `scan_memo` / `highlight` / `atn` / `atn_predict` / `atn_lr` / `latn` gated per-feature). Each fragment is also a real module for dev / test. A standard-library `use` in one is hoisted to the top of the generated file. A type only the generator needs lives outside `src/runtime/`, as the `.g4` lexer's `Token` does.

Two rules follow from every byte of these files landing in every generated parser:

- **No comments.** They would be copied into hundreds of generated files, and the Kiln cache key is comment-blind (`is_wado_source` in `wado-cli/src/kiln_provider.rs` routes `.wado` through the canonical token stream), so editing one silently desynchronises the committed corpus from its generator. State intent through names, decomposition, and asserts.
- **Nothing test-only.** String-level comparison helpers live in `tests/support/tree_compare.wado`; only a helper taking a generated type (`to_lexer_string`, over the generated `TokenStream`) has to stay.

To force regeneration after editing a fragment, delete the invocation cache (`find tests/generated -name '*.kiln.json' -delete`) or pass `wado test --no-cache`. A plain `mise run test-wado` does not notice a comment-only edit.

## Failed approaches (do not repeat)

Prediction dead-ends — the static path always has edges (a decidability limit); the complete answer is the runtime ATN simulator (see `antlr4-compatibility.md`):

- RuleRef expansion via a return stack (2026-03): expanding multi-token RuleRefs during SLL prediction to cut backtracking. Tokens from inside an expanded sub-rule can't be used at the decision point without an ATN-grade depth mapping, and dedup-by-alt merges alts that share a sub-rule. Left as zero-overhead scaffolding.
- LL(\*) static variant emit (2026-05), three over-broad attempts at per-(rule, follow-mask) variants. Static analysis can't distinguish "tail-greedy that should yield to the caller" from "one that legitimately re-enters" — each over-broad guard silently broke a real grammar (`htmlContent`, CSS `selector`). Superseded by the runtime FOLLOW gate; pair any LL repair with a rejection-case fixture, not just a hit-case one.
- A caller-FOLLOW tier on the scan tournament (2026-09): among the alternatives that scan, prefer the longest whose end the caller's continuation can start at, and fall back to plain longest when none clears it. It was aimed at `if any_error { None }`, where `any_error { None }` is the longer expression and only the block the `if` still owes separates them. That is Rust's condition-excludes-struct-literal rule, which neither this grammar nor ANTLR4 encodes. The tier gets that class right and is wrong in general: an alternative's scan end is not the rule's end, so on an LR atom it reads a position the precedence loop has not finished with. `driver_cst_sqlite_oracle_test` catches it on `SELECT CASE WHEN a THEN CASE WHEN b THEN 1 END ELSE 2 END`, where it hands the dangling `ELSE` to the inner `CASE` and the jar gives it to the outer. The answer is full-context simulation rather than a tie-break: ANTLR4 simulates the continuation through the whole rule, which is the ATN.

Semantic-predicate dead-ends — a gate member is parser state, and the parser's
state is not part of what a rollback restores:

- A scope **stack** in a gate member (2026-09), for Rust's "no struct literal in
  a condition". The rule is about the innermost enclosing scope, which a member
  alone cannot name: a flag cleared on the way into a bracket never comes back,
  and a counter cannot tell a function body from a head inside one, because a
  block steps it too. One bit per scope in a single int says it exactly — a head
  pushes `* 2 + 1`, a bracket `* 2`, each pops `/ 2`, and the ban is
  `noStruct % 2`. It parses every hand-written probe and is far worse on real
  files. Error recovery and the repeat-exit probe roll back the position but not
  the parser's members, so a push whose pop never runs shifts the stack for the
  rest of the file. Restoring members across recovery and speculation is the
  precondition for any stack-shaped gate. A flag survives that rollback because
  it is idempotent, and the rule-level scope (`locals` + `@init` / `@after`)
  gives it the nesting a bracket would otherwise take away.

Lexer dead-ends:

- Choosing statically between the arms of a greedy lexer loop (2026-09), for `STR : '"' (~["] | ESC)* '"'`. Every static policy loses a case: first-match ends the token at an escaped quote, maximal munch takes the longest arm and strands the suffix in `('a' | 'ab')* 'b'`, and scoring the arms against the suffix needs a forward scan that repeats the same choice. The class is regular, so the lexer ATN runs the arms together in one pass and decides it (`scan_undecidable_arms`). Pair any static retry with `lexer_loop_arm_longest.g4`, which holds both the hit case and the rejection case.

Performance dead-ends (e.g. data-driven scan) live in [`perf.md`](./perf.md).
