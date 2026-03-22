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

## Golden Fixtures

Golden fixtures live in `tests/golden/` and contain the expected `.wado` output generated from the `.g4` grammars in `tests/grammars/`.

They are tested by `generator_test.wado` using `#include_str`:

```wado
test "generate json golden" {
    let output = generate(parse_grammar(JSON_G4));
    let expected = #include_str("../tests/golden/json.wado");
    assert output == expected, `golden mismatch: ...`;
}
```

**When to regenerate**: whenever `generator.wado` or `runtime.wado` changes in a way that affects generated output, regenerate all golden fixtures:

```sh
mise run update-gale-golden
```

This runs `gale gen` against each `.g4` in `tests/grammars/` and overwrites the corresponding file in `tests/golden/`. Commit the updated golden files.

**Adding a new golden test**:

1. Add the `.g4` file to `tests/grammars/`
2. Add a `mise run update-gale-golden` entry in `mise.toml` for it
3. Run `mise run update-gale-golden` to generate the initial golden file
4. Add a test case in `generator_test.wado` using `#include_str`

## Inlined Runtime

`runtime.wado` is included verbatim into every generated file via `#include_str` in `generator.wado`. It must remain self-contained (no imports from other source files). See [WEP: Compile-Time File Inclusion](../docs/wep-2026-03-02-include-str.md).

## On-Task-Done

When completing a task, run from the project root:

```sh
mise run on-task-done
```

This runs format, clippy-fix, regenerates all golden fixtures (`update-gale-golden`), and runs all tests. Commit the results.
