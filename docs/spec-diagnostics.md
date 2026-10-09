# Diagnostics

A diagnostic is a message the compiler reports about a program. An error stops
the compilation. A warning does not: the program compiles and means what the
rest of the specification says. This chapter states the warnings that come from
lints, the checks the compiler runs on code it accepts.

A lint is waived by [`#[allow(...)]`](./spec-attributes.md#allow) on the item
that holds the code, or by `#![allow(...)]` for the whole file. A lint reports
only in the user's own modules, never in the standard library.

## Constant Integer Arithmetic

Two lints read integer arithmetic whose result its literals already decide.

A _constant operand_ is an integer literal, a negated one such as `-128_i8`, or
a `+`, `-`, `*` or unary `-` whose operands are themselves constant operands.
Each has the type the expression has, so `200_u8 + 100_u8` is computed in `u8`.

### The `arithmetic_overflow` Lint

The `arithmetic_overflow` lint warns about two cases:

- A `+`, `-`, `*` or unary `-` whose operands are constant operands, and whose
  exact result lies outside its type. It wraps, as
  [Overflow and Division by Zero](./spec-expressions.md#overflow-and-division-by-zero)
  says, and the warning names the value it wraps to.
- A `<<` or `>>` whose amount is a constant operand that is negative or not
  below the bit width. The amount is taken modulo the width, so `1 << 33` on an
  `i32` shifts by 1. The left operand may be anything.

Only the innermost overflowing operation is reported. The operations around it
are not, since the wrapped value is no longer a constant operand.

<!-- {"fixture":"lint_arithmetic_overflow.wado"} -->

```wado
test "wraps" {
    assert 200_u8 + 100_u8 == 44;
    assert 0_u32 - 1_u32 == 4294967295;
    assert 2147483647 * 2 == -2;
    assert -(-128_i8) == -128;
    assert 1 << 33 == 2;
    let x = builtin::black_box(1_u8);
    assert x << 9 == 2;
}
```

### The `unconditional_trap` Lint

The `unconditional_trap` lint warns about an integer `/` or `%` that traps
whatever the program does before it:

- Its divisor is a constant operand equal to zero. The dividend may be anything.
- It is a signed `/` whose dividend is the type's `MIN` and whose divisor is
  `-1`, both constant operands. The quotient overflows, so `div_s` traps.

A float `/` or `%` never traps, so neither is reported.

<!-- {"fixture":"lint_unconditional_trap.wado"} -->

```wado
#[expect_trap]
test "divides by zero" {
    let x = builtin::black_box(10);
    let _ = x / 0;
}
```

## Known Gaps

- The lints are built into the compiler, and run on every compilation. Whether
  they move to a separate tool such as `wado lint` is undecided.
- A constant operand is made of literals only. An immutable `global` or an
  associated constant such as `i32::MAX` is not one, even where its value is
  known before the program runs, so `i32::MAX + 1` is not reported.
