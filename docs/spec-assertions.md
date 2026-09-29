# Assertions

`assert` checks a condition while a program runs. It is a statement like any
other, so it works in ordinary code as well as in a `test` block.

## The `assert` Statement

`assert` checks that a condition is true. If it is false, the program panics
with a message showing the condition's source and the values of its operands,
as power-assert does.

<!-- {"fixture":"spec_testing_assert.wado"} -->

```wado
// If x is not greater than 0, the program will panic, printing x.
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
