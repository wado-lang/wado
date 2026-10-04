---
name: coverage-investigation
description: Investigate and improve code coverage for the wado-compiler crate. Use for any question about what the tests do and do not reach, dead code included.
---

# Coverage Investigation

## Collect

`cargo install cargo-llvm-cov`, then:

```sh
cargo llvm-cov --html -p wado-compiler     # target/llvm-cov/html/, per line
cargo llvm-cov --json -p wado-compiler 2>/dev/null \
  | jq -r '.data[0].files[] | "\(.summary.lines.count - .summary.lines.covered)\t\(.summary.lines.percent | round)%\t\(.filename)"' \
  | sort -rn | head -30                    # files by uncovered lines
```

The total is `jq '.data[0].totals.lines.percent'` over the same JSON. Locally the
e2e suite runs only `-O0` and `-O2` unless `WADO_FULL_TEST=1`; CI runs every
level.

## Close a Gap

| Gap                     | Action                                         |
| ----------------------- | ---------------------------------------------- |
| Dead code               | Delete it                                      |
| A language feature      | An e2e fixture                                 |
| An error path           | A `compile_error` fixture                      |
| An optimizer path       | A `wir_expect:O2` / `wir_not_expect:O2` fixture |
| Inspect/Display output  | A template with `${x:?}`                       |

A fixture is named for the language feature it exercises (`closure_nested.wado`,
not `codegen_closure.wado`), joins an existing prefix group where one fits, and
uses a `test` block with `{"test": {}}` when it needs no stdout. The suite
discovers fixtures at compile time, so `touch wado-compiler/tests/e2e.rs` after
adding one.
