---
name: coverage-investigation
description: Investigate and improve code coverage for the wado-compiler crate. Use for any question about what the tests do and do not reach, dead code included.
---

## Coverage Investigation Methodology

### Step 1: Collect Coverage Data

Use `cargo-llvm-cov` to generate line-level coverage. The tool must be installed via `cargo install cargo-llvm-cov`.

Each `cargo llvm-cov` that is not `report` runs the whole test suite again, so
run the tests once and read every report from that run:

```sh
cargo llvm-cov clean --workspace   # a stale profile would merge into this run
cargo llvm-cov --no-report -p wado-compiler -p wado-compiler-tests
cargo llvm-cov report --html                                   # target/llvm-cov/html/
cargo llvm-cov report --json --output-path scratchpad/cov.json
jq '.data[0].totals.lines' scratchpad/cov.json                  # overall
```

### Step 2: Identify Low-Coverage Files

Sort files by uncovered line count to find the biggest improvement opportunities:

```sh
jq -r '.data[0].files[] | "\(.summary.lines.count - .summary.lines.covered)\t\(.summary.lines.percent | round)%\t\(.filename)"' \
  scratchpad/cov.json | sort -rn | head -30
```

Focus on files with:

- High absolute uncovered lines (biggest bang for buck)
- Low percentage but meaningful code (not just error paths)

### Step 3: Classify Coverage Gaps

For each low-coverage file, classify gaps into categories:

| Category                       | Action                                  | Coverage Impact                       |
| ------------------------------ | --------------------------------------- | ------------------------------------- |
| **Dead code**                  | Delete it                               | Immediate improvement, no test needed |
| **Untested language features** | Add e2e test fixtures                   | Medium effort, high impact            |
| **Error handling paths**       | Add `compile_error` test fixtures       | Low effort                            |
| **Optimizer-specific paths**   | Add `wir_expect`/`wir_not_expect` tests | Medium effort                         |
| **Edge cases in codegen**      | Add targeted e2e tests                  | Varies                                |

### Step 4: Analyze a Specific File

To see exactly which lines are uncovered in a file:

```sh
# lcov from the same run
cargo llvm-cov report --lcov --output-path scratchpad/lcov.info

# Or read the HTML report from Step 1:
# target/llvm-cov/html/src/synthesis/inspect.rs.html etc.
```

To find dead code (functions never called):

```sh
# Check if a function has any callers
rg 'function_name' wado-compiler/src/ --type rust
```

### Step 5: Add E2E Tests

E2E tests are `.wado` files in `wado-compiler/tests/fixtures/`. Each file has a `__DATA__` section with JSON expectations.

**Guidelines for test files:**

- Name files based on **language features**, not compiler components (e.g., `closure_nested.wado` not `codegen_closure.wado`)
- Prefer adding test cases to existing fixture files when the feature group matches
- Each fixture group shares a filename prefix (e.g., `closure_*`, `global_*`, `match_*`)
- After adding new files: `touch wado-compiler/tests/e2e.rs` to trigger rediscovery

**Test patterns that maximize coverage:**

- `test "..." { ... }` with `{"test": {}}` — exercises codegen without needing stdout
- `compile_error` tests — exercise error reporting paths
- `wir_expect:O2` / `wir_not_expect:O2` — exercise optimizer paths
- Template strings with `{expr:?}` — exercise inspect/display synthesis

### Step 6: Verify Improvement

After changes, run Step 1 again and compare
`jq '.data[0].totals.lines.percent' scratchpad/cov.json` with the earlier total.

### Key Insights from Past Investigations

- `synthesis/inspect.rs` (~1400 lines) has dead code for type variants that never appear in practice (e.g., `Never` type inspect). Deleting dead code directly improves coverage.
- `codegen.rs` and `emit.rs` are the largest files but have relatively high coverage. Incremental improvements here require many targeted tests.
- The compiler's test suite is driven by `datatest_mini` which discovers `.wado` fixtures at compile time — new files need `touch wado-compiler/tests/e2e.rs`.
- `wado test` runs test-world fixtures; `wado run` runs CLI-world fixtures. Choose the right world for your test.
- Coverage runs all optimization levels by default in CI; locally only O0 and O2 run unless `WADO_FULL_TEST=1`.
