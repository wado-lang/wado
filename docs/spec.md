# Wado Language Specification

Wado is a programming language targeting Wasm/WASI -- Wasm in plain sight.

## Status

This document is normative. It says what the language is meant to be, and you
read a program's meaning from here. It is not a record of what the compiler
happens to do today.

So if this document and the implementation disagree, something is wrong. Which
of the two is wrong is not decided in advance: the document can be the mistaken
one. That gets settled when the disagreement is found.

What is not allowed is leaving the disagreement in place as an accepted
difference. If it is not resolved, it becomes a Known gap, recorded wherever
that area is owned, saying what the disagreement is and what it admits.

| Document               | Holds                                                         |
| ---------------------- | ------------------------------------------------------------- |
| this file              | the rules                                                     |
| `wep-*.md`             | the reasoning behind a rule, and the design it came from      |
| `design-philosophy.md` | why the language is shaped this way                           |
| `cheatsheet.md`        | a quick reference; it promises nothing this file does not     |
| `stdlib-*.md`          | generated from the compiler by `wado doc`; not edited by hand |

## Overview

| Item      | Description               |
| --------- | ------------------------- |
| Name      | Wado                      |
| Extension | `.wado`                   |
| Paradigm  | Imperative, Effect System |
| Typing    | Static, Strong, Inferred  |
| Target    | Wasm/WASI                 |

See also: [Cheatsheet](./cheatsheet.md) for quick syntax reference.

## Design Philosophy

See [Design Philosophy](./design-philosophy.md). The rules those principles
produced are stated here, each where it applies: [Memory Model](#memory-model),
[Concurrency Model](#concurrency-model), [Effect System](#effect-system).

## Lexical Structure

### Whitespace

Whitespace separates tokens and is otherwise ignored. Any character with the
Unicode `White_Space` property is whitespace: space, tab, LF and CR, and also
characters such as the no-break space (U+00A0) and the ideographic space
(U+3000).

### Comments

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

### Shebang

```wado
#!/usr/bin/env -S wado run
export fn run() { ... }
```

`#!` at position 0 is a shebang and is ignored. `#![` is an inner attribute, not a shebang.

### Data Section

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

#### Syntax Rules

- `__DATA__` must appear at the start of a line (after any preceding newline)
- The line must contain only `__DATA__` followed by a newline (no trailing content on the same line)
- Everything after the `__DATA__` line becomes the data section
- The data section is optional; most modules won't have one

#### Accessing Data

Within Wado code, the content is available through the `#data` compile-time location literal. See [Compile-Time Location Literals](#compile-time-location-literals).

### Identifiers

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

### Contextual Keywords

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

### Statements and Expressions

- `expr;` makes a statement.
- `return expr;` is necessary for a function to return a value.

#### Semicolons

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

Only `if`, `match`, `with … do` and [labeled blocks](#labeled-blocks) produce a
block value.
A brace in value position is a struct literal: `let x = { 1 };` is an error, and
`let p = { x: 1, y: 2 };` is an implicit struct literal.

`loop` is a statement, not an expression. A loop that computes a value goes
inside a labeled block, which `break LABEL: expr` leaves with the result.

### Variable Mutability

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

### Variable Scoping

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

#### The `shadowed_name` Lint

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

### Local Item Definitions

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

### Global Variables

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

#### Mutability

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

#### Initialization Order

Initializers run in dependency order, so one may read another global whatever
the declaration order — across modules too, and whether it names the global or
reaches it through a call. A cycle among them is an error.

### Operators

#### Binary Operators

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

#### Design Note

Bitwise operators (`&`, `|`, `^`) have higher precedence than comparison operators, fixing C's well-known design flaw. This means `flags & MASK == EXPECTED` correctly parses as `(flags & MASK) == EXPECTED`.

#### Unary Operators

| Operator | Description |
| -------- | ----------- |
| `-`      | Negation    |
| `!`      | Logical NOT |
| `~`      | Bitwise NOT |
| `&`      | Reference   |
| `&mut`   | Mut ref     |
| `*`      | Dereference |

#### Postfix Operators

| Operator              | Description       |
| --------------------- | ----------------- |
| `.`                   | Field access      |
| `[]`                  | Index access      |
| `()`                  | Function call     |
| `::`                  | Namespace access  |
| `matches { pattern }` | Pattern test      |
| `as Type`             | Type cast         |
| `?`                   | Error propagation |

#### `matches` and `!` binding

These tables group operators by form, not by binding strength. `matches` binds
looser than the binary operators, `as`, and the value-producing unary operators
(`-`, `~`, `&`, `&mut`, `*`), but tighter than logical `!`:

- `!x matches { Some(_) }` is `!(x matches { Some(_) })` — "`x` does not match
  `Some(_)`".
- `*x matches { "kw" }`, `x as i32 matches { 0 }`, `a + b matches { 10 }`, and
  `flags & MASK matches { 0 }` need no parentheses. A comparison, range, or
  assignment scrutinee does: `(a == b) matches { true }`.

#### Prohibited Operators

Wado has no `++`/`--`: write `x += 1` and `x -= 1`. It has no `**` power
operator: call `f64::pow(x, y)` or `f32::pow(x, y)`. See
[WEP: Operator Precedence](./wep-2026-01-11-operator-precedence.md) for why.

#### Type Cast (`as`)

The `as` operator converts between primitive types. It also reinterprets a
value across a newtype boundary, between any two types sharing an ultimate base.
It converts a `flags` value to and from `u32`, and coerces a collection literal
to its target type (see
[Collection Literal Coercion](#collection-literal-coercion)).

Some primitive pairs refuse it. `f16` and `bf16` take no `as` in either
direction, and an integer converts to `char` only from `u8` (see
[char Casting and Conversion](#char-casting-and-conversion)).

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

#### Parentheses for Grouping

Parentheses `()` can be used to override operator precedence:

```wado
let a = 2 + 3 * 4;      // 14 (multiplication first)
let b = (2 + 3) * 4;    // 20 (addition first due to parentheses)

let c = 3 | 4 & 6;      // 7 (& has higher precedence than |)
let d = (3 | 4) & 6;    // 6 (| first due to parentheses)
```

#### Comparison Chaining

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

## Control Flow

### Conditional Statements

```wado
if condition {
    // then block
} else {
    // else block
}

// else-if chains
if x < 0 {
    println("negative");
} else if x == 0 {
    println("zero");
} else {
    println("positive");
}
```

#### If Expression

```wado
let abs = if x < 0 { -x } else { x };

let grade = if score >= 90 { "A" } else if score >= 80 { "B" } else { "C" };
```

#### If Let Pattern Matching

```wado
let opt: Option<i32> = Option::<i32>::Some(42);
if let Some(x) = opt {
    println(`Got: ${x}`);
} else {
    println("None");
}
```

#### Match Ergonomics

When the scrutinee of `if let`, `match`, or `matches` is a reference type (`&T` or `&mut T`), patterns match against the underlying type. Payload bindings become references — e.g. matching `&Option<T>` with `Some(x)` gives `x: &T`, not `x: T` (Rust-compatible, RFC 2005).

A destructuring `let` or `for` binding follows the same rule, and so does a reference met below the top of a pattern: `for let [a, b] of &pairs` gives `a: &A`, and `[n, { x, .. }]` against `[i32, &Point]` gives `x: &i32`.

```wado
let opt: Option<i32> = Option::<i32>::Some(42);
let ro = &opt;
if let Some(x) = ro {       // ro: &Option<i32>, x: &i32
    println(`Got: ${*x}`);   // dereference to use the value
}
```

#### Let Else

`let PATTERN = EXPR else { ... };` binds a refutable pattern whose bindings
escape into the enclosing scope. On a match they cover the rest of the block;
otherwise the `else` block runs and must diverge (`return`, `break`,
`continue`, `panic`, …). The `else` block does not see the bindings.

```wado
fn parse_port(s: String) -> i32 {
    let Ok(port) = i32::from_str(&s) else {
        return -1;
    };
    return port;                // `port` is in scope here
}
```

An irrefutable pattern is rejected — its `else` could never run; use a plain
`let`.

### While Loop

```wado
let mut i = 0;
while i < 10 {
    println(`i = ${i}`);
    i = i + 1;
}
```

#### While Let Pattern Matching

`while let` allows iterating while a pattern matches:

```wado
let items: List<i32> = [1, 2, 3];
let mut iter = items.into_iter();

while let Some(x) = iter.next() {
    println(`${x}`);
}
```

The loop continues as long as the pattern matches. When the pattern fails to match (e.g., `iter.next()` returns `None`), the loop exits.

### For Loop

C-style for loop with initialization, condition, and update. Parentheses are optional:

```wado
for let mut i = 0; i < 10; i = i + 1 {
    println(`i = ${i}`);
}

// With parentheses (also valid)
for (let mut i = 0; i < 10; i = i + 1) {
    println(`i = ${i}`);
}

// All parts are optional
for ;; {
    // infinite loop
}
```

#### Note

`continue` in a for loop executes the update expression before the next iteration, matching C semantics.

#### For with Pattern Condition

The condition part of a C-style for loop can use `let` pattern matching:

```wado
let items: List<i32> = [10, 20, 30];
let mut iter = items.into_iter();

for ; let Some(x) = iter.next(); {
    println(`${x}`);
}

// With update expression
let mut count = 0;
for ; let Some(x) = iter.next(); count += 1 {
    println(`item ${count}: ${x}`);
}
```

The loop continues as long as the pattern matches. This is useful for iterating with additional state (like a counter) alongside pattern matching.

### For-Of Loop

For iterating over any type that implements `IntoIterator`:

```wado
let numbers: List<i32> = [1, 2, 3, 4, 5];
for let n of numbers {
    println(`${n}`);
}

// With mutable binding
for let mut item of items {
    item = item * 2;  // Can modify the local binding
    println(`${item}`);
}

// Custom types that implement IntoIterator also work
for let x of my_collection {
    println(`${x}`);
}
```

#### Semantics

`for let item of collection { body }` calls `collection.into_iter()` once,
before the first iteration. Each iteration then calls `next()` on the result,
binds the element to `item` and runs the body. The loop ends when `next()`
returns `None`.

The binding is a copy of each element (value semantics), so modifying it does
not affect the original collection.

The binding must match every element, as a `let` pattern must. A pattern that can fail (`for let Some(x) of xs`, a narrowing type pattern) is a compile error. Match on the element in the body instead.

#### Tuple for-of (compile-time expansion)

When the iterable is a tuple, the loop body is expanded once per element at compile time. Each expansion independently types the binding, enabling heterogeneous iteration with per-element trait dispatch.

```wado
let t = [42, "hello", true];
for let v of t {
    println(`${v}`);  // expanded to three blocks, each with the correct type
}
```

`break` and `continue` are not allowed inside a tuple for-of body because the loop is unrolled at compile time into sequential blocks and these have no natural target. `.enumerate()` is supported and provides a compile-time index.

#### Tuple comprehension

Wrapping the same walk in `[...]` collects one result element per source element. The braces hold a single expression — the element's value — since every position of the result tuple has one.

```wado
impl<..T: Doubled> Doubled for [..T] {
    fn doubled(&self) -> [..T] {
        return [for let v of *self { v.doubled() }];
    }
}
```

The `.enumerate()` form binds the index alongside the value (`[for let [i, v] of t.enumerate() { ... }]`). The index is a compile-time constant, so it is also the one non-literal a tuple accepts as a subscript (`t[i]`), for reads and writes alike.

The source must be a variadic tuple (`[..T]`); a concrete tuple is not walkable this way.

### Infinite Loop

```wado
loop {
    // runs forever until break
    if should_exit() {
        break;
    }
}
```

### Break and Continue

`break` exits the innermost enclosing loop. `continue` skips to the next iteration.

```wado
// break example
let mut i = 0;
while i < 100 {
    if i == 10 {
        break;  // exit the loop
    }
    i = i + 1;
}

// continue example
for let mut i = 0; i < 10; i = i + 1 {
    if i == 5 {
        continue;  // skip printing 5
    }
    println(`${i}`);
}
```

Both `break` and `continue` work with `while`, `for`, and `loop`, and a loop
carries no label. An unlabeled `break` or `continue` acts on the innermost loop,
and outside a loop both are errors. `continue` never takes a label.
`break LABEL` is a different statement: it leaves an enclosing labeled block,
needs no loop of its own, and is how to leave more than the innermost loop. A
closure body starts its own loop scope, so a loop around a closure binds nothing
written inside it.

### Labeled Blocks

A labeled block is a named block that `break LABEL` leaves from anywhere inside
it. It is Wado's only non-local jump, and it replaces loop labels, labeled
`continue`, and `goto`. It is also the block form that carries a value.

#### Syntax

`LABEL: { ... }`, in statement or expression position.

- The label is an identifier followed by a colon
- `break LABEL;` leaves the block; `break LABEL: expr;` leaves it with a value
- `()` is a value like any other, so `break LABEL: ()` says what `break LABEL`
  says, and `break ()` says what `break` says
- Nested blocks may reuse a label name; a `break` targets the innermost match
- The block opens a new scope, and a name declared inside may shadow an outer
  one

The label is mandatory to tell a block apart from a struct literal, since
`{ field: value }` on its own could be either. An unlabeled `{ ... }` block is
therefore a parse error.

#### Escaping and Early Exit

`break` alone leaves the innermost loop. A labeled block around a nest leaves
all of it at once, and its tail is the path no `break` took. Inside a block,
`break LABEL` skips the rest, so a chain of guards stays flat instead of nesting
one inside the next. A `break` may also leave an effect handler's `do` block,
which restores the outer handler either way.

```wado
search: {
    for let r of 0..<grid.len() {
        for let c of 0..<grid[r].len() {
            if grid[r][c] == needle {
                hit = [r, c];
                break search;   // leaves both loops
            }
        }
    }
    hit = [-1, -1];             // reached only when no break was taken
}
```

#### As an Expression

A labeled block used as a value yields two ways: through `break LABEL: expr`,
and through its trailing statement on the path that reaches the end. Both are
branches, so every path must agree on one type. They unify against the type
expected where the block sits, so a literal coerces to that rather than to its
own default. Any expression position takes one, not only the right-hand side of
a `let`.

```wado
let found = search: {
    for let item of items {
        if item.key == key {
            break search: item.value;
        }
    }
    -1  // the value when no break is taken
};
```

A trailing statement that is not a value yields `()`. A block whose branches all
yield `()` has the type `()`, and one mixing `()` with a value is the same type
error as any other disagreement.

A path that cannot reach the end is no branch at all. When the trailing
statement is a loop that only `break LABEL` leaves, nothing reaches the tail, so
the breaks alone type the block. This is how a loop computes a value:

```wado
let found = scan: {
    loop {
        let line = next();
        if line.is_match() { break scan: line; }
    }
};
```

### Match Expression

Match expression provides exhaustive pattern matching on variants and other types.

```wado
// Match expression (produces a value)
let result = match opt {
    Some(x) => x * 2,
    None => 0,
};

// Match with custom variants
let area = match shape {
    Circle(r) => 3.14159 * r * r,
    Rectangle([w, h]) => w * h,
    Point => 0.0,
};

// Match statement (no value produced)
match command {
    Start => engine.start(),
    Stop => engine.stop(),
}
```

#### Pattern Syntax

| Pattern       | Example                      | Description                                  |
| ------------- | ---------------------------- | -------------------------------------------- |
| Wildcard      | `_`                          | Matches anything                             |
| Variable      | `x`                          | Binds matched value                          |
| Mut variable  | `mut x`, `Some(mut x)`       | Binds as mutable                             |
| Literal       | `0`, `"hello"`, `true`       | Matches exact value                          |
| Constant      | `MAX_LEN`, `i32::MAX`        | Matches an immutable global / const by value |
| Variant       | `Some(x)`, `None`            | Matches variant case                         |
| Tuple         | `[a, b, c]`                  | Destructures tuple                           |
| Nested tuple  | `[10, Some(x)]`              | Literal/variant sub-patterns in tuple        |
| Struct        | `{ x, y }`, `Point { x, y }` | Destructures struct                          |
| Nested struct | `{ x: 0, y }`                | Literal/variant sub-patterns in struct       |
| Or            | `Red \| Blue`                | Matches either pattern                       |
| Guard         | `Some(x) && x > 0`           | Pattern with condition                       |

A string-literal pattern tests the scrutinee with `==` against a `String`, so
any type implementing `Eq<String>` matches one: a `String`, a `StrSlice`, or a
newtype over either. A type parameter bounded by `AsStrSlice` matches as well,
since `AsStrSlice` requires `Eq<String>`.

```wado
fn kind<S: AsStrSlice>(s: S) -> i32 {
    return match s { "cntrl" => 1, "digit" => 2, _ => 0 };
}
kind("digit".as_str_slice());   // 2
```

#### Exhaustiveness

Match must cover all possible cases. Use `_` wildcard for catch-all:

```wado
match color {
    Red => "red",
    Green => "green",
    _ => "other",  // Required for exhaustiveness
}
```

The guardless arms must cover every value together. A guarded arm covers
nothing, and a case is covered only as far as its payload patterns reach:

```wado
match opt {
    Some(1) => "one",
    None => "none",
}   // Error: non-exhaustive match: missing case `Some(-2147483648..=0)`
```

#### Guard Expressions

Guards use `&&` to reflect left-to-right evaluation (pattern first, then guard):

```wado
match customer {
    Premium(years) && years > 5 => 0.3,
    Premium(_) => 0.2,
    _ => 0.1,
}
```

#### Qualified Patterns

A case may be written under the type that declares it: `Color::Green`. Two kinds
of qualifier reach the same cases.

The first is a name that resolves to the scrutinee's type. An import alias
(`M::Nothing` under `use { Maybe as M }`), `Self` inside an `impl`, and a
namespace-qualified type (`dep::Maybe::Nothing`) all qualify. So does any name on
the scrutinee's newtype chain, a newtype's cases being its base's: with
`type C = Color`, both `C::Green` and `Color::Green` qualify. A second newtype
over the same base does not, being a distinct type.

The second is a namespace prefix the scrutinee's type is reachable through, such
as `h::Green` under `use h from "./hue.wado"`. That prefix names a module rather
than a type.

Either way the name must be one the file can see. Only the prelude is in scope
without a `use`, so a qualifier naming an unimported type is an error even where
the bare case would match.

A qualifier may restate the scrutinee's type arguments. It must then write as
many as the scrutinee carries, so `Maybe<i32>::Just` qualifies a `Maybe<i32>`
while `Color<i32>::Red` is an error, because `Color` declares no type parameters.

Only a bare identifier can bind. A qualified path names a case, an associated
constant, or an immutable `global`; anything else is an error, never a variable
of that name.

#### Constant Patterns

A pattern identifier that resolves to an immutable `global` or an associated constant matches by value, instead of binding a new variable:

```wado
global TK_FOO: i32 = 1;
global TK_BAR: i32 = 2;

let kind = match token {
    TK_FOO | TK_BAR => "keyword",
    i32::MAX        => "max",
    _               => "other",
};
```

#### Or Patterns

Or patterns match if any alternative matches. All alternatives must bind the same names with the same types:

```wado
// Enum or-patterns
match color {
    Red | Blue => "cool",
    Green => "warm",
}

// Variant or-patterns with bindings
match expr {
    Num(n) | Neg(n) => use(n),
    Zero => 0,
}

// Literal or-patterns
match n {
    1 | 2 | 3 => "low",
    _ => "high",
}

// Or patterns in matches operator
if shape matches { Circle(_) | Square(_) } { ... }
```

#### Nested Sub-Patterns in Tuple/Struct Destructuring

Tuple and struct patterns support literal, variant, enum, or-pattern, and range sub-patterns:

```wado
// Literal sub-patterns in tuples
match [x, y] {
    [0, 0] => "origin",
    [0, _] => "y-axis",
    [_, 0] => "x-axis",
    _ => "other",
}

// Variant sub-patterns in tuples
match [a, b] {
    [Some(x), Some(y)] => x + y,
    [Some(x), None] => x,
    [None, _] => 0,
}

// Literal sub-patterns in structs
if let { x: 0, y: 0 } = point { println("origin"); }

// Enum sub-patterns in tuples
match [color, size] {
    [Red, Large] => "big red",
    [Blue, _] => "blue",
    _ => "other",
}

// Or-pattern and range sub-patterns
match point {
    { x: 0 | 1, y } => y,
    { x: 10..=20, y } => -y,
    _ => 0,
}
```

#### Mutable Bindings in Patterns

The `mut` keyword before a binding name makes it mutable inside the pattern body:

```wado
if let Some(mut x) = opt {
    x += 10;  // x is mutable
}

match result {
    Ok(mut value) => {
        value *= 2;
        value
    },
    Err(_) => 0,
}
```

### Matches Operator

The `matches` infix operator tests if a value matches a pattern, returning `bool`.

```wado
// Basic usage
let is_some = opt matches { Some(_) };
let is_circle = shape matches { Circle(_) };

// With guard
let is_large = shape matches { Circle(r) && r > 10.0 };

// In conditions
if opt matches { Some(_) } {
    println("has value");
}
```

#### Scope

Pattern bindings are scoped to the guard only and do not escape:

```wado
// Bindings don't escape
if opt matches { Some(x) } && x > 0 { }  // ERROR: x not in scope

// Use guard inside the pattern instead
if opt matches { Some(x) && x > 0 } { }  // OK
```

### Branch Hints

`builtin::cold_path()` marks the code path that contains it as cold (rarely
executed). It is a statement with no runtime effect. It is a performance hint:
the compiler and the Wasm engine treat the other side of the branch that
contains it as the likely one.

Because it is a plain statement rather than a condition wrapper, `cold_path()`
works anywhere a branch body does — including an `if let` or `match` arm, where
no boolean condition is available:

```wado
// Error/abort guard: the taken branch is cold.
fn get(&self, i: i32) -> i32 {
    if i >= self.len {
        builtin::cold_path();
        panic("index out of bounds");
    }
    return self.data[i];
}

// `match` arm with no boolean condition.
match command {
    Command::Run => execute(),
    Command::Crash => {
        builtin::cold_path();
        panic("crashed");
    }
}
```

Placed on the fall-through after a guard whose taken branch diverges, it hints
the guard as likely-taken — the guard-clause idiom:

```wado
fn lookup(&self, key: String) -> i32 {
    if let Some(v) = self.fast_path(key) {
        return v;
    }
    builtin::cold_path(); // the slow path below is rarely reached
    return self.slow_path(key);
}
```

### Optimization Barrier

`builtin::black_box(value)` returns `value` unchanged but never as a
compile-time constant, so the compiler does not fold away a computation reading
it. It emits no instruction. Use it to keep a test or benchmark measuring the
work it names:

```wado
test "sign extension folds the redundant mask" {
    assert to_i8(builtin::black_box(300)) == 44;
}
```

Without it, `to_i8(300)` folds to `44` and the test no longer reaches `to_i8`.

The barrier binds the Wado compiler only. The Wasm engine that runs the program
is still free to fold the value.

### Embedded Data

`List::<T>::from_le_bytes(bytes)` reads `bytes` as little-endian `T`s, back to
back, for any `T: FromLeBytes`: the fixed-width integers, `f16`, `bf16`, `f32`
and `f64`. It panics when the byte count is not a whole number of `T`s.

When the argument is a byte string literal or `#include_bytes` and `T` is
concrete, the compiler folds the call into a constant, so no decode loop runs at
startup. A long list is created from a data segment with one `array.new_data`,
and the Wasm grows by the byte count alone. A short one is built inline, where
that encodes smaller. Either way the result is an ordinary `List<T>`. A ragged
literal is not folded, and panics as it would at run time. Nor is a call whose
`T` is a newtype with its own `FromLeBytes`, which decides what the bytes mean.

`builtin::array_new_data::<T>(bytes)` is the same fold returning an `Array<T>`,
for a caller building its own container. It has no run-time form, so its
argument must be a literal, `T` must be a numeric primitive, and the byte count
must be a whole number of `T`s. Each is a compile error otherwise.

```wado
let weights = List::<f32>::from_le_bytes(#include_bytes("./weights.bin"));
let bias = List::<bf16>::from_le_bytes(b"\x80\x3f\x00\x40");   // [1.0, 2.0]
```

## Memory Model

### Core Principles

- Wasm-GC based: Garbage collection delegated to runtime
- Lifetime inference: No explicit lifetime annotations required
- Value semantics: a value is deeply copied on assignment, parameter passing, and return. References (`&T`, `&mut T`) share state instead, and an affine resource moves

### Value Semantics

See [WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md).

Assignment, parameter passing, and return all perform a deep copy of the value. Primitives, structs, `String`, and `List<T>` all follow this rule uniformly. There are two exceptions. Reference types (`&T`, `&mut T`) alias the underlying value. An affine resource is move-only: assignment, parameter passing, and return move it, and the source is unusable afterwards (see [Resource linearity](#resource-linearity)).

```wado
struct Point { x: i32, y: i32 }

let a = Point { x: 1, y: 2 };
let mut b = a;   // b is a deep copy of a
b.x = 10;        // does not affect a
assert a.x == 1;
```

In-place mutation through a parameter binding (field writes, method calls, index writes) operates on the callee's local copy and is not visible to the caller. To allow callee-side mutation, pass a reference explicitly:

```wado
fn translate(p: &mut Point, dx: i32, dy: i32) {
    p.x += dx;   // visible to caller (reference)
    p.y += dy;
}
```

These semantics are as-if. A program may rely on the value each expression
denotes; it may not rely on the number of copies performed to produce it.

## Type System

### Type Mapping at Component Boundaries

Wado types lift and lower to Component Model types when they cross a component boundary (the Canonical ABI). The compiler performs this conversion automatically.

The table below is the Wado↔CM correspondence, read in both directions: Wado→CM when generating a component's exported interface, and CM→Wado when importing an external component (`use { Iface } from "./c.wasm" with { type: "wasm" }`, see [Wasm Module and Component Imports](#wasm-module-and-component-imports)). CM types are written in their WIT spelling.

| Wado Type                 | CM Type at Boundary       | Notes                                                                                      |
| ------------------------- | ------------------------- | ------------------------------------------------------------------------------------------ |
| `bool`                    | `bool`                    | Boolean value                                                                              |
| `char`                    | `char`                    | Unicode scalar value                                                                       |
| `i8`, `i16`, `i32`, `i64` | `s8`, `s16`, `s32`, `s64` | Signed integers                                                                            |
| `u8`, `u16`, `u32`, `u64` | `u8`, `u16`, `u32`, `u64` | Unsigned integers                                                                          |
| `i128`, `u128`            | `record { low, high }`    | Prelude structs, so each crosses as its own record                                         |
| `f32`, `f64`              | `f32`, `f64`              | Floating point                                                                             |
| `String`                  | `string`                  | UTF-8 string                                                                               |
| `List<T>`                 | `list<T>`                 | Dynamic array                                                                              |
| `TreeMap<K, V>`           | `map<K, V>`               | `K` is `bool`, `char`, `String`, or an integer; a repeated key takes the last pair's value |
| `[T1, T2, ...]`           | `tuple<T1, T2, ...>`      | Tuple types                                                                                |
| `Option<T>`               | `option<T>`               | Optional value                                                                             |
| `Result<T, E>`            | `result<T, E>`            | Result type; `result<ok>` and bare `result` are the payload-elided forms                   |
| `struct { ... }`          | `record { ... }`          | Record                                                                                     |
| `enum { ... }`            | `enum { ... }`            | Enumeration without payloads                                                               |
| `variant { ... }`         | `variant { ... }`         | Variant/sum type with payloads                                                             |
| `flags { ... }`           | `flags { ... }`           | Bit flags                                                                                  |
| `resource`                | `resource`                | Resource handle; owned and borrowed handles both map here                                  |
| `Stream<T>`               | `stream<T>`               | Component Model async stream                                                               |
| `Future<T>`               | `future<T>`               | Component Model async future                                                               |

`f16` and `bf16` have no Component Model type, so they do not cross a component
boundary. An `export fn` whose signature names either one is a compile error.
See [WEP: Half-Precision Primitives](./wep-2026-09-22-half-precision-primitives.md).

### The Prelude

The prelude (`core:prelude`) is automatically imported into every module, providing access to fundamental types without requiring explicit imports:

#### Automatically Available

- `String` - UTF-8 string type
- `List<T>` - Dynamic array type
- `Option<T>` and its cases `Some(x)` and `None` (`null` also denotes `None`)
- `Result<T, E>` and its cases `Ok(x)` and `Err(e)`
- `Stream<T>` - Component Model async stream
- `Future<T>` - Component Model async future
- `i128`, `u128` - 128-bit integer types

A case is written bare (`Some(x)`) only where an expected type says which type
it belongs to. Elsewhere it is qualified: `Option::Some(x)`.

#### Disabling the Prelude

```wado
#![no_prelude]  // At the top of a module

// Now you must explicitly import everything
use {String, List, Option, Result, Stream, Future} from "core:prelude";
```

### Primitive Types

Primitive types are built into the language (no import required):

```wado
// Numeric
i8, i16, i32, i64
u8, u16, u32, u64
f32, f64
f16, bf16   // half precision: storage only, no arithmetic

// Basic
bool
char
```

`f16` and `bf16` hold a value but do no arithmetic, and `as` does not cast them.
`to_bits` / `from_bits` reach the bits, and `From` / `TryFrom` / `from_f32`
convert the value. A comparison widens both operands to `f32` and compares
those. See [WEP: Half-Precision Primitives](./wep-2026-09-22-half-precision-primitives.md).

### Associated Constants

Associated constants are compile-time constants defined in `impl` blocks using the `const` keyword. They cannot be mutated.

```wado
impl f64 {
    pub const PI: f64 = 3.14159265358979323846;
}

let pi = f64::PI;
```

Primitive types provide built-in associated constants and static methods. See [`core:prelude`](./stdlib-core-prelude.md) for the full list.

### 128-bit Integer Types (i128/u128)

Unlike primitive types, `i128` and `u128` are implemented as structs in the prelude. They can be used like primitives thanks to operator overloading:

```wado
let a: u128 = 42;                      // literal coercion
let b = u128::from_u64(1_000_000);     // explicit construction
let sum = a + b;                       // via Add trait
let cmp = a < b;                       // via Ord trait

// Access low/high 64-bit parts
let low = a.low();
let high = a.high();
```

Available operations:

| Category   | Operations                                                     |
| ---------- | -------------------------------------------------------------- |
| Arithmetic | `+`, `-`, `*`, `/`, `%`, unary `-` (i128)                      |
| Comparison | `==`, `!=`, `<`, `<=`, `>`, `>=`                               |
| Bitwise    | `&`, `\|`, `^`, `~`, `<<`, `>>`                                |
| Conversion | `from_u64()`, `from_i64()`, `low()`, `high()`, `as`, `TryFrom` |

Literal and range patterns work on them in every pattern position, nested ones
included: `match [x, y] { [1..=5, _] => … }`. Each pattern matches exactly when
the equivalent `==` or range comparison holds.

`as` casts follow Rust semantics in both directions:

```wado
let a = 42 as u128;           // numeric → wide int
let b = a as f64;             // wide int → float, correctly rounded (ties to even)
let c = a as i64;             // wide int → int, truncates to the low bits
let d = (-1 as i128) as u128; // i128 ↔ u128 reinterprets the bits (u128::MAX)
```

Checked conversions are available through `TryFrom` (e.g. `i64::try_from(a)`, `u128::try_from(n)`), returning `Err` when the value is out of range for the target type.

### SIMD Types (v128)

See [WEP: SIMD v128](./wep-2026-01-31-simd-v128.md) for full design and rationale.

Wado exposes WebAssembly SIMD via the `core:simd` module. A single primitive type `v128` represents a 128-bit vector, with 10 newtype aliases providing type-safe interpretations:

| Category | Types                              |
| -------- | ---------------------------------- |
| Signed   | `i8x16`, `i16x8`, `i32x4`, `i64x2` |
| Unsigned | `u8x16`, `u16x8`, `u32x4`, `u64x2` |
| Float    | `f32x4`, `f64x2`                   |

All SIMD newtypes share the `v128` base and can be reinterpreted via `as` cast (zero-cost). Each type provides `splat()` construction, lane access, per-lane comparison methods such as `eq` and `lt`, and the operators its lanes support. The sets differ by type, following the Wasm SIMD instruction set:

- `/` exists only on `f32x4` and `f64x2`, and the bitwise operators only on the integer types.
- `i8x16` and `u8x16` have no `*`.
- `u64x2` compares only with `eq` and `ne`.
- `i8x16` and `i16x8` read a lane with `extract_lane_s` or `extract_lane_u`; the other types use `extract_lane`.

Sequence literal coercion is supported via `impl From<Array<i32>> for i32x4`:

```wado
use { i32x4, f64x2 } from "core:simd";

let v: i32x4 = [1, 2, 3, 4];     // tuple literal coercion
let w = i32x4::splat(10);         // broadcast
let sum = v + w;                   // [11, 12, 13, 14]
let mask = v.lt(&w);              // per-lane comparison mask
```

Beyond basic arithmetic and comparison, types provide specialized operations: saturating arithmetic (`add_sat_s/u` and `sub_sat_s/u` on signed types, `add_sat` and `sub_sat` on unsigned ones), lane narrowing/extension, extended multiplication, pairwise addition, type conversion between integer and float, and bit selection. See the `core:simd` module documentation for the full API.

#### Relaxed SIMD

Relaxed SIMD operations trade strict determinism for performance. Edge-case behavior (NaN, out-of-range values) is implementation-defined but consistent within a single runtime. Methods use the `relaxed_` prefix on existing newtypes:

- Fused multiply-add: `f32x4/f64x2.relaxed_madd(b, c)`, `relaxed_nmadd(b, c)`
- Min/Max: `f32x4/f64x2.relaxed_min/max` (faster than strict `min`/`max`)
- Truncation: `i32x4::relaxed_trunc_f32x4_s/u`, `relaxed_trunc_f64x2_s/u_zero`
- Lane select: `relaxed_laneselect` on `i8x16`, `i16x8`, `i32x4`, `i64x2`
- Swizzle: `i8x16.relaxed_swizzle`
- Dot product: `i16x8.relaxed_dot_i8x16_i7x16_s`, `i32x4::relaxed_dot_i8x16_i7x16_add_s(a, b, &c)`
- Q15 multiply: `i16x8.relaxed_q15mulr_s`

Relaxed SIMD is not modeled as an effect because: (1) results are deterministic within an environment, (2) hardware behavior cannot be intercepted, and (3) standard floats already have similar NaN non-determinism.

### Reference Types

References in Wado provide indirect access to values. Unlike Rust, Wado uses a GC-based memory model with no borrow checker, enabling simpler semantics at the cost of runtime overhead.

#### Basic Reference Syntax

```wado
let x = 42;
let r = &x;           // Immutable reference
let v = *r;           // Dereference

let mut y = 0;
let mr = &mut y;      // Mutable reference
*mr = 10;             // Assign through reference
```

#### Reference to Reference

References can be nested arbitrarily:

```wado
let x = 42;
let r = &x;           // &i32
let rr = &r;          // &&i32
let val = **rr;       // 42 (double dereference)
```

#### Automatic Coercion (`&mut` to `&`)

Mutable references automatically coerce to immutable references when needed:

```wado
fn read_value(r: &i32) -> i32 {
    return *r;
}

let mut x = 10;
read_value(&mut x);   // OK: &mut i32 coerces to &i32
```

#### Key Differences from Rust (GC-Based Memory Model)

| Aspect                 | Rust                       | Wado                     |
| ---------------------- | -------------------------- | ------------------------ |
| Memory management      | Ownership + borrow checker | Garbage collection       |
| Multiple mutable refs  | Not allowed                | Allowed                  |
| Returning local refs   | Not allowed (dangling)     | Allowed (GC keeps alive) |
| Reference to reference | `&&T` (rare)               | `&&T` (fully supported)  |
| Lifetime annotations   | Required                   | Not needed               |
| Borrow checking        | Compile-time               | None; resources move     |

#### Returning References to Local Variables

Because Wado uses garbage collection, references to local variables remain valid after the function returns:

```wado
fn make_ref() -> &i32 {
    let x = 42;
    return &x;  // OK in Wado (x is GC-managed and stays alive)
}

let r = make_ref();
println(`${*r}`);  // Works: prints "42"
```

This would be a dangling pointer error in Rust, but is safe in Wado due to garbage collection.

#### Multiple Mutable References

Wado allows multiple mutable references to the same value:

```wado
let mut x = 10;
let r1 = &mut x;
let r2 = &mut x;  // OK in Wado (no borrow checker)

*r1 = 20;
*r2 = 30;
```

#### Design Trade-offs

- Simplicity: No lifetime annotations or borrow checker errors
- Flexibility: Can freely share and modify references
- Cost: Runtime overhead from garbage collection
- Safety: Memory safety guaranteed by GC, not compile-time checks

#### Method Receiver: `self` by Value

A method receiver is `&self` or `&mut self`. Bare `self` (by value) is allowed only on a resource, or on an aggregate that holds one:

```wado
impl Point {
    fn sum(&self) -> i32 { ... }          // OK: immutable reference
    fn reset(&mut self) { ... }           // OK: mutable reference
    // fn consume(self) -> i32 { ... }    // ERROR: `self` by value is only allowed on a resource
}
```

A by-value `self` moves the receiver into the method, so the caller's binding cannot be used afterward. That is how an affine resource is consumed (see [Resource linearity](#resource-linearity)). A value type has nothing to consume, so `self` by value on one is a compile error. See [WEP: Resource Ownership](./wep-2026-05-21-resource-ownership.md).

#### `mut` Parameters

A parameter can be declared `mut` to allow the function body to reassign it:

```wado
fn increment(mut n: i32) -> i32 {
    n += 1;   // mutates the local copy
    return n;
}

fn normalize(mut s: String) -> String {
    s = s.to_ascii_uppercase();  // rebinds local binding
    return s;
}
```

The `mut` keyword grants write access to the local parameter binding inside the function. Wado uses value semantics for every parameter: every value is deeply copied when passed to a function. This applies uniformly to primitives, structs, `String`, and `List<T>`. References (`&T`, `&mut T`) are the exception: they share state with the caller. An affine resource is never copied: passing it moves it. Inside the callee, reassignment (`p = new_value`) and in-place mutation operate on the callee's local copy and are not visible to the caller. In-place mutation covers field writes (`p.x = ...`), method calls (`s.push_str("!")`, `arr.push(0)`), and index writes (`arr[0] = ...`). To let the callee mutate the caller's value, declare the parameter as `&mut T` and pass a `&mut`-reference at the call site.

```wado
fn countdown(mut n: i32) with Stdout {
    while n > 0 {
        println(`${n}`);
        n -= 1;         // only modifies the local copy
    }
}

let x = 3;
countdown(x);
// x is still 3 — every parameter is passed by value
```

Closures also support `mut` parameters:

```wado
let add_one = |mut n: i32| {
    n += 1;
    return n;
};
```

Without `mut`, any assignment to a parameter is a compile error:

```wado
fn bad(n: i32) {
    n = 10;  // Error: cannot assign to immutable variable 'n'
}
```

### String Type

`String` is a built-in type representing UTF-8 encoded text with value semantics and GC management.

#### Design Principles

- Value semantics: deep-copied on assignment, parameter passing, and return — passing a `String` to a function gives the callee its own buffer
- Mutable through the local binding: `push_str` modifies the receiver in place and `+=` reassigns the binding, but neither reaches the caller's value
- GC-managed: Memory is automatically managed by Wasm GC
- UTF-8 encoding: Direct mapping to Component Model `string`

#### Semantics and Encoding

- Semantically, a `String` is a sequence of Unicode scalar values
- Invalid UTF-8 byte sequences are not allowed; all String values must be valid UTF-8
- This ensures interoperability with Component Model `string` type and safe string operations

#### Index Access (Prohibited)

Direct index access is prohibited to avoid ambiguity between byte and character indexing:

```wado
let s = "Hello世界";

// Prohibited
s[0]      // Compile error
s[0..<5]  // Compile error
```

Use explicit methods instead:

```wado
// Byte-level access
let bytes: List<u8> = s.bytes().collect();
let first_byte = bytes[0];

// Character-level access
let chars: List<char> = s.chars().collect();
let first_char = chars[0];

// Other methods
s.len() -> i32             // Length in bytes
s.is_empty() -> bool       // Check if empty
```

##### Note

`bytes()` and `chars()` return iterator objects (`StrUtf8ByteIter` and `StrCharIter`) that implement both `Iterator` and `IntoIterator`, so they work with `for-of` directly:

```wado
for let c of "hello".chars() {
    println(`${c}`);  // h, e, l, l, o
}

for let b of "hello".bytes() {
    println(`${b}`);  // 104, 101, 108, 108, 111
}
```

##### String Building

`push_str` appends to a `String` in place:

```wado
let mut builder = String::with_capacity(20);
builder.push_str("Hello");
builder.push_str(", ");
builder.push_str("World!");
// builder is now "Hello, World!"

// `+` operator for two-string concatenation
let combined = "Hello, " + "World!";  // "Hello, World!"

// `join` concatenates a list with a separator
let parts: List<String> = ["a", "b", "c"];
let joined = parts.join(",");         // "a,b,c"
```

#### Concatenation

##### New String (`+` operator)

```wado
let s1 = "hello";
let s2 = " world";
let s3 = s1 + s2;  // Creates new String
```

##### Reassignment (`+=` operator)

`a += b` is `a = a + b` through `Add`. `String` follows the same rule as every other type:

```wado
let mut s = "hello";
s += " world";     // s = s + " world"
s += "!";
```

See `docs/wep-2026-01-15-string-type-design.md` for design rationale.

### Primitive Literals

#### Boolean Literals

```wado
let active = true;
let disabled = false;
```

#### Null Literal

The `null` keyword is equivalent to `None` and represents the absence of a value:

```wado
let missing: Option<i32> = null;            // Same as None
let also_missing = Option::<i32>::None;

// Both are equivalent
assert missing == also_missing;
```

Note: `null` is a language keyword, while `None` is a case of the prelude's `Option`. Bare `None` needs an expected type to say which `Option` it belongs to.

A bare `null` — one no expected type has pinned — has the type `Option<!>`: a value of every `Option<T>` and of no other type. That is what a type converts from to accept `null` where an `Option` is not expected, which is how `core:value::Value` takes JSON's `null` in a literal:

```wado
impl From<Option<!>> for Value {
    fn from(value: Option<!>) -> Value {
        return Value::Null;
    }
}

let doc: Value = { name: "Alice", nickname: null };
```

#### Character Literals

Character literals use single quotes and represent a Unicode scalar value. `char` is a distinct type with Unicode semantics, not an integer, just as `String` is not `List<u8>`:

```wado
let letter = 'A';
let digit = '9';
let unicode = '\u0041';  // Unicode escape (same as 'A')
let emoji = '😀';        // Direct Unicode character
let newline = '\n';
```

See [Escape Sequences](#escape-sequences) for the supported escapes.

```wado
let a = '\u0041';         // 'A' (BMP)
let smiley = '\u{1F600}'; // '😀' (non-BMP)
```

##### char Casting and Conversion

`char` can be cast to any integer type to extract the Unicode scalar value (possibly truncated for smaller types):

```wado
let c = 'A';
let code = c as i32;    // 65
let ucode = c as u32;   // 65
let byte = c as u8;     // 65 (truncated to low byte)
```

`u8 as char` is allowed because all `u8` values (0..255) are valid Unicode scalar values:

```wado
let byte: u8 = 65;
let c = byte as char;  // 'A'
```

All other integer-to-char casts are prohibited because not all values are valid Unicode scalar values (surrogates `0xD800..0xDFFF` and values `> 0x10FFFF` are invalid):

```wado
let x: i32 = 65;
let c = x as char;  // compile error

let y: i8 = 65;
let c = y as char;  // compile error (i8 can be negative)
```

Use checked conversion functions instead:

```wado
let c = char::from_u32(65 as u32);  // Option<char>: Some('A')
let c = char::from_i32(65);         // Option<char>: Some('A')
```

See [`core:prelude`](./stdlib-core-prelude.md) for the full `char` API.

Casting `char` to non-integer types is a compile error:

```wado
let c = 'A';
let f = c as f64;     // compile error: char can only be cast to integer types
let s = c as String;  // compile error: the two types share no representation
```

#### Integer Literals

```wado
let decimal = 42;
let negative = -17;
let with_separator = 1_000_000;    // Underscores for readability
let binary = 0b1010_1100;          // Binary
let octal = 0o755;                 // Octal
let hex = 0xFF_AA_BB;              // Hexadecimal
```

##### Type coercion

When the target type is known from context (type annotation or function argument), integer literals coerce to any compatible integer type, including `i128`/`u128`:

```wado
let byte: i8 = 127;
let long: i64 = 9_223_372_036_854_775_807;
let unsigned: u32 = 4_294_967_295;
let big: u128 = 1_000_000_000_000;
fn foo(n: i64) { ... }
foo(100);  // literal coerced to i64
```

##### Compile-time range checking

The compiler rejects literal coercions whose value falls outside the target type's range. All literal bases (decimal, hex `0x`, octal `0o`, binary `0b`) use strict numeric range: the value must lie within `[MIN, MAX]` for signed types or `[0, MAX]` for unsigned types.

To reinterpret a bit pattern as a signed integer, use an explicit `as` cast.

```wado
let a: i8 = 127;                  // OK: max i8
let b: i8 = 128;                  // compile error: literal out of range for `i8`: 128
let c: i8 = -128;                 // OK: min i8
let d: u32 = -1;                  // compile error: literal out of range for `u32`: -1

let e: i8 = 0xFF;                 // compile error: literal out of range for `i8`: 0xFF
let f: i8 = 0xFF as i8;           // OK: explicit bit-pattern reinterpretation (value: -1)
let g: i32 = 0xFFFF_FFFF;         // compile error: literal out of range for `i32`: 0xFFFF_FFFF
let h: i32 = 0xFFFF_FFFF as i32;  // OK: explicit bit-pattern reinterpretation (value: -1)
let i: u32 = 0x1_0000_0000;       // compile error: literal out of range for `u32`: 0x1_0000_0000
```

A literal that nothing coerces falls back to `i32`, and the same range check applies there. `-NUM` is checked as one literal, so it reaches the signed minimum.

```wado
let j = 2147483647;               // OK: max i32
let k = 4294967296;               // compile error: literal out of range for `i32`: 4294967296
let l = -2147483648;              // OK: min i32
let m = -2147483649;              // compile error: literal out of range for `i32`: -2147483649
```

Type conversion (via `as`):

```wado
let byte: i8 = 127 as i8;
let long: i64 = 9_223_372_036_854_775_807 as i64;
let unsigned: u32 = 4_294_967_295 as u32;
```

#### Floating-Point Literals

```wado
let pi = 3.14159;
let with_separator = 1_000_000.5;
let scientific = 6.022e23;         // 6.022 × 10²³
let negative_exp = 1.6e-19;        // 1.6 × 10⁻¹⁹
let explicit_positive = 2.5e+10;
```

##### Type coercion

Floating-point literals coerce to `f32`, `f64`, `f16` or `bf16` when the target type is known:

```wado
let single: f32 = 3.14;
let double: f64 = 3.14159265358979;
let half: f16 = 0.5;
let weights: List<bf16> = [0.5, -1.25, 3.0];
```

A literal is rounded once, from its decimal text to the nearest value of its
type, ties to even. One that rounds past the type's largest finite value is a
compile error, as an integer literal past its type's range is:

```wado
let x: f16 = 65520.0;             // compile error: literal out of range for `f16`: 65520.0
let y: f32 = 1e39;                // compile error: literal out of range for `f32`: 1e39
```

Type conversion (via `as`):

```wado
let single: f32 = 3.14 as f32;
let double: f64 = 3.14159265358979 as f64;
```

#### String Literals

String literals create `String` values.

Regular strings use double quotes:

```wado
let name = "Alice";           // Type: String
let path = "path/to/file.txt";
let escaped = "Line 1\nLine 2\tTabbed";
```

Byte strings use a `b` prefix and create a constant `ByteList` (the
first-class byte-buffer newtype over `List<u8>`):

```wado
let magic = b"\x89PNG\r\n";             // Type: ByteList, value [137, 80, 78, 71, 13, 10]
let raw: List<u8> = b"\x89PNG\r\n";     // Also OK: newtype literal coercion to the base
```

The content must be ASCII; each `\xNN` escape (two hex digits) or source
character contributes one byte, and the standard escapes (`\n`, `\t`, `\\`,
`\"`, `\0`, `\r`, `\'`) are also accepted. Unicode escapes (`\u{...}` /
`\uHHHH`) are rejected — a Unicode escape denotes a scalar, not a byte; use
`\xNN` for a raw byte. (`#include_bytes("path")` produces the same `ByteList` from a file.)
The default type is `ByteList`, but newtype literal coercion lets it flow into a
`List<u8>` context (or any type whose base is `List<u8>`) with no cast.

Byte literals are the single-byte analog: `b'x'` is one `u8`.

```wado
let a = b'A';              // u8, 65
let hi = b'\xff';          // u8, 255
let n: i32 = b'A';         // 65 — coerces like an integer literal
```

A byte literal is an integer literal defaulting to `u8`, so it coerces like any
integer literal (its value is always `0..=255`). Its content follows the same
one-byte rule as a byte string (`\xNN` for `0x80..=0xFF`; no `\u`); use a char
literal `'…'` for a Unicode scalar.

#### Escape Sequences

Escape sequences are shared between character, string and template literals,
except where the table names one:

| Escape   | Character                   |
| -------- | --------------------------- |
| `\'`     | Single quote                |
| `\"`     | Double quote                |
| `` \` `` | Backtick (template only)    |
| `\\`     | Backslash                   |
| `\/`     | Forward slash               |
| `\b`     | Backspace                   |
| `\f`     | Form feed                   |
| `\n`     | Newline                     |
| `\r`     | Carriage return             |
| `\t`     | Tab                         |
| `\0`     | Null                        |
| `\$`     | Dollar sign (template only) |
| `\{`     | Left brace (template only)  |
| `\}`     | Right brace (template only) |
| `\uHHHH` | Unicode BMP (4 hex digits)  |
| `\u{H+}` | Unicode full range          |

In a template string only `${` opens an interpolation, so `{` and `}` are
literal and need no escaping, though `\{` and `\}` are accepted. Use `\$` to
write a literal `$` before a `{` (e.g. `` `\${x}` `` renders the text `${x}`).

For characters outside BMP (U+10000 and above), use either:

```wado
"\uD83D\uDE00"   // Surrogate pair
"\u{1F600}"      // Variable-length escape
"😀"             // Direct Unicode character
```

Template strings (interpolation) use backticks. Interpolation is introduced
with `${expr}` (ES/TypeScript-style); a bare `{` or `}` is literal text, so
JSON-like content needs no escaping:

```wado
let name = "Alice";
let greeting = `Hello, ${name}!`;  // "Hello, Alice!"

let count = 42;
let message = `Count: ${count}`;   // "Count: 42"

// Format specifiers
let pi = 3.14159;
let formatted = `Pi: ${pi:.2}`;   // "Pi: 3.14"
let hex = `${255:x}`;             // "ff"
let sci = `${1200:e}`;            // "1.2e3" (integers as well as floats)
let padded = `${"あい":>6}`;       // "    あい" (width counts characters)

// Inspect (debug) format — works for any type
let p = Point { x: 10, y: 20 };
let debug = `${p:?}`;            // "Point { x: 10, y: 20 }"
let pretty = `${p:#?}`;          // pretty-print: "Point {\n  x: 10,\n  y: 20,\n}"
// `${p}` (Display) needs an `impl Display` for `Point`; use `${p:?}` for debug output.

// Braces are literal — JSON embeds cleanly without escaping
let json = `{"key": "${name}"}`;  // {"key": "Alice"}
```

See [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md) for the full specifier table, [WEP: Format Traits](./wep-2026-02-01-format-traits.md) for the trait/Formatter infrastructure, and [WEP: Inspect](./wep-2026-02-21-inspect-debug-output.md) for the `:?` debug output format.

Multiline strings are supported in both regular and template strings. Literal newlines are preserved:

```wado
// Regular multiline string
let poem = "Roses are red,
Violets are blue,
Wado is great,
And so are you!";

// Multiline template string
let name = "Alice";
let message = `Dear ${name},

Welcome to Wado!

Best regards`;
```

#### Tuple Literals

Bracket syntax `[...]` creates tuple values by default. This aligns with TypeScript conventions and JSON interoperability.

```wado
let pair = [1, "hello"];              // Type: [i32, String]
let triple = [42, "answer", true];    // Type: [i32, String, bool]
let single = [42];                    // Type: [i32] (1-tuple)
let empty_tuple: [] = [];             // Empty tuple (distinct from unit ())
let trailing = [1, 2, 3,];            // Trailing comma allowed
```

##### Tuple Types

Tuple types use bracket syntax `[T1, T2, ...]`.

```wado
let point: [i32, i32] = [10, 20];
let record: [String, i32, bool] = ["Alice", 30, true];
```

##### Tuple Element Access

Tuple elements are accessed by constant index using dot notation or bracket notation:

```wado
let t = [10, "hello", true];
let x = t.0;      // 10 - dot notation
let y = t[1];     // "hello" - bracket notation
let z = t.2;      // true

// Variable index is not allowed (compile error)
let i = 1;
let w = t[i];     // Error: tuple index must be a constant integer
```

##### Unit vs Empty Tuple

The unit type `()` and empty tuple `[]` are distinct:

```wado
let unit: () = ();    // Unit type/value
let empty: [] = [];   // Empty tuple (rarely used)
```

They take separate `impl`s, so a method defined on one is not found on the other.

`[]` cannot cross a component boundary. It has no Component Model representation:
a `tuple` carries at least one type, and `()` is the type that carries none. An
export naming one is rejected at compile time.

##### The `never` type (`!`) — bottom type

`never` is the bottom type: it is a subtype of every type. An expression of type `never` never returns — it always diverges (traps). `panic()` and `unreachable()` both return `!`.

Because `never` is assignable to any type, a `never`-typed expression may appear in any value position without a type mismatch:

```wado
// In a match arm — the None branch panics, so the match has type i32
let opt: Option<i32> = Option::<i32>::Some(5);
let x = match opt {
    Some(v) => v,
    None => panic("unexpected none"),
};

// In a let binding with explicit type annotation
let y: i32 = panic("unreachable");

// In a binary expression — execution diverges before the addition
let z: i32 = panic("boom") + 1;
```

The `!` type can be written explicitly as a return type:

```wado
fn fail(msg: String) -> ! {
    panic(msg);
}
```

#### List Literals

A bracket literal becomes a `List` through an explicit `as`, or through implicit coercion where the target type is known.

```wado
// Explicit conversion with `as`
let numbers = [1, 2, 3, 4, 5] as List<i32>;

// Implicit coercion (target type known)
fn takes_list(a: List<i32>) { ... }
takes_list([1, 2, 3]);  // OK - compiler knows List<i32> is expected

// Type annotation
let explicit: List<i32> = [1, 2, 3];  // Coerced to List
```

##### Coercion Rules

- Where the target type is known (a function parameter, a type annotation), the literal coerces implicitly
- Elsewhere it is a tuple, and `as List<T>` converts it

```wado
let t = [1, 2, 3];               // Tuple [i32, i32, i32] - no context
let a = [1, 2, 3] as List<i32>; // List - explicit conversion

fn process(data: List<i32>) { ... }
process([1, 2, 3]);              // OK - implicit coercion
```

##### Design Rationale

This design aligns with TypeScript (primary target audience) and enables intuitive JSON interoperability. JSON arrays are heterogeneous and map naturally to tuples:

```json
{ "point": [10, 20], "mixed": [1, "hello", true] }
```

```wado
// A JSON array maps naturally to a tuple:
let point: [i32, i32] = [10, 20];
let mixed: [i32, String, bool] = [1, "hello", true];
```

See `docs/wep-2026-01-15-tuple-and-array-literals.md` for detailed rationale.

##### List Constructors

```wado
let arr = List::<i32>::with_capacity(10);     // empty list with room for 10 elements
let bools = List::<bool>::filled(100, true);  // list of 100 elements, all true
```

##### List Operations

```wado
let mut arr: List<i32> = [1, 2, 3];

// Index access (read)
let first = arr[0];  // 1

// Index assignment (write)
arr[0] = 100;        // Requires a `let mut` binding
arr[1] = 200;

// List methods
arr.push(4);         // Add element to end
let len = arr.len(); // Get length
```

##### Index Assignment Rules

- Requires the list binding to be declared with `let mut`
- Index must be within bounds (runtime check, traps if out of bounds)
- Works with lists of any element type

Sorting (stable, O(n log n) worst case):

| Method        | Mutates? | Comparator                          |
| ------------- | -------- | ----------------------------------- |
| `sort()`      | Yes      | `Ord::cmp` (requires `T: Ord`)      |
| `sort_by()`   | Yes      | Custom `fn mut(&T, &T) -> Ordering` |
| `sorted()`    | No       | `Ord::cmp` (requires `T: Ord`)      |
| `sorted_by()` | No       | Custom `fn mut(&T, &T) -> Ordering` |

On a float, `Ord` is the IEEE 754 total order rather than the order `<` gives,
so a NaN still has a place in a sorted list. See
[WEP: The Operator Order and the Total Order](./wep-2026-09-23-comparison-traits.md).

```wado
let mut nums: List<i32> = [5, 3, 8, 1];
nums.sort();                             // in-place ascending

let orig: List<i32> = [5, 3, 8, 1];
let asc = orig.sorted();                // returns a new sorted list
```

#### Collection Literal Coercion

Sequence literals `[e0, e1, ...]` and key-value literals `{ k: v, ... }` can be
coerced to any collection type through `From`. Coercing one materializes an
`Array`, which the target's `From<Array<…>>` impl builds from:

| Literal         | Materializes    | Impl the target writes                            |
| --------------- | --------------- | ------------------------------------------------- |
| `[e0, e1, ...]` | `Array<E>`      | `From<Array<T>> for List<T>`                      |
| `{ k: v, ... }` | `Array<[K, V]>` | `From<Array<[String, V]>> for TreeMap<String, V>` |

This applies only where a coercion runs. The tuple and struct readings keep
their priority: with no target type `[1, 2, 3]` is still the tuple
`[i32, i32, i32]` (see [List Literals](#list-literals)), and `{ … }` against a
nominal struct with matching fields is still a struct literal.

A key-value literal is an array of pairs, so `[["a", 1]]` builds the same map
`{ a: 1 }` does. `Array<T>` itself needs no impl — the array the coercion
materializes is already the result.

##### Usage

```wado
let arr: List<i32> = [1, 2, 3];

use { TreeMap } from "core:collections";
let map: TreeMap<String, i32> = { width: 1920, height: 1080 };
```

Making a user type literal-constructible is one ordinary impl:

```wado
impl<T> From<Array<T>> for MyVec<T> {
    fn from(elements: Array<T>) -> MyVec<T> { ... }
}
```

Where a type accepts both literal forms, `{ … }` takes the impl whose element
is a two-element tuple and `[ … ]` prefers the one whose element is not;
several candidates for one form are an ambiguity error the site reports, and
`T::from(…)` written out resolves it.

##### Implicit conversion

A literal is implicitly converted to its target type through `From`. No other
expression is implicitly converted — a literal position is the whole of it.

```wado
let v: List<Value> = [1, "x"];   // OK — every element is a literal
let v: List<Value> = [a, b];     // ERROR — write [Value::from(a), Value::from(b)]
```

Coercion is literal-only — it does not apply to bound variables. If the target
type is a struct with matching fields, it is interpreted as a struct literal and
coercion is not attempted.

##### `..base` spread

`..base` inside a literal merges through `LiteralSpread`, last write wins:

```wado
pub trait LiteralSpread with () {
    fn spread_literal(&mut self, base: Self);
}
```

A type without the impl rejects `..base` where it is written, and a sequence
literal cannot carry one at all — `[..xs, 4]` is a tuple spread.

See [`docs/wep-2026-08-24-literal-from-array.md`](./wep-2026-08-24-literal-from-array.md)
for the impl-selection rule and newtype targets.

### Compile-Time Location Literals

Compile-time location literals provide source location information at compile time. They use the `#` prefix to clearly signal compile-time evaluation.

| Literal                  | Type       | Value                                              |
| ------------------------ | ---------- | -------------------------------------------------- |
| `#file`                  | `String`   | Current source file path                           |
| `#line`                  | `i32`      | Current line number (1-indexed)                    |
| `#function`              | `String`   | Name of the enclosing function                     |
| `#data`                  | `String`   | `__DATA__` section content (compile error if none) |
| `#include_str("path")`   | `String`   | External file content as string                    |
| `#include_bytes("path")` | `ByteList` | External file content as bytes                     |

```wado
fn example() {
    println(`Error at ${#file}:${#line}`);
    println(`In function: ${#function}`);
}
```

#### `#data`

Returns the raw text content of the `__DATA__` section as a `String`. This is useful for programs that need to access embedded metadata at runtime (e.g., configuration, test fixtures, embedded documents). Using `#data` in a file that has no `__DATA__` section is a compile error.

```wado
export fn run() with Stdout {
    let config = #data;  // contains the __DATA__ section text
    println(config);
}

__DATA__
{"key": "value"}
```

#### `#include_str` and `#include_bytes`

`#include_str("path")` reads an external file at compile time and returns its content as a `String`. The file must be valid UTF-8; otherwise, a compile error is raised. `#include_bytes("path")` returns the raw bytes as `ByteList` without UTF-8 validation.

The path argument must be a string literal. Paths are resolved relative to the source file containing the expression. See [WEP: Compile-Time File Inclusion](./wep-2026-03-02-include-str.md).

```wado
let template = #include_str("./templates/header.html");
let icon: ByteList = #include_bytes("./assets/logo.png");
```

#### `#function` Format

Returns the name without type arguments or signature:

| Context                 | `#function` value            |
| ----------------------- | ---------------------------- |
| Free function           | `my_function`                |
| Method                  | `Point::distance`            |
| Method of `Box<String>` | `Box::name`                  |
| Closure                 | `parent_function::{closure}` |

#### Call-site evaluation in default arguments

As a [default argument](#default-arguments), `#file` / `#line` / `#function` evaluate at the call site, so a defaulted location parameter reports the caller (cf. Swift's `#file`/`#line` defaults, C++'s `std::source_location::current()`):

```wado
pub fn log(msg: String, file: String = #file, line: i32 = #line) { /* ... */ }

log("started"); // file/line report this call, not where `log` is defined
```

Name resolution in a default otherwise uses the callee's scope; only these three literals are redirected. `#data` / `#include_str` / `#include_bytes` and struct field defaults always report their own defining file. For a nested defaulted call (`fn outer(x = loc())`), every literal reports the outermost call site (`outer(...)`).

### Closures

Closures are anonymous function expressions with `|params| body` syntax.

An expression body returns its value implicitly:

```wado
let add_one = |x: i32| x + 1;
let make_point = |x: i32, y: i32| Point { x, y };
```

A block body requires explicit `return`:

```wado
let compute = |x: i32| {
    let doubled = x * 2;
    return doubled + x * 3;
};
```

An optional `-> Type` declares the return type, and is what a `?` in the body
resolves against:

```wado
let parse = |s: String| -> Result<i32, String> {
    let n = to_int(s)?;
    return Result::Ok(n + 1);
};
```

A parameter type is inferred from the expected `fn(..)` type, matched by
position. Any context that supplies such a type counts: a typed binding, a
function or method parameter, a struct field, a newtype over a `fn(..)`.
Annotate only where nothing supplies one; an annotation always wins:

```wado
let arr: List<i32> = [1, 2, 3];
arr.into_iter().map(|x| x * 2);             // `x: i32`, from `Iterator::Item`
arr.into_iter().fold(0, |acc, x| acc + x);  // `acc: i32`, from the body
let add_one = |x: i32| x + 1;               // no expected type: annotate
```

The expected type may be one of the callee's own type parameters. A sibling
argument then supplies it, whichever side of the closure it is written on. A
numeric-literal sibling does not: `fold(0, |acc, x| acc + x)` over a `List<i64>`
takes `i64` from the body, not `i32` from the `0`. A parameter nothing supplies
is reported at the call, not inside the closure.

A `?` in the body needs the return type known — via `-> Type` or an expected
`fn(..) -> R`.

A closure declares no effects; they are inferred from the body.
`with` after the parameter list, or after `-> Type`, would be that declaration,
and is a compile error. A handler body therefore needs a block or parentheses:

```wado
let f = || (with Log => &mut sink do { Log::emit(`hi`); });
```

Closures auto-capture each free variable by reference; the reference kind is inferred from body usage (`&T` for read-only, `&mut T` for mutating). Pure read-only captures keep the closure type at `fn`; any `&mut` capture promotes it to `fn mut`. Calling a `fn mut` closure requires the _root_ of the callee place to be a mutable binding (mirrors Rust's `FnMut` rule); this applies whether the closure is called directly (`f()`) or reached through field access or indexing (`(h.f)()`, `arr[i]()`). A temporary root — a call result, a literal — has no binding and is always accepted.

Shared mutable state across closures is automatic — multiple closures referring to the same outer binding share the underlying location, with no explicit reference dance needed:

```wado
let mut count = 0;
let mut inc = || count += 1;   // captures &mut count; type fn mut() -> ()
let get = || count;             // captures &count; type fn() -> i32
inc();
inc();
assert get() == 2;
```

See [`docs/wep-2026-01-16-closure-implementation.md`](./wep-2026-01-16-closure-implementation.md) for the full design (`fn` vs `fn mut`, sub-typing, effect generics, iterator API integration).

A closure capturing an outer binding is a separate concept from what a function
does with its reference _parameters_, which nothing in the language states — see
[WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md).

### Function References

A bare function name is an expression of function type. It evaluates to a value of type `fn(P...) -> R [with E...]` matching the function's signature.

```wado
fn double(n: i32) -> i32 { return n * 2; }

let f = double;            // type: fn(i32) -> i32
assert f(21) == 42;

apply(double, 21);         // pass directly; no `&` needed
let g: fn(i32) -> i32 = double;
```

Key points:

- Function values carry no observable identity. There is no state to observe, and no way to compare two `fn` values.
- `&` and `&mut` apply to `fn`-typed values like to any other value, with no special-casing:
  - `&f` has type `&fn(...)`; `&mut f` (on a mutable binding) has type `&mut fn(...)`.
  - These references behave per the [Reference Types](#reference-types) rules. `&fn(...)` is _not_ a synonym for `fn(...)`; passing one where the other is expected is a type error.
  - `&mut fn(...)` parameters are useful as out-parameters: the callee can reassign the referenced slot via `*p = other_fn`, and the caller observes the new value through the same binding.
- A `&fn(...)` or `&mut fn(...)` value is directly callable; the call expression auto-derefs to invoke the underlying `fn(...)`. `let r = &double; r(21)` works without an explicit `*r`.
- Generic functions taken as values need their type arguments pinned. Two principled forms are supported:
  - Turbofish on the name itself: `let f = identity::<i32>;` evaluates to a `fn(i32) -> i32` value, and a non-call use like `apply(identity::<i32>, 7)` works the same way.
  - An expected `fn(...)` type at the use site: `let f: fn(i32) -> i32 = identity;` and `apply(identity, 7)` (where `apply`'s parameter is `fn(i32) -> i32`) both pin the type arguments through positional inference against the expected signature.
  - When neither form applies, it is a compile error, and the diagnostic suggests turbofish or a closure wrapper (`|x| identity(x)`).
- A function type cannot cross the Component Model boundary: an `export fn` whose signature names one is a compile error. See the closure WEP.

### Default Arguments

See [WEP: Default Arguments](./wep-2026-04-11-default-arguments.md).

Trailing function parameters may declare default values with `= expr`. Calls that omit defaulted arguments are expanded at the call site, with no runtime cost:

```wado
fn connect(host: String, port: i32 = 8080, timeout: i32 = 30) { ... }

connect("localhost");           // → connect("localhost", 8080, 30)
connect("localhost", 3000);     // → connect("localhost", 3000, 30)
connect("localhost", 3000, 60);
```

#### Rules

- All defaulted parameters must come after all non-defaulted parameters.
- Default expressions must be effect-free (validated by the effect system).
- Default expressions may reference earlier parameters in the same function:

```wado
fn make_rect(width: f64, height: f64 = width) -> Rect { ... }
make_rect(10.0);  // → make_rect(10.0, 10.0)
```

- Default expressions may name a type parameter of the declaration that wrote them. It stands for the type argument the call site settled on, whether a turbofish spelled it, an argument beside it pinned it, or the parameter's own default supplied it:

```wado
fn info<T: Default>(msg: String, fields: T = T::default()) -> String { ... }
info::<i32>("count");  // → info::<i32>("count", 0)
```

The same holds for an instance or static method, where the `impl` block's parameters come from the receiver, and for a struct field default, where they come from the type the literal is annotated with.

#### Restrictions

- `self` cannot have a default.
- Function types do not carry default information; assigning a function with defaults to a `fn(...)` type erases them, and every call site of that variable must supply every argument.
- Closures cannot declare defaults: a closure value's arity must match its `fn(...)` type, so `= expr` on a closure parameter is a compile error.
- `export fn` cannot declare defaults — exported functions appear in the component's WIT signature where every parameter is required by the CM ABI. Split into a private helper plus a thin `export fn` wrapper if defaults are needed.
- Trait methods may declare defaults only in the trait definition; implementations receive every parameter and cannot add, remove, or change defaults. Direct `impl Type { ... }` methods (not part of any trait) may declare defaults freely.

#### Type Parameter Defaults

A type parameter may declare a default with `= Type`, on a free function, an inherent method or a trait method. An omitted turbofish takes the default; a spelled one wins. `core:log` uses it:

```wado
pub fn info<T: Serialize = NoFields>(message: String, fields: T = NoFields {}, ...) { ... }

info("started");                  // → info::<NoFields>("started", NoFields {})
info::<Fields>("started", f);
```

Inference runs first and the default fills only what it left unbound, so an argument or an expected type always decides the slot it pins.

A default resolves in the declaring module's scope, as a value default does. It may therefore name a type the call site cannot: `NoFields` above is private to `core:log`. By the same rule a parameter the use site declares does not answer for it, however the two are spelled:

```wado
struct Zero {}
struct Marked<M: Mark = Zero> { value: i32 }

fn f<Zero: Mark>(probe: Zero) -> i32 {
    let m: Marked = Marked { value: 1 };  // the module's `Zero`, not `f`'s
    return m.total();
}
```

A default may name a parameter to its left, and stands for that parameter's argument. One naming a parameter at or after its own slot is rejected, since no argument has settled it yet:

```wado
struct Both<A, B = A> { v: A }        // OK: `B` takes `A`'s argument
struct Fwd<A = B, B = i32> { v: B }   // ERROR: `A`'s default names `B`
struct Own<A = A> { v: i32 }          // ERROR: the same, one slot nearer
```

Expanding a default must reach a fixpoint. One that leads back to the declaration it belongs to is rejected, whether it names that declaration directly, under an argument, or through another declaration's defaults:

```wado
struct Rec<T = Rec> { v: i32 }           // ERROR
struct Pair<A, B = Pair<A>> { v: i32 }   // ERROR
struct Ping<X, Y = Pong<X>> { v: i32 }   // ERROR, paired with
struct Pong<X, Y = Ping<X>> { v: i32 }   // this one
```

A trait method's type parameter default belongs to the trait, exactly as its value defaults do. The implementation restates the list — the same parameters in the same order, with the defaults omitted — and every spelling of the call fills them from the trait's declaration:

```wado
pub trait Boxed {
    fn boxed<T: Named = Tag>(&self) -> String;   // `Tag` is private to this module
    fn made<T: Named = Tag>() -> String;
}

impl Boxed for M {
    fn boxed<T: Named>(&self) -> String {        // no default here
        return T::name();
    }

    fn made<T: Named>() -> String {
        return T::name();
    }
}

m.boxed();            // → m.boxed::<Tag>()
m.boxed::<Local>();   // spelled, so `Local`
M::made();            // the static spelling reads the same declaration
M::boxed(&m);         // and so does the receiver-taking one
```

Rust rejects a type parameter default on every function, method and `impl` (rust-lang#36887), allowing them only on type and trait declarations. Wado accepts them wherever a parameter list is written.

The same `= expr` syntax applies to struct fields; see [Struct Field Defaults](#struct-field-defaults).

### Tagged Template Literals

A path written directly before a template literal is a tag. The template then
denotes a call of that function on the template's holes, in their own types,
with the literal text around them, instead of a rendered `String`:

```wado
let q = sql`SELECT * FROM users WHERE id = ${id} AND name = ${user.name}`;
let s = String::raw`${dir}\bin\run.exe`;   // backslashes kept
```

The tag is a function name or a static method path, with no whitespace before
the backtick. The literal is lexed exactly as an untagged template, so every
escape must still be one the lexer knows even where the tag preserves it.

A tag is an ordinary function whose one parameter is bound by `ReflectTemplate`,
the reflected kind of a template literal. The compiler synthesizes one anonymous
type per template shape — its segments, specifiers, hole types and hole source
texts — holding one field per hole. So `` tag`${a}` `` and `` tag`${b}` `` are
two types, each instantiating the tag, even where `a` and `b` share a type. The
type is unnameable and reached only through the bound; a diagnostic and
`Reflect::type_name()` show it as its text with each hole spelled by type,
`` `id = ${i32}` ``, cut at 50 characters. The tag walks the holes with tuple
`for-of`:

```wado
fn sql<T: ReflectTemplate<Holes = [..V]>, ..V: ToSqlParam>(t: T) -> SqlQuery {
    let mut query = "";
    let mut params: List<SqlParam> = [];
    for let h of ReflectTemplate::<T>::members() {
        query.push_str(h.lit());                // literal text before this hole
        query.push_str("?");
        params.push(h.get(&t).to_sql_param());  // the value, storage shared
    }
    query.push_str(ReflectTemplate::<T>::tail());
    return SqlQuery { query, params };
}
```

A hole handle (`TemplateHole<T, V>`) answers `index()` (its position, from 0),
`lit()` / `raw()` (the preceding segment, escapes processed or preserved),
`get(&t)` (the value, `V`), `source()` (the expression text), `has_spec()`, and
`fmt(&t, f)` (rendering as the untagged template would).
`ReflectTemplate::<T>::tail()` and `raw_tail()` give the segment after the last
hole. Every answer but `get` and `fmt` is a constant.

`members()` walks a pack, so `Holes` is bound either as one (`[..V]`) or as the
empty tuple (`()`, for a tag that reads only `tail()`). A concrete tuple
(`Holes = [List<i32>]`) is an error at the call.

A hole's type may not mention a type parameter of the enclosing item, since the
shape is minted once rather than per instantiation. A generic body passes its
tag a concrete value from its caller. The untagged template makes no shape, so
`` `${v}` `` over a `v: X` is accepted where `` format`${v}` `` is not.

Holes are evaluated once, left to right, before the tag runs. A tag may carry
effects and return any type. Whether a call folds at compile time is the
optimizer's decision, as for any other call; the meaning does not depend on it.

An untagged template means what the prelude's `format` tag means: each hole
rendered through its specifier into one buffer.

See [WEP: Tagged Template Literals](./wep-2026-01-10-tagged-template-literals.md)
for the type, the desugaring and the cost model.

### Newtype

`type T = U` creates a newtype: a distinct type with the same values as its base type. See [WEP: Newtype Semantics](./wep-2026-01-29-newtype-semantics.md).

```wado
type Meters = f64;
type Kilometers = f64;

let m: Meters = 1000.0;       // literal coercion
let km: Kilometers = 1.0;

let sum = m + m;              // OK: Meters + Meters -> Meters
// let bad = m + km;          // ERROR: cannot mix Meters and Kilometers

let raw: f64 = m as f64;      // explicit cast required
```

#### Properties

- `T` is a distinct type from `U` (no implicit conversion)
- `T` inherits all methods, operators, and traits from `U`
- Explicit `as` cast required to convert between `T` and `U`
- Zero runtime cost
- Literal coercion to `T` when type context expects `T`

A newtype does not carry an invariant of its own. `as` converts in both
directions at no cost, so `T` admits exactly what `U` admits. An invariant the
base type does not enforce, such as UTF-8 in a byte view, belongs in a `struct`
with a private field and a checked constructor, where the check is the only way
in.

#### Method Signature Substitution

When calling inherited methods on a newtype, parameters and return types are substituted:

```wado
type Location = Point;

impl Point {
    fn distance(&self, other: &Point) -> f64 { ... }
}

let loc1: Location = Point { x: 0, y: 0 } as Location;
let loc2: Location = Point { x: 3, y: 4 } as Location;
loc1.distance(&loc2);  // params expect &Location, returns f64
```

#### Newtype-Specific Methods

```wado
impl Location {
    fn name(&self) -> String { ... }  // only on Location, not Point
}
```

#### Chained Newtypes

```wado
type A = i32;
type B = A;
type C = B;

let c: C = 1;
let a = c as A;    // OK: direct cast through chain
let i = c as i32;  // OK: direct cast to ultimate base
```

For complete type isolation where you want to hide base type methods, use a struct wrapper:

```wado
struct Miles { value: i32 }
```

### Structs

Wado uses `struct` for structured data types. A struct crosses a component boundary as a Component Model `record`.

```wado
// Struct definition
struct User {
    name: String,
    age: i32,
    active: bool,
}

// Recursive struct
struct Node {
    value: i32,
    next: Option<Node>,
}
```

#### Field Visibility

Struct fields follow the same visibility rules as other declarations (see [Visibility](#visibility)). A field without a modifier is private to the defining file; `internal` widens it to the package; `pub` exposes it to other Wado packages.

```wado
pub struct Config {
    pub name: String,   // visible to other packages
    internal tag: i32,  // visible to other files in this package
    secret: i32,        // private to this file
}
```

Within the defining module, all fields (including private ones) are accessible for construction, reading, and mutation. From another file in the same package, `internal` (and `pub`) fields are accessible; from another package, only `pub` fields are. Reading, setting, or binding a field beyond its reach is a compile error, whether through field access (`c.secret`), a struct literal (`Config { secret: ... }`), or a destructuring pattern (`let Config { secret, .. } = c`, `match`). A non-reachable field may still be _omitted_ from a struct literal in another module when it has a default expression (`f: T = expr`): the default is evaluated in the defining module, so the field is never read or set across the boundary and encapsulation is preserved. A non-reachable field without a default cannot be satisfied from another module, so such a struct can only be constructed by a function within reach.

#### Struct Construction

```wado
let user = User { name: "Alice", age: 30, active: true };

// Shorthand (variable name matches field)
let name = "Bob";
let age = 25;
let bob: User = { name, age, active: false };

// Implicit struct literal (requires type annotation)
let user: User = { name: "Alice", age: 30, active: true };
```

Functional update (`..base`): a leading `..base` fills every field the literal
does not list explicitly from the struct value `base` (same type). The listed
fields override; `base` is evaluated once and left unchanged (value semantics).

```wado
let u2 = User { ..user, age: 31 };  // every field from `user`, age replaced
```

The spread is leading and single: a field written before it would be overwritten
and unused, so `User { age: 31, ..user }`, a second spread, and a bare
`User { ..user }` (a plain copy) are all errors. A `..base` cannot read a field
that is not reachable at the use site, so it never exposes a private field across
a module boundary. See [WEP: Literal Spread](./wep-2026-07-03-literal-spread.md).

#### Struct Destructuring

```wado
let p = Point { x: 10, y: 20 };

// Unnamed destructuring (type inferred from RHS)
let { x, y } = p;

// Named destructuring (explicit type)
let Point { x, y } = p;

// Renaming fields
let { x: horizontal, y: vertical } = p;

// Ignore remaining fields with ..
struct Person { name: String, age: i32, email: String }
let { name, .. } = person;

// Mutable destructuring
let mut { x, y } = p;

// Nested destructuring
struct Line { start: Point, end: Point }
let { start: { x: x1, y: y1 }, end: { x: x2, y: y2 } } = line;

// In for-of
for let { x, y } of points {
    println(`${x}, ${y}`);
}
```

#### Auto-derived Traits

Structs derive `Eq` (field-wise equality) and `Ord` (lexicographic comparison by field declaration order) when all fields implement those traits. They are derived where a use or bound needs them, not for every struct. See [Bound-Driven Eq / Ord](#bound-driven-eq--ord). A user-provided `impl Eq` or `impl Ord` takes precedence.

For generic structs, the auto-derived impls have trait bounds on the type parameters: `impl<T: Eq> Eq for Foo<T>`, `impl<T: Ord> Ord for Foo<T>`.

Variants derive `Eq` only (not `Ord`) the same on-demand way, when all payload types implement `Eq`: both values must be the same case, and payloads (if any) are compared. A user-provided `impl Eq` takes precedence. For generic variants, the auto-derived impls have trait bounds on the type parameters: `impl<T: Eq> Eq for Maybe<T>`.

#### Struct Field Defaults

See [WEP: Default Arguments](./wep-2026-04-11-default-arguments.md).

Struct fields may declare a default expression with `= expr`. Fields with defaults may be omitted at construction sites; fields without defaults are required:

```wado
struct ServerConfig {
    host: String,            // required
    port: i32 = 8080,        // optional
    timeout: i32 = 30,       // optional
    debug: bool = false,     // optional
}

let c = ServerConfig { host: "localhost" };
// Desugars to: ServerConfig { host: "localhost", port: 8080, timeout: 30, debug: false }

let c = ServerConfig { host: "localhost", port: 3000 };
// Desugars to: ServerConfig { host: "localhost", port: 3000, timeout: 30, debug: false }

ServerConfig { port: 3000 };  // compile error: missing required field 'host'
```

Default expressions are evaluated at the construction site. They must be effect-free and cannot reference other fields. Field shorthand (`{ host }`) and destructuring are unaffected: destructuring sees every field regardless of defaults.

A non-generic struct whose every field has a default auto-derives `Default`. A fieldless struct has no field to default, so it qualifies. See [Default Trait](#default-trait).

### Generic Type Inference

Wado infers type arguments for struct literals, variant constructors, and generic function and method calls. It uses two complementary mechanisms.

Forward inference derives type parameters from the values provided (fields, payloads, or arguments):

```wado
struct Box<T> { value: T }
let b = Box { value: 42 };             // Box<i32> — T=i32 from field value

let opt = Option::Some("hello");        // Option<String> — T=String from payload
let opt2 = Option::Some(42);            // Option<i32> — T=i32 from payload
```

Backward inference derives type parameters from an expected type context (variable annotation, function parameter type, or return type):

```wado
let none: Option<i32> = Option::None;   // T=i32 from annotation
let ok: Result<i32, String> = Result::Ok(42);
// T=i32 from payload (forward), E=String from annotation (backward)
```

When both mechanisms apply, they must agree. An untyped literal takes its type from the expected type, so `let x: Option<i64> = Option::Some(42)` is an `Option<i64>`. A value whose type is already fixed must match it: with `y: i32`, `let b: Box<i64> = Box { value: y }` is a type mismatch. Backward inference fills in any parameter the values do not mention.

A turbofish on the type name pins the arguments outright. It reaches a parameter
no field mentions, and it overrides one a field would otherwise settle. It says
what the matching annotation says, so the two must agree:

```wado
struct Tagged<T> { tag: i32 }
let b = Box::<i64> { value: 1 };        // T=i64, not the i32 the literal infers
let t = Tagged::<String> { tag: 7 };    // T names no field
let n: Box<i32> = Box::<i64> { … };     // error: the annotation disagrees
```

#### Scope of inference

| Site                   | Forward (from values) | Backward (from expected type) |
| ---------------------- | --------------------- | ----------------------------- |
| Struct literals        | yes                   | yes                           |
| Variant constructors   | yes                   | yes                           |
| Generic function calls | yes                   | yes                           |
| Generic method calls   | yes                   | yes                           |

```wado
fn identity<T>(x: T) -> T { return x; }
fn none_of<T>() -> Option<T> { return null; }

let x = identity(42);                  // T=i32 from the argument
let s = identity("hi");                // T=String from the argument
let n: Option<i64> = none_of();        // T=i64 from the annotation
```

A `_` inside a turbofish leaves that type-argument slot for inference while the
others stay explicit, reusing the same inference an omitted turbofish uses. The
explicit (non-`_`) arguments always win. An uninferable `_` is the same error as
an omitted turbofish on an uninferable parameter. A `_` works only in a
turbofish: in a plain type annotation (`let xs: List<_>`) it is an error.

A turbofish may also stop short of the declared parameters. The ones it does not
name are inferred, as a `_` in their place would be.

```wado
let r = Result::<_, MyErr>::Ok(42);    // infers Ok payload type, pins the error type
let a = pick::<_, bool>(1, true);      // infers the first type argument
let b = pick::<i32>(1, true);          // stops short: infers the second
```

### Traits

Traits define shared behavior that types can implement. Trait methods use static dispatch: every call is resolved at compile time.

A `trait` and an `interface` are declared in the type namespace: one name reaches one declaration wherever it is written. Neither denotes a type. Each names a set of operations, and no value has one as its type, so a type position naming one is a compile error. A trait reaches a type only as a bound (`fn f<T: Greet>(x: T)`).

```wado
// Trait declaration
trait Greet {
    fn greet(&self) -> String;
}

// Trait implementation
struct Person {
    name: String,
}

impl Greet for Person {
    fn greet(&self) -> String {
        return `Hello, ${self.name}!`;
    }
}

// Usage
let p = Person { name: "Alice" };
println(p.greet());  // "Hello, Alice!"
```

#### Supertraits

A trait can require its implementors to implement other traits. `impl Ord for T`
then fails unless `T` also implements `Eq`, and `T: Ord` alone is enough to use
`Eq`'s methods:

```wado
trait Ord: Eq {
    fn cmp(&self, other: &Self) -> Ordering;
}

trait Circle: Shape + Display {
    fn radius(&self) -> i32;
}

// `T: Ord` implies `T: Eq`
fn dedup_sorted<T: Ord>(items: List<T>) -> List<T> { ... }
```

A trait that reaches itself through supertraits is an error. A method name
reachable through more than one of a receiver's bounds is ambiguous at the call
site; name the trait that declares it to resolve it (`Left::name(&x)` — see
[WEP: Overload Resolution](./wep-2026-07-31-overload-resolution.md)). The bounds
a body may name that way include the implied ones, so `Eq::eq(&a, &b)` resolves
under `T: Ord`.

#### Multiple Traits

A struct can implement multiple traits:

```wado
trait Named {
    fn name(&self) -> String;
}

trait Aged {
    fn age(&self) -> i32;
}

impl Named for Person {
    fn name(&self) -> String { return self.name; }
}

impl Aged for Person {
    fn age(&self) -> i32 { return self.age; }
}
```

#### Method Resolution

A call `recv.m(args)` resolves in one order, stated in full by
[WEP: Trait Resolution](./wep-2026-09-01-trait-resolution.md). The receiver
decides which step answers:

1. An inherent method (`impl Type { … }`) shadows every trait method of that
   name, along the whole newtype chain.
2. A reference receiver's `&T` impls come before the base type's.
3. The trait impls that apply to the receiver are ranked, below.
4. A receiver whose type is a type parameter answers from its bounds instead:
   the first bound declaring the method, and two or more declaring it is an
   error.

```wado
struct Robot { id: i32 }

// Inherent method
impl Robot {
    fn greet(&self) -> String { return "Beep boop"; }
}

// Trait method (won't be called because inherent method exists)
impl Greet for Robot {
    fn greet(&self) -> String { return "Hello from trait"; }
}

let r = Robot { id: 1 };
r.greet();  // Returns "Beep boop" (inherent method wins)
```

##### Scope

A trait contributes candidates only where its declaration is in scope: declared
in this module, imported by name or alias, re-exported to it through `pub use`,
or one of the prelude's. Importing a type brings none of the traits its impls
mention. A bound is a name like any other, so calling a supertrait's method
through `T: Sub` needs `Base` imported too. This is what keeps a library's new
blanket impl from changing what a call means in a module that never named it.

Not yet enforced for a supertrait's method called through a bound. See
[WEP: Trait Resolution](./wep-2026-09-01-trait-resolution.md#scope-gates-method-calls-not-the-bounds-path).

##### The Order

Several impls applying to one receiver is normal: a trait carries several
blanket impls. They are ranked:

1. A variadic impl (`impl<..T> Tr for [..T]`) yields to a non-variadic one of
   the same trait at the same argument list.
2. The newtype before its base. The search stops at the first level of the
   receiver's newtype chain that answers.
3. Within one level, the impl that names more of the receiver. One written for
   the receiver (`impl Tag for Box_<i32>`) comes first, then one written for its
   head (`impl<T> Tag for Box_<T>`), then a value blanket
   (`impl<T: Bound> Tr for T`). A blanket names no type at all, only a condition
   the receiver meets. See [Specific Impls Win](#specific-impls-win).

Where an impl was written is read at no rank, so a call means the same thing to
every reader. Specificity is not a rank either: generality reads an impl's
target, never its bounds, so `impl<T: A + B>` beside `impl<T: A>` reports rather
than preferring the narrower one.

##### Ambiguity

Candidates the ranks cannot separate are an error. There are two of them,
because the fix differs:

- Two traits declaring the method name. They share no contract, so the call
  names one: `Alpha::describe(&x)`.
- Two impls of one trait, neither written for the receiver. A blanket has no
  name to call it by, so the fix is an `impl Tr for TheType`, which generality
  puts above both.

Wado has no fully qualified `<Type as Trait>::method()` form, because a leading
`<` in expression position begins JSX. A call names its trait with the
trait-qualified form `Trait::method(recv, …)` instead (see
[WEP: Overload Resolution](./wep-2026-07-31-overload-resolution.md)). An
associated function with no `self` has no receiver argument to bind `Self`
from, so that form cannot name it.

Arguments filter candidates before the ranks run: one trait at several argument
lists is an overload set the call's arguments choose from (see
[One Trait at Two Argument Lists](#one-trait-at-two-argument-lists)). Operators
and indexing select by operand type instead.

#### Default Method Implementations

Trait methods can have default implementations. Implementors can override them or use the defaults:

```wado
trait Summary {
    fn title(&self) -> String;  // required - must be provided

    // Default method - uses self.title()
    fn summary(&self) -> String {
        return `Title: ${self.title()}`;
    }
}

struct Article { headline: String }

// Only provides the required method; summary() uses the default
impl Summary for Article {
    fn title(&self) -> String { return self.headline; }
}

struct Report { headline: String, body: String }

// Overrides the default summary()
impl Summary for Report {
    fn title(&self) -> String { return self.headline; }
    fn summary(&self) -> String { return `${self.headline}: ${self.body}`; }
}
```

Default methods can call other trait methods (both required and default), and the calls are resolved against the implementing type.

#### Associated Types

Traits can declare associated types - placeholder types that are specified by implementors:

```wado
trait Container {
    type Item;  // Associated type declaration

    fn get(&self) -> Self::Item;
    fn set(&mut self, value: Self::Item);
}

struct IntBox {
    value: i32,
}

impl Container for IntBox {
    type Item = i32;  // Associated type binding

    fn get(&self) -> Self::Item {
        return self.value;
    }

    fn set(&mut self, value: Self::Item) {
        self.value = value;
    }
}
```

Within trait methods and implementations, `Self::TypeName` refers to the associated type. The type is resolved at compile time based on the implementing type.

#### Bounded Associated Types

Associated types can have trait bounds that constrain what types can be used as the associated type:

```wado
trait Collection {
    type Element;
    type Builder: CollectionBuilder<Element = Self::Element, Output = Self>;
}
```

Here `Builder` must implement `CollectionBuilder` with matching `Element` and `Output` types. The `Type = ConcreteType` syntax constrains associated types of the bound trait to specific types.

#### Blanket Implementations

A blanket impl provides a trait implementation for all types that satisfy a given bound:

```wado
// Any type that builds itself satisfies Collection automatically
impl<T: CollectionBuilder<Output = T>> Collection for T {
    type Element = T::Element;
    type Builder = T;
}
```

This avoids the need for explicit `impl Collection for ...` on every self-building type. `T::Element` names the `Element` that `T`'s `CollectionBuilder` impl binds.

#### Impl Type Parameters Are Declared

An `impl` declares its type parameters in `impl<...>`, and that list is the only way to introduce one. A name in the target or the trait reference that the list does not hold is a type, and the module must declare it:

```wado
impl<T> List<T> { ... }                 // inherent
impl<T: Ord> List<T> { ... }            // with a bound
impl<K: Ord, V> TreeMap<K, V> { ... }   // every parameter listed
impl<T> Default for List<T> { ... }     // trait implementation
impl Display for List<i32> { ... }      // one instantiation declares none
```

#### Impl Type Parameters Must Be Determined

An `impl`'s target and trait reference between them must name every type parameter it declares. A use site determines them from the receiver and the trait arguments and from nothing else, so one neither mentions has no value to be given:

```wado
impl<A: Eq, T: Eq> Dup for T { ... }  // ERROR: the type parameter `A` is not
                                      //        constrained by the impl target
                                      //        or the trait reference
```

A bound determines one too, through the types it writes:

```wado
// `S` fixes `FieldTypes`, which fixes `..F`
impl<S: ReflectStruct<FieldTypes = [..F]>, ..F: Inspect> Inspect for S { ... }
```

The bound's subject is not itself determined this way: `A: Eq` says what `A` must satisfy, not what `A` is.

#### Standard Library Traits

The prelude defines the indexing traits `IndexValue`, `IndexAssign`, `IndexRef`, and `IndexRefMut`, each with an associated `Output` type. See [Indexing Traits](#indexing-traits) for full definitions.

#### Trait Bounds

Type parameters can have trait bounds that constrain what types can be used:

```wado
// Struct with trait bound
struct SortedPair<T: Ord> {
    first: T,
    second: T,
}

// Multiple bounds with + syntax
struct PrintableOrd<T: Ord + Printable> {
    value: T,
}

// Bounds on function type parameters
fn max<T: Ord>(a: T, b: T) -> T {
    if a > b { return a; }
    return b;
}

// Bounded impl blocks - methods only available when T: Ord
impl<T: Ord> List<T> {
    pub fn sort(&mut self) { ... }
    pub fn sorted(&self) -> List<T> { ... }
}

// Bounded trait implementations - Pair<T> implements Eq only when T: Eq
impl<T: Eq> Eq for Pair<T> {
    fn eq(&self, other: &Self) -> bool {
        return self.first == other.first && self.second == other.second;
    }
}
```

### Coherence and Orphan Rules

Wado enforces coherence: a `(Trait, Type)` pair is implemented once. A second impl of one pair is rejected where it is written, and the orphan rules below keep two packages from each writing one.

That is a rule about where impls may be written, not about how many apply to a call: a trait carries several blanket impls, and more than one of them can apply to a receiver. [Method Resolution](#method-resolution) orders those.

#### Package Boundary

The unit of coherence is a package — all source files compiled together from the same `wado.toml` project. Types and traits are classified relative to that boundary:

| Module source                                       | Classification |
| --------------------------------------------------- | -------------- |
| `./file.wado` (relative path import)                | Local          |
| Entry-point file                                    | Local          |
| A module a Kiln generator produces for this package | Local          |
| A `[dependencies]` package                          | Foreign        |
| `core:*` (standard library)                         | Foreign        |
| `wasi:*` (WASI interfaces)                          | Foreign        |
| A Wasm asset (`with { type: "wasm" }` or `"wat"`)   | Foreign        |
| Remote URL                                          | Foreign        |

#### The Orphan Rule

For `impl<P1..Pn> Trait<A1..Am> for T0`, the implementation is valid if and only if at least one of these conditions holds:

1. `Trait` is local (defined in the current package), or
2. The sequence `T0, A1, A2, …, Am` contains a local type at some position `i`, and no uncovered type parameter appears at any position `j < i`.

##### Uncovered type parameter

A type parameter `Pk` is _uncovered_ at position `i` if the type at position `i` is literally `Pk` (bare, not wrapped inside another type constructor). `List<Pk>` is covered; `Pk` alone is uncovered.

##### Fundamental types

`&T` and `&mut T` are _fundamental_ — they are looked through when checking positions. `impl Trait for &LocalType` counts as having `LocalType` at position `T0`.

#### Examples

| Implementation                       | Verdict   | Reason                                                     |
| ------------------------------------ | --------- | ---------------------------------------------------------- |
| `impl Eq for MyStruct`               | Allowed   | `MyStruct` is local (T0 is local)                          |
| `impl MyTrait for String`            | Allowed   | `MyTrait` is local                                         |
| `impl<T: Eq> Eq for MyBox<T>`        | Allowed   | `MyBox` is local (T0 is local)                             |
| `impl From<MyError> for String`      | Allowed   | `MyError` (local) at A1, no uncovered param before it      |
| `impl<T> From<MyType<T>> for String` | Allowed   | `MyType` (local head) at A1, no uncovered param before it  |
| `impl<T> From<T> for MyType`         | Allowed   | `MyType` is local at T0, reached before T1=`T`             |
| `impl Eq for String`                 | Forbidden | Both `Eq` and `String` are foreign                         |
| `impl Eq for List<i32>`              | Forbidden | `Eq` foreign, `List` (head of T0) is foreign               |
| `impl<T> Eq for T`                   | Forbidden | T0 is uncovered type parameter, `Eq` is foreign            |
| `impl<T> From<T> for String`         | Forbidden | T0=`String` (foreign), T1=`T` (uncovered) before any local |
| `impl From<String> for i32`          | Forbidden | T0=`i32` (foreign), A1=`String` (foreign), no local found  |

#### Rationale

The orphan rule prevents two packages from independently providing `impl Trait for Type` for the same `(Trait, Type)` pair, which would make method resolution ambiguous when both packages are used together. By requiring at least one of the trait or the self type to be local, every valid implementation is "owned" by exactly one package.

The sequence rule (RFC 2451 style) allows `impl From<LocalError> for String` — even though `String` is foreign — because `LocalError` appears in the trait's type argument at position A1 with no uncovered type parameter before it. This makes it unnecessary to define a mirror `Into` trait just to work around stricter rules.

#### Inherent Impls

An inherent impl (`impl Type { … }`, with no trait) is subject to a simpler
coherence rule: it may only be written in the package that owns the type.
The self type's head constructor must be local.

| Implementation           | Verdict   | Reason                                              |
| ------------------------ | --------- | --------------------------------------------------- |
| `impl MyStruct { … }`    | Allowed   | `MyStruct` is local                                 |
| `impl<T> MyBox<T> { … }` | Allowed   | `MyBox` (head) is local                             |
| `impl i32 { … }`         | Forbidden | `i32` is foreign                                    |
| `impl String { … }`      | Forbidden | `String` is foreign                                 |
| `impl<T> Array<T> { … }` | Forbidden | `Array` is foreign                                  |
| `impl List<u8> { … }`    | Forbidden | `List` (head) is foreign — even when fully concrete |

This mirrors the trait-impl rationale: if two packages could each add inherent
methods to the same foreign type, their methods would collide. To extend a
foreign type from another package, define a local trait and implement it for
that type (`impl MyExt for String`) — the orphan rule above permits this because
the trait is local. The owning package itself (e.g. `core` for `String` /
`Array<T>` / `List<T>`) is of course free to spread inherent impls across its own
modules.

#### Specific Impls Win

Two impls of one trait may cover a type when one is written for a single
instantiation and the other is generic over the head:

```wado
impl<T> Tag for Box_<T> { … }         // general
impl Tag for Box_<i32> { … }          // specific — wins for Box_<i32>

impl<..T> Tag for [..T] { … }         // general
impl Tag for [i32, i32] { … }         // specific — wins for [i32, i32]
```

The specific impl applies to the instantiation it names; every other
instantiation takes the general one. Declaration order does not matter. This is
the generality rank of [Method Resolution](#the-order) — the same rank that puts
either of these above a value blanket (`impl<T: Bound> Tag for T`).

This holds only for a **trait** impl, where the trait gives both methods one
signature. An inherent impl carries no such contract, so the pair is rejected:

```wado
impl<T> Box_<T> { fn a(&self) -> String { … } }
impl Box_<i32> { fn a(&self) -> i32 { … } }   // ERROR: duplicate definition of `a`
```

Two impls that are general in the same way cannot be ordered at all, so a second
variadic impl of one trait is rejected where it is written:

```wado
impl<..T: Inspect> Tag for [..T] { … }
impl<..T: Eq> Tag for [..T] { … }     // ERROR: overlapping variadic impls
```

Bounds do not separate them. A trait's own arguments do, since they make the
two impls of different traits:

```wado
impl<..T> Conv<i32> for [..T] { … }    // OK
impl<..T> Conv<String> for [..T] { … } // OK — a different trait
```

#### One Trait at Two Argument Lists

A trait may be implemented for one type at several argument lists — each impl
is legal, and the arguments choose between them
([WEP: Overload Resolution](./wep-2026-07-31-overload-resolution.md)):

```wado
impl Take<A> for bool { … }
impl Take<B> for bool { … }

f.take(B { v: 1 })          // OK: a named struct literal selects Take<B>
f.take(a)                    // OK: the local's declared type selects Take<A>
```

Any argument whose type the call site fixes selects: a local, a field read, a
call's return type, an operator's result, a cast, an associated constant, an
enum case, a range.

Selection is unique-or-error, with no ranking. An argument whose type the
call site does not pin — above all a bare literal, which could coerce to
several widths — admits every candidate it could coerce to and never selects
one, so a literal-only distinction stays ambiguous:

```wado
impl Take<i32> for bool { … }
impl Take<i64> for bool { … }

f.take(42)                   // ERROR: the arguments do not select
f.take(42 as i64)            // OK: the cast selects Take<i64>
Take::<i64>::take(&f, 42)    // OK: the trait turbofish pins the list
```

This is deliberate: letting the literal's default type decide would make
adding an `impl Take<i32>` silently retarget every existing call that meant
`Take<i64>`. A closure or a compound literal is typed by the parameter it is
passed to, so it carries nothing to select on either, and the error names the
argument that came up empty.

Operators resolve their impl by operand type on the same principle, which is
why `List<T>` implements `IndexValue<i32>`, `IndexValue<RangeExclusive<i32>>`,
and `IndexValue<RangeInclusive<i32>>` at once — and why the same impls answer
the method spelling, `l.index_value(i)`.

A trait's associated function obeys the same rule, selected on its first
argument. It has no receiver to fix `Self`, so the type is written out and the
argument chooses among the impls that declare the function. Rust needs
`<M as Enc<A>>::make` here:

```wado
impl Enc<A> for M { fn make(v: A) -> i32 { … } }
impl Enc<B> for M { fn make(v: B) -> i32 { … } }

M::make(A { })               // selects Enc<A>
M::make(B { })               // selects Enc<B>
```

Two _different_ traits declaring one method name for one receiver is a
separate case and is always reported: name the trait
(`Alpha::describe(&x)`). Argument selection never crosses trait lines —
impls of different traits share no contract.

### Iterator Traits

The prelude defines iterator traits for generic iteration over collections.

#### Iterator - Core Iteration Trait

```wado
/// Types that can yield a sequence of values
pub trait Iterator {
    type Item;

    /// Advances the iterator and returns the next value.
    /// Returns None when iteration is complete.
    fn next(&mut self) -> Option<Self::Item>;
}
```

#### IntoIterator - Conversion Trait

```wado
/// Types that can be converted into an iterator
pub trait IntoIterator {
    type Item;
    type Iter: Iterator<Item = Self::Item>;

    /// Creates an iterator from a value
    fn into_iter(&self) -> Self::Iter;
}
```

#### FromIterator - Collection Construction

```wado
/// Types that can be constructed from an iterator of `Elem`
pub trait FromIterator {
    type Elem;
    fn from_iter<I: Iterator<Item = Self::Elem>>(iter: &mut I) -> Self;
}
```

#### SliceValueIter

`SliceValueIter<T>` is the by-value iterator for the whole sequence family: `Array<T>`, `List<T>`, and `Slice<T>` all reach it through `iter_value()`. [The Sequence Family](./wep-2026-06-02-sequence-family.md) owns the `Value` / `Ref` / `RefMut` axis and the rest of the family's iterators.

#### Terminals

Everything `Iterator` declares is available on every implementor, adapters included — the terminals bounded by their element type among them (`sum` / `product` need `Item: Add<Output = Item>` / `Mul<Output = Item>`, `min` / `max` need `Item: Ord`). See [`core:prelude`](./stdlib-core-prelude.md) for the full list and each one's behaviour.

#### Usage

```wado
let arr: List<i32> = [1, 2, 3, 4, 5];

// for-of uses IntoIterator automatically
for let x of arr {
    println(`${x}`);
}

// Explicit iterator
let mut iter = arr.iter_value();
while let Some(x) = iter.next() {
    println(`${x}`);
}

// Collect remaining elements
let mut rest_iter = arr.iter_value();
rest_iter.next();  // skip first
let rest = rest_iter.collect();  // [2, 3, 4, 5]

// Terminals compose with the adapters
let total = arr.iter_value().filter(|x| x % 2 == 1).sum();  // Some(9)
```

#### Value Semantics

By-value iteration (`into_iter()`, `iter_value()`, `for let x of list`) returns copies of elements. Reference iteration yields references instead: `iter_ref()` and `for let x of &list` yield `&T`, and `iter_ref_mut()` and `for let x of &mut list` yield `&mut T`.

`&mut` iteration mutates elements in place when the element type has an addressable interior: `struct`, `List`, `String`, `i128`/`u128`. A write through the `&mut T` lands on the element:

```wado
for let p of &mut points {
    p.x += 1;  // mutates the element in place
}
```

A replace-on-assign element type (`primitive`, `enum`, `flags`, `variant`, `fn`) has no addressable interior, so a write through `&mut T` would be lost. `&mut` iteration over such a list is a compile error; use indexed access instead:

```wado
for let mut i = 0; i < arr.len(); i += 1 {
    arr[i] = arr[i] * 2;
}
```

For `primitive`, `enum`, `flags`, and `fn`, nothing survives the copy, so taking `&mut` of a field or element is a compile error outright. That holds whether it is written `&mut x.f` / `&mut xs[i]` or taken implicitly by a `&mut self` receiver. A `&mut` of a _local_ is fine, since it writes to the variable itself.

A `variant` place admits `&mut`. Its payload is shared, so a mutation _through_ it lands, though replacing the whole value does not.

#### Custom Iterables

Any type can be made iterable by implementing `IntoIterator`:

```wado
struct Stack<T> { items: List<T> }
struct StackIter<T> { items: List<T>, index: i32 }

impl<T> Iterator for StackIter<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> { ... }
}

impl<T> IntoIterator for Stack<T> {
    type Item = T;
    type Iter = StackIter<T>;
    fn into_iter(&self) -> StackIter<T> { ... }
}

// Now for-of works
for let x of stack { ... }
```

#### Iterator Combinators

Iterators support `map`, `filter`, and `fold` for functional-style data processing:

```wado
let arr: List<i32> = [1, 2, 3, 4, 5];

// map - transform each element
let doubled = arr.into_iter().map(|x| x * 2).collect();
// [2, 4, 6, 8, 10]

// filter - keep elements matching predicate
let evens = arr.into_iter().filter(|x| x % 2 == 0).collect();
// [2, 4]

// fold - reduce to single value
let sum = arr.into_iter().fold(0, |acc, x| acc + x);
// 15

// Chaining combinators
let result = arr.into_iter()
    .filter(|x| x > 2)
    .map(|x| x * 10)
    .collect();
// [30, 40, 50]
```

### Builtin Comparison Traits

The prelude defines traits for comparison operators:

#### Eq - Equality

```wado
/// Types that can be compared for equality
pub trait Eq<Rhs = Self> {
    /// Returns true if self equals other
    fn eq(&self, other: &Rhs) -> bool;
}
```

The `==` and `!=` operators use `Eq::eq`:

- `a == b` desugars to `Eq::eq(&a, &b)`
- `a != b` desugars to `!Eq::eq(&a, &b)`

`==` can span two types. The right operand picks among a type's `Eq<Rhs>` impls
exactly as it picks among its `Add<Rhs>` impls, so `StrSlice` and `String`
compare directly, in either order, with nothing copied.

#### Ordering Enum

```wado
/// Result of a three-way comparison
pub enum Ordering {
    Less,    // first value is less than second
    Equal,   // values are equal
    Greater, // first value is greater than second
}
```

#### Ord - Ordering

```wado
/// A total order over the type
pub trait Ord: Eq {
    /// Compares self with other and returns an Ordering
    fn cmp(&self, other: &Self) -> Ordering;
}
```

`Ord` is a total order, and `sort`, `TreeMap` and every `T: Ord` bound read it.
On a float it is IEEE 754-2019 `totalOrder`, so it separates `-0.0` from `0.0`
and places each NaN at one end rather than calling it equal to what it met.

On every type but a float, a comparison operator means what `Ord::cmp` answers:

- `a < b` desugars to `Ord::cmp(&a, &b) == Ordering::Less`
- `a > b` desugars to `Ord::cmp(&a, &b) == Ordering::Greater`
- `a <= b` desugars to `Ord::cmp(&a, &b) != Ordering::Greater`
- `a >= b` desugars to `Ord::cmp(&a, &b) != Ordering::Less`

A float is the one type whose operators are not its `Ord`: all four are IEEE,
so a NaN answers false and the two zeroes are one value. This holds for `f16`
and `bf16` as for `f32` and `f64`. One trait cannot carry both orders, because
`Ordering` has three cases and an IEEE comparison has four answers. See
[WEP: The Operator Order and the Total Order](./wep-2026-09-23-comparison-traits.md).

#### Default Implementations

`String` and `List<T>` implement `Eq` and `Ord` with lexicographic comparison:

```wado
impl Eq for String { ... }  // byte-by-byte equality
impl Ord for String { ... } // lexicographic ordering

// Usage
let a = "apple";
let b = "banana";
if a < b { ... }  // true
```

### Default Trait

See [WEP: Default Trait](./wep-2026-03-04-default-trait.md).

The prelude defines a `Default` trait providing a uniform "zero value" / "empty value" interface:

```wado
pub trait Default {
    fn default() -> Self;
}
```

#### Standard Library Implementations

| Type                                                                 | `default()` |
| -------------------------------------------------------------------- | ----------- |
| `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`, `i128`, `u128` | `0`         |
| `f16`, `bf16`, `f32`, `f64`                                          | `0.0`       |
| `bool`                                                               | `false`     |
| `char`                                                               | `'\0'`      |
| `String`                                                             | `""`        |
| `Array<T>`, `List<T>`                                                | `[]`        |
| `Option<T>`                                                          | `null`      |
| `TreeMap<K, V>` (`K: Ord`)                                           | `{}`        |
| `TreeSet<T>` (`T: Ord`)                                              | `[]`        |

`Result<T, E>` does not implement `Default`, since there is no obvious choice between `Ok` and `Err`.

#### Usage

```wado
let n = i32::default();           // 0
let s = String::default();        // ""

fn make_default<T: Default>() -> T { return T::default(); }

let x = make_default::<i32>();              // 0
let arr = make_default::<List<String>>();  // []
```

#### Auto-Derivation

`Default` is auto-derived for a non-generic struct when every field has a declared default expression (`f: T = expr`). It is derived where a `S::default()` call, a `T: Default` bound, or an `impl Default for S;` marker needs it, not for every eligible struct. A fieldless struct qualifies, having exactly one value. This is what lets a marker like `NoFields` serve as a type parameter's default. See [Struct Field Defaults](#struct-field-defaults). A user-written `impl Default for S` overrides the auto-derived one. Generic structs require an explicit impl.

```wado
struct Config {
    host: String = "localhost",
    port: i32 = 8080,
}

let c = Config::default();  // Config { host: "localhost", port: 8080 }
```

For other types, the user writes the impl manually:

```wado
struct Point { x: i32, y: i32 }

impl Default for Point {
    fn default() -> Point { return Point { x: 0, y: 0 }; }
}
```

### String Parsing Traits

Two prelude traits parse a value from text, both taking any `AsStrSlice` and returning `Result`. `FromStr` is strict; `LenientFromStr` is forgiving of human input. `char`, `bool`, the integer types (`i128`/`u128` included), and the float types implement both; `String` implements only the lenient one, since taking a string as itself cannot fail.

```wado
i32::from_str("42")              // Ok(42)
i32::from_str("0x2A")            // Err — strict rejects the prefix

i32::from_str_lenient("0x2A")    // Ok(42)  — radix prefixes 0x/0o/0b
i32::from_str_lenient("1_000")   // Ok(1000) — `_` digit separators
bool::from_str_lenient("TRUE")   // Ok(true) — casing, plus 1/0
f64::from_str_lenient("inf")     // Ok(f64::INFINITY)
i32::from_str_lenient(" 1 ")     // Err — never trims whitespace
```

`FromStr::from_str` takes any `AsStrSlice` — a `StrSlice` among them, so a field is parsed out of a larger buffer with no substring allocation. See [WEP: String Views](./wep-2026-09-13-string-slice.md) and [WEP: Lenient String Parsing](./wep-2026-06-22-lenient-from-str.md).

### Arithmetic Operator Traits

The prelude's binary operator traits (`Add`, `Sub`, `Mul`, `Div`, `Rem`,
`BitAnd`, `BitOr`, `BitXor`) carry a right-hand type parameter defaulting to
`Self`:

```wado
trait Add<Rhs = Self> {
    type Output;
    fn add(&self, rhs: &Rhs) -> Self::Output;
}
```

Omitting the argument is the ordinary case: `impl Add for Meters` adds two
`Meters`. Writing it lets one type be added to another, and the right operand
selects between the impls:

```wado
impl Add for Meters { … }          // Meters + Meters
impl Add<Feet> for Meters { … }    // Meters + Feet

let total = m + f;                 // selects Add<Feet>
```

Selection follows the same unique-or-error rule as a method call's argument
lists (see [One Trait at Two Argument Lists](#one-trait-at-two-argument-lists)).
`Neg` and `BitNot` are unary and take no argument; `Shl` / `Shr` declare
`rhs: u32`.

The compiler supplies these impls for the integers, and for `f32` / `f64`
except `Rem`. `bool` holds one bit, so it gets the bit operators and no shift;
`v128` gets none, its arithmetic being lane-wise and known only to the lane
type's own impl.

An operator yields `Output`, which a widening impl may make another type, so a
generic body folding back into its own parameter pins it:

```wado
fn sum2<T: Add<Output = T>>(a: T, b: T) -> T { return a + b; }
fn scale<T: Mul>(a: T, b: T) -> T::Output { return a * b; }
```

A bound that writes no argument names the declared default. `T: Add` is
`Add<Self>`, which `impl Add for Cm` answers and `impl Add<Inch> for Cm` does
not.

A bound that writes one asks for that argument. `T: Eq<String>` reaches
`impl Eq<String> for StrSlice`. On a `String` receiver it reaches
`impl Eq for String`, whose `Rhs` is the restated `Self`.

An impl writing `Self` as a trait argument says its own target, so
`impl Add<Self> for Feet` and `impl Add<Feet> for Feet` are one impl.

`Self` in a bound names the type the surrounding declaration implements. A
`trait` binds one and an `impl` binds one. A free function binds none, so `Self`
anywhere in a free function's bounds is an error, and the error names the type
parameter to write instead. A `struct` or `variant` declaration binds none
either, so a bound on its own parameter is the same error.

The position makes no difference. A trait argument (`T: Uses<Self::Item>`) and
an associated-type constraint nested under one
(`T: Sink<Cb = fn(Self::Item) -> i32>`) are both rejected. Rust rejects the same
spelling.

The rule is the same wherever a bound is written: on a type parameter, on a
supertrait (`trait AsStrSlice: Eq<String>`), or on an associated type
(`type Item: Eq<String>`). A bound's arguments are spelled where it is written,
so a supertrait clause naming its own trait's parameter — `trait Gauge<X>:
Measure<X>` — supplies `Measure<i32>` under `T: Gauge<i32>`. A position the
clause leaves out takes the declared default there too, so `trait A<T>: B<T>`
over `trait B<X, Y = i32>: C<Y>` supplies `C<i32>`.

`T::Output` under two bounds that both declare `Output` is ambiguous unless
they bind it to the same type.

An operator names these traits by construction, not by spelling: a trait
declared as `Add` elsewhere shadows the name but does not answer `+`.

A parameter with no default is left open by a bound that writes nothing there:
`T: Pick` holds for every `impl Pick<K>`, and the body cannot say which `K`.
`T: Pick<String>` names it, and holds only for `impl Pick<String>`.

Two bounds on one trait are two obligations, each asking for what it writes.
Under `trait Pick<K = i32>`, `T: Pick + Pick<String>` asks for `impl Pick<i32>`
as well as `impl Pick<String>`. A method call on such a parameter reads the
bound that writes arguments. The trait is one either way, so naming it selects
nothing.

An impl and a bound already in scope read a written argument differently.

An impl answers a bound only where every position agrees, each side counting the
trait's declared default where it wrote nothing. `impl Conv<i32> for Holder` does
not answer `U: Conv` when `Conv` declares `X = String`.

A bound already in scope supplies a bare request whatever it writes, and a
supertrait does the same. `T: Conv<i32>` supplies `Conv`, and
`AsStrSlice: Eq<String>` supplies `Eq`. Nothing is chosen at such a request. The
bound is already fixed, and the question is only whether the trait is among what
the parameter carries.

### Indexing Traits

The prelude defines traits for index-based access:

#### IndexValue - Value Read

```wado
/// Returns element by value (copy)
pub trait IndexValue<IndexType> {
    type Output;
    fn index_value(&self, index: IndexType) -> Self::Output;
}
```

#### IndexAssign - Value Write

```wado
/// Assigns value to element at index
pub trait IndexAssign<IndexType> {
    type Output;
    fn index_assign(&mut self, index: IndexType, value: Self::Output);
}
```

#### IndexRef - Reference Read

```wado
/// Returns element by shared reference
pub trait IndexRef<IndexType> {
    type Output: Ref;
    fn index_ref(&self, index: IndexType) -> &Self::Output;
}
```

#### IndexRefMut - Mutable Reference

```wado
/// Returns element by mutable reference
pub trait IndexRefMut<IndexType> {
    type Output: RefMut;
    fn index_ref_mut(&mut self, index: IndexType) -> &mut Self::Output;
}
```

#### Dispatch

A use site reads the trait that matches what it does with the element:

- A bare read `c[i]` copies the element out through `IndexValue`.
- An assignment `c[i] = v` writes through `IndexAssign`.
- `&c[i]` and a `&self` receiver take `IndexRef` when the container has it, and a copy otherwise.
- A `&mut self` receiver and a field write `c[i].f = v` take `IndexRefMut`. On a container without it they are compile errors.

`Ref` and `RefMut` are sealed marker traits the compiler provides. `Ref` holds for a type whose value `&T` can alias, such as a `struct`, `List`, `String`, tuple, `variant`, `fn`, `i128`/`u128`, or reference. `RefMut` holds for the `Ref` types mutated in place rather than replaced on assignment, which excludes `variant` and `fn`. A scalar, `enum`, `flags`, or `resource` element is neither, so it is read and written by value only.

`List<T>` and `Array<T>` implement `IndexValue` and `IndexAssign` for every element type, `IndexRef` when `T: Ref`, and `IndexRefMut` when `T: RefMut`:

```wado
let mut arr: List<i32> = [1, 2, 3];
let x = arr[0];    // IndexValue::index_value
arr[1] = 100;      // IndexAssign::index_assign
```

See [WEP: Indexing Traits Design](./wep-2026-01-20-indexing-traits.md).

### Enums, Variants, and Flags

Wado follows Component Model's distinction between enums and variants (unlike Rust):

Enums (no payloads - Component Model `enum`):

```wado
// Simple enumeration - all cases have no data
enum Color {
    Red,
    Green,
    Blue,
}

// Construction
let c = Color::Red;
let d: Color = Red; // bare only where the expected type supplies it (an
                    // annotation, a parameter, a return type, a payload);
                    // `let e = Red;` is an error

// Pattern matching: match, if let, matches
let name = match c {
    Red => "red",
    Green => "green",
    Blue => "blue",
};

if let Red = c { /* ... */ }

if c matches { Green } { /* ... */ }

// Match with wildcards and guards
match c {
    Red => "warm",
    _ => "other",
}
```

Enums auto-derive `Display` as the bare case name (`Red`), distinct from `Inspect`'s `Color::Red`. `Eq` (discriminant equality) and `Ord` (declaration order) derive the same on-demand way as for structs. See [Auto-derived Traits](#auto-derived-traits) above.

Enums can have `impl` blocks:

```wado
impl Color {
    fn is_warm(&self) -> bool {
        return match *self {
            Red => true,
            _ => false,
        };
    }
}
```

Variants (with payloads - Component Model `variant`):

Wado variants have exactly one payload type per case. Unit cases have no payload, and multiple values require explicit tuple syntax `[T, U]`:

```wado
// Sum type where variants can carry data
variant Shape {
    Circle(f64),           // single payload (radius)
    Rectangle([f64, f64]), // explicit tuple payload (width, height)
    Point,                 // no payload (unit)
}

// Generic variant
variant Maybe<T> {
    Just(T),
    Nothing,
}

// Construction
let s = Shape::Circle(5.0);
let r = Shape::Rectangle([10.0, 20.0]);
let p = Shape::Point;
let c: Shape = Circle(5.0); // bare where the expected type supplies it

// Option construction — type inferred from payload (forward inference)
let opt = Option::Some(42);              // Option<i32> inferred
let opt_str = Option::Some("hello");     // Option<String> inferred

// Option construction — type inferred from annotation (backward inference)
let none: Option<i32> = Option::None;    // T=i32 from annotation

// Result construction — combined forward and backward inference
let ok: Result<i32, String> = Result::Ok(42);      // T from payload, E from annotation
let err: Result<i32, String> = Result::Err("fail"); // E from payload, T from annotation

// Explicit turbofish syntax (always available)
let opt2 = Option::<i32>::Some(42);

if let Some(x) = opt {
    println(`Got: ${x}`);
}

// Custom variant pattern matching with tuple destructuring.
// A pattern names the case bare or qualified (`ParseResult::Fail`).
variant ParseResult {
    Fail,
    Number([i32, i32]),  // start, end positions
}
let result = ParseResult::Number([0, 10]);
if let Number([start, end]) = result {
    println(`Got number from ${start} to ${end}`);
}
if let Fail = result {
    println("Failed");
}

match s {
    Circle(r) => calculate_circle_area(r),
    Rectangle([w, h]) => w * h,
    Point => 0.0,
}
```

`Option<T>` and `Result<T, E>` are declared as variants in `core:prelude`.

Flags (bit flags - Component Model `flags`):

```wado
// Bit flags - each member is a power-of-two bitmask
pub flags Perms {
    Read,     // bit 0 → value 1
    Write,    // bit 1 → value 2
    Execute,  // bit 2 → value 4
}

// Access members
let r = Perms::Read;   // 1
let w = Perms::Write;  // 2

// Bitwise combination with |
let rw = r | w;        // 3

// Bitwise AND for masking
let masked = rw & Perms::Read;   // 1 (Read bit is set)

// Bitwise XOR for toggling
let toggled = rw ^ Perms::Read;  // 2 (Read bit cleared)

// Special static methods
let none = Perms::none();  // 0 (no bits set)
let all  = Perms::all();   // 7 (all bits set)

// Cast to u32 for numeric comparison
assert rw as u32 == 3;

// Arithmetic operators (+, -, *, /, %) are NOT allowed on flags types
// They produce a compile error; use bitwise operators (|, &, ^) instead
```

Flags auto-derive `Eq` and `Ord` over their raw bits, the same on-demand way enums derive theirs over the discriminant. See [Auto-derived Traits](#auto-derived-traits).

A flags type is a newtype over `u32`: an integer literal coerces to it, `as` converts to and from `u32`, and it inherits `u32`'s methods. Member names can carry `#[cm("...")]` attributes for Component Model name mapping:

```wado
pub flags PathFlags {
    #[cm("symlink-follow")]
    SymlinkFollow,
}
```

Note: Wado's `enum` maps to Component Model's `enum` (simple enumeration), and `variant` maps to Component Model's `variant` (tagged union with payloads). This differs from Rust where `enum` can have payloads.

---

## Object Literals

Object literal syntax supports unquoted keys and shorthand properties.

For struct initialization syntax, see the [Structs](#structs) section.

### TreeMap (Insertion-Order Map)

For associative arrays, use `TreeMap` from `core:collections`:

```wado
use { TreeMap } from "core:collections";

let mut map = TreeMap::<String, i32>::new();
map["x"] = 10;                   // insert or overwrite
map["y"] = 20;

let v = map["x"];                // panics if key not found
let opt = map.get("x");          // returns Option<V>
map.try_insert("x", 99);         // inserts only if absent; reports whether it did

// Keys preserve insertion order
let keys = map.keys();  // an iterator over the keys, in insertion order

// Functional-update spread: seed from a base map, then override/add keys
let m2: TreeMap<String, i32> = { ..map, "x": 99, "w": 40 };
```

A `{ ..base, "k": v }` key-value literal seeds the builder with every entry of
`base` (same map type) and then applies the explicit keys, so explicit keys
override the base. Like the struct form, the spread is leading and single. See
[WEP: Literal Spread](./wep-2026-07-03-literal-spread.md).

### Access Methods

```wado
// Struct: dot notation
user.name

// TreeMap: bracket notation or methods
map["key"]        // panics if key not found
map.get("key")    // returns Option<V>
```

## Serialization and Deserialization

Wado provides a format-agnostic serialization framework via `core:serde` and a JSON implementation via `core:json`. See [WEP: Serialization and Deserialization](./wep-2026-02-28-serde.md).

### Compiler-Synthesized `impl`

The syntax `impl Trait for Type;` (semicolon instead of block) signals that the compiler generates the method body. Supported traits: `From`, `Serialize`, `Deserialize`, `Eq`, `Ord`, `Default`, and `Inspect`. For the structurally-checkable traits (`Eq` / `Ord` / `Default` / serde) the marker is also a conformance check — a compile error at its own span if `Type` is ineligible. An `Inspect` marker always validates. A `Display` marker (`impl Display for Type;`) is rejected — `Display` is not derivable for an arbitrary type; write a real `impl Display { fn fmt … }`, or rely on the automatic enum / newtype `Display`.

```wado
use { Serialize, Deserialize } from "core:serde";

struct User {
    name: String,
    age: i32,
}

impl Serialize for User;      // compiler generates serialize method
impl Deserialize for User;    // compiler generates deserialize method
```

The compiler inspects the type definition (struct, enum, variant, or flags) and synthesizes the appropriate method body. This is a compile error if a field or case's type doesn't implement the required trait.

Deserialization rejects a repeated field or key by default; a format overrides `Deserializer::on_duplicate_key` to be lenient. Nesting past a format's `max_depth` is a `DepthLimitExceeded` error, not a trap.

Struct field names are serialized verbatim by default (identity); see [Serialization Names](./wep-2026-02-28-serde.md#serialization-names) for `name` / `name_policy` overrides.

### Bound-Driven Serialize / Deserialize

The marker above is optional: a `T: Serialize` bound is satisfied structurally once every field or case of `T` satisfies the trait — the same on-demand model `Eq` / `Ord` use (below). This is how an anonymous struct, which has no name for a marker, becomes serializable:

```wado
use { to_string } from "core:json";

struct Point { x: i32, y: i32 }              // no impl marker needed
let json = to_string(&Point { x: 1, y: 2 }); // Ok("{\"x\":1,\"y\":2}")
let anon = to_string(&{ x: 1, y: 2 });        // Ok("{\"x\":1,\"y\":2}") — anonymous struct
```

An anonymous literal may also compose spread bases: `{ ..a, ..b, field: v }`
builds an anonymous struct whose fields are the union of the bases' and explicit
fields, in source order, last contributor winning on a name collision (and its
type). Each base is a struct value, evaluated once. Unlike a named struct's
leading-single `..base`, composition allows spreads in any position and more than
one; a member every one of whose fields is overwritten by a later member is a
dead-write error. See [WEP: Literal Spread](./wep-2026-07-03-literal-spread.md).

```wado
let base = { user_id: 1, ip: "10.0.0.1" };
let event = { ..base, level: "warn" };  // { user_id, ip, level } — auto-Serialize
```

The explicit marker `impl Serialize for T;` still works — write it to force the impl with no bound present, or to attach `#[wire(name_policy = "...")]` customization. Like `Eq` / `Ord`'s marker (below), it is a conformance check: an ineligible field or case is a compile error at the marker's own span. See [WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md).

### Bound-Driven Eq / Ord

`Eq` / `Ord` derive the same on-demand way: the impl for `T` is synthesized only where a `==` / `<` call site, a bound, or an explicit marker needs it. It is not synthesized for every declared type.

The explicit marker `impl Eq for T;` / `impl Ord for T;` is a hard guarantee, not just a request: a compile error, with a reason chain, at the marker's own span if any field or case is ineligible:

```wado
struct Handler { cb: fn(i32) -> i32 }

impl Eq for Handler;
// compile error: cannot derive `Eq` for `Handler`: not every field/case implements `Eq`
```

### Format Traits

`${x:?}` / `${x:#?}` (`Inspect`, plainly or indented) work for every type — no bound needed.

`${x}` (`Display`) uses the type's `impl Display`. Primitives, `String`, plain enums (bare case name), and newtypes (inherited from the base type) have one. So do the prelude's sequences, tuples and ranges, where their elements allow it. Any other struct or variant needs a hand-written `impl Display`; otherwise `${x}` is a compile error and `${x:?}` gives its debug form. So `T: Display` certifies a real string representation. For example, `String::push_display` takes any `Display`. `${x:#}` runs the same `Display` with `Formatter.alternate` set; an impl that ignores the flag renders identically.

```wado
fn describe<T>(v: &T) -> String { return `${v:?}`; }         // any type
fn label<T: Display>(v: &T) -> String { return `${v}`; }     // requires a `Display`
```

See [WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md).

### JSON Module (`core:json`)

```wado
use { to_string, from_string } from "core:json";

// Serialize to JSON string
let json = to_string::<User>(&user);   // Result<String, SerializeError>

// Deserialize from JSON string
let user = from_string::<User>(json);  // Result<User, DeserializeError>
```

JSON serialization returns `Err` for `NaN` and `Infinity` float values. JSON deserialization returns `Err` for malformed input, missing required fields, type mismatches, and trailing data.

### JSON NSD Module (`core:json_nsd`)

Non-self-describing JSON format. Structs are encoded as positional arrays (field names omitted), unit variants as discriminant integers, and payload variants as `[disc, payload]`.

```wado
use { to_string, from_string } from "core:json_nsd";

// Struct as positional array
let json = to_string::<User>(&user);   // Result: Ok("[\"Alice\",30]")

// Deserialize from positional array
let user = from_string::<User>(`["Alice",30]`);  // Result<User, DeserializeError>
```

The same `Serialize` and `Deserialize` trait impls work with both `core:json` and `core:json_nsd`.

### Command-Line Arguments (`core:args`)

`core:args` is a non-self-describing, parse-only `Deserializer` over `argv`. Argument types are ordinary structs with `impl Deserialize for T;`: fields become `--long` options, and fields marked `#[wire(positional)]` are filled from bare tokens in declaration order (required, optional, or variadic). Scalar tokens are converted with `LenientFromStr`. See [WEP: Command-Line Argument Parsing](./wep-2026-06-22-core-args.md).

```wado
use { parse } from "core:args";
use { Deserialize } from "core:serde";

struct Cli {
    #[wire(positional)] input: String,
    jobs: i32 = 1,
    verbose: bool = false,
}
impl Deserialize for Cli;

let cli = parse::<Cli>(["in.txt", "--jobs", "4", "--verbose"]);
```

## Module System

Wado uses an ESM-like import syntax with `use {...} from "module"`. This aligns with JavaScript/TypeScript conventions, as JavaScript is a primary host environment for Wado.

### Visibility

Visibility has two orthogonal axes: a Wado scope ladder (`internal` / `pub`)
and a CM-surface flag (`export`). See [WEP: Visibility — `internal` / `pub` /
`export`](./wep-2026-06-25-visibility-internal-pub-export.md).

| Keyword    | Axis    | Reach                                             |
| ---------- | ------- | ------------------------------------------------- |
| (none)     | scope   | The defining file (private)                       |
| `internal` | scope   | Other files in the same package                   |
| `pub`      | scope   | Other Wado packages — the library API             |
| `export`   | CM flag | Also lowered at the CM boundary; CM-representable |

`pub` is the library boundary (Wado-native, so generics, closures, and traits
may cross it). `export` is the Component Model boundary and is additive:
`export ⟹ pub`, and an `export`ed signature must be CM-representable, checked at
the definition site.

```wado
// Private to this file (default)
fn helper() { ... }

// Package-internal - accessible from other files in this package
internal fn build_ast() -> Doc { ... }

// Library API - accessible from other Wado packages (Wado-native)
pub fn map<T, U>(f: fn(T) -> U, xs: List<T>) -> List<U> { ... }

// Library API + CM boundary export
export fn run() { ... }
```

| Declaration         | Same file | Same package | Other Wado packages | CM boundary |
| ------------------- | --------- | ------------ | ------------------- | ----------- |
| `fn foo()`          | Yes       | No           | No                  | No          |
| `internal fn foo()` | Yes       | Yes          | No                  | No          |
| `pub fn foo()`      | Yes       | Yes          | Yes                 | No          |
| `export fn foo()`   | Yes       | Yes          | Yes                 | Yes         |

A `pub`-only item reaches Wado consumers only (source dependency or
provider-tagged `.wasm`); a non-Wado CM consumer sees `export` items only.

The ladder applies to top-level items, struct fields, and `impl` members
(methods, associated constants); reaching one beyond its rung is a compile
error. `export` on a member is an error — a method has no CM boundary. Only an
_inherent_ member has a ladder; a trait impl's members reach as far as the
trait.

```wado
impl Config {
    fn parse_raw() { }         // this file only
    internal fn reload() { }   // other files in this package
    pub fn get() { }           // other packages
}
```

#### Signature reach

An item's signature may not name a declaration that reaches less far than the
item itself. Naming one is a compile error at the reference. A caller that
reaches the item has to be able to write the types it names, and a `pub fn`
returning a file-private struct hands back a value whose type no caller can
write.

```wado
struct Hidden { n: i32 }

pub fn make() -> Hidden { ... }   // ERROR: widen `Hidden`, or narrow `make`
```

The rule holds at every rung: an `internal` item may not name a file-private
type either. `export` counts as `pub` here. Whether the type crosses the CM
boundary is a separate question, answered at the definition site.

Where an item carries no modifier of its own, its reach comes from what encloses
it. A struct field reaches no further than its struct. An impl's member reaches
no further than the impl's head, and a trait impl's no further than the trait
either: a caller has to be able to write the head to name the member. So
`impl Add for Local` on a file-private `Local` gives its `add` that same reach,
and `add` may then name `Local` freely. An impl's own bounds count as part of
its head: a type that cannot name `T`'s bound cannot satisfy it, so
`impl<T: Local> Add for T` confines `add` the same way.

A declared reach is a claim, so a bound on one is checked instead of narrowing
it. `pub fn f<T: Local>()` and `pub trait F<T: Local>` are errors, because no
caller can supply the `T` they ask for.

A type parameter, `Self`, and an associated-type projection (`Self::Output`,
`I::Item`) are binders rather than declarations, so they carry no reach of their
own and are not checked.

The signature is everything the caller has to be able to write or read back, not
just the parameters and the return type. A type parameter's default is
instantiated at the call, an impl's associated type comes back through
`Self::Out`, and a resource's parent carries the methods it inherits, so all
three obey the rule.

A bound obeys the rule too, so naming a less visible trait in one is the same
error. Rust's sealed-trait pattern seals a trait by giving it a supertrait that
implementors cannot reach, and that is exactly what this forbids. Wado has no
equivalent. If sealing is wanted, it gets a keyword that says so.

#### Re-export visibility

A `use` declaration carrying a visibility modifier re-exports the imported names
as members of the importing module, at the modifier's reach:

| Form                          | Re-exported reach                           |
| ----------------------------- | ------------------------------------------- |
| `pub use { x } from "M"`      | `x` joins this module's public API          |
| `internal use { x } from "M"` | `x` is re-exported package-internal         |
| `use { x } from "M"`          | file-private import; `x` is not re-exported |

A re-export cannot reach further than `x` itself, so `pub use { x }` requires
`x` to be `pub`; claiming more is a compile error at the re-export. Narrowing is
allowed, and the facade still names the API — a package's entry module publishes
its items under its own names, so consumers never name the files behind them:

```wado
// foo/impl.wado — the implementation
pub fn compute() -> i32 { ... }

// foo.wado — the package's entry module
pub use { compute } from "./impl.wado";   // reached as foo's `compute`
```

You may also only re-export a name you can see: `x` must be importable here
(`x` is `pub`, or `x` is `internal` and `M` is in this package). Re-exporting a
file-private name is a visibility error, like any other import. See [Re-export Syntax (`pub use`)](./wep-2026-01-25-pub-use-reexport.md).

### Module Source Types

| Source Type   | Syntax                        | Example                              |
| ------------- | ----------------------------- | ------------------------------------ |
| WASI standard | `"wasi:<package>"`            | `"wasi:cli"`, `"wasi:filesystem"`    |
| Core library  | `"core:<module>"`             | `"core:cli"`, `"core:json"`          |
| CM coordinate | `"<ns>:<pkg>[@<ver>]"`        | `"docs:regex"`, `"docs:regex@1.0.0"` |
| Library alias | `"lib:<nick>"`                | `"lib:router"`, `"lib:shared"`       |
| Local file    | `"./<path>"` or `"../<path>"` | `"./utils.wado"`, `"../config.wado"` |

A specifier names a package only — no interface segment; interfaces and members are selected in the `use { ... }` list. `core:`/`wasi:` are bundled coordinates, not a separate scheme. See [WEP: Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md).

### Module Path Validation

Relative paths in Wado follow the gitignore / shell convention: a path that refers to a file relative to the current file must begin with `./` (next to me) or `../` (up one). A bare path (`foo/bar`, `utils.wado`) is never relative-to-here — it is read as a namespace/coordinate or handed to the host, and is rejected wherever only a relative file path is valid. This rule is uniform across every path literal: module imports (`use ... from`), `#include_str` / `#include_bytes`, and Kiln input paths (`from`, `generator.inputs`, `generator.output_dir`).

Module paths are validated before loading to provide clear error messages:

Namespace Resolution (a namespace is reserved iff the compiler bundles it):

1. Bundled namespaces `core:` / `wasi:`: resolved from the embedded stdlib.

2. Open coordinates `<ns>:<pkg>` (any other namespace): resolved from a `[dependencies]` entry in `wado.toml` or an inline `with` source. An undeclared coordinate is an error.

3. Library aliases `lib:<nick>`: resolved via `wado.toml` or an inline `with`. An alias renames a dependency, shortens its name, tells two major versions apart, or names a dependency with no public coordinate.

4. Local modules (`./` or `../`): Resolved relative to importing module.

5. Invalid paths: Paths not matching any pattern are rejected.
   - Error: `invalid module path 'xxx'; use './' for local modules or 'namespace:' for library modules`

Bare names (`"router"`) are rejected. The one exception is a bare key in `[dependencies]`, which is deprecated and draws a warning. See [WEP: Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md) for resolution and version rules.

### Symbol Notation

A symbol is named `MODULE#SYMBOL` — the written form used by docs, `wado query`, and diagnostics. `MODULE` is the import specifier verbatim (quoted as in `use`; quotes may be dropped for a scheme or bare name with no whitespace). `SYMBOL` uses Wado's own operators, so its kind is visible from the separator: `::` for static scope, `.` for an instance method, `^` for a trait-impl member.

```
core:json#to_string                        # free function / global
core:collections#TreeMap::new              # associated const / static fn
core:collections#TreeMap.get               # instance method
core:collections#TreeMap<String, i32>.get  # generics use Wado angle brackets
core:url#Url^Display::fmt                  # trait-impl member
"./utils.wado"#Helper::new                 # relative path — must be quoted
```

See [WEP: Symbol Notation](./wep-2026-06-14-symbol-notation.md).

### Import Syntax

```wado
// ============================================
// WIT Package = Wado Module
// WIT Interface = Wado interface
// ============================================

// 1. WASI standard modules (wasi:*)
use {Stdout, Stderr} from "wasi:cli";
use {Stdout::{write_via_stream}} from "wasi:cli";

// Interface and its members together
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";

// 2. Core library (core:*)
use {println, eprintln} from "core:cli";
use {to_string, from_string} from "core:json";

// 3. Local files (relative path, extension required)
use {Helper} from "./utils.wado";
use {Config} from "../config.wado";

// 4. CM coordinate (declared in wado.toml, or given an inline `with` source)
use {Regexp} from "docs:regex";

// 5. Library alias (rename / private / coordinate-less dependency)
use {Router} from "lib:router";
```

Implementing a trait requires naming it: `impl Trait for Type` and the bodiless
derive form `impl Trait for Type;` both need `Trait` in scope, whether declared
in the module, imported, or auto-imported from the prelude.

```wado
use {Deserialize} from "core:serde";
impl Deserialize for Config;          // OK

impl Deserialize for Config;          // error without the import
```

An import's local name must not collide with a declaration in the importing
module. The name would mean two declarations at once and nothing could say
which, so the program is rejected; an alias says which one was meant.

```wado
use {Widget} from "./other.wado";
pub struct Widget { … }               // error: collides with the import

use {Widget as Theirs} from "./other.wado";
pub struct Widget { … }               // OK
```

### Import Attributes (`with`)

Use `with { ... }` to specify import metadata:

```wado
// Inline dependency source (single-file scripts; no wado.toml needed).
// Same vocabulary as a [dependencies] value, with an exact version.
use {Regexp} from "docs:regex@1.0.0" with { registry: "oci://ghcr.io/acme" };  // exact pin via the specifier
use {Router} from "lib:router" with { git: "https://github.com/user/router.git", ref: "v1.0" };
use {Parse}  from "lib:rx"     with { registry: "oci://ghcr.io/acme", package: "docs:regex", version: "1.0.0" };

// Type attribute (REQUIRED for non-.wado imports)
use {sin, cos} from "./libm.wasm" with { type: "wasm" };
```

An inline `with` source and a `wado.toml` entry for the same specifier are mutually exclusive. Version ranges (`^`/`~`/`=`) are allowed only in `wado.toml`, where a lock file resolves them; the specifier `@ver` and a single-file `with` take an exact version — a range there is an error.

#### Type Attribute Requirement

| Import Source      | `type` Attribute         | Notes                          |
| ------------------ | ------------------------ | ------------------------------ |
| `.wado` files      | Optional                 | Type inferred from Wado source |
| `.wasm` files      | Required                 | `type: "wasm"`                 |
| `.wat` files       | Required                 | `type: "wat"`                  |
| `core:*`, `wasi:*` | Not applicable           | Bundled namespace handling     |
| `https:` URLs      | Required for non-`.wado` | Must specify content type      |
| CM / `lib:` deps   | Optional                 | Type inferred from package     |

#### Rationale

Explicit type annotations prevent ambiguity and make dependencies clear, aligning with Wado's design philosophy of explicit imports.

### Generated Imports (Kiln)

See [WEP: Kiln](./wep-2026-04-12-kiln.md) and [WEP: Gale](./wep-2026-03-02-gale.md).

A `use` clause whose source is neither a `.wado` module nor a Wasm asset (`.wasm` / `.wat`) is processed by Kiln — a code-generation pipeline that lowers the input to ordinary Wado source which the compiler then handles like any user-authored module. `.g4`, `.proto`, `.graphql`, `.wit`, and a Wado dialect's own extension all take this path. The `with { generator: { ... } }` clause specifies which generator to invoke:

```wado
// Gale generates a parser from an ANTLR4 grammar
use { Parser } from "./Calc.g4" with {
    generator: {
        module: "wado-lang:gale",
    },
};

// With supplementary input files (paths relative to the source file)
use { RustParser } from "./Rust.g4" with {
    generator: {
        module: "wado-lang:gale",
        inputs: ["./RustLexer.g4"],
    },
};
```

#### `with { generator: { ... } }` fields

| Field        | Required | Meaning                                                                                                                                  |
| ------------ | -------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `module`     | yes      | Generator module — either a `<namespace>:<name>[@<version>]` reference resolved against `[build-dependencies]`, or a relative `./` path. |
| `options`    | no       | Record literal whose shape matches the generator's exported `pub struct Options`. Omit when every field has a default.                   |
| `inputs`     | no       | Supplementary input paths the generator cannot discover from the primary alone (e.g. a sibling lexer grammar).                           |
| `output_dir` | no       | Override for the per-invocation generated-source directory (default `build/kiln/<synthesized-id>/`).                                     |

#### Manifest

Generators are declared in `[build-dependencies]` of `wado.toml` (a build-only graph that does not enter the consuming project's runtime dependency graph):

```toml
[build-dependencies]
"wado-lang:gale" = { version = "^0.0.9" }
```

A bare `use { ... } from "./schema.g4"` against such a file with no `with` clause is a hard error (`KILN_MISSING_WITH`). Two `use` clauses for the same `from` in the same file collapse to a single invocation if their `(module, inputs, options, output_dir)` match; mismatched clauses are a duplicate-generator error.

A file that is not `.wado` is only ever reached through a generator. When a `use` names one and no invocation produced a module for that schema, the import is a hard error (`KILN_NO_GENERATED_MODULE`); the compiler never falls back to parsing the schema as Wado.

#### Authoring a generator

A generator is a normal Wado package whose `wado.toml` maps the `core:kiln/generator` world to a module under `[world]`:

```toml
[world]
"core:kiln/generator" = "src/generator.wado"
```

That module exports the world's `generate` function:

```wado
use { Request, Response, Error } from "core:kiln";

pub struct Options {
    namespace: String,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    // ... parse req.primary.content, emit Wado source ...
}
```

Every use site's `options` is type-checked against the generator's `Options`. Generators run in a deterministic sandbox (no clocks, randomness, network, environment, or filesystem): every input they see arrives by value, listed at the use site. Outputs are persisted under `build/kiln/<synthesized-id>/` and stamped with a `#![generated(by = "...", sources = [...])]` header. A compile reruns a generator only when its inputs have changed.

### Wasm Module and Component Imports

A `.wasm` / `.wat` asset is imported directly with `with { type: "wasm" | "wat" }`. The compiler detects from the binary header whether the file is a core module or a Component Model component — both `.wasm` shapes use `type: "wasm"`; the distinction is detected, not declared. A single `use` may pull several names (functions from a core module, interfaces from a component).

| Imported file             | Exposes as                                     | Call style                                |
| ------------------------- | ---------------------------------------------- | ----------------------------------------- |
| Core wasm module / `.wat` | One free `pub fn` per export                   | `helper(x)` — plain function              |
| CM component (`.wasm`)    | One Wado `interface` per exported CM interface | `Iface::method(x)` — effectful, like WASI |

```wado
// Core wasm / wat — exports become free functions.
use { sin, cos } from "./libm.wat" with { type: "wat" };
use { helper }   from "./mod.wasm" with { type: "wasm" };

// CM component — each exported interface becomes a Wado `interface`,
// and its functions are called like WASI methods (effectful).
use { Compress, Decompress } from "./brotli.wasm" with { type: "wasm" };

export fn run() with (Compress, Decompress) {
    let packed = Compress::compress(bytes);
    let back = Decompress::decompress(packed);  // Result<List<u8>, String>
}
```

Values lower/lift across the CM boundary per [Type Mapping at Component Boundaries](#type-mapping-at-component-boundaries). The dependency component is statically composed into the output, so the result runs standalone. See [WEP: Wasm Module Import](./wep-2026-01-10-wasm-import.md) for the core-wasm path and [WEP: Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) for the component path.

### Namespace Import

Use `use name from "..."` (without curly braces) to import an entire module as a namespace:

```wado
// Import a module as a namespace
use utils from "./utils.wado";
utils::helper_function();      // not utils["helper_function"], as it's analyzed at compile time
```

A namespace import binds one name, the namespace. The source module's pub symbols are reached through the `ns::` prefix and are not imported under their bare names, so `distance(p1, p2)` below is an unknown function:

```wado
use geo from "./geo.wado";

// Functions
geo::distance(p1, p2);

// Types (structs, enums, variants)
let p: geo::Point = geo::Point::origin();
let c = geo::Color::Red;
let s = geo::Shape::Circle(3.14);

// Traits and types in an `impl` header, on either side
impl geo::Show for Local { ... }
impl Show for geo::Tag { ... }
```

A qualified head names the namespace's declaration even where the importing
module declares one of its own by that name.

#### Note

Wado does not support `use * as name` or default imports.

### Import Rules

- Named imports use curly braces: `use {x, y} from "..."`
- Namespace imports omit curly braces: `use name from "..."`
- Wildcards prohibited: `use {*} from "..."` is not allowed
- All imports must be explicit (except the prelude)
- Use `::` for effect operation access: `Effect::{op1, op2}`

```wado
// Valid patterns
use {println, eprintln} from "core:cli";        // Named import
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";
use utils from "./utils.wado";                   // Namespace import

// Prohibited patterns
use * from "core:cli";           // Wildcard not allowed
use {*} from "core:cli";         // Wildcard not allowed
```

Without braces, `use println from "core:cli"` is a namespace import named `println`, so the function is `println::println`.

### Calling Effect Operations

Effect operations use `::` syntax:

```wado
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";

fn example() with Stdout {
    // With import - direct call
    write_via_stream(stream);

    // Fully qualified - always works
    Stdout::write_via_stream(stream);
}
```

Notation distinction:

- `.` → struct fields and methods (`user.name`, `stream.read()`)
- `::` → effect operations and namespace access (`Stdout::write_via_stream()`)

### Renaming Imports

```wado
use {Stdout::{write_via_stream as stdout_write}} from "wasi:cli";
use {Stderr::{write_via_stream as stderr_write}} from "wasi:cli";

fn log() with (Stdout, Stderr) {
    stdout_write(out_stream);
    stderr_write(err_stream);
}
```

### Re-exports (`pub use`)

Re-exports make imported symbols available to other modules that import from this module:

```wado
// math/internal/trig.wado
pub fn sin(x: f64) -> f64 { ... }
pub fn cos(x: f64) -> f64 { ... }

// math/mod.wado - re-export from internal modules
pub use {sin, cos} from "./internal/trig.wado";
pub use {sin as sine} from "./internal/trig.wado";  // with rename

// user code - import from the facade (a "math" dependency declared in wado.toml)
use {sin, cos, sine} from "lib:math";
```

Re-export rules:

- `pub use` combines `pub` visibility with import syntax
- A re-export reaches no further than the symbol it names (see [Re-export visibility](#re-export-visibility))
- Re-export chains are resolved transparently (A re-exports from B, B re-exports from C)
- Circular re-exports are prohibited
- Wildcards prohibited: `pub use * from "..."` is not allowed

### Exception: The Prelude

The prelude is automatically imported into every module, making `String`, `List`, `Option`, `Result`, `Stream`, `Future`, and the prelude traits available without explicit imports.

### Standard Library

```
core            # core: namespace for the core library
├── prelude     # Automatically imported (String, List, Option, Result, Stream, Future)
├── cli         # CLI helpers (println, eprintln, args, env, exit, ...)
├── serde       # Serialization traits (Serialize, Deserialize, Serializer, Deserializer)
├── json        # JSON format implementation (to_string, from_string)
├── collections # TreeMap, TreeSet
├── base64      # Base64 encoding/decoding
├── zlib        # Compression
├── ...
wasi            # wasi: namespace for system interfaces
├── cli
├── filesystem
├── ...
```

The [cheatsheet's Standard Library section](./cheatsheet.md#standard-library) links the API reference for every module.

### Global Functions defined in `core:prelude`

```wado
panic("error"); // traps with a message
unreachable(); // traps  with no message
```

## The `assert` Statement

The `assert` keyword is used to assert that a condition is true. If the condition is false, the program will panic with messages that includes the source of the condition and related intermediate values (like the power-assert).

```wado
// If x is not greater than 0, the program will panic, printing x.
assert x > 0;

// Also assert can take an optional message.
assert x > 0, "x must be checked elsewhere";
```

`assert` evaluates its condition exactly as the surrounding code would, so a
guarded operand never runs when its guard fails. An operand the run did not
reach is reported as `<not evaluated>`.

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

To keep a failure readable, each captured operand is rendered with `Inspect`
(`:?`), which caps sequence types at a default length (`DEFAULT_SEQ_LIMIT` = 256):
a `String` operand is truncated to 256 characters with a `...` marker and an
`List` operand to 256 elements (see [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md)).
Non-sequence operands such as floats keep their natural rendering. The optional
user message is formatted with the user's own template specifiers (typically
`Display`, which is never capped) — opt into a longer dump by formatting the
value yourself.

## Testing

Tests are first-class syntax: a `test` block declares one, and `wado test` runs them. The runner's flags, file discovery and output are described by `wado test --help` and [WEP: Test Discovery](./wep-2026-05-02-test-discovery.md).

### Test Declaration Syntax

Tests are declared using the `test` keyword followed by an optional name and a block:

```wado
// Named test
test "addition works" {
    assert 1 + 1 == 2;
}

// Unnamed test
test {
    let result = compute_something();
    assert result > 0;
}

// Test with multiple assertions
test "string operations" {
    let s = "hello";
    assert s.len() == 5;
    assert s + " world" == "hello world";
}

// Expect-trap test: the test passes when the body traps
#[expect_trap]
test "panics on bad input" {
    panic("intentional panic");
}

#[expect_trap]
test {
    unreachable();
}

// TODO test: marks a test for an unimplemented feature.
// Reported on a separate axis from pass/fail (see Test Outcome Model).
#[TODO]
test "not yet implemented" {
    panic("TODO: implement this feature");
}

// Custom timeout: override the default 5000ms limit
#[timeout_ms(30000)]
test "slow computation" {
    let result = expensive_computation();
    assert result == 42;
}

// Synopsis test: runs like any test; `wado doc` renders its body as the
// module's `## Synopsis` section.
#[synopsis]
test {
    let p = Point { x: 3, y: 4 };
    assert p.length() == 5.0;
}
```

#### Syntax Rules

- `test` is a contextual keyword (functions named `test` are still allowed)
- Test name is an optional string literal
- Test body is a block containing statements
- No return type or effect declarations needed
- Tests can use any effects (side effects are allowed in tests)
- Attributes (e.g., `#[expect_trap]`, `#[TODO]`, `#[timeout_ms(N)]`, `#[synopsis]`) may appear before the `test` keyword

### Test Semantics

#### Execution

- Each test runs in isolation with fresh state: every global starts from its initializer
- Tests are independent, so they may run in any order, and concurrently
- A test passes if it completes without panicking or trapping
- A test fails if `assert` fails, `panic` is called, or a trap occurs
- Test blocks belong to the `test` world. Compiling for any other world leaves them out

#### `#[expect_trap]` Attribute

The `#[expect_trap]` attribute inverts the pass/fail condition for a test:

- The test passes if the body traps (calls `panic`, `unreachable`, or fails an `assert`)
- The test fails if the body completes normally without trapping

This is useful for verifying that invalid operations are correctly rejected at runtime:

```wado
#[expect_trap]
test "panics on null dereference" {
    let opt: Option<i32> = null;
    // force a trap by accessing None without checking
    panic("expected None but got value");
}
```

#### `#[TODO]` Attribute

The `#[TODO]` attribute marks a test as a placeholder for a feature not yet implemented. TODO tests are reported on a separate axis from regular pass/fail results (see Test Outcome Model below). When the body traps, the test is reported as pending (expected). When the body completes normally, the test is reported as resolved, which is a hard failure requiring the developer to remove the `#[TODO]` attribute.

#### `#[timeout_ms(N)]` Attribute

The `#[timeout_ms(N)]` attribute overrides the default test timeout (5000ms) for a specific test. `N` is an integer literal specifying the timeout in milliseconds. If a test exceeds its timeout, it is interrupted and fails. This is useful for tests that involve expensive computation or I/O:

```wado
#[timeout_ms(30000)]
test "large data processing" {
    let result = process_large_dataset();
    assert result.len() > 0;
}
```

### Test Outcome Model

Test results are classified into two independent axes: the pass/fail axis for regular tests, and the TODO axis for tests marked with `#[TODO]`.

#### Regular Tests

| Condition                                               | Outcome |
| ------------------------------------------------------- | ------- |
| Body completes normally                                 | pass    |
| Body traps (panic, assert failure, unreachable)         | fail    |
| Body traps and `#[expect_trap]` is present              | pass    |
| Body completes normally and `#[expect_trap]` is present | fail    |

#### TODO Tests

Tests marked with `#[TODO]` are reported separately from regular tests. They do not contribute to the pass or fail count.

| Condition               | Outcome         | Action Required                           |
| ----------------------- | --------------- | ----------------------------------------- |
| Body traps              | todo (pending)  | None — the feature is still unimplemented |
| Body completes normally | todo (resolved) | Remove `#[TODO]` — the feature now works  |

A resolved TODO test fails the run. This enforces cleanup: once the underlying feature is implemented, the `#[TODO]` attribute must be removed so the test joins the regular pass/fail pool.

A pending TODO test never causes a failure. This means fixing a compiler bug cannot increase the failure count — newly-passing TODO tests appear as "resolved" on the TODO axis rather than as unexpected failures on the pass/fail axis.

#### `#![TODO]` Modules

The `#![TODO]` inner attribute applies TODO semantics to an entire module:

- If the module fails to compile, it is reported as a single pending TODO entry.
- If the module compiles successfully, each test block is implicitly treated as `#[TODO]`.
- If the module compiles and all tests pass (i.e., the feature is implemented), it is reported as resolved, which fails the run.

### Test File Conventions

`*_test.wado` is the recommended name for a file that contains only tests. The
suffix is not required: any file with `test` blocks contributes its tests.

```
src/
  math.wado
  math_test.wado      # Tests for math.wado (recommended convention)
  string.wado
  string_test.wado    # Tests for string.wado
```

Tests can also be placed in a separate `tests/` directory:

```
src/
  lib.wado
tests/
  integration_test.wado
```

### Example Test File

```wado
// math_test.wado
use {add, multiply} from "./math.wado";

test "add positive numbers" {
    assert add(2, 3) == 5;
    assert add(0, 0) == 0;
}

test "add negative numbers" {
    assert add(-1, -1) == -2;
    assert add(-5, 3) == -2;
}
```

## Concurrency Model

Wado follows the Component Model's concurrency model. It has no `await`: a
wait blocks the current task until the value is ready, so an ordinary function
may wait without saying so in its signature.

### Async Imports

A Component Model `async func` import is an interface operation declared
`async fn op(...) -> AsyncCall<T>`. Calling it starts the call and returns an
`AsyncCall<T>` at once. The caller decides when to wait:

- `.wait()` blocks until the call returns, then yields its `T`.
- `.cancel()` abandons the call.
- `.join(&set)` adds the call to a `WaitableSet`, so one wait covers several
  calls and streams.

```wado
use { Client, Request, Response, ErrorCode } from "wasi:http";

fn fetch(req: Request) -> Result<Response, ErrorCode> with Client {
    let call = Client::send(req);   // the request starts; nothing waits yet
    // ... work here runs while the host handles the request ...
    return call.wait();             // blocks until the response arrives
}
```

An `AsyncCall<T>` is used once: after `wait` or `cancel` it must not be touched
again. A handler for an async operation resumes with the `T` itself, and the
caller's `.wait()` returns it at once. See
[WEP: Generic `AsyncCall<T>`](./wep-2026-04-22-subtask-generic.md) and
[WEP: Effect Handler](./wep-2026-04-11-effect-handler.md).

### Async Exports

An `export async fn` uses the Component Model async calling convention. Its
body delivers the result with [`task return`](#task-return-statement) and may
keep running afterwards, for example to write a response's trailers.

### Streams and Futures

`Stream<T>` and `Future<T>` are unbuffered channels. A `write` blocks until the
other end reads, and a `read` blocks until the other end writes, so the two ends
must be driven by different tasks.

## Effect System

### Design Philosophy

The Effect System is equivalent to:

- Tracking access to external resources / global variables
- Implicitly propagating DI (Dependency Injection)
- Direct correspondence with WASI Capabilities

### Effect Definition

An effect is declared as an `interface`, whose operations are free functions:

```wado
// WASI CLI effects (see wasi:cli for the real definitions)
interface Stdout {
    fn write_via_stream(data: Stream<u8>) -> Future<Result<(), ErrorCode>>;
}

interface Stderr {
    fn write_via_stream(data: Stream<u8>) -> Future<Result<(), ErrorCode>>;
}

interface Environment {
    fn get_environment() -> List<[String, String]>;
    fn get_arguments() -> List<String>;
    fn get_initial_cwd() -> Option<String>;
}

// A custom effect interface
interface Http {
    fn get(url: String) -> String;
    fn post(url: String, body: String) -> String;
}
```

#### Default Implementations

An `interface` is a trait with a different dispatch story, and its members are written exactly as a trait's are: an operation is a signature ending in `;`, or a signature followed by a block. That block is the operation's default implementation: what the operation does when it is dispatched with no handler installed. Without one, dispatching an unhandled operation traps.

```wado
interface Log {
    fn emit(message: String) {
        log_stderr(message);          // no handler installed: degrade, don't trap
    }

    fn level() -> i32;                // no default: unhandled dispatch traps
}
```

A default fills a handler that leaves the operation out, and it is what `..forward` reaches when the outermost handler forwards an operation nobody else handles. So a layer that only decorates one operation is installable on its own. An explicit `..trap` still wins: a mock that says an operation must not be called means it.

A default is a handler body, so it runs in the outer scope like every other one (see [Handlers](#handlers)): an `Effect::op(...)` inside a default reaches the next handler out, not the handler the default is filling. That is what keeps a forward from recursing into itself, and it is the one place the analogy with a trait's default method stops. A trait default calling `self.other()` reaches the impl's override, and a filled operation's call does not.

A parameter may declare a default, and a call that omits the argument gets it filled in at the call site, as a function call does. The handler receives the argument already in place.

Beyond a name, parameters and a return type, an operation declares nothing else. Each of these is a compile error, for the reason given:

- A body on an operation a Component Model import backs (one carrying `#[cm(...)]`, and every `resource` method). Its no-handler case is the CM adapter, so the body could never run.
- A body on an `async` operation. Its call site is typed as an `AsyncCall`, which a plain body does not produce.
- A `self` receiver. An operation is called as `Effect::op(args)`, with no receiver to bind it to.
- A `with` clause. An operation's effects are not required at its call sites, so one would let a default perform a capability its caller never declared. A default has to be performable wherever it is dispatched, which means pure or `#[ambient]` code.
- A `#[retain(...)]` attribute. An operation dispatches to a handler, whose own body states what it keeps, so one here would constrain call sites on a promise the handler never makes.
- Type parameters. Dispatch holds one slot per operation, not one per instantiation.

#### Async Operations

An operation that maps to a WIT `async func` is declared `async fn`, and its return type must be `AsyncCall<T>`. How a caller waits on the result is in [Async Imports](#async-imports).

```wado
// From wasi:http
#[cm("wasi:http/client@0.3.0")]
pub interface Client {
    #[cm("wasi:http/client@0.3.0#send")]
    async fn send(request: Request) -> AsyncCall<Result<Response, ErrorCode>>;
}
```

A function that calls an async operation is not itself async. `async` marks only the operation, and the `export async fn` of a world export (see [`task return`](#task-return-statement)).

### Effect Declaration in Functions

```wado
// Declare required effects with `with`
fn greet(name: String) with Stdout {
    println(`Hello, ${name}!`);
}

// Multiple effects
fn show_env() with (Stdout, Environment) {
    let args = Environment::get_arguments();
    println(`Arguments: ${args:?}`);
}

// A method declares its effects the same way
impl Logger {
    fn log(&self, message: String) with Stderr {
        eprintln(`${self.prefix}${message}`);
    }
}

// No effects = pure function
fn add(a: i32, b: i32) -> i32 {
    return a + b;
}
```

A row of one goes bare; a row of more than one is parenthesized, wherever the
row appears. So a comma after a bare effect always belongs to the enclosing
list, never to the row:

```wado
fn apply<T, effect E>(f: fn(T) -> T with E, x: T) -> T with E { ... }
fn both(f: fn() with (Stdout, Stderr), x: i32) { ... }
```

Every row member is an effect.

### Importing Effect Operations

To avoid the verbosity of `Effect::operation()` calls, you can explicitly import effect operations:

```wado
// Import effect operations
use {Stdout::{write_via_stream}} from "wasi:cli";
use {Environment::{get_environment, get_arguments}} from "wasi:cli";

pub fn println(message: String) with Stdout {
    // Create stream, start consumer, write data, close stream
    // (simplified - see core:cli for full implementation)
    write_via_stream(...);
}

pub fn env(name: String) -> Option<String> with Environment {
    let vars = get_environment();  // No need for Environment:: prefix
    for let [key, value] of vars {
        if key == name {
            return Some(value);
        }
    }
    return None;
}
```

#### Import Rules

- Effect operations use `::` syntax: `use {Effect::{op1, op2}} from "..."`
- Multiple operations can be imported: `Effect::{op1, op2, op3}`
- Renaming is supported: `use {Effect::{op as renamed}} from "..."`
- Wildcards are prohibited: `use {Effect::{*}}` is not allowed
- An imported operation demands what `Effect::op()` demands (see [Effect Propagation](#effect-propagation))

#### Name Resolution

- Imported effect operations can be called directly without the `Effect::` prefix
- If an operation name is ambiguous, use the fully qualified `Effect::operation()` syntax
- Non-imported effect operations must always use the `Effect::operation()` syntax

```wado
// Example with name collision handling
use {Stdout::{write_via_stream}} from "wasi:cli";
use {Stderr::{write_via_stream as stderr_write}} from "wasi:cli";

pub fn log(message: String) with (Stdout, Stderr) {
    write_via_stream(...);  // Calls Stdout::write_via_stream
    stderr_write(...);      // Calls Stderr::write_via_stream (renamed)
}
```

### Effect Propagation

Every function declares its effects, whatever its visibility. Nothing is inferred from the body. A call demands of its caller:

- for a function, the effects in its `with` clause;
- for an operation of a host-backed interface (one carrying `#[cm(...)]`), that interface;
- for an operation of a user-defined interface, nothing. An installed handler answers it, and it traps where none is installed (see [Handlers](#handlers)).

```wado
fn helper() {
    println("x");      // ERROR: missing effect 'Stdout' required by 'println'
}

fn next_id() -> i32 {
    return Counter::next();   // OK: `Counter` is a user-defined interface
}

pub fn report() with (Stdout, Preopens) {   // `pub` changes nothing
    // ...
}
```

### Generic Effects (Effect Polymorphism)

Use `<effect E>` to declare a generic effect parameter. `E` can represent zero or more concrete effects, inferred from function-typed arguments at each call site.

```wado
fn wrapper<effect E>(f: fn() with E) with E {
    f();
}

fn map<T, U, effect E>(arr: List<T>, f: fn(T) -> U with E) -> List<U> with E {
    // ...
}
```

Effect parameters:

- Are declared with the `effect` keyword in generic parameter lists
- At most one effect parameter is allowed per function
- Are inferred from the effects of function-typed arguments at each call site; when multiple function-typed arguments reference the same effect parameter, `E` resolves to the union of all their effects
- Can coexist with type parameters: `<T, effect E>`
- Test functions implicitly have all effects

### Effects on Trait Methods

A trait method's `with` clause is the contract every impl of it writes to. An impl method may not declare an effect the trait method leaves out, and a call to the method requires what the trait declares, whichever impl runs.

```wado
trait Source {
    fn next(&mut self) -> i32 with Stdout;
}

impl Source for Loud {
    fn next(&mut self) -> i32 with Stdout { ... }   // matching the declaration
}

fn draw<S: Source>(s: &mut S) -> i32 with Stdout {  // required: `s.next()` needs it
    return s.next();
}
```

A call reaches a method through a type parameter's bound in three shapes: a method call on a receiver whose type is the parameter, a static call written `T::make()`, and a `for-of` over an iterable whose type is the parameter. None of them knows which impl runs, so each demands what the trait method declares.

An `interface` is exempt: its operations declare no effects, and a handler method answers an operation rather than implementing a trait contract.

#### The Trait Head

A `with` clause on the trait itself says what every impl of it may do. A method's own clause overrides it.

| Head                          | Every impl of it       |
| ----------------------------- | ---------------------- |
| `trait Foo { … }`             | as `with _`, diagnosed |
| `trait Foo with () { … }`     | is pure                |
| `trait Foo with Stdout { … }` | gets exactly `Stdout`  |
| `trait Foo with _ { … }`      | brings its own effects |

`with _` is sugar for `<effect E> with E`, so an open head hands each impl its own effects and names none of them:

```wado
trait Source with _ {
    fn next(&mut self) -> i32;
}

impl Source for Loud {
    fn next(&mut self) -> i32 with Stdout { ... }   // E = Stdout
}

fn draw<S: Source>(s: &mut S) -> i32 with _ {       // as effectful as `S`
    return s.next();
}

export fn run() with Stdout {
    println(`${draw(&mut loud)}`);                  // Stdout, from `Loud`'s impl
}
```

A fixed head demands the same effects of every caller. An open one is resolved from the type each call names, so `draw(&mut quiet)` demands nothing when `Quiet`'s impl declares nothing. The `with _` in `draw`'s signature is what leaves that open. A caller that forwards the effects instead of resolving them writes one of its own.

A head that writes nothing reads as `with _`, so a bare trait is open rather than pure. Publishing an undecided contract is reported: a `pub` trait warns, a file-private or `internal` one remarks, and `#[allow(undecided_effects)]` on the declaration or `#![allow(undecided_effects)]` on the module waives it while the decision is pending.

A body dispatching on a type parameter has no impl to read. In `s.next()`, where `s: S` and `S: Source`, an open head's hole survives, and the enclosing function forwards it with `with _`. Every trait in the standard library says `with ()` instead. An impl of one that performs I/O is a design error, for comparison, conversion and iteration alike.

See [WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md).

### Variadic Type Packs

Use `<..T>` to declare a type pack parameter that represents zero or more types. Type packs enable writing functions that operate on tuples of any arity.

```wado
fn identity<..T>(x: [..T]) -> [..T] {
    return x;
}

fn prepend<A, ..T>(a: A, rest: [..T]) -> [A, ..T] {
    return [a, ..rest];
}
```

Type pack parameters:

- Are declared with `..` prefix in generic parameter lists: `<..T>`, `<A, ..T>`
- May appear more than once per list, each settled on its own (see
  [Multiple Type Packs](#multiple-type-packs))
- Cannot be combined with `effect`: `<effect ..T>` is invalid
- Appear inside tuple types as `[..T]` (type pack spread)
- Can be mixed with fixed type elements: `[A, ..T]`, `[..T, B]`
- Type arguments are inferred from tuple argument types at call sites

#### Multiple Type Packs

A parameter list may declare more than one pack. Each is settled from the
argument that carries it alone, so nothing has to find a boundary that was
never written:

```wado
fn concat<..A, ..B>(a: [..A], b: [..B]) -> [..A, ..B] {
    return [..a, ..b];
}

concat([1, "x"], [true]);   // A = [i32, String], B = [bool]
```

A tuple holding two packs (`[..A, ..B]`) settles neither pack, because matching
it against a concrete tuple admits every split. Such a tuple is legal anywhere.
It just cannot be the only thing naming a pack. Something else must settle each
one: another parameter, a turbofish, or an annotation. The tuple is then checked
against the arity they fix.

```wado
fn wrap<X, ..A, ..B, Y>(x: X, a: [..A], b: [..B], y: Y) -> [X, ..A, ..B, Y] {
    return [x, ..a, ..b, y];   // the parameters settle both packs
}

fn joined<..A, ..B>(split: [[..A], [..B]], both: [..A, ..B]) -> i32 { … }
joined([[1], [true]], [1, true]);         // `split` settles both
joined([[1], [true]], [1, true, "x"]);    // ERROR: expected `[i32, bool]`

fn middle<..Pre, K, ..Post>(t: [..Pre, K, ..Post]) -> i32 { … }
middle::<[i32], String, [bool]>([1, "mid", true]);   // the turbofish settles them
middle([1, "mid", true]);                 // ERROR: cannot infer `Pre`, `K`, `Post`
```

A pack nothing settles is reported at the use site as an uninferred type
parameter, as a scalar parameter no argument reaches is. To settle both packs
from one value, give each a tuple of its own (`[[..A], [..B]]`).

Where such a tuple is produced, its ends still place elements. The elements ahead
of the first pack and behind the last keep their positions, so `[X, ..A, ..B, Y]`
settles `X` and `Y` and nothing else. An element between two packs is settled by
no argument, so name it in a turbofish.

Inside the body that declares them, packs are rigid, as a scalar parameter is. A
value matches such a tuple by layout: the same fixed positions and the same packs
in the same order. Naming the same packs is not enough.

```wado
fn reorder<..A, ..B>(a: [..A], b: [..B]) {
    let ab: [..A, ..B] = [..a, ..b];      // OK
    // let ba: [..B, ..A] = [..a, ..b];   // ERROR: the order is part of the type
    // let shifted: [i32, ..A] = [..a];   // ERROR: so is a fixed element
}
```

A turbofish spells each pack as its own tuple. A flat list carries no boundary
either:

```wado
concat::<[i32], [bool, String]>([1], [true, "x"]);
// concat::<i32, String>(…)  // ERROR: spell each type pack as a tuple
```

Writing one argument per pack is refused as well: `<i32, String>` and
`<[i32, String], []>` split the same list, and nothing says which was meant. The
flat form stays available where a single pack absorbs the surplus on its own
(`make_defaults::<i32, String>()`).

`zip` transposes its operands position by position, so its rows must be equally
long. Two distinct packs are never known to be, so `[[..a], [..b]].zip()` is
rejected where it is written.

#### Lexical note

`..` is one token. Writing `...` (three dots) is a parse error with the diagnostic _"unexpected `...`; did you mean `..`?"_.

#### Value Spread

Value spread `[..expr]` splices a tuple's elements into an enclosing tuple literal:

```wado
let a = [1, "hello"];
let b = [..a, true];   // b: [i32, String, bool]
let c = [42, ..a];     // c: [i32, i32, String]
```

The spread expression is evaluated exactly once:

```wado
// make_pair() is called once, not twice
let t = [..make_pair(), 30];
```

### Handlers

A handler is an `impl Effect for Type` whose methods may use `resume value` to deliver a value to the suspended caller. The `with E => h do { body }` block installs `h` as the handler for effect `E` for the duration of `body`. The `=>` arrow reads as a dispatch binding ("calls to `E` go to `h`"), not an assignment. An inner `with` for the same effect takes over until its body ends, and the outer handler answers again after it.

```wado
with Stdin => &mut mock do { ... }
with Stdin => &mut s, Stdout => &mut o do { ... }
with &mut bundle do { ... }                       // bundled (omits effect name)
```

See [WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md) for resource-as-effect and effect propagation, and [WEP: Effect Handler](./wep-2026-04-11-effect-handler.md) for handler syntax and semantics.

## World System

### What is a World?

A world in Wado corresponds directly to the Component Model's `world` concept. A world defines the contract between a Wasm component and its environment:

1. Imports: Which capabilities the component requires (provided by the host or by other components)
2. Exports: Which functions and types the component provides

Worlds are classified into two categories:

- Hosted world: A world that a runtime knows how to instantiate and drive. The runtime provides all imports and invokes the exports according to a defined lifecycle. Examples: `wasi:cli/command` (executed by `wado run`), `wasi:http/service` (executed by `wado serve`). Informally called a "well-known world."
- Library world: A world that defines a component's public API for composition. It is not directly executed by a runtime; instead, other components import its exports. Example: a `json` library that exports parsing functions.

This distinction is not part of the Component Model specification, which treats all worlds uniformly. In Wado, the distinction matters for tooling: `wado run` and `wado serve` select a hosted world, while `wado.toml`'s `[package].lib` field defines a library world.

### World Declaration

A world imports whole interfaces and exports interfaces or functions:

```wado
#[cm("example:app/plugin@0.1.0")]
pub world Plugin {
    import Stdout;
    import Environment;

    export Run;                                              // an interface
    export fn transform(input: String) -> String;            // a function
    export async fn fetch(url: String) -> Result<String, String>;
}
```

- `import Iface;` and `export Iface;` name a `pub interface`. The interface's own `#[cm(...)]` gives its Component Model name and version.
- `export [async] fn name(...) -> T;` exports a freestanding function. `async` marks an export that maps to a WIT `async func`.
- `#[cm("namespace:package/world@version")]` on the world gives its Component Model name.

A package's `wado.toml` maps each hosted world it targets to an entry file in its `[world]` table, and `[package].lib` names the entry of its library world. See [WEP: Package Manifest](./wep-2026-02-14-package-manifest.md).

### WASI CLI World Example

The standard WASI CLI `command` world, as `wasi:cli` declares it:

```wado
#[cm("wasi:cli/command@0.3.0")]
pub world Command {
    import Environment;
    import Exit;
    import Stdin;
    import Stdout;
    import Stderr;
    import TerminalStdin;
    import TerminalStdout;
    import TerminalStderr;
    import MonotonicClock;
    import SystemClock;
    import Timezone;
    import Preopens;
    import IpNameLookup;
    import Random;
    import Insecure;
    import InsecureSeed;

    export Run;
}
```

`Run` declares `async fn run() -> AsyncCall<Result<(), ()>>`. A program implements it with an `export fn run()`:

```wado
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("Hello, WASI world!");
}
```

### Selecting a World

A program does not name its world in source. The `--world` option of `wado compile` selects it, or the `[world]` table of `wado.toml` maps each world to its entry file. Without either, `wado compile` and `wado run` target `wasi:cli/command`, and `wado serve` targets `wasi:http/service`.

A package may target several worlds, one entry file each:

```toml
[world]
"wasi:cli/command" = "src/cli.wado"
"wasi:http/service" = "src/server.wado"
```

### Design Notes

- Interface imports: a world imports whole interfaces, as a WIT world does.
- Versions: the `#[cm(...)]` of each interface and of the world carries its version (`@0.3.0`).
- Exports: an interface export takes its signatures from the interface. A function export spells its own.

## Error Handling

### Unrecoverable Errors (Traps)

```wado
panic("Fatal error");      // logs the message to stderr, then traps
unreachable();             // traps
assert condition;          // panics when the condition is false
```

A trap cannot be caught in Wado. It ends the program.

### Recoverable Errors (Result Type)

```wado
fn parse_int(s: String) -> Result<i32, ParseError> {
    // ...
}

fn read_config(path: String) -> Result<Config, ConfigError> with Preopens {
    let content = fs::read_to_string(path)
        .map_err(|e| ConfigError::Io(e))?;
    let config = parse_config(content)?;
    return Ok(config);
}

// Handle with pattern matching
match result {
    Ok(value) => process(value),
    Err(e) => handle_error(e),
}
```

## WASI / Browser Support

Wado targets WASI Preview 3 (0.3.0), which introduces native `stream<T>` and `future<T>` types that map directly to Wado's `Stream<T>` and `Future<T>`.

All Wado types map directly to Component Model (WIT) types. See the [Type Mapping at Component Boundaries](#type-mapping-at-component-boundaries) table in the Type System section for the complete mapping reference.

### WASI P3 CLI Interfaces

Wado effects map to WASI P3 interfaces:

| Wado Effect   | WASI Interface         | Key Functions                                                           |
| ------------- | ---------------------- | ----------------------------------------------------------------------- |
| `Stdout`      | `wasi:cli/stdout`      | `write-via-stream(stream<u8>) -> future<result<_, error-code>>`         |
| `Stderr`      | `wasi:cli/stderr`      | `write-via-stream(stream<u8>) -> future<result<_, error-code>>`         |
| `Stdin`       | `wasi:cli/stdin`       | `read-via-stream() -> tuple<stream<u8>, future<result<_, error-code>>>` |
| `Environment` | `wasi:cli/environment` | `get-arguments()`, `get-environment()`                                  |
| `Exit`        | `wasi:cli/exit`        | `exit(result)`, `exit-with-code(u8)`                                    |

### Entry Points

Each hosted world defines its entry point:

| World                 | Entry Point                                                               | Driver       |
| --------------------- | ------------------------------------------------------------------------- | ------------ |
| `wasi:cli/command`    | `export fn run()`                                                         | `wado run`   |
| `wasi:http/service`   | `export async fn handle(request: Request) -> Result<Response, ErrorCode>` | `wado serve` |
| `core:kiln/generator` | `export fn generate(...)`                                                 | Kiln         |
| `test`                | the entry module's `test` blocks                                          | `wado test`  |

`test` is a synthetic world: it exports the entry module's `test` blocks and nothing else. See [Selecting a World](#selecting-a-world) for how a program's world is chosen.

### `task return` Statement

`task return expr;` is a statement valid only inside `export async fn` bodies. It calls the Component Model `task.return` instruction, delivering the function's result to the CM runtime without terminating the Wasm function. Execution continues after `task return`, allowing the function to fulfill outstanding futures (e.g. trailers) or perform cleanup.

#### Motivation

HTTP handlers return a `Response` that contains a `Future`-based trailers channel. With a regular `return`, the Wasm function exits immediately, making it impossible to write to that channel. `task return` separates result delivery from function termination:

```wado
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let [trailers_future, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
    let headers = Headers::new();
    let [response, _tx_future] = Response::new(headers, null, trailers_future);

    task return Result::<Response, ErrorCode>::Ok(response); // deliver result; function continues
    trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null)); // fulfill trailers
}
```

#### Rules

- `task return` is only valid inside `export async fn` bodies.
- An `export async fn` body must carry a `task return`. One that carries none can never deliver, so every call of it would reach the boundary with the task unfinished; the compiler rejects it instead. A body whose every path provably exits first (`panic`, an endless loop) has no delivery to make and is exempt.
- Whether a `task return` under a branch is reached is not checked. A path that misses it traps at the boundary, the same as a declared result the body never binds.
- Regular `return` is forbidden in `async fn` bodies. It would exit the Wasm function without notifying the CM runtime.
- The `task return` expression is type-checked against the declared return type of the enclosing `export async fn`.
- `task return` names the function's result, and where it goes depends on who entered the function. The Component Model runtime receives it when the export binding did; a Wado caller receives it as an ordinary return value.
- The `async` of an `export async fn` asks nothing of a Wado call site, which calls it as any other function. It selects the CM async calling convention at the component boundary.

### Attribute Syntax for Component Model Linking

Use `#[cm(...)]` attributes to link Wado definitions to Component Model interfaces:

```wado
// Link an effect interface to a CM interface
#[cm("wasi:cli/stdout@0.3.0")]
pub interface Stdout {
    #[cm("wasi:cli/stdout@0.3.0#write-via-stream")]
    fn write_via_stream(data: Stream<u8>) -> Future<Result<(), ErrorCode>>;
}

// Link a resource to a CM resource
#[cm("wasi:cli/terminal-output@0.3.0#terminal-output")]
pub resource TerminalOutput;

// Link an enum to a CM enum, and each case to its WIT case
#[cm("wasi:cli/types@0.3.0#error-code")]
pub enum ErrorCode {
    #[cm("io")]
    Io,
    #[cm("illegal-byte-sequence")]
    IllegalByteSequence,
    #[cm("pipe")]
    Pipe,
}
```

`#[cm_params("name", ...)]` on an operation gives the CM-side names of its parameters. Without it, each parameter's CM name is its Wado name in kebab-case.

#### Resource linearity

A `#[cm(...)]` resource may declare what may be done with its handle: `linearity = "affine"` or `linearity = "unrestricted"`. Omitting the field reads as `"affine"`.

An affine resource is move-only and carries a drop obligation, per [Resource Ownership](./wep-2026-05-21-resource-ownership.md). An unrestricted one owns nothing, so it is an ordinary copyable value. Assigning or passing one leaves the original usable, and nothing is dropped at the end of a scope.

The representation follows from the linearity. An affine resource crosses the Component Model boundary as an `own` / `borrow` handle, an unrestricted one as a plain `f64` the host interprets. `as` converts an unrestricted handle to or from `f64`, keeping every bit, or upcasts it to a resource it extends. No other cast accepts one.

### Resource Inheritance

`resource Child extends Parent` declares that a child handle is usable wherever the parent is. Both resources must declare `linearity = "unrestricted"`, because an upcast copies the handle and an affine one may not be copied. Single inheritance only, and a cycle is an error.

```wado
#[cm("example:ui/target", linearity = "unrestricted", classes = "0..=1")]
resource Target {
    #[cm("example:ui/target#add-listener")]
    fn add_listener(&self, kind: String);
}

#[cm("example:ui/widget", linearity = "unrestricted", classes = "1..=1")]
resource Widget extends Target {
    #[cm("example:ui/widget#label")]
    fn label(&self) -> Option<String>;
}

fn use_it(w: Widget) {
    w.add_listener("click");   // inherited, no cast
    let t: Target = w;         // upcast is implicit
}
```

Rules:

- The upcast is implicit wherever a value, a `return`, or a `&T` referent is expected, and where branches of an `if` or `match` meet. `&mut T`, container elements (`List<T>`, `Option<T>`, …) and function types are invariant.
- Narrowing back to a child is never implicit. It is written as a type pattern (below), which tests the class the host tagged the handle with.
- `classes = "lo..=hi"` numbers those classes: a resource's own is `lo`, and the resources extending it hold the rest. A child's range lies inside its parent's, above the parent's own class. Sibling ranges do not overlap, and an `extends` tree declares `classes` on every resource or on none. A type pattern narrows only to a resource that declares them.
- `==` and `!=` compare two handles when one type extends the other. The host hands out one handle per object, so equal handles name one object. Handles compare by bits, so a NaN handle equals itself and `-0.0` differs from `0.0`. An unrestricted resource is `Eq`, so a type holding one derives `Eq` too. There is no ordering.
- A child may not redeclare a method it inherits. A name reachable through both the chain and a trait impl is ambiguous: write `Declaring::method(&value)` or `Trait::method(&value)` to pick one.
- Static methods (no `&self`) are not inherited, and `Self` in an inherited method names the resource that declares it.
- A generic resource takes no part in `extends`.

See [Resource Inheritance and Narrowing](./wep-2026-04-28-resource-inheritance.md) for the design and its known gaps.

### Type Patterns

A pattern may ascribe a type: `p: T` matches when the subject is a `T`, and `p` binds it. The ascription on a `let` is this pattern, so one rule covers both spellings.

Whether the pattern can fail is decided statically, from the subject's type `S`:

| Relation          | Meaning                                                              |
| ----------------- | -------------------------------------------------------------------- |
| `S <: T`          | irrefutable — an upcast, or an ordinary type annotation              |
| `T <: S`, `T ≠ S` | refutable — a runtime test, and only where `extends` relates the two |
| otherwise         | a type error, as a mismatched annotation is                          |

An irrefutable ascription still drives type context, so `let x: i64 = 42` coerces the literal. A refutable one needs a pattern position that admits failure, so `let` and a `for` binding reject it exactly as they reject `Some(x)`:

```wado
let n: Node = el;                                   // Element <: Node — irrefutable upcast
let input: HtmlInputElement = el;                   // ERROR: refutable pattern in `let`
let input: HtmlInputElement = el else { return; };  // the guard form
if let input: HtmlInputElement = el { ... }
if node matches { _: Element } { ... }              // the predicate form

match node {
    input: HtmlInputElement => input.value(),
    elem: Element => elem.tag_name(),
    _ => "other",                                   // required: the hierarchy is open
}
```

A type match over resources always needs a final `_` arm, because the host may hand back a type the program does not name. An arm whose type is a supertype of a later arm's makes that later arm dead, which is reported.

A refutable ascription tests a handle, so it binds a name or `_` and nothing deeper, and its subject is the value rather than a reference to it. `T` must be a concrete type: a type parameter says nothing about whether it narrows.

This is not [`match type`](./wep-2026-09-05-total-reflection.md), which narrows a type parameter at compile time, is exhaustive, and takes no `_`.

## Compiler Attributes

Wado uses `#[...]` attributes (item-level) and `#![...]` inner attributes (module-level) to control compiler behavior.

### User-Facing Attributes

These attributes are part of the language surface and can be used in any Wado source file.

#### `#[inline]` / `#[inline(always)]` / `#[inline(never)]`

Inlining hints for the optimizer. Applies to functions.

```wado
#[inline]              // hint: prefer inlining
fn small_helper() -> i32 { return 42; }

#[inline(always)]      // always inline (ignores threshold)
fn critical_path() -> i32 { return 1; }

#[inline(never)]       // never inline
fn error_handler() { panic("error"); }
```

#### `#[benign(E, ...)]`

Lets a function perform the listed effects without declaring `with E`, and stops them from propagating to callers. It is meant for effects that are observationally pure, that is, unobservable through the function's interface. Only the named effects are suppressed. Others propagate normally, and the world import for each is still required. The compiler cannot verify observational purity, so this is an unchecked assertion that must be audited. See [WEP: Effect System and Randomness in Collections](./wep-2026-01-20-effect-system-randomness.md).

```wado
#[benign(InsecureSeed)]
fn hash_seed() -> u64 {
    let [seed, _] = InsecureSeed::get_insecure_seed(); // not required of callers
    return seed;
}
```

#### `#[secret]`

Hides a struct field from debug/inspect output (the `:?` format specifier).

```wado
struct Foo {
    pub name: String,
    #[secret]
    password: String, // excluded from `${foo:?}` output
}
```

#### `#[allow(...)]`

Waives a lint on the item carrying it. As the module inner attribute
`#![allow(...)]` it waives the lint for every item in the file. There is no
`#[deny(...)]`. The lints are:

- `dead_code`: an unused or test-only item. See [WEP: Unused Diagnostics](./wep-2026-05-16-unused-diagnostics.md).
- `shadowed_name`: a binder that takes a name already reaching a known symbol.
- `undecided_effects`: a trait head that writes no `with` clause (see [The Trait Head](#the-trait-head)).

```wado
#[allow(dead_code)]
fn scaffolding() -> i32 {  // no "function `scaffolding` is never used"
    return 0;
}
```

#### `#[param]` / `#[param(from_env = "...")]` / `#[param(name = "...")]`

Marks a `global` as a compile-time build input. The type annotation gives the type, the initializer is the fallback, and read sites are ordinary global references. Each parameter resolves highest-priority-first: `-D NAME=value` (alias `--define`) on the `wado` invocation, then `from_env`, then the initializer. Overrides are parsed into the declared scalar type with the `LenientFromStr` spellings. See [WEP: Compile-Time Parameters](./wep-2026-04-26-compile-time-params.md).

```wado
#[param]
global API_URL: String = "http://localhost";   // -D API_URL=...

#[param(from_env = "PORT")]
global PORT: i32 = 8080;                        // read from an env var

#[param(name = "build.id")]
global BUILD_ID: String = "dev";                // -D build.id=...
```

#### `#[unavailable("reason")]`

Declares a name that is deliberately not offered, on a declaration with no body. A call to it is an error that reports the reason. The reason is the only argument, and a removal writes its version into it. The declaration reserves a name rather than a signature, so its parameters are never checked against a call. It goes on a module function, an `impl` method, or a trait method. See [WEP: Declared Absence](./wep-2026-09-13-declared-absence.md).

```wado
impl File {
    #[unavailable("write `open_with(Options::default())` instead")]
    pub fn open(&self);

    #[unavailable("removed in 0.5.0; use `open_with`")]
    pub fn open_timeout(&self);
}
```

#### `#[expect_trap]`

Test block attribute. Marks a test that is expected to trap. The test passes if the body traps, and fails if it completes normally.

```wado
#[expect_trap]
test "panics on invalid input" {
    panic("bad input");
}
```

#### `#[TODO]`

Test block attribute. Marks a test for an unimplemented feature. TODO tests are reported on a separate axis from regular pass/fail results:

- If the body traps, the test is pending. That is expected while the feature is unimplemented.
- If the body completes normally, the test is resolved. That is a hard failure: the `#[TODO]` attribute must be removed.

A pending TODO test never fails the run, and a resolved one always does. See [Test Outcome Model](#test-outcome-model).

```wado
#[TODO]
test "not yet implemented" {
    panic("TODO: implement this");
}
```

#### `#[timeout_ms(N)]`

Test block attribute. Overrides the default test timeout (5000ms). `N` is an integer literal specifying the timeout in milliseconds.

```wado
#[timeout_ms(30000)]
test "slow computation" {
    let result = expensive_computation();
    assert result == 42;
}
```

#### `#[synopsis]`

Test block attribute. The test runs like any other, and `wado doc` renders its body as the module's `## Synopsis` section, a usage example that is compiled and so stays current. See [WEP: Synopsis Tests](./wep-2026-04-26-synopsis-tests.md).

```wado
#[synopsis]
test {
    let p = Point { x: 3, y: 4 };
    assert p.length() == 5.0;
}
```

#### `#[wire(name = "...")]` / `#[wire(name_policy = "...")]` / `#[wire(number = N)]` / `#[wire(positional)]`

Controls serialization and deserialization of struct fields. See [WEP: Serialization and Deserialization](./wep-2026-02-28-serde.md) and [`core:serde`](./stdlib-core-serde.md).

- `#[wire(name = "...")]` overrides the wire key of one field.
- `#[wire(name_policy = "...")]` on a struct renames every field by a convention (`"camelCase"`, `"snake_case"`, `"kebab-case"`, ...).
- `#[wire(number = N)]` gives a field the numeric key that number-keyed formats such as `core:protobuf` read. A struct carries it on every field or on none, and a numbered struct satisfies `WireNumbered`. See [WEP: Grog](./wep-2026-09-22-grog.md).
- `#[wire(positional)]` marks a field as ordinal: it is resolved by position, never by name. Name-only and sequence-only formats ignore the hint. [`core:args`](./wep-2026-06-22-core-args.md) uses it to bind a bare token to the field.

A field is optional on deserialization when it has a default value (`f: T = expr`), and it falls back to that expression when absent. This is the only mechanism for optional fields.

### Standard Library Attributes

These attributes are used in the standard library (`lib/`) to wire Wado code to Wasm and the Component Model. They are not intended for user code.

#### `#![no_prelude]`

Module-level inner attribute. Prevents the automatic import of `core:prelude`. Used by low-level modules that define the prelude itself or that operate below the prelude layer.

```wado
#![no_prelude]
// This module does not import core:prelude
```

#### `#![TODO]`

Module-level inner attribute. Marks the entire module as TODO for `wado test`. The source must parse successfully (otherwise the attribute cannot be recognized), but compilation errors are tolerated:

- If compilation fails, the module is reported as a single pending TODO entry.
- If compilation succeeds, all test blocks are implicitly treated as `#[TODO]` tests.
- If the module compiles and all tests pass, it is reported as resolved. That is a hard failure: the `#![TODO]` attribute must be removed.

See [Test Outcome Model](#test-outcome-model).

```wado
#![TODO]

test "not yet implemented" {
    panic("TODO");
}
```

#### `#![generated]`

Module-level inner attribute. Indicates that the module contains machine-generated code (e.g. from `wado-from-idl` or `gale`). It does not change how the module compiles. Tools read it: Kiln stamps it on every file it generates, and deletes a stamped file that the current run did not produce.

The attribute accepts optional metadata so that generators can attach provenance information directly to the attribute instead of as free-form comments. Two argument shapes are supported inside the parentheses:

- Scalar `key = "value"` pairs (e.g. `by = "wado-from-idl"`).
- List `key = ["v1", "v2", ...]` pairs whose values are a comma-separated list of string literals (e.g. `sources = ["a.wit", "b.wit"]`).

Conventional keys are `by` (the tool that produced the file) and `sources` (the list of source paths it was generated from). Unknown keys are tolerated, so generators can introduce additional metadata without requiring a spec change.

```wado
#![generated]

#![generated(by = "wado-from-idl", sources = ["deps/random.wit"])]

#![generated(by = "wado-from-idl", sources = ["cli.wit", "clocks.wit"])]
```

#### `#![wasm_module("name")]`

Module-level inner attribute. All items in this module are compiled into a separate Wasm core module with the given name, which owns its own linear memory.

The Component Model requires a component to provide a linear memory and a `realloc` function for data crossing the boundary. `core:allocator` provides both as the core module `"mem"`, the only `wasm_module` in the standard library.

```wado
#![wasm_module("mem")]
#![no_prelude]

global mut heap_offset: i32 = 8;

#[allocator("bump")]
export fn bump_realloc(oldptr: i32, oldsize: i32, align: i32, newsize: i32) -> i32 {
    // ...
}
```

#### `#[allocator("name")]`

Marks a function in a `wasm_module` as the `realloc` implementation named `name`. The world selects which one the component uses: `bump` for CLI programs, `freelist` for HTTP services, and `debug` for the test world. `debug` never reuses freed memory and fills it with `0xFF`.

#### `#[export_name("name")]`

Overrides the Wasm export name of a function within its core module.

#### `#[canonical("namespace", "name")]`

Declares that a bodyless function is imported rather than defined. Used in `core:builtin` to map intrinsic declarations to their imports.

| Namespace       | Description                                                    |
| --------------- | -------------------------------------------------------------- |
| `"wasi"`        | CM canonical builtins (streams, futures, tasks)                |
| `"mem"`         | Exports of the `"mem"` core module (`realloc`)                 |
| `"wasm:<path>"` | Exports of an imported core-wasm asset (e.g. the bundled libm) |

```wado
#[canonical("wasi", "stream-new")]
fn stream_new() -> i64;

#[canonical("mem", "realloc")]
fn realloc(oldptr: i32, oldsize: i32, align: i32, newsize: i32) -> i32;
```

#### `#[compiler_item("name")]`

Binds a stdlib declaration to the language item of that name, such as `#[compiler_item("option")]` on `variant Option` or `#[compiler_item("display")]` on the `Display` trait. It is valid only in `core:*` modules, and an error elsewhere.

#### `#![stdlib("path")]`

Module-level inner attribute. Names the bundled stdlib module a file is, such as `#![stdlib("core:cbor")]`. The file is that module however it was loaded, so a file an editor opens directly is the same module an import reaches.

#### `#[cm("namespace:pkg/interface@version")]` / `#[cm_params(...)]`

Links Wado definitions (interfaces, worlds, resources, enums) to their Component Model names. See [Attribute Syntax for Component Model Linking](#attribute-syntax-for-component-model-linking).

#### `#[retain(...)]` / `#[result(...)]`

What a call does with the reference parameters it is handed: whether its result
aliases one, and whether it keeps one past the return. Neither is a safety
condition, since every referent is GC-managed and cannot dangle. The compiler
reads both from a function's body. These attributes are for a declaration that
has none: a `core:builtin` primitive, a Component Model import, a `.wasm` /
`.wat` asset import. See
[WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md).

```wado
#[result(part_of = arr)]
pub fn array_get_ref<T>(arr: &Array<T>, idx: i32) -> &T;

#[retain(value, into = arr)]
pub fn array_set<T>(arr: &mut Array<T>, idx: i32, value: T);

#[retain(elements_of = src, into = dst)]
pub fn array_copy<T>(dst: &mut Array<T>, dst_offset: i32, src: &Array<T>, src_offset: i32, len: i32);
```

`#[result(owned)]` says the result is freshly allocated; `#[result(part_of = p)]`
says it is part of `p`.

`#[retain(...)]` names one retained thing and repeats where there is more than
one, so each carries its own destination. A bare name is the parameter itself
and `elements_of = p` is that parameter's elements; `into = q` names the
parameter it lands in, and without it the destination is unknown. Silence is the
conservative reading.

Both are an error on a function with a body, which states these facts itself,
and on a `trait` or `interface` method requirement: a call to one is statically
dispatched to an impl that has a body, so the impl states it.

#### `#[immediate(...)]`

Names a parameter that becomes a Wasm immediate: the argument's literal value is
encoded into the instruction itself.

```wado
#[immediate(value)]
pub fn v128_const(value: i128) -> v128;
```

It names one parameter, unquoted, and repeats for a second. Like
`#[retain(...)]`, it belongs to a declaration with no body, because a body is
called rather than encoded as one instruction. A `trait` or `interface` method
requirement is an error for the same reason: it reaches an impl, which is
called.

#### `#[trap(...)]`

When a call to a declaration with no body traps. Silence means it may trap.
`#[trap(never)]` says it never traps, and a check names the one condition it
traps on:

```wado
#[trap(never)]
pub fn f64_sqrt(x: f64) -> f64;

#[trap(outside = arr, at = idx)]
#[trap(unset = arr)]
pub fn array_get_value<T>(arr: &Array<T>, idx: i32) -> T;

#[trap(outside = dst, at = dst_offset, len = len)]
#[trap(outside = src, at = src_offset, len = len)]
pub fn array_copy<T>(dst: &mut Array<T>, dst_offset: i32, src: &Array<T>, src_offset: i32, len: i32);

#[result(owned)]
#[trap(negative = len, result_len = len)]
pub fn array_new<T>(len: i32) -> Array<T>;
```

`negative = p` traps when `p` is below zero. `outside = a` traps unless the
range from `at` (0 when absent) of `len` elements (1 when absent) lies within
the array `a`, and says the call does not replace `a`. `unset = a` traps when
the element read holds no value: `array_new` leaves a reference element empty,
while a primitive element always holds one. Each attribute states one
check and repeats for another; the call traps where any fails. `result_len = p`
is no check: it says the returned array holds `p` elements, so a later check
against it can be proved. Running out of memory is not a trap any of these
describe.

It is an error on a function with a body, which states when it traps itself,
and on a `trait` or `interface` method requirement, for the reason `#[retain]`
is.

#### `#[linear_memory(...)]`

How a call to a declaration with no body touches linear memory: `read` or
`write`. Silence means it touches none. A linear-memory address is a plain
`i32`, so no parameter type says this, and the attribute is the only source.

```wado
#[linear_memory(read)]
pub fn i32_load(addr: i32) -> i32;

#[linear_memory(write)]
pub fn i32_store(addr: i32, value: i32);
```

It is written once, and is an error where `#[trap]` is.

## Appendix

### Naming Conventions

| Element            | Style            |
| ------------------ | ---------------- |
| Package name       | `kebab-case`     |
| Module/file name   | `snake_case`     |
| Primitive types    | `lowercase`      |
| User-defined types | `UpperCamelCase` |
| Enum/variant cases | `UpperCamelCase` |
| Functions          | `snake_case`     |
| Local variables    | `snake_case`     |

Component Model interop: The compiler automatically converts between Wado conventions and WIT conventions (kebab-case) at component boundaries.

### Terminology

- Wasm: WebAssembly (not WASM)
- WASI: WebAssembly System Interface
- CM: Wasm Component Model
- module: a Wado file
- package: a collection of modules, described by one `wado.toml`
- Wado standard library: consists of `core:` and `wasi:`
- effect: the concept; e.g., "the `Stdout` effect"
- effect interface: the declaration (`interface Stdout { ... }`); synonyms in literature: "effect signature", "effect type"
- operation: a function in an effect interface; synonym: "effect operation"
- handler: provides implementations for operations
- hosted world: a world that a runtime knows how to instantiate and drive (e.g., `wasi:cli/command` for `wado run`, `wasi:http/service` for `wado serve`); informally called "well-known world"
- library world: a world that defines a component's public API for composition with other components, rather than for direct execution by a runtime
