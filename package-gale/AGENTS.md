# Gale Development Guide

## Overview

Gale is a Wado-native parser generator. See [README.md](./README.md) and [WEP: Gale](../docs/wep-2026-03-02-gale.md) for design context.

## Compatibility Principle

Gale aims for full compatibility with the ANTLR4 `.g4` grammar syntax. The g4 parser must accept any well-formed grammar that the upstream `antlr4` tool accepts. Treat this as a hard contract: if you find a real-world `.g4` file that ANTLR4 accepts but Gale rejects, that is a bug in Gale.

The single intentional exception is action bodies, whose contents are skipped:

- `{ ... }` action blocks (rule-level, element-level, named actions like `@header`/`@members`/`@parser::name`)
- `{ ... }?` semantic predicates
- `catch [ ... ] { ... }` and `finally { ... }` exception handlers
- `@init { ... }` / `@after { ... }` rule prequel actions

The parser must still recognize these constructs (so files containing them parse without error) and preserve their presence and position in the surrounding IR — only the host-language code inside the braces is discarded. Everything else is first-class.

When fixing or extending the g4 frontend:

- Cross-check the canonical semantics against `vendor/antlr4/tool/src/org/antlr/v4/parse/ANTLRParser.g` and the curated doc index below.
- Drive every change with a unit test in `src/g4/{lexer,parser}_test.wado` (TDD: failing test first, then implementation).
- If an existing test encodes a wrong expectation that diverges from ANTLR4, fix the test — the spec wins.

## ANTLR4 Reference

The upstream ANTLR4 source — including its full documentation and the `LICENSE.txt` (BSD 3-Clause) — is vendored as a shallow git submodule at `vendor/antlr4/`. Read it directly when you need the canonical semantics of a `.g4` construct; do not duplicate the docs into this repo.

Initialize the submodule (first time only):

```sh
git submodule update --init --recommend-shallow vendor/antlr4
```

To bump the pinned revision later:

```sh
git -C vendor/antlr4 fetch --depth 1 origin dev
git -C vendor/antlr4 checkout FETCH_HEAD
git add vendor/antlr4
```

### Curated doc index for Gale development

These are the upstream pages that matter most when working on the g4 parser, the lexer/parser code generator, or the runtime. Read them in roughly this order when ramping up.

| File                                                                                                | Why it matters for Gale                                                                                |
| --------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| [`vendor/antlr4/doc/grammars.md`](../vendor/antlr4/doc/grammars.md)                                 | Top-level grammar structure: combined vs. `lexer grammar` / `parser grammar`, `tokens {}`, `import`.   |
| [`vendor/antlr4/doc/lexer-rules.md`](../vendor/antlr4/doc/lexer-rules.md)                           | Lexer rule semantics: fragments, modes, channels, lexer commands (`skip`, `more`, `pushMode`, `type`). |
| [`vendor/antlr4/doc/parser-rules.md`](../vendor/antlr4/doc/parser-rules.md)                         | Parser rule semantics: alternatives, EBNF operators, labels, rule arguments and return values.         |
| [`vendor/antlr4/doc/left-recursion.md`](../vendor/antlr4/doc/left-recursion.md)                     | How ANTLR4 rewrites direct left recursion. Essential context for any parser-generator design choice.   |
| [`vendor/antlr4/doc/wildcard.md`](../vendor/antlr4/doc/wildcard.md)                                 | Semantics of `.` and non-greedy operators — easy to get wrong in code generation.                      |
| [`vendor/antlr4/doc/options.md`](../vendor/antlr4/doc/options.md)                                   | Grammar / rule / element options the g4 parser must accept (e.g. `caseInsensitive`, `assoc`).          |
| [`vendor/antlr4/doc/lexicon.md`](../vendor/antlr4/doc/lexicon.md)                                   | Lexical structure of `.g4` source itself: identifiers, literals, comments, escapes.                    |
| [`vendor/antlr4/doc/actions.md`](../vendor/antlr4/doc/actions.md)                                   | Action / attribute syntax. Gale skips these, but the parser must recognize and warn on them.           |
| [`vendor/antlr4/doc/predicates.md`](../vendor/antlr4/doc/predicates.md)                             | Semantic predicate syntax. Same story as actions: must be recognized and skipped.                      |
| [`vendor/antlr4/doc/target-agnostic-grammars.md`](../vendor/antlr4/doc/target-agnostic-grammars.md) | Best practices for writing host-language-free grammars — exactly the subset Gale targets.              |

For everything else, browse `vendor/antlr4/doc/` directly.

## Debugging Grammars with `gale dump`

`gale dump` pretty-prints the parsed `Grammar` IR so you can inspect what the
g4 frontend actually produced, without going through code generation. Use it
to check whether a construct was parsed into the IR as expected before
blaming the code generator.

(note: each `wado` command is actually `cargo run --bin wado`)

```sh
# Dump the full IR (multiple files are merged, same as `gale gen`).
wado run package-gale -- dump path/to/Grammar.g4

# Dump a single rule — searches parser rules first, then lexer rules.
wado run package-gale -- dump --rule expr path/to/Grammar.g4
```

## Running Tests

```sh
# Run all Wado tests for this package
wado test package-gale/**/*.wado

# Run a specific file
wado test package-gale/src/codegen_test.wado
```

## E2E Test Architecture

Gale has two layers of e2e testing, both driven by `.g4` files in `tests/grammars/`.

### Driver Tests: S-expression Tree Assertions

Driver tests verify generated parsers by parsing real input and checking the CST structure. Each test invokes the generator at compile time via `use ... with { generator: ... }` (Kiln inline invocation), then parses real input and uses `to_string_tree()` for ANTLR4-style S-expression output and `normalize_tree()` to write readable multi-line expected values:

```wado
use json from "./grammars/JSON.g4"
    with {
        generator: {
            module: "../src/generator.wado",
            options: { highlight: false },
        },
    };
use { normalize_tree } from "./grammars/JSON.g4";

fn assert_tree(input: &String, expected: &String) {
    let root = json::parse(input).unwrap();
    let tree = json::to_tree(&root);
    let actual = tree.to_string_tree();
    let norm = normalize_tree(expected);
    assert actual == norm, `\ninput:    {*input}\nexpected: {norm}\nactual:   {actual}`;
}

test "tree: nested object with array" {
    assert_tree(&"{\"a\":[1,true,null]}", &"
        (json
          (value
            (obj
              { (pair \"a\"
                  : (value
                      (arr [ (value 1) , (value true) , (value null) ])))
              })))
    ");
}
```

- `to_string_tree()` outputs `(ruleName child1 child2 ...)` with tokens as their text. EOF is omitted.
- `normalize_tree()` collapses whitespace (preserving quoted strings) so multi-line indented expected values compare correctly with compact single-line output.
- Both functions are defined in `runtime.wado` and available in all generated parsers.

### Layer 1: G4 Parse Tests (`g4/integration_test.wado`)

Verify that the g4 parser can parse real-world `.g4` files into `Grammar` IR without errors. Each test uses `#include_str` to load the `.g4` file and calls `parse()`.

```wado
test "parse JSON.g4" {
    let input = #include_str("../../tests/grammars/JSON.g4");
    let g = parse(input).unwrap();
    assert g.name == "JSON";
    assert g.parser_rules.len() == 5;
}
```

### Test Grammars (`tests/grammars/`)

| File                  | Language     | Notes                                                        |
| --------------------- | ------------ | ------------------------------------------------------------ |
| `JSON.g4`             | JSON         | Combined grammar. Clean (no actions).                        |
| `sexpression.g4`      | S-expression | Combined grammar. Clean.                                     |
| `calculator.g4`       | Calculator   | Combined grammar. Clean.                                     |
| `SQLite.g4`           | SQLite       | Combined grammar. Large, clean.                              |
| `css3Lexer.g4`        | CSS3         | Split lexer. Clean.                                          |
| `css3Parser.g4`       | CSS3         | Split parser. Clean.                                         |
| `HTMLLexer.g4`        | HTML         | Split lexer. Clean.                                          |
| `HTMLParser.g4`       | HTML         | Split parser. Clean.                                         |
| `ANTLRv4Lexer.g4`     | ANTLR4       | Split lexer. Has action blocks and `superClass`.             |
| `ANTLRv4Parser.g4`    | ANTLR4       | Split parser. Clean.                                         |
| `RustLexer.g4`        | Rust         | Split lexer. Has semantic predicates and `superClass`.       |
| `RustParser.g4`       | Rust         | Split parser. Has semantic predicates and `superClass`.      |
| `TypeScriptLexer.g4`  | TypeScript   | Split lexer. Has semantic predicates and `superClass`.       |
| `TypeScriptParser.g4` | TypeScript   | Split parser. Has many semantic predicates and `superClass`. |

Clean grammars (JSON, sexpression, calculator, SQLite, CSS3, HTML) contain no target-language-dependent elements and should be fully parseable and code-generatable.

Grammars with actions/predicates (ANTLR4, Rust, TypeScript) contain `{...}` action blocks and/or `{...}?` semantic predicates. These must be warned and skipped during parsing. They serve as e2e tests for Gale's ability to consume real-world grammars without manual cleanup.

### Adding a New E2E Test Grammar

1. Add the `.g4` file to `tests/grammars/` (include `// Source:` and `// License:` headers)
2. Add a parse test in `g4/integration_test.wado`
3. Add a driver test under `tests/` that imports the grammar via `use ... with { generator: { module: "../src/generator.wado", options: { ... } } }`. The compiler runs Gale on the `.g4` at build time and resolves the `use` against the freshly generated parser.

### Layer 3: ANTLR4 Descriptor Compatibility Tests (`tests/antlr4-compat/`)

A separate, long-lived effort that imports ANTLR4's upstream
runtime-testsuite descriptors as parse-only Wado tests. Tracked in
[`antlr4-compatibility.md`](./antlr4-compatibility.md) — read that
for the stages, the descriptor pipeline, the regeneration commands,
and the triage workflow. The doc remains the single source of truth
even after the contract is fully met; the entry here is just a
pointer.

## Inlined Runtime

`runtime.wado` is included verbatim into every generated file via `#include_str` in `codegen.wado`. It must remain self-contained (no imports from other source files). See [WEP: Compile-Time File Inclusion](../docs/wep-2026-03-02-include-str.md).

## Generated Parser Rules

- **No backtracking in new code.** Use static k-token lookahead prediction to disambiguate alternatives. If prediction cannot resolve within depth 5, file an issue rather than adding backtracking. Existing backtracking sites are being migrated to prediction; do not add new ones.

## Failed Approaches (Do Not Repeat)

### RuleRef Expansion via Return Stack (2026-03)

**Goal:** Expand multi-token RuleRefs during SLL prediction to reduce backtracking.

**What was tried:** Added `return_stack` to `SllConfig` to track continuation points when entering a referenced rule. `sll_expand_rule_ref` pushed return frames and advanced inside sub-rules. `try_expand_opaque` called expansion when `build_sll_node` would otherwise produce `Backtrack`.

**Why it failed (3 distinct bugs):**

1. **Consume node corruption:** `build_sll_node` emits `Consume(element, child)` when all configs share a common terminal. For expanded configs inside a sub-rule, this emits `p.expect(K_FROM)` at the _decision point_, consuming a token that belongs to the referenced rule (e.g., `delete_stmt`). Fix attempted: `strip_all_consume` — but this loses disambiguation information.

2. **Depth-mixed Dispatch:** Expanded configs produce Dispatch branches for tokens _inside_ sub-rules (e.g., `K_RECURSIVE` from `with_clause`). When multiple alternatives share the same prefix rule (`with_clause`), these dispatches are meaningless — every alternative sees the same tokens. The generated parser enters wrong branches and fails or times out.

3. **Dedup false resolution:** `sll_dedup_by_alt` keeps one config per `alt_index`. When two alternatives expand to configs with identical FIRST sets (e.g., `join_clause` and `table_or_subquery` both start with `table_or_subquery`), dedup merges them into a single alt. The prediction then emits a `Leaf` for the wrong alternative, silently dropping the other.

**What remains:** The `return_stack` field on `SllConfig`, `push_return`, `pop_return`, and return-stack-aware `sll_config_first` / `sll_advance` are committed as zero-overhead infrastructure. They don't affect generated output.

**Lessons:**

- Tokens from inside expanded sub-rules cannot be used for prediction at the decision point level
- To use expansion correctly, the prediction must map expanded tokens back to the decision point's lookahead depth (essentially an ATN simulator)
- `sll_dedup_by_alt` is too aggressive for expanded configs — alternatives sharing sub-rules get merged

### Removing the LR Gate in `is_rule_scannable` without Smarter Stage C (2026-04)

**Goal:** Drop the "has-left-recursive-alt" guard at `gen_context.wado:is_rule_scannable` so Stage C group-level dispatch can activate for `sql_stmt`'s statement-type alternation (nine shared-first-token alts that all transitively reference the LR rule `expr`). Expected: eliminate the last nine `bt_try` blocks in `sqlite.wado`.

**What was tried:** Generated faithful scan-side twins of the parser's precedence-climbing infrastructure (`scan_X_atom` / `scan_X_prec` / `scan_X_lr_N`), mirroring `parse_X_prec` with token-kind dispatch, `peek_at(1)` overlap dispatch, and precedence guards. Then removed the LR gate in `is_rule_scannable`.

**Why it failed:**

- `bt_try` count on `sqlite.wado` dropped 54 → 38 as expected, but `sqlite_parse` regressed **14,379 → 136,947 µs/iter (10×)**.
- Root cause: Stage C uses a **tournament dispatch** (scan every candidate alt to completion, then pick the longest match). For `sql_stmt`'s nine statement-type alts that all share first tokens on `{WITH, SELECT, DELETE, INSERT, REPLACE, UPDATE, VALUES}`, the tournament runs nine full LR scans per statement. Each scan is a deep precedence-climbing traversal over every suffix operator, so the total cost is `O(statement_length × 9)` — no matter which alt actually matches.
- `bt_try` wins here by being **first-success-wins**: when alt 1 parses cleanly, alts 2..9 are never touched. The scan-based tournament has no such short-circuit.

**Second attempt (also reverted, 2026-04):** Switched the four Stage C dispatch sites (`gen_group_scan_dispatch`, `gen_general_group_scan_dispatch`, `gen_rule_ref_group_scan_dispatch`, and rule-level `emit_scan_partition_body`) to first-success-wins on `sort_group_by_element_count` order, then dropped the gate. `bt_try` went to 0 across `sqlite.wado` / `sqlite_highlight.wado` / `rust.wado` / `type_script.wado` / `antlrv4.wado`, and gale's own driver tests stayed green — but the larger `benchmark/sqlite_parse/queries.sql` corpus failed with `Parse error: expected Eof, got "("`. The divergence is not in the `gen_rule_ref_group_scan_dispatch` site (where alts are heterogeneous statement rules and the longer one happens to come first under the element-count sort), but in `emit_scan_partition_body` for `expr`'s atom: `column_ref` matches a single `IDENTIFIER` while `function_call` matches `IDENTIFIER '(' ... ')'`. With first-success-wins on element-count sort, `column_ref` (2 elements) commits before `function_call` (4 elements) is even tried, and the trailing `(...)` becomes an unattached suffix that surfaces only when the outer parser checks for `Eof`. The element-count sort is a poor proxy for "longest token consumption" once an alt's prefix overlap is shorter than its full match. Reverted; the longest-match tournament stays in all four sites.

**What remains:** The faithful `scan_X_atom` / `scan_X_prec` / `scan_X_lr_N` generators are correct and committed — they fix a real divergence bug where the old naive scan could commit to the wrong LR alt on shared-first-token suffixes (e.g. `K_NOT K_IN` vs `K_NOT K_LIKE` vs `K_NOT K_BETWEEN` under `expr`). The LR gate in `is_rule_scannable` is kept, with a docstring explaining the cost tradeoff. Net win: `sqlite_parse` 14,883 → 11,826 µs (−21%) from other scan improvements, not from LR unlock.

**Lessons:**

- `bt_try` is not strictly worse than scan-dispatch: ordered-lazy first-success-wins beats exhaustive tournament whenever alts are not mutually-exclusive-by-first-token and the cost of the wrong scan is large.
- Faithful scan semantics (correctness) and cheap Stage C dispatch (performance) are **independent concerns**. Solving one does not unlock the other.
- First-success-wins on `sort_group_by_element_count` is **not** a drop-in replacement for longest-match: it agrees with longest-match only when the first-tried alt also consumes the most tokens, which holds for the heterogeneous RuleRef alts of `sql_stmt`'s WITH partition but breaks for the `expr` atom alts where `column_ref` is a strict prefix of `function_call`.
- Before removing the LR gate, Stage C needs one of: (a) k=2/3 lookahead partitioning to shrink the candidate set before the tournament, (b) per-site first-success-wins guarded by a static "alts are mutually disjoint after k tokens" check, or (c) ATN-style adaptive prediction. See `TODO.md` for the plan.
