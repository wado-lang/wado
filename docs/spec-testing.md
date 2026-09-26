# Assertions and Testing

## The `assert` Statement

The `assert` keyword is used to assert that a condition is true. If the condition is false, the program will panic with messages that includes the source of the condition and related intermediate values (like the power-assert).

```wado
// If x is not greater than 0, the program will panic, printing x.
assert x > 0;

// Also assert can take an optional message.
assert x > 0, "x must be checked elsewhere";
```

`assert` evaluates its condition exactly as the surrounding code would, so a
guarded operand never runs when its guard fails. An operand the run did not
reach is reported as `<not evaluated>`.

```wado
let list: List<i32> = [1, 2, 3];
let i = 99;
assert i < list.len() && list[i] == 1;
// condition: i < list.len() && list[i] == 1
// i: 99
// list: [1, 2, 3]
// list.len(): 3
// i < list.len(): false
// i: <not evaluated>
// list[i]: <not evaluated>
// list[i] == 1: <not evaluated>
```

To keep a failure readable, each captured operand is rendered with `Inspect`
(`:?`), which caps sequence types at a default length (`DEFAULT_SEQ_LIMIT` = 256):
a `String` operand is truncated to 256 characters with a `...` marker and an
`List` operand to 256 elements (see [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md)).
Non-sequence operands such as floats keep their natural rendering. The optional
user message is formatted with the user's own template specifiers (typically
`Display`, which is never capped) — opt into a longer dump by formatting the
value yourself.

## Testing

Tests are first-class syntax: a `test` block declares one, and `wado test` runs them. The runner's flags, file discovery and output are described by `wado test --help` and [WEP: Test Discovery](./wep-2026-05-02-test-discovery.md).

### Test Declaration Syntax

Tests are declared using the `test` keyword followed by an optional name and a block:

```wado
// Named test
test "addition works" {
    assert 1 + 1 == 2;
}

// Unnamed test
test {
    let result = compute_something();
    assert result > 0;
}

// Test with multiple assertions
test "string operations" {
    let s = "hello";
    assert s.len() == 5;
    assert s + " world" == "hello world";
}

// Expect-trap test: the test passes when the body traps
#[expect_trap]
test "panics on bad input" {
    panic("intentional panic");
}

#[expect_trap]
test {
    unreachable();
}

// TODO test: marks a test for an unimplemented feature.
// Reported on a separate axis from pass/fail (see Test Outcome Model).
#[TODO]
test "not yet implemented" {
    panic("TODO: implement this feature");
}

// Custom timeout: override the default 5000ms limit
#[timeout_ms(30000)]
test "slow computation" {
    let result = expensive_computation();
    assert result == 42;
}

// Synopsis test: runs like any test; `wado doc` renders its body as the
// module's `## Synopsis` section.
#[synopsis]
test {
    let p = Point { x: 3, y: 4 };
    assert p.length() == 5.0;
}
```

#### Syntax Rules

- `test` is a contextual keyword (functions named `test` are still allowed)
- Test name is an optional string literal
- Test body is a block containing statements
- No return type or effect declarations needed
- Tests can use any effects (side effects are allowed in tests)
- Attributes (e.g., `#[expect_trap]`, `#[TODO]`, `#[timeout_ms(N)]`, `#[synopsis]`) may appear before the `test` keyword

### Test Semantics

#### Execution

- Each test runs in isolation with fresh state: every global starts from its initializer
- Tests are independent, so they may run in any order, and concurrently
- A test passes if it completes without panicking or trapping
- A test fails if `assert` fails, `panic` is called, or a trap occurs
- Test blocks belong to the `test` world. Compiling for any other world leaves them out
- [`core:eval`](./stdlib-core-eval.md) belongs to the `test` world too. A program for any other world that reaches it does not compile

#### `#[expect_trap]` Attribute

The `#[expect_trap]` attribute inverts the pass/fail condition for a test:

- The test passes if the body traps (calls `panic`, `unreachable`, or fails an `assert`)
- The test fails if the body completes normally without trapping

This is useful for verifying that invalid operations are correctly rejected at runtime:

```wado
#[expect_trap]
test "panics on null dereference" {
    let opt: Option<i32> = null;
    opt.unwrap();
}
```

#### `#[TODO]` Attribute

The `#[TODO]` attribute marks a test as a placeholder for a feature not yet implemented. TODO tests are reported on a separate axis from regular pass/fail results (see Test Outcome Model below). When the body traps, the test is reported as pending (expected). When the body completes normally, the test is reported as resolved, which is a hard failure requiring the developer to remove the `#[TODO]` attribute.

#### `#[timeout_ms(N)]` Attribute

The `#[timeout_ms(N)]` attribute overrides the default test timeout (5000ms) for a specific test. `N` is an integer literal specifying the timeout in milliseconds. If a test exceeds its timeout, it is interrupted and fails. Time spent inside `core:eval`'s `eval` does not count against it. This is useful for tests that involve expensive computation or I/O:

```wado
#[timeout_ms(30000)]
test "large data processing" {
    let result = process_large_dataset();
    assert result.len() > 0;
}
```

### Test Outcome Model

Test results are classified into two independent axes: the pass/fail axis for regular tests, and the TODO axis for tests marked with `#[TODO]`.

#### Regular Tests

| Condition                                               | Outcome |
| ------------------------------------------------------- | ------- |
| Body completes normally                                 | pass    |
| Body traps (panic, assert failure, unreachable)         | fail    |
| Body traps and `#[expect_trap]` is present              | pass    |
| Body completes normally and `#[expect_trap]` is present | fail    |

#### TODO Tests

Tests marked with `#[TODO]` are reported separately from regular tests. They do not contribute to the pass or fail count.

| Condition               | Outcome         | Action Required                           |
| ----------------------- | --------------- | ----------------------------------------- |
| Body traps              | todo (pending)  | None — the feature is still unimplemented |
| Body completes normally | todo (resolved) | Remove `#[TODO]` — the feature now works  |

A resolved TODO test fails the run. This enforces cleanup: once the underlying feature is implemented, the `#[TODO]` attribute must be removed so the test joins the regular pass/fail pool.

A pending TODO test never causes a failure. This means fixing a compiler bug cannot increase the failure count — newly-passing TODO tests appear as "resolved" on the TODO axis rather than as unexpected failures on the pass/fail axis.

#### `#![TODO]` Modules

The `#![TODO]` inner attribute applies TODO semantics to an entire module:

- If the module fails to compile, it is reported as a single pending TODO entry.
- If the module compiles successfully, each test block is implicitly treated as `#[TODO]`.
- If the module compiles and all tests pass (i.e., the feature is implemented), it is reported as resolved, which fails the run.

### Test File Conventions

`*_test.wado` is the recommended name for a file that contains only tests. The
suffix is not required: any file with `test` blocks contributes its tests.

```
src/
  math.wado
  math_test.wado      # Tests for math.wado (recommended convention)
  string.wado
  string_test.wado    # Tests for string.wado
```

Tests can also be placed in a separate `tests/` directory:

```
src/
  lib.wado
tests/
  integration_test.wado
```

### Example Test File

```wado
// math_test.wado
use {add, multiply} from "./math.wado";

test "add positive numbers" {
    assert add(2, 3) == 5;
    assert add(0, 0) == 0;
}

test "add negative numbers" {
    assert add(-1, -1) == -2;
    assert add(-5, 3) == -2;
}
```
