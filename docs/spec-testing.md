# Assertions and Testing

## The `assert` Statement

`assert` checks that a condition is true. If it is false, the program panics
with a message showing the condition's source and the values of its operands,
as power-assert does.

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

Each captured operand is rendered with `Inspect` (`:?`), so a long `String` or
`List` operand is cut at `Inspect`'s default length and marked where it was cut
(see [Inspect Truncation](./spec-literals.md#inspect-truncation)). This keeps a
failure readable. The optional message is an ordinary expression, formatted by
whatever template specifiers it uses. `Display` is never cut, so formatting a
value into the message yourself shows all of it.

## Testing

Tests are first-class syntax: a `test` block declares one, and `wado test`
finds and runs them. The runner's flags are described by `wado test --help`.

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
- Attributes (e.g., `#[expect_trap]`, `#[TODO]`, `#[timeout_ms(N)]`, [`#[synopsis]`](./spec-attributes.md#synopsis)) may appear before the `test` keyword

### Test Semantics

#### Execution

- Each test runs in isolation with fresh state: every global starts from its initializer
- Tests are independent, so they may run in any order, and concurrently
- A test passes if it completes without panicking or trapping
- A test fails if `assert` fails, `panic` is called, or a trap occurs
- Test blocks belong to the `test` world. Compiling for any other world leaves them out
- Only the test blocks of the file being tested run. A test block in a module it imports is compiled but not run, so each test runs once, from the file that declares it

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

The `#[TODO]` attribute marks a test as a placeholder for a feature not yet implemented. Its outcome is reported on a separate axis from regular pass/fail results (see [TODO Tests](#todo-tests)).

#### `#[timeout_ms(N)]` Attribute

The `#[timeout_ms(N)]` attribute overrides the default test timeout (5000ms) for a specific test. `N` is an integer literal specifying the timeout in milliseconds. If a test exceeds its timeout, it is interrupted and fails. This is useful for tests that involve expensive computation or I/O:

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

A pending TODO test never causes a failure. So fixing a compiler bug cannot
raise the failure count: a TODO test the fix makes pass shows as resolved on the
TODO axis, not as a failure on the pass/fail axis.

#### `#![TODO]` Modules

The `#![TODO]` inner attribute applies TODO semantics to an entire module:

- If the module fails to compile, it is reported as a single pending TODO entry.
- If the module compiles successfully, each test block is implicitly treated as `#[TODO]`.
- If the module compiles and all tests pass (i.e., the feature is implemented), it is reported as resolved, which fails the run.

#### Run Result

A run reports a third axis beside the two above: compile (passed / failed),
over every [discovered](#test-discovery) file. It exits non-zero when any test
fails, any TODO test is resolved, or any file fails to compile other than a
`#![TODO]` module, which counts as a pending TODO instead.

### Test Discovery

`wado test` with no path argument walks the current directory for `*.wado`
files. It skips:

- An entry a `.gitignore` matches. Each directory's `.gitignore` applies, and
  so do those of its ancestors up to the git repository root, with git's rules:
  the last matching rule wins, a pattern ending in `/` matches only a
  directory, `!` negates, and `**` spans directories. No `git` binary is needed.
- A directory listed in `.gitmodules`.
- A file or directory whose name starts with `.`.
- A directory holding its own `wado.toml`. That directory is a separate
  package, walked on its own under its own `[test]` section. With no path
  argument, its files are reported as a run of their own.
- An entry the package's `[test].exclude` matches, unless its
  `[test].include` matches it too.

Symbolic links are followed, and a directory reached twice is walked once. A
symbolic link that points nowhere is skipped. A `#![generated]` file is not
skipped.

A file needs neither a `test` block nor a particular world. Every discovered
file compiles under the `test` world, where an entry point written for another
world still type-checks, and a file with no `test` block is compiled and not
run.

#### `[test]` in `wado.toml`

```toml
[test]
exclude = ["tests/fixtures/**"]
include = ["lib/**/*_test.wado"]
```

`exclude` and `include` are lists of glob patterns, matched against a path
relative to the package root. `*` and `?` stop at a `/`, and `**` spans
directories. `dir/**` also matches `dir` itself, so the walk does not enter it.

`include` carves files back out of what `exclude` removes: a file both match is
discovered. While `include` has any pattern, the walk still enters an excluded
directory to look for one, but a `wado.toml` inside an excluded directory does
not start a run of its own.

#### Path Arguments

A path argument replaces the walk of the current directory. The files the
arguments name are reported as one run, whichever packages they belong to.

- A directory argument is walked as above. A directory inside a package is
  walked from that package's root, so its `[test]` globs match as they are
  written, and the files under the directory are kept. A directory argument
  that yields no file is an error.
- A file argument is tested as given. `[test].exclude` and `[test].include` do
  not apply to it.

#### File Naming

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

Rationale: [WEP: Test Discovery](./wep-2026-05-02-test-discovery.md).

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
