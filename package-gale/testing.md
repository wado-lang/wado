# Testing Gale

The rules for writing a test are in [`AGENTS.md`](./AGENTS.md#tests).

## Test Layers

All are driven by `.g4` in `tests/grammars/` plus the descriptor corpus:

1. g4 parse tests (`src/g4/integration_test.wado`) — real `.g4` files parse into `Grammar` IR.
2. Driver tests (`tests/driver_*_test.wado`) — invoke the generator at compile time via `use ... with { generator: ... }`, parse input, and assert `to_string_tree()` output (EOF omitted; `normalize_tree()` from `tests/support/tree_compare.wado` lets you write indented expected trees).
3. ANTLR4 descriptor compatibility (`tests/antlr4-compat/`) — the extracted corpus as a long-lived regression suite; see [`antlr4-compatibility.md`](./antlr4-compatibility.md).

A codegen test runs the generated parser through `parses`, `lexes`, `printed`
or `traced`, which return the trees, tokens, action output or trace lines. Each
run costs a few seconds of compile, cached across runs.

## The `verify` Drivers

The css3 and SQLite drivers, and the tools sharing their parsers, generate with `atn: "verify"`. Their trees are then `never`'s, and each static pick `auto` or `always` would hand the simulator is checked against it. A run pins that `never` builds the simulator's tree on these grammars, except where the README's "Choosing the simulator" says what `verify` leaves unchecked. A grammar that fails it goes back to `"auto"` with the failing case recorded in `TODO.md`, as Rust and TypeScript have.

## Oracle-Pinned Grammars

Real-world grammars can also be oracle-pinned (Stage B′ over the published jar, not hand-written trees): `mise run oracle-gale-regen <key>` (`scripts/regen_oracle.wado`) regenerates `tests/driver_cst_<key>_oracle_test.wado` from `tests/oracle/<key>/cases.*`, marking cases Gale currently parses differently `#[TODO]`. Java runs only at regen time; the committed trees keep CI Java-free. `sqlite` and `json` are pinned this way. What adding a grammar takes, and which grammars fit, is in "Stage B′ for real-world grammars" in [`antlr4-compatibility.md`](./antlr4-compatibility.md).

A `superClass` grammar has no behaviour without its hand-written base class, so `antlr4_oracle.wado` refuses to guess one. Pass `--super tests/grammars/java/<Base>.java`, once per base — a lexer and its sibling parser declare their own. Each is the Java twin of a Wado `impl` in the matching driver test, and keeping the pair in sync is what makes the comparison mean anything. `--probe-super` only reports what an input does against a synthesized base — it never yields pinnable output, for the reason in "Oracling a `superClass` grammar" in [`antlr4-compatibility.md`](./antlr4-compatibility.md). `mise run oracle-gale-selftest` pins both paths (run it after touching the oracle).

A split grammar takes its lexer through `--lexer <N>Lexer.g4` beside `<N>Parser.g4`. `mise run oracle-gale-typescript [file.ts]` lists where Gale's tree for a whole TypeScript file and the jar's disagree; see "Comparing a whole TypeScript file" in [`antlr4-compatibility.md`](./antlr4-compatibility.md).
