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

Consecutive semicolons enclose empty statements, which mean nothing. `wado
format` removes them.

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

A bare name that a pattern reads as a case or a constant binds nothing, so the
lint skips it. [Patterns](./spec-patterns.md#patterns-that-cannot-fail) says
which reading a name takes. In a `let` or `for` binding a name reaching a
`global` binds, so the lint reports it.

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

A global may have any type. Its initializer runs at module initialization, with
no handler installed for it and in an order it does not choose. It declares no
`with` clause and has nowhere to declare one. So calling a function that
declares an effect is an error, as is dispatching an operation backed by the
host.

An initializer may dispatch a user-defined effect's operation, which behaves as
it does in a function body
([With No Handler Installed](./spec-effects.md#with-no-handler-installed)). It
may also install its own handler.

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

The bitwise operators bind tighter than comparison, so `flags & MASK ==
EXPECTED` is `(flags & MASK) == EXPECTED`. A postfix operator binds tighter than
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

Integer `+`, `-`, `*` and unary `-` wrap in two's complement at every width,
signed and unsigned alike, so their overflow never traps. An integer `/` or `%`
whose divisor is zero traps. A signed `MIN / -1` traps too, at every width from
`i8` to `i128`, because the quotient is one past `MAX`. A signed `MIN % -1` is
0. A float operation follows IEEE 754: dividing by zero gives an infinity, or
NaN for `0.0 / 0.0`.

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

Some primitive pairs refuse it. `f16` and `bf16` take no `as` in either
direction, and an integer converts to `char` only from `u8` (see
[`char` Casts](#char-casts)).

A float converts to an integer as Rust's `as` does. It truncates toward zero,
and a value outside the target's range becomes the target's `MIN` or `MAX`.
NaN becomes 0. The cast never traps, whatever the target's width, `i128` and
`u128` included.

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

#### `char` Casts

A `char` casts to any integer type, which yields its Unicode scalar value. A
type too narrow for the value keeps its low bits:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let c = 'A';
let code = c as i32;
let ucode = c as u32;
let byte = c as u8;     // truncated to low byte
assert code == 65 && ucode == 65 && byte == 65;
```

`u8 as char` is allowed, because every `u8` value is a Unicode scalar value:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let b: u8 = 65;
assert b as char == 'A';
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
let b = (-1 as u8)..=5;   // Error: `-1 as u8` is 255
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
- `` `${r}` `` renders the range as written, `0..<10` or `1..=5`, where `T`
  implements `Display`.

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
