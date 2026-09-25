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

A doc comment is a line comment that documents code. `///` documents the item
that follows it, and consecutive `///` lines form one doc string. `//!`
documents the module, and appears before any item. Neither changes what a
program means. See [WEP: Documentation Generation](./wep-2026-02-28-doc-command.md).

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
unrelated blocks may declare the same name without collision. A local item
cannot be `pub` or `internal`: it is always private to its enclosing function.

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
`impl`/`trait` blocks that give a local type methods.

Not yet implemented. See
[WEP: Local Item Definitions](./wep-2026-07-09-local-item-definitions.md).

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

### Binary Operators

In order of precedence, lowest to highest:

| Precedence | Operators                        | Description    | Associativity |
| ---------- | -------------------------------- | -------------- | ------------- |
| 1          | `=`, `+=`, `-=`, `*=`, `/=`, etc | Assignment     | Right         |
| 2          | `\|\|`                           | Logical OR     | Left          |
| 3          | `&&`                             | Logical AND    | Left          |
| 4          | `==`, `!=`, `<`, `<=`, `>`, `>=` | Comparison     | Restricted    |
| 5          | `\|`                             | Bitwise OR     | Left          |
| 6          | `^`                              | Bitwise XOR    | Left          |
| 7          | `&`                              | Bitwise AND    | Left          |
| 8          | `<<`, `>>`                       | Bitwise shift  | Left          |
| 9          | `+`, `-`                         | Additive       | Left          |
| 10         | `*`, `/`, `%`                    | Multiplicative | Left          |

Between assignment (1) and logical OR (2), the range operators sit at precedence level 1.5:

| Precedence | Operators    | Description | Associativity   |
| ---------- | ------------ | ----------- | --------------- |
| 1.5        | `..<`, `..=` | Range       | Non-associative |

- `..<` creates a half-open range `[start, end)` — `RangeExclusive<T>`
- `..=` creates an inclusive range `[start, end]` — `RangeInclusive<T>`
- Non-associative: `a..<b..<c` is a compile error
- Both operands must have the same type (after literal coercion)

See [WEP: Range Object](./wep-2026-03-03-range-object.md) for the full design.

### Design Note

Bitwise operators (`&`, `|`, `^`) have higher precedence than comparison operators, fixing C's well-known design flaw. This means `flags & MASK == EXPECTED` correctly parses as `(flags & MASK) == EXPECTED`.

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

These tables group operators by form, not by binding strength. `matches` binds
looser than the binary operators, `as`, and the value-producing unary operators
(`-`, `~`, `&`, `&mut`, `*`), but tighter than logical `!`:

- `!x matches { Some(_) }` is `!(x matches { Some(_) })` — "`x` does not match
  `Some(_)`".
- `*x matches { "kw" }`, `x as i32 matches { 0 }`, `a + b matches { 10 }`, and
  `flags & MASK matches { 0 }` need no parentheses. A comparison, range, or
  assignment scrutinee does: `(a == b) matches { true }`.

### Prohibited Operators

Wado has no `++`/`--`: write `x += 1` and `x -= 1`. It has no `**` power
operator: call `f64::pow(x, y)` or `f32::pow(x, y)`. See
[WEP: Operator Precedence](./wep-2026-01-11-operator-precedence.md) for why.

### Type Cast (`as`)

The `as` operator converts between primitive types. It also reinterprets a
value across a newtype boundary, between any two types sharing an ultimate base.
It converts a `flags` value to and from `u32`, and coerces a collection literal
to its target type (see
[Collection Literal Coercion](./spec-literals.md#collection-literal-coercion)).

Some primitive pairs refuse it. `f16` and `bf16` take no `as` in either
direction, and an integer converts to `char` only from `u8` (see
[char Casting and Conversion](./spec-literals.md#char-casting-and-conversion)).

`as` binds tighter than every binary operator and looser than a prefix unary
operator: `-x as u32` is `(-x) as u32`, and `a / b as f64` is `a / (b as f64)`.

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

See [WEP: Operator Precedence](./wep-2026-01-11-operator-precedence.md) for the
rationale.
