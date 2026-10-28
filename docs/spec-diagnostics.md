# Diagnostics

A diagnostic is a message the compiler reports about a program. An error stops
the compilation. A warning does not: the program compiles and means what the
rest of the specification says. A remark is a milder warning, for code that is
not yet wrong. This chapter states the warnings and remarks that come from
lints, the checks the compiler runs on code it accepts.

[`#[allow(...)]`](./spec-attributes.md#allow) waives a lint. A lint reports
only in [the modules of the package](./spec-packages.md#the-modules-of-a-package)
being compiled, never in the standard library or another package such as a
dependency, and never in a
[`#![generated]`](./spec-attributes.md#generated) module: its generator, not
whoever compiles it, is the one who could fix it.

Every compiling subcommand of `wado` takes three flags that choose where the
lints report. They set one value, so the last one given wins:

- `--lint`: the rule above. It is the default.
- `--lint-generated`: `#![generated]` modules too, for a generator's author.
- `--no-lint`: nowhere. A run that wants only errors skips the passes only the
  lints need.

The lints are:

| Lint                                                       | Reports                                                                         |
| ---------------------------------------------------------- | ------------------------------------------------------------------------------- |
| [`shadowed_name`](#the-shadowed_name-lint)                 | a binder that takes a name already reaching a known symbol                      |
| [`self_comparison`](#the-self_comparison-lint)             | a comparison of an expression with itself                                       |
| [`float_literal_pattern`](#the-float_literal_pattern-lint) | a pattern naming one float other than a zero, an infinity or a NaN              |
| [`arithmetic_overflow`](#the-arithmetic_overflow-lint)     | constant integer arithmetic that wraps, or a constant shift amount out of range |
| [`unconditional_trap`](#the-unconditional_trap-lint)       | an integer division or remainder that always traps                              |
| [`hex_suffix_lookalike`](#the-hex_suffix_lookalike-lint)   | a hex literal ending in `_f16`, `_bf16`, `_f32` or `_f64`                       |
| [`undecided_effects`](#the-undecided_effects-lint)         | a trait head that writes no `with` clause                                       |
| [`dead_code`](#the-dead_code-lint)                         | an unused or test-only free function or global                                  |
| [`unused_imports`](#the-unused_imports-lint)               | a `use` bringing in a name its module never refers to                           |
| [`unused_variables`](#the-unused_variables-lint)           | a variable or parameter nothing reads                                           |

## The `shadowed_name` Lint

A binder that takes a name already reaching a known symbol is legal and warns.
Every binder counts: a `let`, a parameter, a closure parameter, a type
parameter, a pattern binding, a local item. So does every kind of symbol, in
any namespace: a function, a global, a type, a trait, a case, an outer binding.
The exemption is a binder whose value derives from the name it takes, which
[Variable Scoping](./spec-expressions.md#variable-scoping) already sanctions.

<!-- {"fixture":"spec_lexical_shadowed_name.wado"} -->

```wado
fn draw(Point: i32) { }        // warns: `Point` shadows the struct of the same name
fn keep<i32>(v: i32) { }       // warns: `i32` shadows the builtin type of the same name

test {
    let println = 1;           // warns: `println` shadows the function of the same name
    assert println == 1;
}
```

Where the shadowing is deliberate, mark the binder:

<!-- {"fixture":"spec_lexical_shadowed_name_allow.wado"} -->

```wado
fn twice(#[allow(shadowed_name)] String: i32) -> i32 { return String * 2; }

test {
    assert twice(21) == 42;
}
```

Only a name that binds counts, as for
[redeclaration](./spec-expressions.md#variable-scoping).
[Patterns](./spec-patterns.md#patterns-that-cannot-fail) says which reading a
bare name takes. In a `let` or `for` binding a name reaching a `global` binds,
so the lint reports it.

The derivation is read off the binder's own source, not off the `let` keyword,
and holds at any scope. `let x = x + 1` under an `if`, `if let Some(x) = x` and
`while let Some(x) = x` are all exempt; `if let Some(x) = y` warns. A match arm
is not exempt. Its scrutinee stays in scope across every arm, so
`match x { Some(x) => … }` gives one name two meanings side by side.

A binder shadows only what is in scope where it is written. A name binds after
the expression it binds from, and reaches only what the construct carries it to.
So an `if let` binding is not in scope in the `else`, and the alternatives of an
or-pattern bind one set of names rather than shadowing each other.

## The `self_comparison` Lint

The `self_comparison` lint warns about a comparison whose two operands are the
same expression, where evaluating that expression twice cannot answer
differently. It performs no effect and writes nothing: it assigns nothing, takes
no `&mut`, and calls only declared functions, and methods that do not take
`&mut self`. No call in it is handed a value reaching a `&mut`: one the value is
or holds at any depth, or one a function, resource or signal it holds may
capture. Such a comparison has one answer on every type, because the laws of
[`Eq`](./spec-standard-traits.md#eq---equality) and
[`Ord`](./spec-standard-traits.md#ord---ordering) make both reflexive:
`x == x`, `x <= x` and `x >= x` are true, and `x != x`, `x < x` and `x > x` are
false. In a chain, each adjacent pair is one comparison. A `#line` is the line
it is written on, so two on different lines are not the same expression.

On a float the warning adds that a NaN is tested with `is_nan()`, since `x != x`
is the NaN test other languages teach.

Rationale: [WEP: One Order per Type](./wep-2026-09-23-comparison-traits.md).

## The `float_literal_pattern` Lint

The `float_literal_pattern` lint warns about a pattern that names one float,
since a rounding step can miss it: a float literal, an integer literal on a
float, a primitive's limit, a range that takes one float, or a constant whose
type is or holds a float. A zero, an infinity and a NaN are exempt where the
compiler knows the value, which it does for a literal and a primitive's limit.
A constant's value shows only when the match runs, so a constant always warns.
[Float Patterns](./spec-patterns.md#float-patterns) gives the examples.

Rationale: [WEP: Float Literal Patterns](./wep-2026-10-11-float-literal-patterns.md).

## Constant Integer Arithmetic

Two lints read integer arithmetic whose result its literals already decide.

A _constant operand_ is an integer or byte literal, a negated integer literal,
any of these cast with `as` such as `-128 as i8`, or a `+`, `-`, `*` or unary
`-` whose operands are themselves constant operands. Each has the type the expression has, so
`200 as u8 + 100` is computed in `u8`. A cast of any other operand converts a
value, so it is not a constant operand.

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
    assert 200 as u8 + 100 as u8 == 44;
    assert 0 as u32 - 1 == 4294967295;
    assert 2147483647 * 2 == -2;
    assert -(-128 as i8) == -128;
    assert b'\xC8' + 100 == 44;
    assert 1 << 33 == 2;
    let x = builtin::black_box(1 as u8);
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

## The `hex_suffix_lookalike` Lint

The `hex_suffix_lookalike` lint warns about a hex literal whose last
`_`-separated group is `f16`, `bf16`, `f32` or `f64` in any case. Those are hex
digits, so the literal is an integer, as it is in Rust, but it reads as a float
type. A float is written as a cast, `0xFF as f16`, and the integer without the
`_`, `0xFFF16`.

<!-- {"fixture":"lint_hex_suffix_lookalike.wado"} -->

```wado
test "a hex literal ending in a float type name warns" {
    assert 0xff_f16 == 0xFFF16;
    assert 0x1_F32 == 0x1F32;
    assert 0x2_bf16 == 0x2BF16;
}
```

## The `undecided_effects` Lint

The `undecided_effects` lint reports a trait head that writes no `with` clause,
which [The Trait Head](./spec-effects.md#the-trait-head) leaves open. A `pub`
trait warns, and a file-private or `internal` one remarks. Waive it while the
decision is pending.

<!-- {"fixture":"effect_trait_head_undecided.wado", "assert": false} -->

```wado
pub trait Undecided {
    fn next(&mut self) -> i32;
}

#[allow(undecided_effects)]
pub trait Deciding {
    fn next(&mut self) -> i32;
}

trait Private {
    fn next(&mut self) -> i32;
}

pub trait Settled with () {
    fn next(&mut self) -> i32;
}
```

`Undecided` warns, `Private` remarks, and `Deciding` and `Settled` report
nothing.

## The `dead_code` Lint

The `dead_code` lint warns about a free function or a global that its package
does not use, from any of its modules.

The package's entries are the modules its `wado.toml` declares: `lib` and each
`[world]` entry. Its production code is what the entries reach by `use`. Every
other module of the package is test code: test files, and helpers only they
import.

An item is used when one of these roots reaches it:

- What an entry offers: its `pub` and `export` items, and what it re-exports
  with `pub use`. A `pub` item of any other module is not a root, since only an
  entry's are offered beyond the package. Neither is an `internal` item.
- An `export` function in any module, since any module may be compiled as an
  entry of its own, as `wado run` runs a script.
- A function of an entry whose name a world export names, so a misdeclared
  entry point (`fn run()` without `export`) reaches the world's check rather
  than this lint.
- A method. A method is not itself reported, and a free function that only a
  method calls counts as used.
- A struct field default, an associated constant's value, and the default body
  of an `interface` or `resource` operation.

A trait's default body counts as reached only where a call lands on it, so a
function that only an unreached default body calls is unused.

An item the roots do not reach is reported one of two ways:

- Reached from a `test` block: "only used by tests", for production code. Test
  code exists to be reached from tests, so it reports nothing. Compiling for
  the test world omits this warning too, since there those tests are what the
  item is for.
- Reached from nothing: "never used".

`wado check` given a package, or given nothing inside one, decides the lint
over the whole package. A compilation of one file loads only what that file
reaches, so it decides what it can see: every `pub` item counts as used, and
so does every `internal` one, since a module it did not load may use it.

Rationale: [WEP: Unused Diagnostics](./wep-2026-05-16-unused-diagnostics.md).

## The `unused_imports` Lint

The `unused_imports` lint warns about a name a `use` brings in that its module
never refers to. One `use` can bring in several names, and each is reported on
its own, where the `use` binds it.

<!-- {"fixture": "unused_import.wado", "assert": false} -->

```wado
use { println, eprintln, eprint, Stdout, Stderr } from "core:cli";
use { Stdin, Stdin::{read_via_stream} } from "wasi:cli";
use traits from "./sub/unused_import_traits.wado";
use parts from "./sub/unused_import_parts.wado";
use { part } from "./sub/unused_import_parts.wado";
use { TreeMap as Map, TreeSet } from "core:collections";
use json from "core:json";
use base64 from "core:base64";
use cbor from "core:cbor";
use { to_bytes } from "core:cbor";
use { Doubled, Halved, Half } from "./sub/unused_import_traits.wado";
pub use { to_string } from "core:json";
#[allow(unused_imports)]
use { from_string } from "core:json";

#[benign(Stderr)]
fn quiet() {
    eprint("");
}

export fn run() with Stdout {
    quiet();
    let m = Map::<i32, i32>::new();
    let half = Half { n: 2 }.halved();
    let bytes = to_bytes(&half).unwrap().len();
    let whole = match part() {
        parts::Whole(n) => n,
        _ => 0,
    };
    println(`${m.len()} ${json::to_string(&21.doubled()).unwrap()} ${half} ${bytes} ${2.tripled()} ${whole}`);
}
```

A name is used when a reference written in the module resolves through it. Two
imports of one declaration are two names, and each is used only where it is
written. Beyond that:

- A trait's methods are candidates only where the trait is in scope, so an
  import putting a trait in scope is used where one of the module's calls
  dispatches to a method the trait declares or implements: `Doubled` for
  `doubled`, and the namespace `traits` for `tripled`. `Halved` stays unused:
  `halved` here is `Half`'s own method.
- An effect named in `#[benign(…)]` is used there, here `Stderr`, and so is an
  interface a world declaration imports or exports.
- A namespace import is used where the module spells it: naming a member
  through it, or qualifying a case, as `parts` does. `cbor` stays unused:
  `to_bytes` reaches its member by an import of its own.
- An interface import with functions (`Stdin::{read_via_stream}`) brings in
  the functions alone, and each is reported on its own. The interface is a
  name only where imported by itself, as `Stdin` is here.

A `pub` or `internal` `use` re-exports what it names, and an `export use`
publishes it, so neither is reported: its users are the modules that import it.

Rationale: [WEP: Unused Diagnostics](./wep-2026-05-16-unused-diagnostics.md).

## The `unused_variables` Lint

The `unused_variables` lint warns about a variable or parameter that nothing
reads. Every binder of a value counts: a `let`, a function or closure
parameter, and a pattern binding.

Some parameters are not reported, because their names are not the function's
own to choose, or nothing could read them:

- A parameter of a method that a trait or an interface declares. The
  declaration names the parameters of every implementation. That covers the
  trait's default body, a method of an `impl` of it, and an effect handler's
  operation.
- A parameter of an `export` function. The component boundary names it, often
  through a world, such as `handle` in `wasi:http/service`.
- A parameter of a function without a body.

The locals and closures of these functions are reported as anywhere else, and
so is a method of an inherent `impl`, whose signature is its own.

A name used only as the target of an assignment is not read. That covers `=`, a
destructuring assignment, and a compound assignment such as `+=`, whether they
write the name itself or a field or element of the value it holds (`p.x = 1`,
`xs[0] = 1`). Such a variable warns that it is assigned to, but never read.

A write through a reference reads the reference, since it lands in what the
reference points to. So does an index assignment on a value holding a `&mut`,
which may write through it, and any write to an affine value, whose drop reads
what was written.

<!-- {"fixture":"spec_diagnostics_unused_variables.wado"} -->

```wado
fn first(a: i32, b: i32) -> i32 {   // warns: `b` is never used
    return a;
}

trait Shape {
    fn scaled(&self, by: i32) -> i32;
}

impl Shape for Point {
    fn scaled(&self, by: i32) -> i32 {  // no warning: `Shape` names `by`
        return self.x;
    }
}

test {
    let unread = 1;                       // warns: never used
    let mut total = 0;                    // warns: assigned to, but never read
    total += first(1, 2);
    let _kept = 2;                        // starts with `_`: no warning
    let { x, y } = Point { x: 1, y: 2 };  // warns: write `y: _`
    assert x == 1;
    assert Point { x: 3, y: 4 }.scaled(2) == 3;
}
```

A name that starts with `_` is never reported, which is how to keep a binding
on purpose. A struct pattern's `{ y }` binds the field's own name, so `_y` would
name another field. Write `y: _` there instead, or leave the field to `..`.

The alternatives of an or-pattern bind one variable, so `A(n) | B(n)` is read
when either `n` is.

Rationale: [WEP: Unused Diagnostics](./wep-2026-05-16-unused-diagnostics.md).

## Known Gaps

- The lints are built into the compiler, and run on every compilation. Whether
  they move to a separate tool such as `wado lint` is undecided.
- A constant operand is made of literals only. An immutable `global` or an
  associated constant such as `i32::MAX` is not one, even where its value is
  known before the program runs, so `i32::MAX + 1` is not reported.
