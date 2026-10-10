# WEP: Numeric Literal Suffixes — Withdrawn

## Context

A numeric literal has no type of its own. It takes the type its context
expects, and `i32` or `f64` where nothing expects one. Inside an expression,
`255 as u8` gives it another type, checked as the annotation `let x: u8 = 255`
is, so `300 as u8` is an error.

This WEP first adopted Rust-style suffixes written after an `_`: `255_u8`,
`1.5_f32`. Its argument was that `as` does two jobs: on a literal it types and
range-checks, on a value it converts without a check. A reader has to look at
the operand to know which job a cast does.

Using the suffix showed two problems.

- The `_` both separates digits and introduces the suffix, and in a hex literal
  `b` and `f` are digits. `0xff_f16` is the integer `0xFFF16`, not `255` as an
  `f16`, with no diagnostic. Rust reads it the same way, and neither rustc nor
  clippy warns.
- The argument for it is weak. Whether a cast's operand is a literal is visible
  in the source text. The risky job is the other one, a value cast that
  truncates, and a suffix does nothing for it.

## Decision

Wado has no numeric literal suffix. A literal takes its type from its context
or from `as T`, and otherwise defaults to `i32` or `f64`.

Three ways to give a literal a type were compared:

| Property                              | `255_u8`                    | `255@u8`                           | `255 as u8`                    |
| ------------------------------------- | --------------------------- | ---------------------------------- | ------------------------------ |
| New syntax                            | a suffix grammar            | a suffix grammar and the `@` token | none: `as` exists already      |
| Unambiguous in every base             | no: `0x1_f32` is `0x1F32`   | yes                                | yes                            |
| Role of `_`                           | separator and suffix marker | separator only                     | separator only                 |
| Names a newtype (`Meters`)            | no                          | only with a further rule           | yes: `4 as Meters`             |
| Range-checked as an annotation        | yes                         | yes                                | yes                            |
| Left operand of `<<`                  | `1_i64 << 40`               | `1@i64 << 40`                      | `1 as i64 << 40`               |
| Left operand of `<`                   | `2_u8 < x`                  | `2@u8 < x`                         | `(2 as u8) < x`                |
| Method receiver                       | `1_u64.to_string()`         | `1@u64.to_string()`                | `(1 as u64).to_string()`       |
| Pattern                               | `1_i32 =>`                  | `1@i32 =>`                         | `1 =>`: typed by the scrutinee |
| Lint steering one spelling to another | `literal_cast`              | `literal_cast`                     | none                           |

`as` binds tighter than every binary operator, so a cast operand needs
parentheses in two places only: as a method receiver, and left of `<`, where
the `<` would open the type's generic arguments. Those parentheses are the whole
loss. Wado chooses the smaller language: one way to type a literal, no new
token, and no lexical rule that depends on the base.

### What Rust writes

A lexeme Rust accepts never means a different value in Wado. Wado may reject or
warn about one that Rust accepts.

- Letters written directly after a literal's digits are an error, and the
  diagnostic suggests the cast: `255u8` and `255_u8` both say to write
  `255 as u8`.
- In a hex literal, `_f16`, `_bf16`, `_f32` and `_f64` are digits, so the
  literal keeps the value Rust gives it. A literal ending in one warns, since
  it reads as a type.

### The `_` separator

An `_` stands between two digits, one at a time. `1_000` and `0xFF_FF` are
literals. `1_`, `1__0`, `0x_FF`, `1_.5` and `1._5` are errors.

### Exponent literals

A literal with an exponent and no type context is an `f64`, as `1e10` is in
Rust. Where an integer type is expected, it is that integer if its value is a
whole number in the type's range: `let n: i64 = 1e10` is `10_000_000_000`, and
`let n: i32 = 1e-1` is an error.

### Diagnostics

`literal_cast` is gone, since there is no other spelling to suggest. The
arithmetic lints treat `lit as T` as a constant operand of type `T`, so
`2 as u8 * 200` is reported as an `arithmetic_overflow`.

## Roadmap

- [x] Remove the suffix from the lexer and the parser. Letters after a
  literal's digits are an error that suggests `as`.
- [x] Hold `_` to one between two digits.
- [x] Type an exponent literal as above.
- [x] Warn about a hex literal ending in `_f16`, `_bf16`, `_f32` or `_f64`
  (`hex_suffix_lookalike`).
- [x] Remove `literal_cast`, and count `lit as T` as a constant operand.
- [x] Migrate the repository's sources from suffixes to `as`.
- [ ] The specification, the cheatsheet, the formatter tests and the syntax
  highlighting grammars.

## Known gaps

- `hex_suffix_lookalike` reads literal expressions only. A literal pattern
  carries no span of its own, so `0x1_f32 =>` is not reported.

- Whether a literal with both a `.` and an exponent, such as `1.5e1`, can be an
  integer where one is expected.
