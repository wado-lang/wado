# Assertions

`assert` checks a condition while a program runs. It is a statement like any
other, so it works in ordinary code as well as in a `test` block.

## The `assert` Statement

`assert` checks that a condition is true. If it is false, the program traps.
First it writes a message to standard error, showing the condition's source and
the values of its operands as power-assert does, unless the build drops it (see
[Assertions Are Never Removed](#assertions-are-never-removed)).

<!-- {"fixture":"spec_testing_assert.wado"} -->

```wado
// Traps if x is not greater than 0.
assert x > 0;

// Also assert can take an optional message.
assert x > 0, "x must be checked elsewhere";
```

## Evaluation

`assert` evaluates its condition exactly as the surrounding code would, so a
guarded operand never runs when its guard fails. An operand the run did not
reach is reported as `<not evaluated>`.

<!-- {"fixture":"spec_testing_power_assert.wado"} -->

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

## The Failure Message

Each captured operand is rendered with `Inspect` (`:?`), so a long `String` or
`List` operand is cut at `Inspect`'s default length and marked where it was cut
(see [Inspect Truncation](./spec-literals.md#inspect-truncation)). This keeps a
failure readable. The optional message is an ordinary expression, formatted by
whatever template specifiers it uses. `Display` is never cut, so formatting a
value into the message yourself shows all of it.

## Assertions Are Never Removed

No build removes an assertion. At every optimization level the condition is
evaluated, and a false one traps. An invariant an `assert` states therefore
holds in every build, so a program can rely on `assert` to protect its state.

Checking costs run time. The part that can be dropped is the power-assert
overhead: capturing the operands and building the failure message. Under
`-f bare-asserts` a failed assertion traps without printing anything, and
nothing that captures or builds the message is compiled in. `-Os` turns it on,
and `-f no-bare-asserts` turns it back off:

```sh
wado compile -Os app.wado                     # a failed assert traps silently
wado compile -Os -f no-bare-asserts app.wado  # the message is kept
wado compile -O2 -f bare-asserts app.wado     # silent at -O2 too
```

## Contract Checks

A function states the preconditions its caller owes with `contract` clauses,
written after the `with` clause and before the body. Each clause is a condition
and an optional message, as an `assert` is, and a function may have any number
of them. A condition is parsed as an `if` condition is, with no struct literal.
It is evaluated where the function is defined, so it may name a private field
its caller could not, and it performs no effect.

<!-- {"fixture":"contract_clause.wado", "assert": false} -->

```wado
fn half(n: i32) -> i32
    contract n % 2 == 0, "n must be even"
{
    return n / 2;
}
```

A call traps when it is outside the contract, as a failed assertion does,
message included. A caller that guarantees the contract itself marks the call
with the prefix operator `unchecked`, and a marked call is checked only where
the build checks contracts. A call without the marker is checked in every
build. The operand of `unchecked` is a call to a function with a contract;
anything else is a compile error.

<!-- {"fixture":"contract_clause.wado"} -->

```wado
test "calls inside the contract" {
    assert half(4) == 2;
    assert unchecked half(6) == 3;
}
```

A build that does not check evaluates nothing, and the compiler does not assume
the contract holds: the violation keeps the class the function's documentation
gives it (see [Behavior Classes](./spec-overview.md#behavior-classes)). A call
from another component carries no marker, so a contract on an `export fn` is
always checked.

A function may also state a check of its own with `builtin::contract_checks()`,
as the whole condition of an `if` statement with no `else`. Its body runs only
where the build checks contracts.

`-f contract-checks` and `-f no-contract-checks` choose. Without either, the
test world checks at every optimization level, and any other world checks at
`-O0` only:

```sh
wado test app.wado                              # checks
wado test -f no-contract-checks app.wado        # does not
wado run -O0 app.wado                           # checks
wado run app.wado                               # does not (-O2)
wado compile -O2 -f contract-checks app.wado    # checks
```
