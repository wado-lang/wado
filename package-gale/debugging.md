# Debugging Gale

Each `wado` command below is `cargo run --bin wado --`. For a grammar outside
the repository, grant its directory: `wado run --dir <dir> package-gale dump Grammar.g4`.

## `gale dump`

`gale dump` prints a static per-rule prediction report — rule shape, first sets, the prediction tree, and ATN-class `Ambiguous(...)` decisions with the reason the static path halted. It reflects what the emitter sees, not the raw IR.

```sh
wado run package-gale dump path/to/Grammar.g4
```

## `gale dump --lexer`

`gale dump --lexer` is the same for the lexer: per rule, the matcher covering its text (own `try_`, the keyword classifier and its carrier, the shared literal matcher, the earlier rules that subsume it, a fragment's `frag_` matchers or its inlining, `latn_match`), then each emit decision inside it with the reason a cheaper strategy was not available — plain vs lookahead-aware repeat, first-match vs arm scoring, first-match vs maximal munch. A trailing summary tallies them, so "did my change flip a strategy" is a diff rather than a regenerate-and-grep loop.

```sh
wado run package-gale dump --lexer path/to/Grammar.g4
```

Which matcher covers a rule is its `lexer_rule_routes` entry, derived once per grammar, and the emit reads it rather than re-deciding: both "does this rule get its own `try_`" and "does the dispatch call it" are derived from that one answer, so a shortcut added to the route is one every emit site already knows about. Asking the shortcuts separately is what let a rule keep its actions past the keyword classifier and still lose them to the shared literal matcher.

The emit decisions below the route — plain vs lookahead-aware repeat, first-match vs arm scoring, maximal munch, suffix cutting, fragment calls — are `lexer_rule_plan`: one tree per rule that `gen_lexer` emits from and the dump renders. Neither decides for itself, so neither can reach a construct the other does not, and tail position is a property of the plan rather than a parameter each function re-derives. A new strategy is a new plan node with two consumers; adding a branch to only one does not compile.

The plan never holds a second copy of the same elements. A scored alternation only peeks what follows it, so that suffix stays a step of the enclosing sequence and `gen_lexer_alt_seq` re-emits those steps from a `from` index; planning it apart would let the peek and the commit choose differently. Only a non-greedy repeat's exit try is cut out, since it alone lowers what follows outside the sequence's tail position.

The test "every strategy the dump reports is one the emitter emitted" in `codegen_test.wado` counts the strategies the dump reports against the locals the emitter mints for them (`alts_best_`, `la_win_`, `la_end_`, `ng_saved_`), over shapes that force each one. The grammars are action-free on purpose: an action-carrying rule emits its body twice.

## The `trace` Option

The `trace` generator option logs a runtime event stream (enter / ok / FAIL per rule, per-alt scan lengths, the committed `pick`); its `alt#N` indices match `gale dump`. Strictly opt-in — off is byte-identical output.

It goes through `core:log` at `Trace` under the target `gale.trace`, so four gates stand between a decision and a line: the codegen switch, then `core:log`'s three tiers. `Trace` is below what `core:log` admits with no sink, so a trace build exports `trace_to_stderr(|| …)`, which installs one. Under `wado test` tier 1 is compiled down to `warn`, so add `-D log.level=trace` there as well.

```sh
gale gen --trace Grammar.g4
```

or `options: { trace: true }` in a Kiln `with { generator: ... }` block.

## The Rust Corpus Check

`tools/rust_corpus_check.wado` parses a list of Rust files with the generated `RustParser.g4` parser and prints one `ok` / `ng` line each — path, diagnostic count, line, message, and the first diagnostic's rule stack — plus a summary on stderr. The stack is the part that carries information: recovery restarts at an item boundary, so most failures report the same `expected 'extern'` whatever went wrong inside, and only the stack names the rule. Repository-wide it is the one number that says whether a prediction change helped.

```sh
git ls-files '*.rs' > target/rs-corpus.txt
wado run package-gale/tools/rust_corpus_check.wado -- --paths-from target/rs-corpus.txt
```

`--paths-from` reads one path per line, the only form that survives a path with a space.

Every `.rs` this repository tracks parses clean, so any `ng` line is a regression. Each failing file reports exactly one diagnostic — it dies once and recovery carries the rest — so the count is files, not errors.

`tools/rust_inline_paths.wado`, which `mise run check-rust-paths` runs, is the parser's second consumer. It needs the same clean corpus, since a file it cannot parse is reported rather than counted. Both tools share `tools/rust_corpus.wado`, which holds the recognizer with its base ports installed.
