# Lexical Structure

## Whitespace

Whitespace separates tokens and is otherwise ignored. Any character with the
Unicode `White_Space` property is whitespace: space, tab, LF and CR, and also
characters such as the no-break space (U+00A0) and the ideographic space
(U+3000).

## Comments

```wado
// Line comment (extends to end of line)

/* Block comment */

/*
 * Multi-line
 * block comment
 */

//! Module doc comment
/// Doc comment
```

Block comments do not nest.

A doc comment is a line comment that documents code. Neither kind changes what
a program means.

- `///` documents the declaration that follows it: an item, or a field, case or
  method inside one. Consecutive `///` lines form one doc string. Attributes may
  stand between the doc comment and the declaration, but a blank line may not:
  it detaches the comment.
- `//!` documents the module. The `//!` lines ahead of the first item form the
  module's doc string.
- `////` and longer runs of `/` are ordinary line comments.

A doc string is Markdown. Each line loses its `///` or `//!` marker and one
space after it, so a line holding only `///` is an empty line.

```wado
//! Geometry helpers.

/// A point on the plane.
///
/// Both coordinates are in pixels.
#[wire(name_policy = "camelCase")]
pub struct Point {
    /// Distance from the left edge.
    x: i32,
    y: i32,
}
```

Rationale: [WEP: Documentation Generation](./wep-2026-02-28-doc-command.md).

## Shebang

```wado
#!/usr/bin/env -S wado run
export fn run() { ... }
```

`#!` at position 0 is a shebang and is ignored. `#![` is an inner attribute, not a shebang.

## Data Section

The `__DATA__` marker separates source code from embedded data. Everything after `__DATA__` on its own line is captured as raw text and is not parsed as Wado code.

```wado
use {println} from "core:cli";

export fn run() with Stdout {
    println("Hello!");
}

__DATA__
This is raw data, not Wado code.
It can contain any text, including JSON, YAML, or test expectations.
```

### Syntax Rules

- `__DATA__` must appear at the start of a line (after any preceding newline)
- The line must contain only `__DATA__` followed by a newline (no trailing content on the same line)
- Everything after the `__DATA__` line becomes the data section
- The data section is optional; most modules won't have one

### Accessing Data

Within Wado code, the content is available through the `#data` compile-time location literal. See [Compile-Time Location Literals](./spec-literals.md#compile-time-location-literals).

## Identifiers

An identifier starts with an ASCII letter or `_`. Each later character is `_`
or any Unicode letter or number (a character with the `Alphabetic` property or a
numeric general category):

```wado
foo
foo_bar
fooBar
FooBar
FOO_BAR
_private
name123
café        // OK: `é` is not the first character
é           // Error: the first character must be ASCII
```

Identifiers are case-sensitive.

## Contextual Keywords

The following keywords are contextual. Each acts as a keyword only in the
position listed:

| Keyword   | Keyword context                                     |
| --------- | --------------------------------------------------- |
| `flags`   | `flags` declaration                                 |
| `type`    | `type` declaration                                  |
| `of`      | `for let <pattern> of <expr>`                       |
| `from`    | `use { ... } from "..."`                            |
| `test`    | `test "name" { ... }` block                         |
| `extends` | `resource Child extends Parent`                     |
| `do`      | `with Effect => handler do { ... }`                 |
| `task`    | `task return expr;`                                 |
| `trap`    | `..trap` rest clause of an effect handler `impl`    |
| `forward` | `..forward` rest clause of an effect handler `impl` |
| `resume`  | `resume expr` in an effect handler                  |

Elsewhere each is an ordinary identifier: a variable, field, parameter, or type
name. `resume` is the exception. It is a keyword in every expression position,
so it serves only as a field name.

```wado
// 'of' as a variable name
let of = 42;
println(`${of}`);

// 'of' as a struct field
struct Item { of: i32 }
let item = Item { of: 10 };

// 'of' as a for-of binding
let arr: List<i32> = [1, 2, 3];
for let of of arr {
    println(`${of}`);
}
```

A variable, parameter, item, case or import may not be named `resume`. Only a
field or a method, reached through `.`, may take the name. The reason is that
`resume` begins an expression (`resume value`), so such a name could never be
read.

## Statements and Expressions

- `expr;` makes a statement.
- `return expr;` is necessary for a function to return a value.

### Semicolons

`;` separates statements; it does not terminate them. A block's last statement
may drop it, whatever kind of statement it is — but dropping it does not make
the statement an expression, so a value-returning function still needs
`return`.

```wado
fn f() -> i32 {
    let x = 1;
    return x + 1   // no `;` needed on the last statement
}
```

A newline never separates. There is no automatic semicolon insertion, so two
statements always need a `;` between them:

```wado
let x = 1 let y = 2    // error: expected `;`
```

Consecutive semicolons enclose empty statements, which mean nothing. `wado
format` removes them.

A block's value is its last expression whether or not a `;` follows it —
unlike Rust, a trailing `;` does not turn it into `()`. Write `()` to mean
`()`:

```wado
let a = if c { 1 } else { 2 };         // 1 or 2
let b = if c { 1; } else { 2; };       // also 1 or 2
let u = if c { g(); () } else { () };  // ()
```

Only `if`, `match`, `with … do` and [labeled blocks](./spec-control-flow.md#labeled-blocks) produce a
block value.
A brace in value position is a struct literal: `let x = { 1 };` is an error, and
`let p = { x: 1, y: 2 };` is an implicit struct literal.

`loop` is a statement, not an expression. A loop that computes a value goes
inside a labeled block, which `break LABEL: expr` leaves with the result.

## Variable Mutability

`mut` governs every write reaching the binding's storage, not just
reassignment. A `&mut self` method, a mutable borrow, and a store to a field or
element all require it.

```wado
let xs: List<i32> = [1, 2, 3];
xs.push(4);      // Error: `push` takes `&mut self`
xs[0] = 9;       // Error: the store roots at an immutable binding
```

A write through a `&mut T` is what that reference grants, so the binding
holding one needs no `mut` of its own.

## Variable Scoping

Variables are scoped to their enclosing block. Variables declared inside control flow bodies (`if`, `while`, `for`, `loop`) are not accessible outside.

```wado
for let mut i = 0; i < 10; i = i + 1 {
    let x = i * 2;
}
// i and x are not in scope here

if true {
    let y = 42;
}
// y is not in scope here
```

Shadowing in an inner block creates a new binding:

```wado
let x = 1;
if true {
    let x = x + 1;  // New binding, initialized from outer x
    println(`${x}`); // 2
}
println(`${x}`);     // 1 (outer x unchanged)
```

Same-scope shadowing is allowed when the new value is derived from the old one:

```wado
let x = 1;
let x = x + 1;  // OK: RHS references x
let x = transform(x);  // OK: RHS references x
```

Same-scope redeclaration without referencing the old value is not allowed:

```wado
let x = 1;
let x = 2;  // Error: cannot redeclare 'x' in the same scope
let x = |x: i32| x + 1;  // Error: the x inside is the closure parameter, not the outer variable
```

### The `shadowed_name` Lint

A binder that takes a name already reaching a known symbol is legal and warns.
Every binder counts: a `let`, a parameter, a closure parameter, a type
parameter, a pattern binding, a local item. So does every kind of symbol, in
any namespace: a function, a global, a type, a trait, a case, an outer binding.
The exemption is the derivation above, since the language already sanctions it.

```wado
fn draw(Point: i32) { }        // warns: `Point` shadows the struct of the same name
fn keep<i32>(v: i32) { }       // warns: `i32` shadows the builtin type of the same name
let println = 1;               // warns: `println` shadows the function of the same name
```

Mark the binder `#[allow(shadowed_name)]` where the shadowing is deliberate, or
the module `#![allow(shadowed_name)]`:

```wado
fn twice(#[allow(shadowed_name)] String: i32) -> i32 { return String * 2; }
```

A bare identifier pattern is exempt where the name reaches a case or a
`global`. Such a pattern matches by value rather than binding, and the
scrutinee's type decides which it does.

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

```wado
fn area(width: i32, height: i32) -> i32 {
    struct Size {
        width: i32,
        height: i32,
    }
    let s = Size { width, height };
    return s.width * s.height;
}
```

A local item is in scope for the whole of its block: unlike `let`, a use may
precede the declaration statement, and one local item may name another declared
later in the same block. Once the block closes the name is gone, so a nested
`if`/`while`/`for` body cannot export an item to the rest of the function.
Within its block a local item shadows a same-named module-level one, and two
unrelated blocks may declare the same name without collision. A local item never
reaches another function, even one in the same module.

A local item is always private to its enclosing function. A `pub`, `internal`
or `export` prefix on one is an error, whether or not attributes precede it.

Local structs support their own generic parameters:

```wado
fn wrap<T>(value: T) -> i32 {
    struct Box<T> {
        value: T,
    }
    let b = Box { value };
    return 0;
}
```

So do local newtypes: `type N<T> = List<T>;`.

A function body may also declare `enum`, `variant` and `flags` items, and
`impl`/`trait` blocks that give a local type methods (see
[Known gaps](#known-gaps)).

No other item is local. `fn`, `use`, `interface`, `global`, `world`, `test`
and `resource` are module-level only, and one inside a function body is a parse
error.

Rationale: [WEP: Local Item Definitions](./wep-2026-07-09-local-item-definitions.md).

## Global Variables

Global variables are module-level state. Unlike local variables (`let`), they
live as long as the module.

```wado
// Immutable global
global PI: f64 = 3.14159;

// Mutable global
global mut counter: i32 = 0;

// With visibility
pub global VERSION: i32 = 1;
```

Any type is supported. Any pure expression (no effects) can be used as an
initializer. An initializer runs at module initialization, with no handler
installed for it and in an order it does not choose. It declares no `with`
clause and has nowhere to declare one, so calling a function that declares an
effect is an error, as is dispatching an operation backed by the host.

A user-defined effect's operation is answered by an installed handler and traps
where none is, in an initializer as in a function body, so an initializer may
dispatch one. It may also install its own handler.

### Mutability

Globals follow [Variable Mutability](#variable-mutability): without `mut` a
global keeps what its initializer gave it for the whole program.

```wado
global CONSTANT: i32 = 42;
global TABLE: List<i32> = [1, 2, 3];
global mut variable: i32 = 0;

fn example() {
    variable = 10;    // OK: mutable global
    CONSTANT = 10;    // Error: cannot assign to immutable global
    TABLE.push(4);    // Error: `push` takes `&mut self`
}
```

### Initialization Order

Initializers run in dependency order, so one may read another global whatever
the declaration order — across modules too, and whether it names the global or
reaches it through a call. A cycle among them is an error.

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
a prefix one, so `-x?` is `-(x?)` and `*p.x` is `*(p.x)`.

### Unary Operators

| Operator | Description |
| -------- | ----------- |
| `-`      | Negation    |
| `!`      | Logical NOT |
| `~`      | Bitwise NOT |
| `&`      | Reference   |
| `&mut`   | Mut ref     |
| `*`      | Dereference |

### Postfix Operators

| Operator              | Description       |
| --------------------- | ----------------- |
| `.`                   | Field access      |
| `[]`                  | Index access      |
| `()`                  | Function call     |
| `::`                  | Namespace access  |
| `matches { pattern }` | Pattern test      |
| `as Type`             | Type cast         |
| `?`                   | Error propagation |

### `matches` and `!` binding

The two tables above group operators by form, not by binding strength. The
[precedence table](#precedence) puts `matches` below every binary operator and
logical `!` between `matches` and comparison, so:

- `!x matches { Some(_) }` is `!(x matches { Some(_) })` — "`x` does not match
  `Some(_)`".
- `*x matches { "kw" }`, `x as i32 matches { 0 }`, `a + b matches { 10 }`, and
  `flags & MASK matches { 0 }` need no parentheses. A comparison, range, or
  assignment scrutinee does: `(a == b) matches { true }`.
- `!a == b` is `(!a) == b`.

### Prohibited Operators

Wado has no `++`/`--`: write `x += 1` and `x -= 1`. Neither pair forms an
expression, so `a--b` is an error rather than `a - (-b)`. Write `- -x` for a
double negation.

It has no `**` power operator: call `f64::pow(x, y)` or `f32::pow(x, y)`.

### Type Cast (`as`)

The `as` operator converts between primitive types. It also reinterprets a
value across a newtype boundary, between any two types sharing an ultimate base.
It converts a `flags` value to and from `u32`, and coerces a collection literal
to its target type (see
[Collection Literal Coercion](./spec-literals.md#collection-literal-coercion)).

Some primitive pairs refuse it. `f16` and `bf16` take no `as` in either
direction, and an integer converts to `char` only from `u8` (see
[char Casting and Conversion](./spec-literals.md#char-casting-and-conversion)).

By its [precedence](#precedence), `-x as u32` is `(-x) as u32`, and
`a / b as f64` is `a / (b as f64)`.

```wado
let i = 42;
let f = i as f64;           // i32 to f64
let truncated = 3.14 as i32; // f64 to i32 (truncates to 3)

// Chained casts
let x = 10 as f64 as i32 as f64;

// Cast in expressions
let result = (a as f64) + b;
```

### Parentheses for Grouping

Parentheses `()` can be used to override operator precedence:

```wado
let a = 2 + 3 * 4;      // 14 (multiplication first)
let b = (2 + 3) * 4;    // 20 (addition first due to parentheses)

let c = 3 | 4 & 6;      // 7 (& has higher precedence than |)
let d = (3 | 4) & 6;    // 6 (| first due to parentheses)
```

### Comparison Chaining

Wado supports mathematical comparison chaining, allowing natural range expressions. It borrows Python's syntax, but not Python's evaluation:

```wado
a < b < c       // (a < b) & (b < c)
a >= b >= c     // (a >= b) & (b >= c)
a == b == c     // (a == b) & (b == c)
0 <= x <= 100   // a range check
```

A chain uses operators from one group only: ascending (`<`, `<=`), descending
(`>`, `>=`), or equality (`==`). `!=` never chains. Any other chain is a parse
error:

```wado
a < b > c       // Error: mixed directions
a == b < c      // Error: mixing == and inequality
a != b != c     // Error: != chaining not allowed
```

A chain evaluates every operand exactly once, left to right, and then tests
them. It does not short-circuit, so a later operand runs even where an earlier
comparison already decided the answer. Write `&&` where an operand must not run
on that path.

Rationale: [WEP: Operator Precedence](./wep-2026-01-11-operator-precedence.md).

## Ranges

A range operator builds a range value from two bounds:

```wado
0..<10        // RangeExclusive<i32>: 0 up to, but not including, 10
1..=10        // RangeInclusive<i32>: 1 through 10
'a'..='z'     // RangeInclusive<char>
0.0..<1.0     // RangeExclusive<f64>
```

`RangeExclusive<T>` and `RangeInclusive<T>` are prelude structs with public
`start` and `end` fields. Every range has both bounds: there is no `a..<`,
`..<b` or bare `..` range.

Both bounds must have the same type after literal coercion, and that type is
`T`. `T` must implement `Ord`, so a range over any other type is an error.

A range whose bounds are both literals must not run backwards. An integer,
float or `char` literal counts as one, negated or cast too:

```wado
let a = 10..<5;           // Error: reversed range
let b = (-1 as u8)..=5;   // Error: `-1 as u8` is 255
let c = 5..<5;            // OK: empty
```

Bounds known only at run time are not checked. A reversed range is then empty.

### Range Methods

- `r.contains(&v)` is whether `v` lies between the bounds, the end included only
  for `..=`.
- `r.is_empty()` is whether no value does.
- `r1 == r2` compares the bounds.
- `` `${r}` `` renders the range as written, `0..<10` or `1..=5`, where `T`
  implements `Display`.

### Range Iteration

A range is an iterator over `T` where `T` implements the prelude trait `Step`,
which every integer type and `char` does. A range is its own iterator, so it
serves `for`-of and every `Iterator` method:

```wado
for let i of 0..<3 { }                        // 0, 1, 2
let sum = (1..=100).fold(0, |acc, x| acc + x); // 5050
for let i of (0..<10).step_by(3) { }          // 0, 3, 6, 9
```

Iteration never steps past `T`'s maximum. `0 as u8..=255` yields all 256 values
and stops. A float range does not iterate, but `contains` still works on it.

### Range Indexing

A `List<T>`, `Array<T>` or `Slice<T>` indexed by a range of `i32` gives a
`Slice<T>` of the elements the range covers: `xs[1..<4]` holds `xs[1]`, `xs[2]`
and `xs[3]`, as does `xs[1..=3]`.

### Range Patterns

A range pattern matches a value between its bounds. It stands wherever a
refutable pattern may, nested ones included:

```wado
let grade = match score {
    0..<60 => "F",
    60..=100 => "P",
    _ => "invalid",
};
let lower = c matches { 'a'..='z' };
```

- The scrutinee is an integer or `char`.
- Each bound is an integer, `char` or byte literal, optionally negated, or a
  primitive type's associated constant such as `i32::MAX`. A user-defined
  constant is not a bound.
- A reversed range pattern is an error, and so is an empty one (`5..<5`).
- Two arms' range patterns must not overlap. The alternatives of one arm's
  or-pattern may.
- Range patterns count toward
  [exhaustiveness](./spec-control-flow.md#exhaustiveness): `0 => …` and
  `1..=255 => …` together cover a `u8`.

Rationale: [WEP: Range Object](./wep-2026-03-03-range-object.md).

## Known gaps

- `--` reads as two `-` operators, so `a--b` compiles as `a - (-b)` and `--x`
  as `-(-x)`.
- A local `enum`, `variant` or `flags` parses, but its cases do not resolve:
  `Color::Red` is an unknown identifier and `Shape::Circle(1)` an unknown
  function.
- A local `impl` or `trait` parses, but gives the local type no methods: a call
  of one reports that no such method exists.
