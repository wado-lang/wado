# WEP: Numeric Literal Suffixes

## Context

A numeric literal has no type of its own. It takes the type its context
expects, and `i32` or `f64` where nothing expects one. Inside an expression the
only way to give it another type is a cast: `255 as u8`, `1.5 as f32`.

A cast on a literal does two jobs at once. It types the literal as an
annotation of the target would, range check included, so `300 as u8` is an
error. Applied to a value, the same `as` converts and never checks a range, so
`300_i32 as u8` would be `44`. A reader has to look at the operand to know
which job a cast does.

Rust gives a literal its type with a suffix, `255u8`. In a hex literal the
letters `b` and `f` are digits, so Rust reads `0x1f32` as the hex number
`0x1F32`, not as `1` of type `f32`.

## Decision

A numeric literal may end with a type suffix: one of `i8`, `i16`, `i32`, `i64`,
`i128`, `u8`, `u16`, `u32`, `u64`, `u128`, `f16`, `bf16`, `f32` and `f64`.

```wado
let a = 255_u8;          // u8
let b = 1_000_000_i64;   // i64
let c = 1.5_f32;         // f32
let d = 0xFF_u8;         // u8
let e = 1_f64;           // f64: an integer literal with a float suffix
```

### The `_` separator is required

An underscore always separates the digits from the suffix. `255u8` is an error
that says to write `255_u8`. Letters written directly after a literal are read
as its suffix, so `255_u9` is an error naming the suffix `u9`, not a number
followed by a name.

The separator makes the suffix visible at a glance, where `1e5f32` and `1e5_f32`
otherwise look alike. It costs one character against Rust, and the error tells
a Rust writer exactly what to type.

### Which literal takes which suffix

A decimal literal takes any suffix. An integer literal with a float suffix is a
float, as in Rust. A float literal with an integer suffix is an error, as
`let x: i32 = 1.5` is.

A hex, octal or binary literal takes an integer suffix only. In a hex literal
`b` and `f` are digits, so `0x1_f32` stays the hex number `0x1F32`, as it is
today. Reading it as a suffix would change the value of programs that already
compile. In an octal or binary literal a float suffix is an error.

### A suffix is a type annotation

A suffixed literal has the type its suffix names. It is checked as the same
literal annotated with that type: `300_u8` and `1e40_f32` are errors, as
`let x: u8 = 300` and `let x: f32 = 1e40` are. A minus sign in front belongs to
the literal for this check, so `-128_i8` is valid and `-1_u8` is an error.

The context does not retype a suffixed literal. `let x: i64 = 1_i32` is a type
mismatch, and so is `let m: Meters = 1.0_f64` for a newtype `Meters`. An
operand beside it takes its type from it, as beside any typed value:
`1_u8 + 2` adds two `u8`s.

A suffixed literal is a pattern of its suffix type, which must be the type of
the scrutinee.

### The `literal_cast` lint

`lit as T` warns where `T` is a suffix type and the literal can carry that
suffix, and the warning gives the suffixed spelling: `255 as u8` suggests
`255_u8`. The cast and the suffix mean the same thing there, and the suffix
says it without the reader checking the operand. `0x10 as f64` does not warn,
since no suffix can write it. `#[allow(literal_cast)]` waives the lint for an
item, and `#![allow(literal_cast)]` for a module.

The cast keeps its meaning on a literal. Changing it to always convert would
silently change programs that compile today.

## Roadmap

- [x] Lex and parse the suffix, with the errors for a missing separator, an
  unknown suffix, and a float suffix on an octal or binary literal.
- [x] Type a suffixed literal as an annotated one, in expressions and patterns.
- [x] The `literal_cast` lint, and the repository's own sources migrated to the
  suffix.
- [x] The specification, the cheatsheet, the formatter tests and the syntax
  highlighting grammar.
