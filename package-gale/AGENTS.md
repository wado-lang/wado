# Gale Development Guide

## Overview

Gale is a Wado-native parser generator. See [README.md](./README.md) and [WEP: Gale](../docs/wep-2026-03-02-gale.md) for design context.

## Running Tests

```sh
# Run all Wado tests for this package
wado test package-gale/src

# Run a specific file
wado test package-gale/src/generator_test.wado

# Run tests from project root (included in on-task-done)
mise run test-wado
```

## E2E Test Architecture

Gale has two layers of e2e testing, both driven by `.g4` files in `tests/grammars/`.

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

### Layer 2: Golden Tests (`generator_test.wado`)

Verify that code generation from `Grammar` IR produces the expected `.wado` output. Each test compares `generate(parse(...))` against a golden file in `tests/golden/`.

```wado
test "generate json golden" {
    let output = generate(parse_grammar(JSON_G4));
    let expected = #include_str("../tests/golden/json.wado");
    assert output == expected, `golden mismatch: ...`;
}
```

Golden fixtures are regenerated with:

```sh
mise run update-gale-golden
```

### Test Grammars (`tests/grammars/`)

| File | Language | License | Notes |
| --- | --- | --- | --- |
| `JSON.g4` | JSON | | Combined grammar. Clean (no actions). |
| `sexpression.g4` | S-expression | | Combined grammar. Clean. |
| `calculator.g4` | Calculator | | Combined grammar. Clean. |
| `SQLite.g4` | SQLite | | Combined grammar. Large, clean. |
| `css3Lexer.g4` | CSS3 | MIT | Split lexer. Clean. |
| `css3Parser.g4` | CSS3 | MIT | Split parser. Clean. |
| `HTMLLexer.g4` | HTML | BSD 3-Clause | Split lexer. Clean. |
| `HTMLParser.g4` | HTML | BSD 3-Clause | Split parser. Clean. |
| `ANTLRv4Lexer.g4` | ANTLR4 | BSD 3-Clause | Split lexer. Has action blocks and `superClass`. |
| `ANTLRv4Parser.g4` | ANTLR4 | BSD 3-Clause | Split parser. Clean. |
| `RustLexer.g4` | Rust | MIT | Split lexer. Has semantic predicates and `superClass`. |
| `RustParser.g4` | Rust | MIT | Split parser. Has semantic predicates and `superClass`. |
| `TypeScriptLexer.g4` | TypeScript | MIT | Split lexer. Has semantic predicates and `superClass`. |
| `TypeScriptParser.g4` | TypeScript | MIT | Split parser. Has many semantic predicates and `superClass`. |

All files sourced from [antlr/grammars-v4](https://github.com/antlr/grammars-v4). Each file has a `// Source:` and `// License:` header.

**Clean** grammars (JSON, sexpression, calculator, SQLite, CSS3, HTML) contain no target-language-dependent elements and should be fully parseable and code-generatable.

**Grammars with actions/predicates** (ANTLR4, Rust, TypeScript) contain `{...}` action blocks and/or `{...}?` semantic predicates. These must be warned and skipped during parsing (see WEP). They serve as e2e tests for Gale's ability to consume real-world grammars without manual cleanup.

### Adding a New E2E Test Grammar

1. Add the `.g4` file to `tests/grammars/` (include `// Source:` and `// License:` headers)
2. Add a parse test in `g4/integration_test.wado`
3. For golden tests: add an entry in `mise.toml` under `[tasks.update-gale-golden]`, run `mise run update-gale-golden`, and add a test case in `generator_test.wado`

## Golden Fixtures

Golden fixtures live in `tests/golden/` and contain the expected `.wado` output generated from the `.g4` grammars in `tests/grammars/`.

**When to regenerate**: whenever `generator.wado`, `lexer_gen.wado`, `parser_gen.wado`, or `runtime.wado` changes in a way that affects generated output:

```sh
mise run update-gale-golden
```

Commit the updated golden files.

## Inlined Runtime

`runtime.wado` is included verbatim into every generated file via `#include_str` in `generator.wado`. It must remain self-contained (no imports from other source files). See [WEP: Compile-Time File Inclusion](../docs/wep-2026-03-02-include-str.md).

## On-Task-Done

When completing a task, run from the project root:

```sh
mise run on-task-done
```

This runs format, clippy-fix, regenerates all golden fixtures (`update-gale-golden`), and runs all tests. Commit the results.
