# WEP: Float Literal Patterns

## Context

[One Order per Type](./wep-2026-09-23-comparison-traits.md#float-patterns) kept
floats out of patterns except as a range. A float literal, a constant holding a
float (`f64::INFINITY` among them) and a range with equal bounds were each an
error. The reason was rounding: an arm `0.3 =>` misses `0.1 + 0.2`.

The rule refused the patterns that do not round as well. `0.0`, `f64::INFINITY`
and `f64::NAN` name values no arithmetic misses by a rounding step, and a
`match` testing for them had to move each into a guard. A `match` on a float
could not be exhaustive either: no range takes a NaN, so every one needed `_`.

## Decision

A float literal, an integer literal on a float scrutinee, a constant holding a
float, and a float range with equal bounds are all patterns. This supersedes
[One Order per Type § Float patterns](./wep-2026-09-23-comparison-traits.md#float-patterns)
where the two disagree. A NaN range bound stays an error.

A float pattern compares by the float order, as `==` does. `0.0` matches
`-0.0`, and a NaN pattern (`f64::NAN`, or a constant holding a NaN) matches
every NaN.

A pattern naming one float warns, under the lint `float_literal_pattern`,
unless the compiler knows the value and it is a zero, an infinity or a NaN:

- A literal or a primitive's limit (`f64::MAX`) names a known value, so `0.0`,
  `-0.0`, `f64::INFINITY` and `f64::NAN` do not warn and `1.5` does.
- A range that takes one float names one value, so it warns as the literal
  would: `1.0..=1.0`, or an exclusive range ending at the next float. The rule
  follows from what the lint is about, one exact float, rather than from how
  the bounds are written. `-0.0..=0.0` is the value 0 and does not.
- A `global`, an associated constant, or a constant whose type holds a float
  (a struct with an `f64` field) always warns. Its value shows only when the
  match runs.

The warning names the fix: a range states the tolerance the literal hides.
`#[allow(float_literal_pattern)]` waives it, as any lint is waived.

Exhaustiveness counts float patterns. A float is a NaN or one value of the
ordered line from `-Inf` to `+Inf`, so `f64::NAN` and
`f64::NEG_INFINITY..=f64::INFINITY` together need no `_`. A constant other than
a primitive's limit counts as no value, as any constant pattern does.

## Known gaps

- A pattern on a type parameter warns only where every float type reads it as
  one value: a decimal literal, or a range that takes one `f64`. Two
  instances match one float without a warning: an integer literal on a float
  instance (`7` in `fn is_seven<T>(x: T)` called with an `f64`), and a narrower
  type rounding two bounds onto one (`1.0..=1.0001` on `f16`).
