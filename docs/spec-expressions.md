# Statements and Expressions

A function body is a sequence of statements. This chapter covers what a
statement is, how variables and globals are declared and scoped, the operators
and their precedence, and ranges. Branches and loops are in
[Control Flow](./spec-control-flow.md), and literal values in
[Literals](./spec-literals.md).

## Statements

An expression may stand as a statement. It is evaluated and its value
discarded. A function returns a value only through `return`
([Return Values](./spec-functions.md#return-values)).

### Semicolons

`;` separates statements; it does not terminate them. A block's last statement
may drop it, whatever kind of statement it is. Dropping it does not make the
statement an expression, so a value-returning function still needs `return`.

<!-- {"fixture":"spec_lexical_semicolons.wado"} -->

```wado
fn f() -> i32 {
    let x = 1;
    return x + 1   // no `;` needed on the last statement
}

test {
    assert f() == 2;
}
```

A newline never separates. There is no automatic semicolon insertion, so two
statements always need a `;` between them:

<!-- {"fixture":"spec_lexical_semicolon_required.wado"} -->

```wado
let x = 1 let y = 2    // error: expected `;`
```

Consecutive semicolons enclose empty statements, which mean nothing. `wado format` removes them.

A block's value is its last expression whether or not a `;` follows it. Unlike
in Rust, a trailing `;` does not turn it into `()`. Write `()` to mean `()`:

<!-- {"fixture":"spec_lexical_block_value.wado"} -->

```wado
let a = if c { 1 } else { 2 };
let b = if c { 1; } else { 2; };       // a trailing `;` keeps the value
let u = if c { g(); () } else { () };  // write `()` to mean `()`
assert a == 1 && b == 1 && u == ();
```

Only `if`, `match`, `with … do` and
[labeled blocks](./spec-control-flow.md#labeled-blocks) produce a block value.
A brace in value position is a struct literal: `let x = { 1 };` is an error, and
`let p = { x: 1, y: 2 };` is an implicit struct literal.

`loop` is a statement, not an expression. A loop that computes a value goes
inside a labeled block, which `break LABEL: expr` leaves with the result.

## Variable Mutability

`mut` governs every write reaching the binding's storage, not just
reassignment. A `&mut self` method, a mutable borrow, and a store to a field or
element all require it.

<!-- {"fixture":"spec_lexical_mut_method.wado"} -->

```wado
let xs: List<i32> = [1, 2, 3];
xs.push(4);      // Error: `push` takes `&mut self`
```

<!-- {"fixture":"spec_lexical_mut_store.wado"} -->

```wado
let xs: List<i32> = [1, 2, 3];
xs[0] = 9;       // Error: the store roots at an immutable binding
```

A write through a `&mut T` is what that reference grants, so the binding
holding one needs no `mut` of its own.

## Deferred Initialization

`let x: T;` declares one name without an initializer, and the type is required.
Reading the name is a compile error unless every path reaching the read has
assigned it. The compiler follows paths through every construct that branches
or repeats:

- A `return`, `break` or `continue` takes its path to where it lands: out of
  the function, past the loop or labeled block, or to the loop's next
  iteration. The statements it skips are on no path, so nothing there is
  checked. A call does not end a path, even one that never returns, such as
  `panic`.
- A `loop`, and a `for` without a condition, ends only through a `break`, so
  after it the name is assigned when every `break` leaving it follows an
  assignment.
- A `while`, a `for` with a condition, a `for-of` and a tuple comprehension may
  run their body zero times, whatever the condition says, so an assignment in
  the body never counts after them.
- The right side of `&&` and `||`, a guard, and each element of a `let` chain
  after the first may not run, so what they assign counts only on the paths
  that ran them. A guard that fails passes its assignments on to the next arm.
- A `let ... else` block diverges, so it never reaches the statement after the
  `let`.

<!-- {"fixture":"var_uninit_valid.wado"} -->

```wado
let x: i32;
let mut i = 0;
loop {
    i += 1;
    if i == 3 {
        x = i;
        break;
    }
}
assert x == 3;
```

Without `mut`, the name is assigned at most once: an assignment that a path can
reach after another assignment is an error. A loop body that assigns runs again
on the next iteration, so it counts as such a path.

<!-- {"fixture":"var_uninit_assign_twice_error.wado"} -->

```wado
let x: i32;
if builtin::black_box(true) {
    x = 1;
}
x = 2;
```

A closure may name the variable only where it is already assigned, even if the
closure only assigns it. Its body runs when it is called, if ever, so nothing it
assigns counts after it.

## Variable Scoping

Variables are scoped to their enclosing block. Variables declared inside control flow bodies (`if`, `while`, `for`, `loop`) are not accessible outside.

<!-- {"fixture":"spec_lexical_block_scope.wado"} -->

```wado
for let mut i = 0; i < 10; i = i + 1 {
    let x = i * 2;
}
// i and x are not in scope here

if true {
    let y = 42;
}
assert y == 42;  // Error: y is not in scope here
```

Shadowing in an inner block creates a new binding:

<!-- {"fixture":"spec_lexical_shadowing.wado"} -->

```wado
let x = 1;
if true {
    let x = x + 1;  // New binding, initialized from outer x
    assert x == 2;
}
assert x == 1;      // outer x unchanged
```

Same-scope shadowing is allowed when the new value is derived from the old one:

<!-- {"fixture":"spec_lexical_shadowing.wado"} -->

```wado
let x = 1;
let x = x + 1;  // OK: RHS references x
let x = transform(x);  // OK: RHS references x
assert x == 20;
```

Same-scope redeclaration without referencing the old value is not allowed:

<!-- {"fixture":"spec_lexical_redeclare.wado"} -->

```wado
let x = 1;
let x = 2;  // Error: cannot redeclare 'x' in the same scope
```

<!-- {"fixture":"spec_lexical_redeclare_closure.wado"} -->

```wado
let x = 1;
let x = |x: i32| x + 1;  // Error: the x inside is the closure parameter, not the outer variable
```

A function's parameters share the body's scope, so a `let` taking a parameter's
name redeclares it, and so does a parameter named twice. One pattern binding a
name twice is a redeclaration too. Only a name that binds counts: a bare name
that a case or a constant pattern answers binds nothing, so writing
`let None = a else { … };` twice in one scope redeclares nothing.

### The `shadowed_name` Lint

A binder that takes a name already reaching a known symbol is legal and warns.
Every binder counts: a `let`, a parameter, a closure parameter, a type
parameter, a pattern binding, a local item. So does every kind of symbol, in
any namespace: a function, a global, a type, a trait, a case, an outer binding.
The exemption is the derivation above, since the language already sanctions it.

<!-- {"fixture":"spec_lexical_shadowed_name.wado"} -->

```wado
fn draw(Point: i32) { }        // warns: `Point` shadows the struct of the same name
fn keep<i32>(v: i32) { }       // warns: `i32` shadows the builtin type of the same name

test {
    let println = 1;           // warns: `println` shadows the function of the same name
    assert println == 1;
}
```

Mark the binder `#[allow(shadowed_name)]` where the shadowing is deliberate, or
the module `#![allow(shadowed_name)]`:

<!-- {"fixture":"spec_lexical_shadowed_name_allow.wado"} -->

```wado
fn twice(#[allow(shadowed_name)] String: i32) -> i32 { return String * 2; }

test {
    assert twice(21) == 42;
}
```

As for redeclaration, only a name that binds counts.
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

## Local Item Definitions

`struct` and `type` (newtype) may be declared inside a function or method
body, scoped to the block that declares them:

<!-- {"fixture":"spec_lexical_local_items.wado"} -->

```wado
fn area(width: i32, height: i32) -> i32 {
    struct Size {
        width: i32,
        height: i32,
    }
    let s = Size { width, height };
    return s.width * s.height;
}

test {
    assert area(3, 4) == 12;
}
```

A local item is in scope for the whole of its block: unlike `let`, a use may
precede the declaration statement, and one local item may name another declared
later in the same block. Once the block closes the name is gone, so a nested
`if`/`while`/`for` body cannot export an item to the rest of the function.
Within its block a local item shadows a same-named module-level one, and two
unrelated blocks may declare the same name without collision.

A local item is always private to its enclosing function: it never reaches
another function, even one in the same module. A `pub`, `internal` or `export`
prefix on one is an error, whether or not attributes precede it.

Local structs support their own generic parameters:

<!-- {"fixture":"spec_lexical_local_items.wado"} -->

```wado
fn wrap<T>(value: T) -> T {
    struct Box<T> {
        value: T,
    }
    let b = Box { value };
    return b.value;
}

test {
    assert wrap("boxed") == "boxed";
}
```

So do local newtypes: `type N<T> = List<T>;`.

A function body may also declare `enum`, `variant` and `flags` items, and
`impl`/`trait` blocks that give a local type methods.

No other item is local. `fn`, `use`, `interface`, `global`, `world`, `test`
and `resource` are module-level only, and one inside a function body is a parse
error.

Rationale: [WEP: Local Item Definitions](./wep-2026-07-09-local-item-definitions.md).

## Global Variables

Global variables are module-level state. Unlike local variables (`let`), they
live as long as the module.

<!-- {"fixture":"spec_lexical_globals.wado"} -->

```wado
// Immutable global
global PI: f64 = 3.14159;

// Mutable global
global mut counter: i32 = 0;

// With visibility
pub global VERSION: i32 = 1;

test {
    counter += 1;
    assert PI > 3.0 && counter == 1 && VERSION == 1;
}
```

A global may have any type. Its initializer declares no `with` clause and has
nowhere to declare one. So calling a function that declares an effect is an
error, as is dispatching an operation, except for the effects a
[`#[benign(E, ...)]`](./spec-attributes.md#benigne-) on the global lists.

An initializer runs outside every handler, those installed where the global is
first read included. Unless it installs a handler of its own, an operation it
performs runs as it would with no handler installed: the host answers a
host-backed interface ([Handlers](./spec-effects.md#handlers)). The handler it
installs has a body that holds the effect it handles as a function body's would.
Installing it demands what the handler performs
([Installing a Handler](./spec-effects.md#installing-a-handler)), so only a
handler that performs nothing can be installed here.

Whether an initializer runs, and when, is unspecified, except that it runs at
most once and has run before the global is read. An initializer that traps may
trap the program at start, at the first read, or never, if nothing reads the
global. A `#[benign]` global is no exception.

### Mutability

Globals follow [Variable Mutability](#variable-mutability): without `mut` a
global keeps what its initializer gave it for the whole program.

<!-- {"fixture":"spec_lexical_global_mutability.wado"} -->

```wado
global CONSTANT: i32 = 42;
global TABLE: List<i32> = [1, 2, 3];
global mut variable: i32 = 0;

fn example() {
    variable = 10;    // OK: mutable global
}

test {
    example();
    assert variable == 10 && CONSTANT == 42 && TABLE.len() == 3;
}
```

> Not yet implemented: the [effect system](./spec-effects.md) will track access
> to globals, so `example` will have to declare it. Until then, a function reads
> and writes a global without declaring an effect.

Assigning an immutable global, or calling a `&mut self` method on one, is an
error:

<!-- {"fixture":"spec_lexical_global_assign_immutable.wado"} -->

```wado
fn example() {
    CONSTANT = 10;    // Error: cannot assign to immutable global
}
```

<!-- {"fixture":"spec_lexical_global_mut_method.wado"} -->

```wado
fn example() {
    TABLE.push(4);    // Error: `push` takes `&mut self`
}
```

### Initialization Order

Initializers run in dependency order, so one may read another global whatever
the declaration order. This holds across modules, and whether the initializer
names the global or reaches it through a call. A cycle among them is an error.
The order between two initializers that do not depend on each other is
[unspecified](./spec-overview.md#behavior-classes).

## Operators

### Precedence

From the tightest binding to the loosest:

| Operators                                       | Kind           | Associativity                   |
| ----------------------------------------------- | -------------- | ------------------------------- |
| `.`, `::`, `()`, `[]`, `?`                      | Postfix        | Left                            |
| `-`, `~`, `*`, `&`, `&mut`                      | Prefix unary   | Right                           |
| `as Type`                                       | Type cast      | Left                            |
| `*`, `/`, `%`                                   | Multiplicative | Left                            |
| `+`, `-`                                        | Additive       | Left                            |
| `<<`, `>>`                                      | Bitwise shift  | Left                            |
| `&`                                             | Bitwise AND    | Left                            |
| `^`                                             | Bitwise XOR    | Left                            |
| `\|`                                            | Bitwise OR     | Left                            |
| `matches { pattern }`                           | Pattern test   | Left                            |
| `!`                                             | Logical NOT    | Right                           |
| `==`, `!=`, `<`, `<=`, `>`, `>=`                | Comparison     | [Chained](#comparison-chaining) |
| `&&`                                            | Logical AND    | Left                            |
| `\|\|`                                          | Logical OR     | Left                            |
| `..<`, `..=`                                    | Range          | None: `a..<b..<c` is an error   |
| `=`, `+=`, `-=`, `*=`, `/=`, `%=`, and the rest | Assignment     | Right                           |

The bitwise operators bind tighter than comparison, so `flags & MASK == EXPECTED` is `(flags & MASK) == EXPECTED`. A postfix operator binds tighter than
a prefix one, so `-x?` is `-(x?)` and `*p.x` is `*(p.x)`. The postfix `?` is
[error propagation](./spec-control-flow.md#error-propagation).

### `matches` and `!`

`matches` binds looser than the arithmetic and bitwise operators. Logical `!`
binds looser than `matches` and tighter than comparison, unlike the other prefix
operators. So:

- `!x matches { Some(_) }` is `!(x matches { Some(_) })`, which says `x` does
  not match `Some(_)`.
- `*x matches { "kw" }`, `x as i32 matches { 0 }`, `a + b matches { 10 }`, and
  `flags & MASK matches { 0 }` need no parentheses. A comparison, range, or
  assignment scrutinee does: `(a == b) matches { true }`.
- `!a == b` is `(!a) == b`.

### Prohibited Operators

Wado has no `++`/`--`: write `x += 1` and `x -= 1`. Neither is a token, so each
reads as two operators. `--x` is `-(-x)` and `a--b` is `a - (-b)`. There is no
prefix `+`, so `x++` and `a++b` are parse errors.

It has no `**` power operator: call `f64::pow(x, y)` or `f32::pow(x, y)`.

### Overflow and Division by Zero

An arithmetic operator behaves as the Wasm instruction for it does. A width Wasm
has no instruction for (`i8`, `i16`, `u8`, `u16`, `i128`, `u128`) behaves as
that instruction would at its width. Arithmetic that behaves otherwise, such as
checked or saturating arithmetic, is a method rather than an operator.

So integer `+`, `-` and `*` wrap in two's complement at every width, signed and
unsigned alike, as does unary `-` on a signed integer, and their overflow never
traps. An integer `/` or `%` whose divisor is zero traps. A signed `MIN / -1` traps, as `div_s` does, because
the quotient is one past `MAX`. A signed `MIN % -1` is 0, as `rem_s` gives. A
float operation follows IEEE 754: dividing by zero gives an infinity, or NaN for
`0.0 / 0.0`.

<!-- {"fixture":"spec_lexical_arithmetic.wado"} -->

```wado
test "integer arithmetic wraps" {
    let max: i8 = 127;
    assert max + 1 == -128;
    let zero: u8 = 0;
    assert zero - 1 == 255;
    assert i32::MAX * 2 == -2;
    assert -i64::MIN == i64::MIN;
}

#[expect_trap]
test "integer division by zero traps" {
    let divisor = 0;
    let _ = 10 / divisor;
}

#[expect_trap]
test "signed MIN / -1 traps" {
    let min: i8 = -128;
    let _ = min / -1;
}

test "signed MIN % -1 is 0" {
    let min: i8 = -128;
    assert min % -1 == 0;
}
```

A shift's two operands have one type, as a binary arithmetic operator's do, so
the amount of `x << n` is the type of `x`. The amount is taken modulo the
operand's bit width at every width, as `i32.shl` and `i64.shl` take it, so it
never traps and never shifts everything out. `>>` is arithmetic on a signed
integer and logical on an unsigned one.

`%`, the bitwise operators and the shifts take integers only: on a float each
is a compile error. Wasm has no float remainder.

<!-- {"fixture":"int_shift_amount_masked.wado"} -->

```wado
test "u8 and i8 shift modulo 8" {
    assert builtin::black_box(1_u8) << builtin::black_box(9_u8) == 2;
    assert builtin::black_box(-128_i8) >> builtin::black_box(15_i8) == -1;
    assert builtin::black_box(0x80_u8) >> builtin::black_box(15_u8) == 1;
    let one: u8 = 1;
    assert one << 9 == 2;
}
```

### Type Cast (`as`)

The `as` operator converts between primitive types. It also reinterprets a
value across a newtype boundary, between any two types sharing an ultimate base.
It converts a `flags` value to and from `u32`, and coerces a collection literal
to its target type (see
[Collection Literal Coercion](./spec-literals.md#collection-literal-coercion)).
A named struct literal or a range literal takes its type arguments from the
target, or from the target's base where that is a newtype, as it would from an
annotation: `Pair { a: 1, b: 2 } as Wide`, where `type Wide = Pair<u64>`, builds
a `Pair<u64>`. A diverging operand (`!`) casts to any type. References and
function types follow [Casts](./spec-types.md#casts).

Between primitives, `as` follows Rust's `as`. A cast between two numbers is a
[numeric cast](#numeric-casts), and `bool` and `char` have rules of their own
([`bool` Casts](#bool-casts), [`char` Casts](#char-casts)). An `enum` or a
`variant` casts to no number and from none. `ReflectEnum::<T>::discriminant(&v)`
reads a case's tag, and `ReflectEnum::<T>::from_discriminant(tag)` finds the
case with a tag (see [Static Reflection](./spec-reflection.md)).

By its [precedence](#precedence), `-x as u32` is `(-x) as u32`, and
`a / b as f64` is `a / (b as f64)`.

<!-- {"fixture":"spec_lexical_operators.wado"} -->

```wado
let i = 42;
let f = i as f64;           // i32 to f64
let truncated = 3.14 as i32; // f64 to i32 (truncates to 3)
let clamped = 300.0 as u8;   // saturates to 255
assert f == 42.0 && truncated == 3 && clamped == 255;

// Chained casts
let x = 10 as f64 as i32 as f64;
assert x == 10.0;

// Cast in expressions
let result = (a as f64) + b;
assert result == 1.5;
```

#### Numeric Casts

A numeric cast converts between two integer or float types. The integer types
include `i128` and `u128`, and the float types include `f16` and `bf16`. The
result is stated in these terms, which follow the Rust Reference:

- Transmute: keep the bit pattern and read it as the target type. Wado names
  the operation but offers no operator for it.
- Truncate: keep the low bits that fit the target's width.
- Zero-extend: widen by filling the new high bits with 0.
- Sign-extend: widen by filling the new high bits with the sign bit.
- Round toward zero: drop the fractional part.
- Saturate: replace a value below the target's range with its `MIN`, and one
  above it with its `MAX`.
- Round to nearest: take the target's value closest to the source, and the one
  with an even last digit on a tie. A value beyond the target's finite range
  becomes an infinity of its sign.

Each pair of types gets one of them:

| Source → target                  | Result                                                   |
| -------------------------------- | -------------------------------------------------------- |
| integer → integer of equal width | transmute                                                |
| integer → narrower integer       | truncate                                                 |
| unsigned integer → wider integer | zero-extend                                              |
| signed integer → wider integer   | sign-extend                                              |
| float → integer                  | round toward zero, then saturate; NaN becomes 0          |
| integer → float                  | round to nearest                                         |
| float → float                    | round to nearest; exact where the target holds the value |

A float-to-float cast keeps a NaN a NaN. No numeric cast traps.

<!-- {"fixture":"spec_expressions_numeric_casts.wado"} -->

```wado
let n: i32 = -1;
assert n as u32 == u32::MAX;           // transmute
assert 300 as u16 as u8 == 44;         // truncate
assert (200 as u8) as i32 == 200;      // zero-extend
assert n as i64 == -1;                 // sign-extend
assert -3.9 as i32 == -3;              // round toward zero
assert 1.0e10 as i32 == i32::MAX;      // saturate
assert f64::NAN as u8 == 0;
assert 16_777_217 as f32 == 16_777_216.0;   // round to nearest
let big: f64 = 1.0e300;
assert big as f32 == f32::INFINITY;    // round to nearest
assert 0.1 as f16 as f32 == 0.0999755859375;
```

A cast types a literal operand as an annotation of its target would, wherever
the literal can take that type: an integer literal takes any numeric type, and
a float literal any float type. The literal must then lie in the target's range
(see
[Compile-time range checking](./spec-literals.md#compile-time-range-checking)).
Rust gives an integer literal no type from a float target and types it `i32`,
so `3_000_000_000 as f64` is an error there. Wado types it by every numeric
target alike, since Rust's exception for float targets is the inconsistent rule.
A float literal cast to an integer type stays a float and converts as a float
value does. To reinterpret a bit pattern, cast a value that already has the
unsigned type:

<!-- {"fixture":"spec_expressions_numeric_casts.wado"} -->

```wado
assert (0xFF as u8) as i8 == -1;
```

<!-- {"fixture":"spec_expressions_numeric_cast_literal_range.wado"} -->

```wado
let a = 0xFF as i8;   // compile error: literal out of range for `i8`: 0xFF
let b = 300 as u8;    // compile error: literal out of range for `u8`: 300
let c = -1 as u32;    // compile error: literal out of range for `u32`: -1
```

#### `bool` Casts

A `bool` casts to any integer type: `false` is 0 and `true` is 1. It casts to no
float, and nothing casts to `bool`:

<!-- {"fixture":"spec_expressions_numeric_casts.wado"} -->

```wado
assert true as i32 == 1 && false as u8 == 0;
```

<!-- {"fixture":"spec_expressions_bool_cast_error.wado"} -->

```wado
let x: i32 = 1;
let a = x as bool;     // compile error
let b = 1.0 as bool;   // compile error
let c = true as f64;   // compile error
```

#### `char` Casts

A `char` casts to any integer type, which yields its Unicode scalar value. A
type too narrow for the value truncates it:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let c = 'A';
let code = c as i32;
let ucode = c as u32;
let byte = c as u8;     // truncated to low byte
assert code == 65 && ucode == 65 && byte == 65;
```

`u8 as char` is allowed, because every `u8` value is a Unicode scalar value. An
integer literal cast to `char` is typed `u8`:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let b: u8 = 65;
assert b as char == 'A';
assert 97 as char == 'a';
```

Every other integer-to-`char` cast is an error, because the source type holds
values that are not scalar values: a surrogate (`0xD800..=0xDFFF`), a value past
`0x10FFFF`, or a negative number:

<!-- {"fixture":"spec_literals_char_from_int.wado"} -->

```wado
let x: i32 = 65;
let c = x as char;  // compile error

let y: i8 = 65;
let d = y as char;  // compile error (i8 can be negative)
```

The checked conversions take their place. They answer `None` for a value that
is not a scalar value. [`core:prelude`](./stdlib-core-prelude.md) has the full
`char` API.

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let from_u32 = char::from_u32(65 as u32);
let from_i32 = char::from_i32(65);
assert from_u32 == Option::Some('A') && from_i32 == Option::Some('A');
```

A `char` casts to no type but an integer:

<!-- {"fixture":"spec_literals_char_to_non_int.wado"} -->

```wado
let c = 'A';
let f = c as f64;     // compile error: char can only be cast to integer types
let s = c as String;  // compile error: the two types share no representation
```

#### Casts in Generic Code

A cast with a type parameter or a projection on either side is judged by the
bounds alone, as Rust's is, since every type that settles it later must take
the same cast. A `ReflectNewtype<Base = B>` bound makes the cast a newtype step
to or from `B`, and a cast it does not make one is an error:

<!-- {"fixture":"cast_type_param_newtype_bound.wado"} -->

```wado
fn to_base<N: ReflectNewtype<Base = B>, B>(n: N) -> B {
    return n as B;
}

test {
    let m: Meters = 2.5;
    assert to_base(m) == 2.5;
}
```

<!-- {"fixture":"cast_type_param_source.wado"} -->

```wado
fn to_f32<T>(x: T) -> f32 {
    return x as f32;
}
```

### Parentheses for Grouping

Parentheses `()` can be used to override operator precedence:

<!-- {"fixture":"spec_lexical_operators.wado"} -->

```wado
let a = 2 + 3 * 4;      // multiplication first
let b = (2 + 3) * 4;    // addition first due to parentheses
assert a == 14 && b == 20;

let c = 3 | 4 & 6;      // & has higher precedence than |
let d = (3 | 4) & 6;    // | first due to parentheses
assert c == 7 && d == 6;
```

### Comparison Chaining

Comparisons chain as they do in mathematics. The syntax is Python's, but the
evaluation is not:

<!-- {"fixture":"spec_lexical_operators.wado"} -->

```wado
let [a, b, c, x] = [1, 2, 3, 50];
assert a < b < c;               // (a < b) & (b < c)
assert c >= b >= a;             // (c >= b) & (b >= a)
assert a + 1 == b == c - 1;     // (a + 1 == b) & (b == c - 1)
assert 0 <= x <= 100;           // a range check
```

A chain uses operators from one group only: ascending (`<`, `<=`), descending
(`>`, `>=`), or equality (`==`). `!=` never chains. Any other chain is a parse
error:

<!-- {"fixture":"spec_lexical_chain_mixed_direction.wado"} -->

```wado
a < b > c       // Error: mixed directions
```

<!-- {"fixture":"spec_lexical_chain_mixed_equality.wado"} -->

```wado
a == b < c      // Error: mixing == and inequality
```

<!-- {"fixture":"spec_lexical_chain_not_equal.wado"} -->

```wado
a != b != c     // Error: != chaining not allowed
```

A chain evaluates every operand exactly once, left to right, and then tests
them. It does not short-circuit, so a later operand runs even where an earlier
comparison already decided the answer. Write `&&` where an operand must not run
on that path.

Rationale: [WEP: Operator Precedence](./wep-2026-01-11-operator-precedence.md).

### The `self_comparison` Lint

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

Mark the item `#[allow(self_comparison)]`, or the module
`#![allow(self_comparison)]`, where the comparison is deliberate.

Rationale: [WEP: One Order per Type](./wep-2026-09-23-comparison-traits.md).

## Ranges

A range operator builds a range value from two bounds:

<!-- {"fixture":"spec_lexical_ranges.wado"} -->

```wado
let a: RangeExclusive<i32> = 0..<10;     // 0 up to, but not including, 10
let b: RangeInclusive<i32> = 1..=10;     // 1 through 10
let c: RangeInclusive<char> = 'a'..='z';
let d: RangeExclusive<f64> = 0.0..<1.0;
assert !a.contains(&10) && b.contains(&10) && c.contains(&'q') && d.contains(&0.5);
```

`RangeExclusive<T>` and `RangeInclusive<T>` are prelude structs with public
`start` and `end` fields. Every range has both bounds: there is no `a..<`,
`..<b` or bare `..` range.

Both bounds must have the same type after literal coercion, and that type is
`T`. `T` must implement `Ord`, so a range over any other type is an error.

An expected range of the same kind over a numeric `T` types both bounds as that
`T` would type them alone. A `let` annotation supplies it, and so do a parameter
type and a generic parameter the call's other arguments settle. So
`let r: RangeExclusive<u64> = 0..<0x1_0000_0000;` types both bounds as `u64`,
and so does `0..<(1 << 40)`. A range names its own type, as a named struct
literal does, so an expected newtype over a range supplies nothing; a cast to
one does (see [Type Cast](#type-cast-as)).

A range whose bounds are both literals must not run backwards. An integer,
float or `char` literal counts as one, negated or cast too:

<!-- {"fixture":"spec_lexical_range_reversed.wado"} -->

```wado
let a = 10..<5;           // Error: reversed range
```

<!-- {"fixture":"spec_lexical_range_reversed_cast.wado"} -->

```wado
let b = (10 as u8)..=5;   // Error: reversed range
```

<!-- {"fixture":"spec_lexical_ranges.wado"} -->

```wado
let c = 5..<5;            // OK: empty
assert c.is_empty();
```

Bounds known only at run time are not checked. A reversed range is then empty.

A range written as a pattern is a
[range pattern](./spec-patterns.md#range-patterns), with rules of its own. A
range of `i32` used as an index gives a `Slice<T>`
([The Sequence Family](./spec-types.md#the-sequence-family)).

### Range Methods

- `r.contains(&v)` is whether `v` lies between the bounds, the end included only
  for `..=`.
- `r.is_empty()` is whether no value does.
- `r1 == r2` compares the bounds.
- `` `${r}` `` renders the range as
  [Display Output](./spec-literals.md#display-output) states.

### Range Iteration

Where `T` implements the prelude trait `Step`, as every integer type and `char`
does, a range is itself an iterator over `T`. It serves `for`-of and every
`Iterator` method:

<!-- {"fixture":"spec_lexical_ranges.wado"} -->

```wado
let mut seen: List<i32> = [];
for let i of 0..<3 { seen.push(i); }
assert seen == [0, 1, 2];
let sum = (1..=100).fold(0, |acc, x| acc + x);
assert sum == 5050;
let steps = (0..<10).step_by(3).collect();
assert steps == [0, 3, 6, 9];
```

Iteration never steps past `T`'s maximum. `0 as u8..=255` yields all 256 values
and stops. A float range does not iterate, but `contains` still works on it.

Rationale: [WEP: Range Object](./wep-2026-03-03-range-object.md).
