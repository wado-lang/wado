# Wado Language Specification

Wado is a programming language targeting Wasm/WASI -- Wasm in plain sight.

## Overview

| Item      | Description                         |
| --------- | ----------------------------------- |
| Name      | Wado                                |
| Extension | `.wado`                             |
| Paradigm  | Imperative, Reactive, Effect System |
| Typing    | Static, Strong, Inferred            |
| Target    | Wasm/WASI                           |

See also: [Cheatsheet](docs/cheatsheet.md) for quick syntax reference.

## Design Philosophy

- **Wasm only**: Zero abstraction to Wasm
- **Explicitness**: Make intent explicit
- **Colorless async**: Eliminates async/await "color" problem via Wasm Stack Switching
- **Effect System**: Side effect tracking and control, swappable via Handlers

## Lexical Structure

### Whitespace

The lexer recognizes exactly four whitespace characters:

| Code Point | Name  |
| ---------- | ----- |
| `\u0020`   | Space |
| `\u000A`   | LF    |
| `\u000D`   | CR    |
| `\u0009`   | Tab   |

The lexer skips whitespace between tokens. Other Unicode whitespace characters (e.g., `\u00A0` non-breaking space) are not recognized as whitespace and will cause a lexer error if used outside strings.

### Comments

```wado
// Line comment (extends to end of line)

/* Block comment */

/*
 * Multi-line
 * block comment
 */
```

Block comments do not nest.

TODO: the parser keeps comments in the AST.

### Shebang

```wado
#!/usr/bin/env wado
export fn run() { ... }
```

`#!` at position 0 is a shebang and is ignored. `#![` is an inner attribute, not a shebang.

### Data Section

The `__DATA__` marker separates source code from embedded data. Everything after `__DATA__` on its own line is captured as raw text and is not parsed as Wado code.

```wado
use {println} from "core:cli";

fn run() with Stdout {
    println("Hello!");
}

__DATA__
This is raw data that can be accessed via the compiler API.
It can contain any text, including JSON, YAML, or test expectations.
```

**Syntax Rules:**

- `__DATA__` must appear at the start of a line (after any preceding newline)
- The line must contain only `__DATA__` followed by a newline (no trailing content on the same line)
- Everything after the `__DATA__` line becomes the data section
- The data section is optional; most modules won't have one

**Accessing Data:**

The data section is accessible via the compiler API through `Module::data_section()`, which returns `Option<&str>`. This enables tooling like test frameworks to embed expected results directly in source files.

```rust
// Compiler API example
let result = wado_compiler::compile_file(path)?;
if let Some(data) = result.module.data_section() {
    // Process the data section content
}
```

The data section content can be accessed at runtime using the `#data` compile-time location literal. See [Compile-Time Location Literals](#compile-time-location-literals).

### Identifiers

Identifiers match the pattern `[a-zA-Z_][a-zA-Z0-9_]*`:

```wado
foo
foo_bar
fooBar
FooBar
FOO_BAR
_private
name123
```

Identifiers are case-sensitive.

### Statements and Expressions

- `expr;` makes a statement. A semicolon is required for every statement including the last one.
- `return expr;` is necessary for a function to return a value.
- Control flow statements do not need to be followed by a semicolon.

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
    println(`{x}`); // 2
}
println(`{x}`);     // 1 (outer x unchanged)
```

Same-scope redeclaration is not allowed (unlike Rust):

```wado
let x = 1;
let x = 2;  // Error: cannot redeclare 'x' in the same scope
```

### Global Variables

Global variables are module-level state that compile directly to WebAssembly globals. Unlike local variables (`let`), globals have module lifetime and are accessed via `global.get`/`global.set` instructions.

```wado
// Immutable global
global PI: f64 = 3.14159;

// Mutable global
global mut counter: i32 = 0;

// With visibility
pub global VERSION: i32 = 1;
```

Any type is supported. Any pure expression (no effects) can be used as an initializer.

**Mutability:**

Assignment is only allowed for `global mut` declarations:

```wado
global CONSTANT: i32 = 42;
global mut variable: i32 = 0;

fn example() {
    variable = 10;    // OK: mutable global
    CONSTANT = 10;    // Error: cannot assign to immutable global
}
```

### Operators

**Binary Operators** (in order of precedence, lowest to highest):

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

TODO: Add range operators once the range syntax is fully designed.

**Design Note**: Bitwise operators (`&`, `|`, `^`) have **higher** precedence than comparison operators, fixing C's well-known design flaw. This means `flags & MASK == EXPECTED` correctly parses as `(flags & MASK) == EXPECTED`.

**Unary Operators** (highest precedence):

| Operator | Description |
| -------- | ----------- |
| `-`      | Negation    |
| `!`      | Logical NOT |
| `~`      | Bitwise NOT |
| `&`      | Reference   |
| `&mut`   | Mut ref     |
| `*`      | Dereference |

**Postfix Operators** (highest precedence):

| Operator  | Description       |
| --------- | ----------------- |
| `.`       | Field access      |
| `[]`      | Index access      |
| `()`      | Function call     |
| `::`      | Namespace access  |
| `as Type` | Type cast         |
| `?`       | Error propagation |

**Prohibited Operators**:

Wado intentionally omits certain operators found in other languages:

- **No `++`/`--`**: Use `x += 1` and `x -= 1` instead. These operators cause undefined behavior in C/C++ and add unnecessary complexity.
- **No `**`power operator**: Use`pow(x, y)`function instead. The`**`operator has counterintuitive precedence in languages that have it (e.g., Python's`-1**2 = -1`).

**Type Cast (`as`):**

The `as` operator performs explicit type conversion between primitive types:

```wado
let i = 42;
let f = i as f64;           // i32 to f64
let truncated = 3.14 as i32; // f64 to i32 (truncates to 3)

// Chained casts
let x = 10 as f64 as i32 as f64;

// Cast in expressions
let result = (a as f64) + b;
```

**Parentheses for Grouping:**

Parentheses `()` can be used to override operator precedence:

```wado
let a = 2 + 3 * 4;      // 14 (multiplication first)
let b = (2 + 3) * 4;    // 20 (addition first due to parentheses)

let c = 3 | 4 & 6;      // 7 (& has higher precedence than |)
let d = (3 | 4) & 6;    // 6 (| first due to parentheses)
```

**Comparison Chaining:**

Wado supports mathematical comparison chaining similar to Python, allowing natural range expressions:

```wado
// Valid chains (same direction)
a < b < c       // Equivalent to: a < b && b < c
a > b > c       // Equivalent to: a > b && b > c
a <= b <= c     // Equivalent to: a <= b && b <= c
a >= b >= c     // Equivalent to: a >= b && b >= c
a == b == c     // Equivalent to: a == b && b == c
0 <= x <= 100   // Natural range check
```

```wado
// Invalid chains (semantic error)
a < b > c       // Error: mixed directions
a > b < c       // Error: mixed directions
a < b >= c      // Error: mixing < and >=
a == b < c      // Error: mixing == and inequality
a != b != c     // Error: != chaining not allowed
```

**Chaining Rules:**

1. **Same-direction inequality**: `<`/`<=` can only chain with `<`/`<=`, and `>`/`>=` can only chain with `>`/`>=`
2. **Equality chaining**: `==` can only chain with `==`
3. **No `!=` chaining**: `!=` cannot be chained (the meaning of `a != b != c` is ambiguous)
4. **No mixing**: Cannot mix equality operators with inequality operators

See `docs/wep-2026-01-11-operator-precedence.md` for detailed rationale.

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

**If Expression:**

```wado
let abs = if x < 0 { -x } else { x };

let grade = if score >= 90 { "A" } else if score >= 80 { "B" } else { "C" };
```

Trailing semicolons are optional in expression blocks (like trailing commas).

**If with Init (Go-style):**

```wado
if let x = get_value(); x > 0 {
    println(`positive: {x}`);
} else {
    println(`non-positive: {x}`);
}
// x is not in scope here
```

**If Let Pattern Matching:**

```wado
let opt: Option<i32> = Option::<i32>::Some(42);
if let Some(x) = opt {
    println(`Got: {x}`);
} else {
    println("None");
}
```

**Match Ergonomics:** When the scrutinee of `if let`, `match`, or `matches` is a reference type (`&T` or `&mut T`), patterns match against the underlying type. Payload bindings become references — e.g. matching `&Option<T>` with `Some(x)` gives `x: &T`, not `x: T` (Rust-compatible, RFC 2005).

```wado
let opt: Option<i32> = Option::<i32>::Some(42);
let ro = &opt;
if let Some(x) = ro {       // ro: &Option<i32>, x: &i32
    println(`Got: {*x}`);   // dereference to use the value
}
```

### While Loop

```wado
let mut i = 0;
while i < 10 {
    println(`i = {i}`);
    i = i + 1;
}
```

#### While Let Pattern Matching

`while let` allows iterating while a pattern matches:

```wado
let items: Array<i32> = [1, 2, 3];
let mut iter = items.iter();

while let Some(x) = iter.next() {
    println(`{x}`);
}
```

The loop continues as long as the pattern matches. When the pattern fails to match (e.g., `iter.next()` returns `None`), the loop exits.

### For Loop

C-style for loop with initialization, condition, and update. Parentheses are optional:

```wado
for let mut i = 0; i < 10; i = i + 1 {
    println(`i = {i}`);
}

// With parentheses (also valid)
for (let mut i = 0; i < 10; i = i + 1) {
    println(`i = {i}`);
}

// All parts are optional
for ;; {
    // infinite loop
}
```

**Note:** `continue` in a for loop executes the update expression before the next iteration, matching C semantics.

#### For with Pattern Condition

The condition part of a C-style for loop can use `let` pattern matching:

```wado
let items: Array<i32> = [10, 20, 30];
let mut iter = items.iter();

for ; let Some(x) = iter.next(); {
    println(`{x}`);
}

// With update expression
let mut count = 0;
for ; let Some(x) = iter.next(); count += 1 {
    println(`item {count}: {x}`);
}
```

The loop continues as long as the pattern matches. This is useful for iterating with additional state (like a counter) alongside pattern matching.

### For-Of Loop

For iterating over any type that implements `IntoIterator`:

```wado
let numbers: Array<i32> = [1, 2, 3, 4, 5];
for let n of numbers {
    println(`{n}`);
}

// With mutable binding
for let mut item of items {
    item = item * 2;  // Can modify the local binding
    println(`{item}`);
}

// Custom types that implement IntoIterator also work
for let x of my_collection {
    println(`{x}`);
}
```

**For-of desugaring:**

```wado
// Source
for let item of collection {
    body(item);
}

// Desugars to
scope: {
    let mut __iter = collection.into_iter();
    loop {
        if let Some(__item) = __iter.next() {
            let item = __item;
            body(item);
        } else {
            break;
        }
    }
}
```

**Note:** The binding is a copy of each element (value semantics), so modifying it does not affect the original collection. For-of works with any type implementing `IntoIterator`, not just arrays.

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
    println(`{i}`);
}
```

Both `break` and `continue` work with `while`, `for`, and `loop`.

### Labeled Blocks

Labeled blocks create a new scope for variable bindings. The label is required to avoid syntactic ambiguity with struct literals.

```wado
let x = 10;

scope: {
    let x = 20;  // shadows outer x
    println(`x = {x}`);  // prints "x = 20"
}

println(`x = {x}`);  // prints "x = 10" (outer x unchanged)
```

**Syntax**: `LABEL: { ... }`

- The label must be a valid identifier followed by a colon
- The block creates a new variable scope
- Variables declared inside are not accessible outside
- Shadowing is allowed within the block

**Nested Blocks**:

```wado
outer: {
    let a = 1;
    inner: {
        let b = 2;
        let sum = a + b;  // a is visible from outer scope
        println(`{sum}`);
    }
    // b is not visible here
    println(`{a}`);
}
```

**Design Rationale**: The label is mandatory because `{ field: value }` without context could be either a block with a labeled statement or a struct literal. Requiring the label removes this ambiguity.

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

**Pattern Syntax:**

| Pattern  | Example                      | Description            |
| -------- | ---------------------------- | ---------------------- |
| Wildcard | `_`                          | Matches anything       |
| Variable | `x`                          | Binds matched value    |
| Literal  | `0`, `"hello"`, `true`       | Matches exact value    |
| Variant  | `Some(x)`, `None`            | Matches variant case   |
| Tuple    | `[a, b, c]`                  | Destructures tuple     |
| Struct   | `{ x, y }`, `Point { x, y }` | Destructures struct    |
| Or       | `Red \| Blue`                | Matches either pattern |
| Guard    | `Some(x) && x > 0`           | Pattern with condition |

**Exhaustiveness:**

Match must cover all possible cases. Use `_` wildcard for catch-all:

```wado
match color {
    Red => "red",
    Green => "green",
    _ => "other",  // Required for exhaustiveness
}
```

**Guard Expressions:**

Guards use `&&` to reflect left-to-right evaluation (pattern first, then guard):

```wado
match customer {
    Premium(years) && years > 5 => 0.3,
    Premium(_) => 0.2,
    _ => 0.1,
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

**Scope:** Pattern bindings are scoped to the guard only and do not escape:

```wado
// Bindings don't escape
if opt matches { Some(x) } && x > 0 { }  // ERROR: x not in scope

// Use guard inside the pattern instead
if opt matches { Some(x) && x > 0 } { }  // OK
```

## Memory Model

### Core Principles

- **Wasm-GC based**: Garbage collection delegated to runtime
- **Lifetime inference**: No explicit lifetime annotations required
- **Explicit move**: Ownership transfer only when explicitly stated

### Move Syntax (not yet implemented)

```wado
// Default: copy or reference (depending on type)
let a = some_value;
let b = a;          // a is still usable

// Explicit move
let b = move a;     // a is invalidated
println(a);         // Compile error: a has been moved

// Move to function
consume(move data);
```

### Unique Ownership (not yet implemented)

```wado
// Enforce unique ownership
let unique handle = open_file("data.txt");
let other = handle;       // Error: unique cannot be implicitly copied
let other = move handle;  // OK: explicit move
```

## Type System

### Type Mapping at Component Boundaries

Wado types are represented using WebAssembly core types (including GC types) internally within components, and are converted to Component Model types only when crossing component boundaries (import/export interfaces).

**Internal vs Boundary Representation:**

- **Internal**: Wado uses Wasm core types and GC types for efficient in-component representation
- **Boundary**: Component Model types are used at import/export interfaces (Canonical ABI)
- **Conversion**: The compiler automatically handles translation at component boundaries

This separation allows Wado to use optimal internal representations (e.g., Wasm GC structs) while maintaining interoperability through standardized Component Model types at boundaries.

| Wado Type                 | Internal Representation      | CM Type at Boundary                  | Notes                                            |
| ------------------------- | ---------------------------- | ------------------------------------ | ------------------------------------------------ |
| `bool`                    | `i32`                        | `bool`                               | Boolean value                                    |
| `char`                    | `i32`                        | `char`                               | Unicode scalar value                             |
| `i8`, `i16`, `i32`, `i64` | `i32`, `i32`, `i32`, `i64`   | `s8`, `s16`, `s32`, `s64`            | Signed integers                                  |
| `u8`, `u16`, `u32`, `u64` | `i32`, `i32`, `i32`, `i64`   | `u8`, `u16`, `u32`, `u64`            | Unsigned integers                                |
| `i128`, `u128`            | `i64` pair (Wide Arithmetic) | `tuple<s64, s64>`, `tuple<u64, u64>` | 128-bit integers                                 |
| `f32`, `f64`              | `f32`, `f64`                 | `f32`, `f64`                         | Floating point                                   |
| `f16`                     | -                            | -                                    | TODO: Wasm half-precision proposal (Phase 1)     |
| `String`                  | GC `array i8` (UTF-8)        | `string`                             | UTF-8 string, GC-managed internally              |
| `Array<T>`                | GC `array T`                 | `list<T>`                            | Dynamic array, GC-managed internally             |
| `[T1, T2, ...]`           | GC `struct {T1, T2, ...}`    | `tuple<T1, T2, ...>`                 | Tuple types                                      |
| `Option<T>`               | GC variant                   | `option<T>`                          | Optional value                                   |
| `Result<T, E>`            | GC variant                   | `result<T, E>`                       | Result type                                      |
| `Result<(), ()>`          | GC variant                   | `result`                             | Unit result (no payload)                         |
| `struct { ... }`          | GC `struct`                  | `record { ... }`                     | Wasm GC struct internally, record at CM boundary |
| `enum { ... }`            | `i32`                        | `enum { ... }`                       | Enumeration without payloads                     |
| `variant { ... }`         | GC variant                   | `variant { ... }`                    | Variant/sum type with payloads                   |
| `flags { ... }`           | `i32`/`i64`                  | `flags { ... }`                      | Bit flags                                        |
| `resource`                | `i32` (handle)               | `resource`                           | Resource handle                                  |
| `Stream<T>`               | CM stream (P3)               | `stream<T>`                          | Component Model async stream                     |
| `Future<T>`               | CM future (P3)               | `future<T>`                          | Component Model async future                     |

### The Prelude

The **prelude** (`core:prelude`) is automatically imported into every module, providing access to fundamental types without requiring explicit imports:

**Automatically Available:**

- `String` - UTF-8 string type
- `Array<T>` - Dynamic array type
- `Tuple<T1, T2, ...>` - Alias for `[T1, T2, ...]`
- `Reactive<T>` - Reactive value
- `Option<T>` and its variants: `Some(x)`, `None` (also accessible via `null` keyword)
- `Result<T, E>` and its variants: `Ok(x)`, `Err(e)`
- `Stream<T>` - Component Model async stream
- `Future<T>` - Component Model async future
- `Pollable` - WASI I/O polling resource
- `i128`, `u128` - 128-bit integer types

**Disabling the Prelude:**

```wado
#![no_prelude]  // At the top of a module

// Now you must explicitly import everything
use {String, Array, Tuple, Reactive, Option, Result, Stream, Future, Pollable} from "core:prelude";
```

### Primitive Types

Wasm primitive types are built into the language (no import required):

```wado
// Numeric
i8, i16, i32, i64
u8, u16, u32, u64
f32, f64

// Basic
bool
char
```

### Associated Constants

Associated constants are compile-time constants defined in `impl` blocks using the `const` keyword. They are inlined at every use site and cannot be mutated.

```wado
impl f64 {
    pub const PI: f64 = 3.14159265358979323846;
}

let pi = f64::PI;  // inlined as the literal value
```

Primitive types provide built-in associated constants and static methods. See [Core Standard Library Reference](./stdlib-core.md) for the full list.

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

WebAssembly has no native 128-bit integer type, so Wado represents them as pairs of 64-bit values. Addition and subtraction use Wasm Wide Arithmetic instructions (`i64.add128`, `i64.sub128`) for efficiency. Other operations (division, bitwise, etc.) use software implementations.

Available operations:

| Category   | Operations                                    |
| ---------- | --------------------------------------------- |
| Arithmetic | `+`, `-`, `*`, `/`, `%`, unary `-` (i128)     |
| Comparison | `==`, `!=`, `<`, `<=`, `>`, `>=`              |
| Bitwise    | `&`, `\|`, `^`, `~`, `<<`, `>>`               |
| Conversion | `from_u64()`, `from_i64()`, `low()`, `high()` |

### Reference Types

References in Wado provide indirect access to values. Unlike Rust, Wado uses a GC-based memory model with no borrow checker, enabling simpler semantics at the cost of runtime overhead.

**Basic Reference Syntax:**

```wado
let x = 42;
let r = &x;           // Immutable reference
let v = *r;           // Dereference

let mut y = 0;
let mr = &mut y;      // Mutable reference
*mr = 10;             // Assign through reference
```

**Reference to Reference:**

References can be nested arbitrarily:

```wado
let x = 42;
let r = &x;           // &i32
let rr = &r;          // &&i32
let val = **rr;       // 42 (double dereference)
```

**Automatic Coercion (`&mut` to `&`):**

Mutable references automatically coerce to immutable references when needed:

```wado
fn read_value(r: &i32) -> i32 {
    return *r;
}

let mut x = 10;
read_value(&mut x);   // OK: &mut i32 coerces to &i32
```

**Key Differences from Rust (GC-Based Memory Model):**

| Aspect                 | Rust                       | Wado                          |
| ---------------------- | -------------------------- | ----------------------------- |
| Memory management      | Ownership + borrow checker | Garbage collection            |
| Multiple mutable refs  | Not allowed                | **Allowed**                   |
| Returning local refs   | Not allowed (dangling)     | **Allowed** (GC keeps alive)  |
| Reference to reference | `&&T` (rare)               | `&&T` (fully supported)       |
| Lifetime annotations   | Required                   | **Not needed**                |
| Borrow checking        | Compile-time               | **None** (runtime GC instead) |

**Returning References to Local Variables:**

Because Wado uses garbage collection, references to local variables remain valid after the function returns:

```wado
fn make_ref() -> &i32 {
    let x = 42;
    return &x;  // OK in Wado (x is GC-managed and stays alive)
}

let r = make_ref();
println(`{*r}`);  // Works: prints "42"
```

This would be a dangling pointer error in Rust, but is safe in Wado due to garbage collection.

**Multiple Mutable References:**

Wado allows multiple mutable references to the same value:

```wado
let mut x = 10;
let r1 = &mut x;
let r2 = &mut x;  // OK in Wado (no borrow checker)

*r1 = 20;
*r2 = 30;
```

**Design Trade-offs:**

- **Simplicity**: No lifetime annotations or borrow checker errors
- **Flexibility**: Can freely share and modify references
- **Cost**: Runtime overhead from garbage collection
- **Safety**: Memory safety guaranteed by GC, not compile-time checks

**Method Receiver: `self` by Value is Prohibited**

In method definitions, the `self` parameter must always be a reference (`&self` or `&mut self`). Bare `self` (by value) is a syntax error:

```wado
impl Point {
    fn sum(&self) -> i32 { ... }          // OK: immutable reference
    fn reset(&mut self) { ... }           // OK: mutable reference
    // fn consume(self) -> i32 { ... }    // ERROR: `self` by value is not allowed
}
```

In languages with ownership semantics (e.g., Rust), `self` by value transfers ownership to the method, preventing subsequent use of the receiver. Wado has no ownership system — there is no concept of "consuming" a value — so `self` by value serves no purpose. The parser rejects it with a clear error message guiding the user to `&self` or `&mut self`.

### String Type

`String` is a built-in type representing UTF-8 encoded text with value semantics and GC management.

**Design Principles**:

- Value semantics: Conceptually behaves like a value type
- Immutable content: String data cannot be modified in-place
- GC-managed: Memory is automatically managed by Wasm GC
- UTF-8 encoding: Direct mapping to Component Model `string`

**Semantics and Encoding**:

- Semantically, a `String` is a sequence of Unicode scalar values
- Internally represented as a UTF-8 byte array (`Array<u8>`)
- Invalid UTF-8 byte sequences are not allowed; all String values must be valid UTF-8
- This ensures interoperability with Component Model `string` type and safe string operations

**Internal Structure**:

```wado
// Conceptual representation (not user-visible)
struct String {
    data: GcArray<u8>,   // UTF-8 bytes
    len: i32,            // Length in bytes
    capacity: i32,       // Buffer capacity for += operations
}
```

#### Index Access (Prohibited)

Direct index access is prohibited to avoid ambiguity between byte and character indexing:

```wado
let s = "Hello世界";

// Prohibited
s[0]      // Compile error
s[0..5]   // Compile error
```

Use explicit methods instead:

```wado
// Byte-level access
let bytes: Array<u8> = s.bytes();
let first_byte = bytes[0];

// Character-level access
let chars: Array<char> = s.chars();
let first_char = chars[0];

// Other methods
s.len() -> i32             // Length in bytes
s.is_empty() -> bool       // Check if empty
```

**Note**: `bytes()` and `chars()` return iterator objects (`StrUtf8ByteIter` and `StrCharIter`) that implement both `Iterator` and `IntoIterator`, so they work with `for-of` directly:

```wado
for let c of "hello".chars() {
    println(`{c}`);  // h, e, l, l, o
}

for let b of "hello".bytes() {
    println(`{b}`);  // 104, 101, 108, 108, 111
}
```

**String Building:**

The `append` method provides efficient O(1) amortized string building:

```wado
let mut builder = String::with_capacity(20);
builder.append("Hello");
builder.append(", ");
builder.append("World!");
// builder is now "Hello, World!"

// Static method for two-string concatenation
let combined = String::concat("Hello, ", "World!");  // "Hello, World!"
```

#### Concatenation

**New String (`+` operator)**:

```wado
let s1 = "hello";
let s2 = " world";
let s3 = s1 + s2;  // Creates new String
```

**Mutation (`+=` operator)**:

The `+=` operator provides efficient in-place concatenation:

```wado
let mut s = "hello";
s += " world";     // Efficient: uses internal capacity
s += "!";          // May reallocate if capacity exceeded
```

**Implementation**: `+=` desugars to `String::add_assign(&mut s, suffix)` which manages an internal buffer with amortized O(1) complexity.

**Pre-allocation**:

```wado
// Allocate capacity upfront for efficient building
let mut result = String::with_capacity(1000);
for let item of items {
    result += item;  // No reallocations if within capacity
}
```

#### Semantic vs Implementation

```wado
// Semantically: value copy
let s1 = "hello";
let s2 = s1;  // s1 still usable

// Implementation: reference sharing (safe because immutable)
// No actual copy of string data occurs
```

**Explicit move**:

```wado
let s1 = "hello";
let s2 = move s1;  // s1 invalidated, no copy
// s1 is no longer accessible
```

**Implicit move optimization**:

```wado
let mut s = "hello";
let temp = "world";
s = temp;  // If temp is not used after, compiler may optimize to move
```

#### Operator Consistency

The `+=` operator has special semantics for String:

```wado
// For numeric types
x += 5  ≡  x = x + 5  // Exact equivalence

// For String
s += t  ≈  s = s + t  // Same result, different implementation
                      // += is more efficient (no intermediate allocation)
```

This special treatment will be generalized via traits in the future, allowing user types to define their own `+=` behavior.

#### Performance Guidelines

**Efficient patterns**:

```wado
// 1. Pre-allocate when size is known
let mut s = String::with_capacity(estimated_size);
for let item of items {
    s += item;
}

// 2. Use += for repeated concatenation
let mut result = "";
result += "Line 1\n";
result += "Line 2\n";
result += "Line 3\n";

// 3. Join arrays of strings (future)
let parts = ["a", "b", "c"];
let result = parts.join(",");
```

**Inefficient patterns**:

```wado
// Avoid: creates intermediate String objects
let s = "a" + "b" + "c" + "d";

// Prefer:
let mut s = "a";
s += "b";
s += "c";
s += "d";
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
let missing: Option<i32> = null;  // Same as None
let also_missing = None;          // Standard library identifier

// Both are equivalent
assert null == None;
```

Note: `null` is a language keyword, while `None` is an identifier from the prelude (`Option::None`). They compile to the same instructions.

#### Character Literals

Character literals use single quotes and represent a Unicode scalar value. While internally represented as a 32-bit value (like `u32`), `char` is a distinct type with Unicode semantics—similar to how `String` differs from `Array<u8>`:

```wado
let letter = 'A';
let digit = '9';
let unicode = '\u0041';  // Unicode escape (same as 'A')
let emoji = '😀';        // Direct Unicode character
let newline = '\n';
```

See [Escape Sequences](#escape-sequences) for supported escapes (`\'` for char, `\"` for string).

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

All other integer-to-char casts are **prohibited** because not all values are valid Unicode scalar values (surrogates `0xD800..0xDFFF` and values `> 0x10FFFF` are invalid):

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

See [Core Standard Library Reference](./stdlib-core.md) for the full `char` API.

Casting `char` to non-integer types is a compile error:

```wado
let c = 'A';
let f = c as f64;     // compile error: char can only be cast to integer types
let s = c as String;  // compile error: char can only be cast to integer types
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

**Type coercion**: When the target type is known from context (type annotation or function argument), integer literals coerce to any compatible integer type, including `i128`/`u128`:

```wado
let byte: i8 = 127;
let long: i64 = 9_223_372_036_854_775_807;
let unsigned: u32 = 4_294_967_295;
let big: u128 = 1_000_000_000_000;
fn foo(n: i64) { ... }
foo(100);  // literal coerced to i64
```

**Compile-time range checking**: The compiler rejects literal coercions whose value falls outside the target type's range. All literal bases (decimal, hex `0x`, octal `0o`, binary `0b`) use strict numeric range: the value must lie within `[MIN, MAX]` for signed types or `[0, MAX]` for unsigned types.

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
let i: u32 = 0x1_0000_0000;       // compile error: 33-bit value does not fit in u32
```

**Type conversion** (via `as`):

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

**Type coercion**: Floating-point literals coerce to either `f32` or `f64` when the target type is known:

```wado
let single: f32 = 3.14;
let double: f64 = 3.14159265358979;
```

**Type conversion** (via `as`):

```wado
let single: f32 = 3.14 as f32;
let double: f64 = 3.14159265358979 as f64;
```

#### String Literals

String literals create `String` values.

**Regular strings** use double quotes:

```wado
let name = "Alice";           // Type: String
let path = "path/to/file.txt";
let escaped = "Line 1\nLine 2\tTabbed";
```

#### Escape Sequences

Escape sequences are shared between character and string literals:

| Escape   | Character                  |
| -------- | -------------------------- |
| `\'`     | Single quote (char only)   |
| `\"`     | Double quote (string only) |
| `\\`     | Backslash                  |
| `\/`     | Forward slash              |
| `\b`     | Backspace                  |
| `\f`     | Form feed                  |
| `\n`     | Newline                    |
| `\r`     | Carriage return            |
| `\t`     | Tab                        |
| `\uHHHH` | Unicode BMP (4 hex digits) |
| `\u{H+}` | Unicode full range         |

For characters outside BMP (U+10000 and above), use either:

```wado
"\uD83D\uDE00"   // Surrogate pair
"\u{1F600}"      // Variable-length escape
"😀"             // Direct Unicode character
```

**Template strings** (interpolation) use backticks:

```wado
let name = "Alice";
let greeting = `Hello, {name}!`;  // "Hello, Alice!"

let count = 42;
let message = `Count: {count}`;   // "Count: 42"

// Format specifiers
let pi = 3.14159;
let formatted = `Pi: {pi:0.2f}`;  // "Pi: 3.14"
let hex = `{255:x}`;              // "ff"

// Inspect (debug) format — works for any type
let p = Point { x: 10, y: 20 };
let debug = `{p:?}`;             // "Point { x: 10, y: 20 }"
let fallback = `{p}`;            // same — falls back to inspect when no Display impl
```

See [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md) for the full specifier table, [WEP: Format Traits](./wep-2026-02-01-format-traits.md) for the trait/Formatter infrastructure, and [WEP: Inspect](./wep-2026-02-21-inspect-debug-output.md) for the `:?` debug output format.

**Multiline strings** are supported in both regular and template strings. Literal newlines are preserved:

```wado
// Regular multiline string
let poem = "Roses are red,
Violets are blue,
Wado is great,
And so are you!";

// Multiline template string
let name = "Alice";
let message = `Dear {name},

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

**Tuple Types:**

Tuple types use bracket syntax `[T1, T2, ...]`. `Tuple<T1, T2, ...>` is available as an alias.

```wado
let point: [i32, i32] = [10, 20];
let record: [String, i32, bool] = ["Alice", 30, true];
```

**Tuple Element Access:**

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

**Unit vs Empty Tuple:**

The unit type `()` and empty tuple `[]` are distinct:

```wado
let unit: () = ();    // Unit type/value
let empty: [] = [];   // Empty tuple (rarely used)
```

**The `never` type (`!`) — bottom type:**

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

#### Array Literals

Arrays require explicit conversion from tuple literals using `as` or implicit coercion when the target type is known at compile time.

```wado
// Explicit conversion with `as`
let numbers = [1, 2, 3, 4, 5] as Array<i32>;

// Implicit coercion (target type known)
fn takes_array(a: Array<i32>) { ... }
takes_array([1, 2, 3]);  // OK - compiler knows Array<i32> is expected

// Type annotation
let explicit: Array<i32> = [1, 2, 3];  // Coerced to array
```

**Coercion Rules:**

- **Compile-time**: When the target type is known (function parameter, type annotation), implicit coercion is allowed
- **Runtime/ambiguous**: Explicit `as Array<T>` is required

```wado
let t = [1, 2, 3];               // Tuple [i32, i32, i32] - no context
let a = [1, 2, 3] as Array<i32>; // Array - explicit conversion

fn process(data: Array<i32>) { ... }
process([1, 2, 3]);              // OK - implicit coercion
```

**Design Rationale:**

This design aligns with TypeScript (primary target audience) and enables intuitive JSON interoperability. JSON arrays are heterogeneous and map naturally to tuples:

```json
{ "point": [10, 20], "mixed": [1, "hello", true] }
```

```wado
use config from "./config.json" with { type: "json" };
// config::point is [i32, i32]
// config::mixed is [i32, String, bool]
```

See `docs/wep-2026-01-15-tuple-and-array-literals.md` for detailed rationale.

**Array Operations:**

Arrays support index-based access and assignment:

**Array Constructors:**

```wado
let arr = Array::<i32>::with_capacity(10);     // empty array with pre-allocated capacity
let bools = Array::<bool>::filled(100, true);  // array of 100 elements, all true
```

**Array Operations:**

```wado
let mut arr: Array<i32> = [1, 2, 3];

// Index access (read)
let first = arr[0];  // 1

// Index assignment (write)
arr[0] = 100;        // Requires mutable array
arr[1] = 200;

// Array methods
arr.append(4);       // Add element to end
let len = arr.len(); // Get length
```

**Index Assignment Rules:**

- Requires the array variable to be declared with `let mut`
- Index must be within bounds (runtime check, traps if out of bounds)
- Works with arrays of any element type

**Sorting** (stable, O(n log n) worst case):

| Method        | Mutates? | Comparator                      |
| ------------- | -------- | ------------------------------- |
| `sort()`      | Yes      | `<` (requires `T: Ord`)         |
| `sort_by()`   | Yes      | Custom `fn(&T, &T) -> Ordering` |
| `sorted()`    | No       | `<` (requires `T: Ord`)         |
| `sorted_by()` | No       | Custom `fn(&T, &T) -> Ordering` |

```wado
let mut nums: Array<i32> = [5, 3, 8, 1];
nums.sort();                             // in-place ascending

let orig: Array<i32> = [5, 3, 8, 1];
let asc = orig.sorted();                // returns new sorted array
```

#### Collection Literal Coercion

Sequence literals `[e0, e1, ...]` and key-value literals `{ k: v, ... }` can be
coerced to any collection type by implementing the corresponding builder trait:

| Literal         | Trait                    | Example target       |
| --------------- | ------------------------ | -------------------- |
| `[e0, e1, ...]` | `SequenceLiteralBuilder` | `Array<T>`           |
| `{ k: v, ... }` | `KeyValueLiteralBuilder` | `TreeMap<String, V>` |

**Builder Traits:**

```wado
pub trait SequenceLiteralBuilder {
    type Element;
    type Output;
    fn new_literal(capacity: i32) -> Self;
    fn push_literal(&mut self, value: Self::Element);
    fn build(&self) -> Self::Output;
}

pub trait KeyValueLiteralBuilder {
    type Value;
    type Output;
    fn new_literal(capacity: i32) -> Self;
    fn insert_literal(&mut self, key: String, value: Self::Value);
    fn build(&self) -> Self::Output;
}
```

When a type implements `SequenceLiteralBuilder<Output = Self>` or `KeyValueLiteralBuilder<Output = Self>`, a blanket impl provides the corresponding `SequenceLiteral` / `KeyValueLiteral` trait automatically (self-as-builder pattern).

**Usage:**

```wado
let arr: Array<i32> = [1, 2, 3];

use { TreeMap } from "core:collections";
let map: TreeMap<String, i32> = { width: 1920, height: 1080 };
```

Coercion is literal-only — it does not apply to bound variables. If the target type is a struct with matching fields, it is interpreted as a struct literal and coercion is not attempted.

See [`docs/wep-2026-01-18-iterator-based-literal-coercion.md`](./wep-2026-01-18-iterator-based-literal-coercion.md)
for desugaring rules, the immutable-output (separate builder) pattern, and concrete type validation.

### Compile-Time Location Literals

Compile-time location literals provide source location information at compile time. They use the `#` prefix to clearly signal compile-time evaluation.

| Literal     | Type     | Value                                              |
| ----------- | -------- | -------------------------------------------------- |
| `#file`     | `String` | Current source file path                           |
| `#line`     | `i32`    | Current line number (1-indexed)                    |
| `#function` | `String` | Fully specialized function name                    |
| `#data`     | `String` | `__DATA__` section content (compile error if none) |

```wado
fn example() {
    println(`Error at {#file}:{#line}`);
    println(`In function: {#function}`);
}
```

**`#data`:**

Returns the raw text content of the `__DATA__` section as a `String`. This is useful for programs that need to access embedded metadata at runtime (e.g., configuration, test fixtures, embedded documents). Using `#data` in a file that has no `__DATA__` section is a compile error.

```wado
export fn run() with Stdout {
    let config = #data;  // contains the __DATA__ section text
    println(config);
}

__DATA__
{"key": "value"}
```

**`#function` Format:**

Returns the fully specialized name without signature:

| Context        | `#function` value            |
| -------------- | ---------------------------- |
| Free function  | `my_function`                |
| Method         | `Point::distance`            |
| Generic method | `Array<String>::len`         |
| Closure        | `parent_function::{closure}` |

### Closures

Closures are anonymous function expressions with `|params| body` syntax.

**Expression Body (no braces):**

```wado
// Single expression, implicit return
let add_one = |x: i32| x + 1;
let result = add_one(5);  // 6

// Multiple parameters
let add = |a: i32, b: i32| a + b;
let sum = add(3, 4);  // 7

// Returning different types
let is_even = |x: i32| x % 2 == 0;
let check = is_even(4);  // true

// Struct literal as expression body
let make_point = |x: i32, y: i32| Point { x, y };
let p = make_point(10, 20);
```

**Block Body (with braces):**

Block body closures require explicit `return` statements:

```wado
let compute = |x: i32| {
    let doubled = x * 2;
    let tripled = x * 3;
    return doubled + tripled;
};
let result = compute(4);  // 20
```

**Current Limitations:**

- **Type annotations required**: Parameter types must be explicitly specified
- **No inference**: Unlike some languages, closure parameter types are not inferred

```wado
// Pure closure (no captures)
let pure = |x: i32| x * 2;

// Capturing outer variables (value semantics - copy)
let outer = 10;
let capture = |x: i32| x + outer;  // Captures `outer` by value
capture(5);  // Returns 15
```

Closures capture variables by value (copy semantics) by default. Use `&mut ||` for mutable capture (see below).

Note: `stores[...]` is a separate concept for declaring that a _function_ stores reference _parameters_ beyond the call. It is not yet implemented. See [Reference Storage](#reference-storage-stores) and [`docs/wep-2026-01-12-value-semantics-and-stores.md`](./wep-2026-01-12-value-semantics-and-stores.md).

**Mutable Closures (`&mut ||`):**

`&mut ||` creates a closure that captures variables by mutable reference instead of by value:

```wado
let mut count = 0;
let inc = &mut || { count += 1; };
inc();
inc();
println(`{count}`);  // 2

// Multiple closures sharing the same mutable variable
let mut count = 0;
let inc = &mut || { count += 1; };
let get = || count;
inc();
inc();
println(`{get()}`);  // 2
```

### Tagged Template Literals

Tagged template literals enable compile-time function execution on string literals, allowing zero-overhead binary encoding, DSL validation, and custom compile-time transformations.

**Syntax:**

```wado
let result = tag`literal string`;
```

Where `tag` is an effect-free function that executes at compile time.

**Requirements:**

- The tag function must have no `with` clause (effect-free/pure function)
- Function signature: `fn(String) -> T` where `T` is any type
- The function is executed during compilation
- If the function panics, it becomes a compile error
- Only other effect-free functions can be called within the tag function

**Example - Binary Literals:**

```wado
use {base64, hex} from "core:encoding";

// base64 and hex are standard library functions, not keywords
let embedded_image = base64`iVBORw0KGgoAAAANSUhEUgAAAAUA...`;  // Type: Array<u8>
let crypto_key = hex`48656c6c6f20576f726c64`;                // Type: Array<u8>

// Invalid base64 causes compile error
let invalid = base64`!!!invalid!!!`;  // Compile error: Invalid base64 encoding
```

**Example - DSL Validation:**

```wado
// User-defined compile-time validation
fn regex(pattern: String) -> Regex {
    match compile_regex(pattern) {
        Ok(r) => r,
        Err(e) => panic(`Invalid regex pattern: {e}`),  // Compile error
    }
}

let email_pattern = regex`^[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}$`;

// SQL query validation
fn sql(query: String) -> SqlQuery {
    match parse_sql(query) {
        Ok(q) => q,
        Err(e) => panic(`Invalid SQL syntax at {e.position}: {e.message}`),
    }
}

let query = sql`SELECT * FROM users WHERE id = ?`;  // Validated at compile time
```

**Standard Library Support:**

The `core:encoding` module provides common binary encodings:

```wado
use {base64, hex} from "core:encoding";

// Base64 decoding (RFC 4648)
pub fn base64(input: String) -> Array<u8> {
    match decode_base64_impl(input) {
        Ok(data) => data,
        Err(e) => panic(`Invalid base64 encoding: {e}`),
    }
}

// Hexadecimal decoding
pub fn hex(input: String) -> Array<u8> {
    match decode_hex_impl(input) {
        Ok(data) => data,
        Err(e) => panic(`Invalid hex encoding: {e}`),
    }
}
```

**Compile-Time Execution Constraints:**

Tagged template functions are executed at compile time with the following constraints:

- **Effect-free only**: Functions with `with` clauses cannot be used as tags
- **Pure computation**: Only other effect-free functions can be called
- **Deterministic**: Execution must be deterministic (guaranteed by Wado's deterministic libm)
- **Heap allocation**: Allowed via Wasm GC (unlike Rust's `const fn`)
- **Recursion**: Allowed with reasonable depth limits
- **No I/O**: Functions requiring effects (FileSystem, Network, etc.) cannot be called

**Design Rationale:**

Tagged template literals provide a general mechanism for compile-time computation, avoiding the need for built-in syntax for each use case. This aligns with Wado's philosophy of minimal built-ins and explicit dependencies. See `docs/wep-2026-01-10-tagged-template-literals.md` for detailed design decisions.

**Future Extensions:**

Interpolation support may be added in future versions:

```wado
// Future: interpolation syntax (not yet implemented)
let id = 42;
let query = sql`SELECT * FROM users WHERE id = ${id}`;
```

### Newtype

`type T = U` creates a **newtype** - a distinct type that shares representation with its base type.

```wado
type Meters = f64;
type Kilometers = f64;

let m: Meters = 1000.0;       // literal coercion
let km: Kilometers = 1.0;

let sum = m + m;              // OK: Meters + Meters -> Meters
// let bad = m + km;          // ERROR: cannot mix Meters and Kilometers

let raw: f64 = m as f64;      // explicit cast required
```

**Properties:**

- `T` is a distinct type from `U` (no implicit conversion)
- `T` inherits all methods, operators, and traits from `U`
- Explicit `as` cast required to convert between `T` and `U`
- Zero runtime cost (same Wasm representation)
- Literal coercion to `T` when type context expects `T`

**Method Signature Substitution:**

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

**Newtype-Specific Methods:**

```wado
impl Location {
    fn name(&self) -> String { ... }  // only on Location, not Point
}
```

**Chained Newtypes:**

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

### Literal Types

```wado
// String literal types
type Direction = "north" | "south" | "east" | "west";

// Numeric literal types
type Digit = 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9;

// Object literal types
type Status = {
    status: "loading" | "success" | "error",
    message: String,
};
```

### Structs

Wado uses `struct` for structured data types. Internally they are implemented as Wasm-GC structs, and automatically converted to Component Model `record` at component boundaries.

```wado
// Struct definition
struct User {
    name: String,
    age: i32,
    active: bool,
}

// Struct with recursive type (enabled by GC)
struct Node {
    value: i32,
    next: Option<Node>,
}

// Inline struct type
type UserData = struct {
    name: String,
    age: i32,
};
```

**Field Visibility:**

Struct fields follow the same visibility rules as other declarations. Fields without `pub` are private to the defining module. Fields marked `pub` are accessible from other modules.

```wado
pub struct Config {
    pub name: String,   // accessible from other modules
    secret: i32,        // private to this module
}
```

Within the defining module, all fields (including private ones) are accessible for construction, reading, and mutation. From other modules, accessing or constructing with a private field produces a compile error.

**Struct Construction:**

```wado
let user = User { name: "Alice", age: 30, active: true };

// Shorthand (variable name matches field)
let name = "Bob";
let age = 25;
let bob: User = { name, age, active: false };

// Implicit struct literal (requires type annotation)
let user: User = { name: "Alice", age: 30, active: true };
```

**Struct Destructuring:**

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
    println(`{x}, {y}`);
}
```

### Generic Type Inference

Wado infers type arguments for generic type constructors (struct literals and variant constructors) using two complementary mechanisms:

**Forward inference** derives type parameters from the values provided (fields or payload arguments):

```wado
struct Box<T> { value: T }
let b = Box { value: 42 };             // Box<i32> — T=i32 from field value

let opt = Option::Some("hello");        // Option<String> — T=String from payload
let opt2 = Option::Some(42);            // Option<i32> — T=i32 from payload
```

**Backward inference** derives type parameters from an expected type context (variable annotation, function parameter type, or return type):

```wado
let none: Option<i32> = Option::None;   // T=i32 from annotation
let ok: Result<i32, String> = Result::Ok(42);
// T=i32 from payload (forward), E=String from annotation (backward)
```

When both mechanisms are available, forward inference takes precedence for type parameters that appear in the payload, and backward inference fills in any remaining parameters.

**Scope of inference:**

| Constructor kind       | Forward | Backward | Status              |
| ---------------------- | ------- | -------- | ------------------- |
| Struct literals        | yes     | yes      | implemented         |
| Variant constructors   | yes     | yes      | implemented         |
| Generic function calls | —       | —        | not yet implemented |
| Generic method calls   | —       | —        | not yet implemented |

For generic function and method calls, explicit turbofish syntax is required:

```wado
fn identity<T>(x: T) -> T { return x; }
let x = identity::<i32>(42);           // turbofish required (for now)
// let y = identity(42);               // not yet supported
```

### Traits

Traits define shared behavior that types can implement. Wado uses **static dispatch** for trait methods - all calls are resolved at compile time.

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
        return `Hello, {self.name}!`;
    }
}

// Usage
let p = Person { name: "Alice" };
println(p.greet());  // "Hello, Alice!"
```

**Multiple Traits:**

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

**Method Resolution:**

When a method is called on a value:

1. **Inherent methods** (defined in `impl Type { }`) are checked first
2. **Trait methods** (defined in `impl Trait for Type { }`) are checked if no inherent method matches
3. If multiple traits define the same method name, it's a compile error

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

**Default Method Implementations:**

Trait methods can have default implementations. Implementors can override them or use the defaults:

```wado
trait Summary {
    fn title(&self) -> String;  // required - must be provided

    // Default method - uses self.title()
    fn summary(&self) -> String {
        return `Title: {self.title()}`;
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
    fn summary(&self) -> String { return `{self.headline}: {self.body}`; }
}
```

Default methods can call other trait methods (both required and default), and the calls are resolved against the implementing type.

**Associated Types:**

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

**Bounded Associated Types:**

Associated types can have trait bounds that constrain what types can be used as the associated type:

```wado
trait Collection {
    type Element;
    type Builder: CollectionBuilder<Element = Self::Element, Output = Self>;
}
```

Here `Builder` must implement `CollectionBuilder` with matching `Element` and `Output` types. The `Type = ConcreteType` syntax constrains associated types of the bound trait to specific types.

**Blanket Implementations:**

A blanket impl provides a trait implementation for all types that satisfy a given bound:

```wado
// Any type that builds itself satisfies Collection automatically
impl<T: CollectionBuilder<Output = T>> Collection for T {
    type Element = T::Element;
    type Builder = T;
}
```

This avoids the need for explicit `impl Collection for ...` on every self-building type. The compiler resolves `T::Element` via associated type projection on the type parameter.

**Standard Library Traits:**

The prelude defines `IndexValue`, `IndexAssign`, and `Index` traits using associated types. See [Indexing Traits](#indexing-traits) for full definitions.

**Trait Bounds:**

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
impl Array<T: Ord> {
    pub fn sort(&mut self) { ... }
    pub fn sorted(&self) -> Array<T> { ... }
}

// Bounded trait implementations - Pair<T> implements Eq only when T: Eq
impl<T: Eq> Eq for Pair<T> {
    fn eq(&self, other: &Self) -> bool {
        return self.first == other.first && self.second == other.second;
    }
}
```

**Not Yet Implemented:**

- Trait objects (`dyn Trait`)
- Fully qualified syntax for disambiguation (`<Type as Trait>::method()`)
- Using bounds for method resolution on type parameters (calling `T.method()` where `T: Trait`)

### Iterator Traits

The prelude defines iterator traits for generic iteration over collections.

**Iterator - Core Iteration Trait:**

```wado
/// Types that can yield a sequence of values
pub trait Iterator {
    type Item;

    /// Advances the iterator and returns the next value.
    /// Returns None when iteration is complete.
    fn next(&mut self) -> Option<Self::Item>;
}
```

**IntoIterator - Conversion Trait:**

```wado
/// Types that can be converted into an iterator
pub trait IntoIterator {
    type Item;
    type Iter;  // The iterator type

    /// Creates an iterator from a value
    fn into_iter(&self) -> Self::Iter;
}
```

**FromIterator - Collection Construction:**

```wado
/// Types that can be constructed from an iterator
pub trait FromIterator<T> {
    type Iter;
    fn from_iter(iter: Self::Iter) -> Self;
}
```

**ArrayIter:**

The prelude provides `ArrayIter<T>` as the iterator type for `Array<T>`:

```wado
/// Iterator over Array<T> elements
pub struct ArrayIter<T> {
    // internal fields
}

impl Iterator for ArrayIter<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> { ... }
}

impl IntoIterator for Array<T> {
    type Item = T;
    type Iter = ArrayIter<T>;
    fn into_iter(&self) -> ArrayIter<T> { ... }
}
```

**Usage:**

```wado
let arr: Array<i32> = [1, 2, 3, 4, 5];

// for-of uses IntoIterator automatically
for let x of arr {
    println(`{x}`);
}

// Explicit iterator
let mut iter = arr.iter();
while let Some(x) = iter.next() {
    println(`{x}`);
}

// Collect remaining elements
let mut iter2 = arr.iter();
iter2.next();  // skip first
let rest = iter2.collect();  // [2, 3, 4, 5]
```

**Value Semantics:**

Iterator `next()` returns copies of elements (value semantics). Wasm GC cannot yield `&mut T` for array elements, so `iter_mut()` is not available. For in-place mutation, use indexed access:

```wado
for let mut i = 0; i < arr.len(); i += 1 {
    arr[i] = arr[i] * 2;
}
```

**Custom Iterables:**

Any type can be made iterable by implementing `IntoIterator`:

```wado
struct Stack<T> { items: Array<T> }
struct StackIter<T> { items: Array<T>, index: i32 }

impl Iterator for StackIter<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> { ... }
}

impl IntoIterator for Stack<T> {
    type Item = T;
    type Iter = StackIter<T>;
    fn into_iter(&self) -> StackIter<T> { ... }
}

// Now for-of works
for let x of stack { ... }
```

**Iterator Combinators:**

Iterators support `map`, `filter`, and `fold` for functional-style data processing:

```wado
let arr: Array<i32> = [1, 2, 3, 4, 5];

// map - transform each element
let doubled = arr.iter().map(|x: i32| x * 2).collect();
// [2, 4, 6, 8, 10]

// filter - keep elements matching predicate
let evens = arr.iter().filter(|x: i32| x % 2 == 0).collect();
// [2, 4]

// fold - reduce to single value
let sum = arr.iter().fold(0, |acc: i32, x: i32| acc + x);
// 15

// Chaining combinators
let result = arr.iter()
    .filter(|x: i32| x > 2)
    .map(|x: i32| x * 10)
    .collect();
// [30, 40, 50]
```

### Builtin Comparison Traits

The prelude defines traits for comparison operators:

**Eq - Equality:**

```wado
/// Types that can be compared for equality
pub trait Eq {
    /// Returns true if self equals other
    fn eq(&self, other: &Self) -> bool;
}
```

The `==` and `!=` operators use `Eq::eq`:

- `a == b` desugars to `Eq::eq(&a, &b)`
- `a != b` desugars to `!Eq::eq(&a, &b)`

**Ordering Enum:**

```wado
/// Result of a three-way comparison
pub enum Ordering {
    Less,    // first value is less than second
    Equal,   // values are equal
    Greater, // first value is greater than second
}
```

**Ord - Ordering:**

```wado
/// Types that can be ordered
pub trait Ord {
    /// Compares self with other and returns an Ordering
    fn cmp(&self, other: &Self) -> Ordering;
}
```

Comparison operators use `Ord::cmp`:

- `a < b` desugars to `Ord::cmp(&a, &b) == Ordering::Less`
- `a > b` desugars to `Ord::cmp(&a, &b) == Ordering::Greater`
- `a <= b` desugars to `Ord::cmp(&a, &b) != Ordering::Greater`
- `a >= b` desugars to `Ord::cmp(&a, &b) != Ordering::Less`

**Default Implementations:**

`String` and `Array<T>` implement `Eq` and `Ord` with lexicographic comparison:

```wado
impl Eq for String { ... }  // byte-by-byte equality
impl Ord for String { ... } // lexicographic ordering

// Usage
let a = "apple";
let b = "banana";
if a < b { ... }  // true
```

### Indexing Traits

The prelude defines traits for index-based access:

**IndexValue - Value Read:**

```wado
/// Returns element by value (copy)
pub trait IndexValue<IndexType> {
    type Output;
    fn index_value(&self, index: IndexType) -> Self::Output;
}
```

**IndexAssign - Value Write:**

```wado
/// Assigns value to element at index
pub trait IndexAssign<IndexType> {
    type Input;
    fn index_assign(&mut self, index: IndexType, value: Self::Input);
}
```

**Index - Reference Read:**

```wado
/// Returns element by reference (for reference-type elements only)
pub trait Index<IndexType> {
    type Output;
    fn index(&self, index: IndexType) -> &Self::Output;
}
```

**Design Note:** `IndexValue` returns by value because Wasm GC's `array.get` instruction copies elements. For primitives like `i32`, you cannot get `&i32` from an array element. `Index` is for containers of reference-type elements where returning a reference is possible.

`Array<T>` implements `IndexValue` and `IndexAssign`:

```wado
let mut arr: Array<i32> = [1, 2, 3];
let x = arr[0];    // IndexValue::index_value
arr[1] = 100;      // IndexAssign::index_assign
```

### Enums, Variants, and Flags

Wado follows Component Model's distinction between enums and variants (unlike Rust):

**Enums** (no payloads - Component Model `enum`):

```wado
// Simple enumeration - all cases have no data
enum Color {
    Red,
    Green,
    Blue,
}

// Construction
let c = Color::Red;

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

Enums auto-derive `Display` (case name), `Eq` (discriminant equality), and `Ord` (declaration order).

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

**Variants** (with payloads - Component Model `variant`):

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
    println(`Got: {x}`);
}

// Custom variant pattern matching with tuple destructuring
// Note: pattern uses case name only, not Type::CaseName
variant ParseResult {
    Fail,
    Number([i32, i32]),  // start, end positions
}
let result = ParseResult::Number([0, 10]);
if let Number([start, end]) = result {
    println(`Got number from {start} to {end}`);
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

**Implementation Status**:

- Variant declarations and construction: implemented
- Generic variant type inference (forward from payload, backward from annotation): implemented
- `if let` pattern matching for `Option<T>`: implemented
- `if let` pattern matching for non-generic custom variants: implemented
- Tuple payload pattern destructuring (`if let Foo([a, b]) = x`): implemented
- `match` expression/statement: implemented
- `matches` operator: implemented
- Match ergonomics (`&T` scrutinees in `if let`/`match`/`matches`; payload bindings become refs): implemented
- Generic custom variant pattern matching (e.g., `Maybe<T>`): not yet implemented
- `Result<T, E>` pattern matching: not yet implemented

Note: `Option<T>` and `Result<T, E>` are declared as variants in `core:prelude`.

**Flags** (bit flags - Component Model `flags`):

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

Flags are implemented as newtypes over `u32`. Member names can carry `#[wasi("...")]` attributes for WIT/Component Model name mapping:

```wado
pub flags PathFlags {
    #[wasi("symlink-follow")]
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
map.insert("x", 10);
map.insert("y", 20);

// Index syntax
map["z"] = 30;                    // assignment
let v = map["x"];                 // panics if key not found
let opt = map.get("x");          // returns Option<V>

// Keys preserve insertion order
let keys = map.keys();  // returns Array<K> in insertion order
```

### Access Methods

```wado
// Struct: dot notation
user.name

// TreeMap: bracket notation or methods
map["key"]        // panics if key not found
map.get("key")    // returns Option<V>
```

## Module System

Wado uses an ESM-like import syntax with `use {...} from "module"`. This aligns with JavaScript/TypeScript conventions, as JavaScript is a primary host environment for Wado.

### Visibility

Wado distinguishes between two kinds of "public" visibility:

| Keyword  | Term              | Meaning                                                          |
| -------- | ----------------- | ---------------------------------------------------------------- |
| `pub`    | **module public** | Visible to other Wado modules that import this module            |
| `export` | **world export**  | Exposed at the Component Model boundary (WASI world conformance) |

The `pub` keyword controls **module public** visibility - whether a symbol can be accessed by other Wado modules:

```wado
// Private to this module (default)
fn internal_helper() { ... }

// Module public - accessible from other Wado modules
pub fn api_function() -> i32 { ... }

// World export - exposed at CM boundary
export fn run() { ... }

// Both module public and world export
pub export fn shared_entry() { ... }
```

| Declaration           | Within module | Other Wado modules | CM world boundary |
| --------------------- | ------------- | ------------------ | ----------------- |
| `fn foo()`            | Yes           | No                 | No                |
| `pub fn foo()`        | Yes           | Yes                | No                |
| `export fn foo()`     | Yes           | No                 | Yes               |
| `pub export fn foo()` | Yes           | Yes                | Yes               |

All entity definitions can have `pub` visibility, including struct fields.

### Module Source Types

| Source Type   | Syntax                        | Example                              |
| ------------- | ----------------------------- | ------------------------------------ |
| WASI standard | `"wasi:<package>"`            | `"wasi:cli"`, `"wasi:filesystem"`    |
| Core library  | `"core:<module>"`             | `"core:cli"`, `"core:fmt"`           |
| Remote (HTTP) | `"https://..."`               | `"https://example.com/lib.wado"`     |
| Local file    | `"./<path>"` or `"../<path>"` | `"./utils.wado"`, `"../config.wado"` |
| Package       | `"<package-name>"`            | `"parser-lib"`, `"json-utils"`       |

### Module Path Validation

Module paths are validated before loading to provide clear error messages:

**Namespace Resolution:**

1. **Reserved namespaces** (`identifier:`): Paths matching `xxx:` pattern are namespace paths
   - `core:` - Wado standard library
   - `wasi:` - WASI interface modules
   - Unknown namespaces result in compile error: `unknown module namespace 'xxx'; expected 'core' or 'wasi'`

2. **Remote modules** (`http://` or `https://`): Delegated to CompilerHost

3. **Local modules** (`./` or `../`): Resolved relative to importing module

4. **Invalid paths**: Paths not matching any pattern are rejected
   - Error: `invalid module path 'xxx'; use './' for local modules or 'namespace:' for library modules`

### Import Syntax

```wado
// ============================================
// WIT Package = Wado Module
// WIT Interface = Wado Effect
// ============================================

// 1. WASI standard modules (wasi:*)
use {Stdout, Stderr} from "wasi:cli";
use {Stdout::{write_via_stream}} from "wasi:cli";

// Effect and its functions together
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";

// 2. Core library (core:*)
use {println, eprintln} from "core:cli";
use {format} from "core:fmt";

// 3. Remote modules (https:)
use {ApiClient} from "https://example.com/api.wado";
use config from "https://example.com/data.json" with { type: "json" };

// 4. Local files (relative path, extension required)
use {Helper} from "./utils.wado";
use {Config} from "../config.wado";

// 5. Package dependencies (name only)
use {Parser} from "parser-lib";
```

### Import Attributes (`with`)

Use `with { ... }` to specify import metadata:

```wado
// Version specification (optional for standard namespaces)
use {Stdout} from "wasi:cli" with { version: "0.3.0" };

// Type attribute (REQUIRED for non-.wado imports)
use config from "./config.json" with { type: "json" };
use {sin, cos} from "./libm.wasm" with { type: "wasm" };

// WIT specification for Wasm imports (optional)
use {foo} from "./external.wasm" with {
    type: "wasm",
    wit: "./external.wit",
};
```

**Type Attribute Requirement**:

| Import Source        | `type` Attribute | Notes                          |
| -------------------- | ---------------- | ------------------------------ |
| `.wado` files        | Optional         | Type inferred from Wado source |
| `.wasm` files        | **Required**     | `type: "wasm"`                 |
| `.json` files        | **Required**     | `type: "json"`                 |
| `core:*`, `wasi:*`   | Not applicable   | Special namespace handling     |
| `https:` URLs        | **Required**     | Must specify content type      |
| Package dependencies | Optional         | Type inferred from package     |

**Rationale**: Explicit type annotations prevent ambiguity and make dependencies clear, aligning with Wado's design philosophy of explicit imports.

### Namespace Import

Use `use name from "..."` (without curly braces) to import an entire module as a namespace:

```wado
// Import JSON file as a namespace
use config from "./config.json" with { type: "json" };
let value = config::key; // not config["key"], as it's analyzed at compile time

// Import a module as a namespace
use utils from "./utils.wado";
utils::helper_function();
```

**Note:** Wado does not support `use * as name` or default imports.

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
use println from "core:cli";     // Named items require curly braces
```

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
use {write_via_stream as stdout_write} from "wasi:cli/Stdout";
use {write_via_stream as stderr_write} from "wasi:cli/Stderr";

fn log() with Stdout, Stderr {
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

// user code - import from the facade
use {sin, cos, sine} from "math";
```

Re-export rules:

- `pub use` combines `pub` visibility with import syntax
- Can only re-export `pub` symbols from the source module
- Re-export chains are resolved transparently (A re-exports from B, B re-exports from C)
- Circular re-exports are prohibited
- Wildcards prohibited: `pub use * from "..."` is not allowed

### Exception: The Prelude

The prelude is automatically imported into every module, making `Option`, `Result`, `Stream`, `Future`, and `Pollable` available without explicit imports.

### Standard Library

```
core            # core: namespace for the core library
├── prelude     # Automatically imported (Option, Result, Stream, Future, Pollable)
├── cli         # CLI helpers (println, eprintln, args, env, exit, ...)
├── ...
wasi            # wasi: namespace for system interfaces
├── cli
├── filesystem
├── ...
```

For full API documentation, see:

- [Core Standard Library Reference](./stdlib-core.md)
- [WASI Standard Library Reference](./stdlib-wasi.md)

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

## Testing

Wado has built-in support for writing and running tests. Test declarations are first-class syntax, and the `wado test` command provides a test runner similar to `cargo test` or `moon test`.

### Test Declaration Syntax

Tests are declared using the `test` keyword followed by an optional name and a block:

```wado
// Named test
test "addition works" {
    assert 1 + 1 == 2;
}

// Unnamed test (identified by file:line)
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

// TODO test: passes while the feature is unimplemented (body traps).
// When it stops trapping, the runner warns to remove the attribute.
#[TODO]
test "not yet implemented" {
    panic("TODO: implement this feature");
}
```

**Syntax Rules:**

- `test` is a contextual keyword (functions named `test` are still allowed)
- Test name is an optional string literal
- Test body is a block containing statements
- No return type or effect declarations needed
- Tests can use any effects (side effects are allowed in tests)
- Attributes (e.g., `#[expect_trap]`, `#[TODO]`) may appear before the `test` keyword

**Test Identification:**

- Named tests: identified by their string name
- Unnamed tests: identified by `{filename}:{line_number}`

### Test Semantics

**Execution:**

- Each test runs in isolation with fresh state
- Test order is deterministic (declaration order within a file)
- A test passes if it completes without panicking or trapping
- A test fails if `assert` fails, `panic` is called, or a trap occurs

**`#[expect_trap]` Attribute:**

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

**`#[TODO]` Attribute:**

The `#[TODO]` attribute marks a test as a placeholder for a feature not yet implemented. It has the same trap-expectation semantics as `#[expect_trap]`, but the runner emits a distinct failure message when the body unexpectedly passes, reminding the developer to remove the attribute.

**Effects:**

- Tests implicitly have access to all effects (no `with` declaration required)
- This allows tests to perform I/O, use the filesystem, etc.

**No Runtime Overhead:**

- Test functions are only included when running `wado test`
- Regular compilation (`wado compile`, `wado run`) excludes test code via dead code elimination

### Test Runner CLI

The `wado test` command discovers and runs tests:

```sh
# Auto-discover and run all **/*_test.wado files recursively
wado test

# Run tests in specific file(s)
wado test path/to/file.wado
wado test path # find path/**/*_test.wado

# Filter tests by name pattern
wado test --filter "addition"
wado test -f "string"

# Show help
wado test --help
```

**Discovery:**

When no files are specified, `wado test` searches for `**/*_test.wado` files recursively from the current directory.

**Output:**

```
Running tests in math_test.wado...
  ✓ addition works
  ✓ subtraction works
  ✗ division edge case
    assertion failed at line 15

Running tests in string_test.wado...
  ✓ concatenation

3 passed, 1 failed
```

**Exit Codes:**

- `0`: All tests passed
- `1`: One or more tests failed

### Test File Conventions

By convention, test files are named with a `_test.wado` suffix:

```
src/
  math.wado
  math_test.wado      # Tests for math.wado
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

### Implementation Notes

**Component Model Export:**

Test functions are exported at the Component Model level with kebab-case names:

- `test "simple addition"` → exported as `test-0-simple-addition`
- `test { ... }` (unnamed, line 10) → exported as `test-1`

The numeric prefix preserves declaration order for deterministic execution.

**Async Support:**

Test functions use the same async wrapper as `run()`, ensuring compatibility with WASI P3's async model. Each test properly completes its async task before reporting results.

## Reactive System

Wado has built-in reactive signals (called "signals" in other frameworks like SolidJS, Svelte 5). The compiler analyzes dependencies at compile-time and generates efficient update code.

### The `reactive` Keyword

**Source** values are mutable reactive state:

```wado
let reactive mut count = 0;

count = 5;          // Mutation triggers updates
count += 1;         // Also triggers updates
```

**Derived** values are computed from other reactive values:

```wado
let reactive doubled = || count * 2;
let reactive quadrupled = || doubled * 2;

// Reading derived values
let x = doubled;    // Returns current computed value
```

Derived values are recomputed when their dependencies change. The compiler builds a dependency graph and updates values in topological order.

### The `observe` Function

The `observe` function (from `core:reactive`) executes side effects when reactive dependencies change. Dependencies are automatically tracked—any reactive value read within the closure becomes a dependency.

```wado
use {observe} from "core:reactive";

let reactive mut count = 0;
let reactive doubled = || count * 2;

observe(|| {
    println(`Count is now: {count}`);
    // Dependencies: count
});

observe(|| {
    println(`Doubled is now: {doubled}`);
    // Dependencies: doubled (and transitively, count)
});
```

**Cleanup:**

Return a cleanup function to run when the observation is disposed or before re-running:

```wado
observe(|| {
    let subscription = external_api.subscribe(`event-{count}`);
    println(`Subscribed to event-{count}`);

    return || {
        subscription.unsubscribe();
        println(`Cleaned up subscription for event-{count}`);
    };
});
```

The cleanup function runs:

- Before the effect re-runs (when dependencies change)
- When the enclosing scope ends
- When the component unmounts (in UI contexts)

**Manual disposal:**

```wado
let dispose = observe(|| {
    println(`Count: {count}`);
});

// Later, stop observing
dispose();
```

### Reactive References

Reactive values can be passed by reference to functions:

```wado
fn increment(counter: &reactive mut i32) {
    *counter += 1;  // Triggers updates in caller's scope
}

let reactive mut count = 0;
let reactive doubled = || count * 2;

increment(&reactive count);  // count becomes 1, doubled becomes 2
```

### Execution Semantics

Reactive behavior differs between execution contexts:

#### CLI World (Synchronous)

In CLI programs, reactive updates are **synchronous and immediate**:

```wado
use {observe} from "core:reactive";

let reactive mut count = 0;
let reactive doubled = || count * 2;

observe(|| {
    println(`doubled = {doubled}`);
});

count = 5;
// observe() callback runs immediately here, before next line
// Output: "doubled = 10"

println("after mutation");
// Output: "after mutation"
```

- Updates propagate immediately when a source is mutated
- observe() callbacks run synchronously before execution continues
- Observations live for the duration of their enclosing scope

#### Event-Looped World (Browser/GUI)

In event-driven contexts, reactive updates are triggered by **external events**:

```wado
use {observe} from "core:reactive";

fn Counter() -> Element with Dom {
    let reactive mut count = 0;
    let reactive doubled = || count * 2;

    observe(|| {
        println(`Count changed to {count}`);
    });

    return <div>
        <p>{doubled}</p>
        <button onclick={|_| count += 1}>+1</button>
    </div>;
}
```

- Updates are triggered by events (clicks, timers, network responses)
- Multiple mutations within a single event handler may be **batched**
- observe() callbacks and UI bindings persist for the component's lifetime
- The event loop keeps the program alive to receive future events

#### Comparison

| Aspect             | CLI                       | Event-looped                    |
| ------------------ | ------------------------- | ------------------------------- |
| Trigger            | Direct assignment in code | External events                 |
| Propagation        | Synchronous, immediate    | May be batched per event        |
| observe() lifetime | Enclosing scope duration  | Component/subscription lifetime |
| Primary use case   | Computed dependencies     | UI binding, subscriptions       |

### JSX Integration

Reactive values integrate seamlessly with JSX:

```wado
fn Counter() -> Element with Dom {
    let reactive mut count = 0;

    return <button onclick={|_| count += 1}>
        {count}
    </button>;
}
```

The compiler tracks that `{count}` depends on the reactive value and generates code to update only that text node when `count` changes—no virtual DOM diffing required.

`Reactive` is built into the language; no `with` declaration required.

---

## Concurrency Model

### Stack Switching Based (Colorless)

```wado
// No async keyword needed in function implementations
fn fetch_user(id: i32) -> Result<User, HttpError> with Http {
    let response = Http::get(`users/{id}`)?;  // Even if Http::get is async in WIT
    let user = response.json()?;
    return Ok(user);
}

// Called normally
fn main() with Http {
    let user = fetch_user(1);
}

// Concurrent execution
fn load_data() -> Data with Http {
    let [users, posts] = join(
        || fetch_users(),
        || fetch_posts(),
    );
    return Data { users, posts };
}
```

Note: Wado is fully colorless — the `async` keyword only appears in world declarations (the Component Model surface) to match WIT's `async func` signatures for exports. Effect declarations and function implementations never use `async`.

## Effect System

### Design Philosophy

The Effect System is equivalent to:

- Tracking access to external resources / global variables
- Implicitly propagating DI (Dependency Injection)
- Direct correspondence with WASI Capabilities

### Effect Definition

Effects can be defined in two ways:

**1. Effect interfaces** (declaring operations as free functions):

```wado
// WASI CLI effects (see wasi:cli for real definitions)
effect Stdout {
    fn write_via_stream(data: Stream<u8>) -> Result<(), ErrorCode>;
}

effect Stderr {
    fn write_via_stream(data: Stream<u8>) -> Result<(), ErrorCode>;
}

effect Environment {
    fn get_environment() -> Array<[String, String]>;
    fn get_arguments() -> Array<String>;
    fn get_initial_cwd() -> Option<String>;
}

// Custom effect interfaces
effect Http {
    fn get(url: String) -> Response;
    fn post(url: String, body: String) -> Response;
}

effect Dom {
    fn query(selector: String) -> Option<Element>;
    fn create_element(tag: String) -> Element;
}
```

**Colorless Async:**

- Effect declarations never use the `async` keyword—Wado is fully colorless
- The `async` keyword only appears in world export declarations (the Component Model surface)
- WIT's `async func` is handled transparently via Wasm Stack Switching at runtime

**2. Methods with effect requirements**:

```wado
// Methods can declare required effects
impl TcpStream {
    fn read(&mut self, buffer: &mut Array<u8>) -> Result<i32, IoError> with Network;
    fn write(&mut self, data: &Array<u8>) -> Result<i32, IoError> with Network;
    fn close(&mut self) with Network;
}

impl TcpListener {
    fn accept(&self) -> Result<TcpStream, IoError> with Network;
}

// Free functions can also require effects
fn listen(addr: String) -> Result<TcpListener, IoError> with Network;
```

This approach makes effect requirements explicit and visible in method signatures, maintaining consistency with the language's design philosophy of being clear and explicit.

### Effect Declaration in Functions

```wado
// Declare required effects with `with`
fn greet(name: String) with Stdout {
    Stdout::write_via_stream(to_stream(`Hello, {name}!\n`));
}

// Multiple effects
fn show_env() with Stdout, Environment {
    let args = Environment::get_arguments();
    Stdout::write_via_stream(to_stream(`Arguments: {args}\n`));
}

// No effects = pure function
fn add(a: i32, b: i32) -> i32 {
    return a + b;
}
```

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

**Import Rules:**

- Effect operations use `::` syntax: `use {Effect::{op1, op2}} from "..."`
- Multiple operations can be imported: `Effect::{op1, op2, op3}`
- Renaming is supported: `use {op as renamed} from "..."`
- Wildcards are prohibited: `use {Effect::{*}}` is not allowed
- The `with` declaration is still required for effect tracking

**Name Resolution:**

- Imported effect operations can be called directly without the `Effect::` prefix
- If an operation name is ambiguous, use the fully qualified `Effect::operation()` syntax
- Non-imported effect operations must always use the `Effect::operation()` syntax

```wado
// Example with name collision handling
use {Stdout::{write_via_stream}} from "wasi:cli";
use {Stderr::{write_via_stream as stderr_write}} from "wasi:cli";

pub fn log(message: String) with Stdout, Stderr {
    write_via_stream(...);  // Calls Stdout::write_via_stream
    stderr_write(...);      // Calls Stderr::write_via_stream (renamed)
}
```

### Effect Propagation

- Local functions: Inferred
- pub functions: Must be explicit

```wado
// Local functions are inferred
fn internal() {
    callee();  // Automatically inherits callee's effects
}

// Public functions must be explicit
pub fn api_function() with Http, FileSystem {
    // ...
}
```

### Reference Storage (`stores[...]`)

> **Not yet implemented.** See [`docs/wep-2026-01-12-value-semantics-and-stores.md`](./wep-2026-01-12-value-semantics-and-stores.md) for the design.

The `stores[...]` keyword declares that a function stores reference parameters beyond the function call. This enables compile-time escape analysis and automatic heap promotion.

**Syntax**: `with stores[param1, param2, ...]`

```wado
// Function that stores a reference parameter
fn register(data: &Data) -> Handle with stores[data] {
    registry.append(data);  // Stores the reference
    return new_handle();
}

// Function that does NOT store (no stores declaration)
fn process(data: &Data) -> Result {
    return compute(*data);  // Uses but doesn't store
}

// Combined with effects
fn store_and_log(data: &Data) -> Handle with Stdout, stores[data] {
    println("Storing data...");
    return register(data);
}
```

**Naming Rationale**: The keyword is `stores` (not `captures`) because:

- "Capture" is established terminology for closures (`let f = || x + 1` captures `x`)
- `stores` describes what the function _does_ with the reference—it stores it for later use
- This avoids conflating two different concepts: closures capturing variables vs functions storing parameters

**Functor types** can also declare stores (positional: 0 = first parameter):

```wado
fn take_storing(f: fn(&Data) with stores[0]) { ... }
fn take_pure(f: fn(&Data) -> Result) { ... }  // cannot store
```

See `docs/wep-2026-01-12-value-semantics-and-stores.md` for detailed design rationale.

### Handlers

> **TBD**: The handler system is in an early design stage. The syntax and semantics below are provisional and subject to change.

Handlers provide implementations for effect operations, enabling dependency injection and testing.

#### Built-in Handlers

```wado
use {WasiStdout, WasiStderr, WasiEnvironment, BrowserDom} from "core:handlers";

fn main() {
    with Stdout => WasiStdout, Stderr => WasiStderr, Environment => WasiEnvironment {
        app();
    }
}
```

#### Inline Handler

```wado
with handler Stdout {
    write_via_stream(data) => actual_write(data),
} {
    greet("Alice");
}
```

#### Named Handler

```wado
handler MockStdout for Stdout {
    let mut output: Array<u8> = [];

    write_via_stream(data) => {
        output.extend(collect_stream(data));
        return Ok(());
    },
}

// Usage
fn test() {
    with Stdout => MockStdout {
        greet("Bob");
    }
}
```

#### Continuation Control

```wado
effect Generator<T> {
    fn yield(value: T);
}

fn range(start: i32, end: i32) with Generator<i32> {
    let mut i = start;
    while i < end {
        Generator::yield(i);
        i += 1;
    }
}

fn collect_all() -> Array<i32> {
    let mut result: Array<i32> = [];

    with handler Generator<i32> {
        yield(value) => |resume| {
            result.append(value);
            resume();
        },
    } {
        range(0, 5);
    }

    return result;  // [0, 1, 2, 3, 4]
}
```

#### Composing Multiple Handlers

```wado
fn main() {
    with Stdout => WasiStdout, Stderr => WasiStderr, Http => WasiHttp {
        app();
    }
}
```

## World System

### What is a World?

A **world** in Wado corresponds directly to the Component Model's `world` concept. A world defines the contract between a Wasm component and its environment:

1. **Imports**: Which capabilities the component requires (provided by the host or by other components)
2. **Exports**: Which functions and types the component provides

Worlds are classified into two categories:

- **Hosted world**: A world that a runtime knows how to instantiate and drive. The runtime provides all imports and invokes the exports according to a defined lifecycle. Examples: `wasi:cli/command` (executed by `wado run`), `wasi:http/service` (executed by `wado serve`). Informally called a "well-known world."
- **Library world**: A world that defines a component's public API for composition. It is not directly executed by a runtime; instead, other components import its exports. Example: a `json` library that exports parsing functions.

This distinction is not part of the Component Model specification — the CM treats all worlds uniformly. In Wado, the distinction matters for tooling: `wado run` and `wado serve` select a hosted world, while `wado.toml`'s `lib` field defines a library world.

### World Declaration

```wado
world WorldName {
    import EffectName {
        function_name_1,
        function_name_2,
    }

    import AnotherEffect {
        function_name_3,
    }

    // Use async for exports that map to WIT's "async func" (CM surface only)
    export async fn exported_function(arg: Type) -> ReturnType;
    export fn synchronous_function() -> i32;
}
```

> **TBD: Component/Module Structure**
> The relationship between files, modules, and components is still under discussion. The intended design is "1 file = 1 module, 1 component = multiple modules", but the exact syntax for declaring which modules compose a component has not been finalized.

Note: The `async` keyword only appears in world export declarations to indicate correspondence with WIT's `async func`. This is the only place `async` appears in Wado—effect declarations and function implementations are fully colorless.

### WASI CLI World Example

The standard WASI CLI `command` world in Wado syntax:

```wado
// Based on wasi:cli@0.3.0-rc-2025-09-16 command world
// Effect definitions are in "core:cli" (see cli.wado)

world Command {
    // Standard I/O streams
    import Stdout {
        write_via_stream,
    }

    import Stderr {
        write_via_stream,
    }

    import Stdin {
        read_via_stream,
    }

    // Environment access
    import Environment {
        get_arguments,
        get_environment,
        get_initial_cwd,
    }

    // Process control
    import Exit {
        exit,
        exit_with_code,
    }

    // Terminal interaction (optional)
    import TerminalStdin {
        get_terminal_stdin,
    }

    import TerminalStdout {
        get_terminal_stdout,
    }

    import TerminalStderr {
        get_terminal_stderr,
    }

    // Entry point: maps to WIT's "run: async func() -> result"
    // async keyword only appears here (world export) - the CM surface
    export async fn run() -> Result<(), ()>;
}

// Declare conformance to the Command hosted world
contract Command;

// Implementation — `export` exposes it at the CM boundary
export fn run() with Stdout {
    println("Hello, WASI world!");
}
```

### Multiple Worlds

A single codebase can define multiple worlds for different deployment targets:

```wado
world BrowserApp {
    import Dom {
        query_selector,
        create_element,
    }

    export fn mount(root: String);
}

world CliApp {
    import Stdout {
        write_via_stream,
    }

    export fn run() -> Result<(), ()>;
}

// Declare conformance — select world at compile time
contract CliApp;  // or: contract BrowserApp;
```

### Design Notes

- **Explicit function listing**: Unlike WIT's `include` directive, Wado requires listing each imported function explicitly for clarity
- **Effect-based imports**: Imports are organized by effect, which maps to WIT interfaces
- **Type signatures on exports**: Export declarations include full function signatures
- **async keyword**: Only appears in world export declarations (the Component Model surface); effect declarations and implementations are fully colorless
- **Versioning**: Version information (`@0.3.0-rc-2025-09-16`) is specified in the effect definitions (e.g., `cli.wado`), not in the world declaration

## Error Handling

### Unrecoverable Errors (Wasm Exceptions)

```wado
panic("Fatal error");      // Immediate termination
unreachable();             // Unreachable code
assert condition;          // Condition check, panic on failure
```

These cannot be caught in Wado; the program terminates.

### Recoverable Errors (Result Type)

```wado
fn parse_int(s: String) -> Result<i32, ParseError> {
    // ...
}

fn read_config(path: String) -> Result<Config, ConfigError> with FileSystem {
    let content = FileSystem::read(path)
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

## JSX

JSX is built into the language:

```wado
fn App() -> Element with Dom {
    let reactive mut count = 0;

    return <div class="container">
        <h1>Counter</h1>
        <p>Count: {count}</p>
        <button onclick={|_| count += 1}>
            Increment
        </button>
    </div>;
}

// Conditional rendering
<div>
    {match status {
        "loading" => <Spinner />,
        "success" => <Content data={data} />,
        "error" => <Error message={error} />,
    }}
</div>

// Lists
<ul>
    {items.map(|item| <li key={item.id}>{item.name}</li>)}
</ul>
```

## WASI / Browser Support

Wado targets **WASI Preview 3** (0.3.0-rc-2025-09-16), which introduces native `stream<T>` and `future<T>` types that map directly to Wado's `Stream<T>` and `Future<T>`.

All Wado types map directly to Component Model (WIT) types. See the [Type Mapping at Component Boundaries](#type-mapping-at-component-boundaries) table in the Type System section for the complete mapping reference.

### WASI P3 CLI Interfaces

Wado effects map to WASI P3 interfaces:

| Wado Effect   | WASI Interface         | Key Functions                                         |
| ------------- | ---------------------- | ----------------------------------------------------- |
| `Stdout`      | `wasi:cli/stdout`      | `write-via-stream(stream<u8>)`                        |
| `Stderr`      | `wasi:cli/stderr`      | `write-via-stream(stream<u8>)`                        |
| `Stdin`       | `wasi:cli/stdin`       | `read-via-stream() -> tuple<stream<u8>, future<...>>` |
| `Environment` | `wasi:cli/environment` | `get-arguments()`, `get-environment()`                |
| `Exit`        | `wasi:cli/exit`        | `exit(result)`, `exit-with-code(u8)`                  |

### Entry Points

Entry points are integrated in World system.

Each hosted world defines its entry point. Currently supported:

| Hosted World        | Entry Point                                                               | CLI Command  |
| ------------------- | ------------------------------------------------------------------------- | ------------ |
| `wasi:cli/command`  | `export fn run()`                                                         | `wado run`   |
| `wasi:http/service` | `export async fn handle(request: Request) -> Result<Response, ErrorCode>` | `wado serve` |

When no explicit `contract` declaration is present, the runtime determines the expected world (e.g., `wado run` expects `wasi:cli/command`).

### `task return` Statement

`task return expr;` is a statement valid only inside `export async fn` bodies. It calls the Component Model `task.return` instruction, delivering the function's result to the CM runtime **without terminating the Wasm function**. Execution continues after `task return`, allowing the function to fulfill outstanding futures (e.g. trailers) or perform cleanup.

#### Motivation

HTTP handlers return a `Response` that contains a `Future`-based trailers channel. With a regular `return`, the Wasm function exits immediately, making it impossible to write to that channel. `task return` separates result delivery from function termination:

```wado
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let [trailers_future, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
    let headers = Fields::new();
    let [response, _tx_future] = Response::new(headers, null, trailers_future);

    task return Result::<Response, ErrorCode>::Ok(response); // deliver result; function continues
    trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null)); // fulfill trailers
}
```

#### Rules

- `task return` is only valid inside `export async fn` bodies.
- Regular `return` is forbidden in `async fn` bodies — it would exit the Wasm function without notifying the CM runtime.
- The `task return` expression is type-checked against the declared return type of the enclosing `export async fn`.
- The `async` keyword on a function declaration has no effect on callers; Wado is fully colorless. `async` only appears in `export async fn` declarations to signal CM async calling convention at the component boundary.

### Attribute Syntax for WASI Linking

Use `#[wasi(...)]` attributes to link Wado definitions to WASI interfaces:

```wado
// Link an effect interface to a WASI interface
#[wasi("wasi:cli/stdout@0.3.0-rc-2025-09-16")]
pub effect Stdout {
    #[wasi("wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream")]
    fn write_via_stream(data: Stream<u8>) -> Result<(), ErrorCode>;
}

// Link a resource to a WASI resource
#[wasi("wasi:cli/terminal-output@0.3.0-rc-2025-09-16")]
resource TerminalOutput;

// Link an enum to a WASI enum
pub enum ErrorCode {  // Maps to WIT: enum error-code
    Io,               // Maps to WIT: io
    IllegalByteSequence,  // Maps to WIT: illegal-byte-sequence
    Pipe,             // Maps to WIT: pipe
}
```

## Appendix

### Naming Conventions

| Element            | Style            |
| ------------------ | ---------------- |
| Project name       | `kebab-case`     |
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
- project: a collection of modules
- Wado standard library: consists of `core:` and `wasi:`
- effect: the concept; e.g., "the `Stdout` effect"
- effect interface: the declaration (`effect Stdout { ... }`); synonyms in literature: "effect signature", "effect type"
- operation: a function in an effect interface; synonym: "effect operation"
- handler: provides implementations for operations
- hosted world: a world that a runtime knows how to instantiate and drive (e.g., `wasi:cli/command` for `wado run`, `wasi:http/service` for `wado serve`); informally called "well-known world"
- library world: a world that defines a component's public API for composition with other components, rather than for direct execution by a runtime
