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

**Future: `#[data]` Attribute (TODO):**

A future `#[data]` attribute will allow injecting the data section content into code:

```wado
#[data]
const TEST_DATA: String;  // Injected from __DATA__ section

#[data("json")]
const CONFIG: Config;     // Parsed as JSON from __DATA__ section
```

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

See `docs/adr-2026-01-11-operator-precedence.md` for detailed rationale.

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

### While Loop

```wado
let mut i = 0;
while i < 10 {
    println(`i = {i}`);
    i = i + 1;
}
```

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

### For-Of Loop

For iterating over arrays:

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
```

**Note:** For-of loops are for arrays only, not tuples. The binding is a copy of each element, so modifying it does not affect the original array.

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

## Memory Model

### Core Principles

- **Wasm-GC based**: Garbage collection delegated to runtime
- **Lifetime inference**: No explicit lifetime annotations required
- **Explicit move**: Ownership transfer only when explicitly stated

### Move Syntax

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

### Unique Ownership

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
| `Dict<K, V>`              | GC struct (hash table)       | `list<tuple<K, V>>`                  | Hash map internally, list at boundary            |
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
- `Dict<K, V>` - Dictionary type
- `Tuple<T1, T2, ...>` - Alias for `[T1, T2, ...]`
- `Reactive<T>` - Reactive value
- `Option<T>` and its variants: `Some(x)`, `None` (also accessible via `null` keyword)
- `Result<T, E>` and its variants: `Ok(x)`, `Err(e)`
- `Stream<T>` - Component Model async stream
- `Future<T>` - Component Model async future
- `Pollable` - WASI I/O polling resource

**Disabling the Prelude:**

```wado
#![no_prelude]  // At the top of a module

// Now you must explicitly import everything
use {String, Array, Dict, Tuple, Reactive, Option, Result, Stream, Future, Pollable} from "core:prelude";
```

### Primitive Types

Wasm primitive types are built into the language (no import required):

```wado
// Numeric
i8, i16, i32, i64, i128
u8, u16, u32, u64, u128
f32, f64

// Basic
bool
char
```

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
    len: usize,          // Length in bytes
    capacity: usize,     // Buffer capacity for += operations
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
s.len() -> usize           // Length in bytes
s.is_empty() -> bool       // Check if empty
```

**Note**: `bytes()` and `chars()` currently return `Array<T>` (copy). Future versions will support slices and iterators for zero-copy access.

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
for item in items {
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
for item in items {
    s += item;
}

// 2. Use += for repeated concatenation
let mut result = String::new();
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

See `docs/adr-2026-01-15-string-type-design.md` for design rationale.

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
assert(null == None);
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

#### Integer Literals

```wado
let decimal = 42;
let negative = -17;
let with_separator = 1_000_000;    // Underscores for readability
let binary = 0b1010_1100;          // Binary
let octal = 0o755;                 // Octal
let hex = 0xFF_AA_BB;              // Hexadecimal
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

// With formatting
let pi = 3.14159;
let formatted = `Pi: {pi:0.2f}`;  // "Pi: 3.14"
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

See `docs/adr-2026-01-15-tuple-and-array-literals.md` for detailed rationale.

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

Tagged template literals provide a general mechanism for compile-time computation, avoiding the need for built-in syntax for each use case. This aligns with Wado's philosophy of minimal built-ins and explicit dependencies. See `docs/adr-2026-01-10-tagged-template-literals.md` for detailed design decisions.

**Future Extensions:**

Interpolation support may be added in future versions:

```wado
// Future: interpolation syntax (not yet implemented)
let id = 42;
let query = sql`SELECT * FROM users WHERE id = ${id}`;
```

### Type Alias

`type T = U` creates an alias where `T` and `U` are identical types—completely interchangeable in type checking.

```wado
type Kilometers = i32;
type UserID = String;

let km: Kilometers = 100;
let m: i32 = km;  // OK - same type
```

For distinct types that should not be interchangeable, use a struct wrapper (newtype pattern):

```wado
struct Miles { value: i32 }

let miles = Miles { value: 50 };
let m: i32 = miles.value;  // explicit access required
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

### Enums, Variants, and Flags

Wado follows Component Model's distinction between enums and variants (unlike Rust):

**Enums** (no payloads - Component Model `enum`):

```wado
// Simple enumeration - all variants have no data
enum Color {
    Red,
    Green,
    Blue,
}

// Used as:
let c = Color::Red;
```

**Variants** (with payloads - Component Model `variant`):

```wado
// Sum type where variants can carry data
variant Shape {
    Circle(f64),           // radius
    Rectangle(f64, f64),   // width, height
    Point,                 // no payload
}

// Used as:
let s = Shape::Circle(5.0);

match s {
    Shape::Circle(r) => calculate_circle_area(r),
    Shape::Rectangle(w, h) => w * h,
    Shape::Point => 0.0,
}
```

**Flags** (bit flags - Component Model `flags`):

```wado
// Bit flags - can be combined with | operator
flags Permissions {
    Read,
    Write,
    Execute,
}

// Used as:
let perms = Permissions::Read | Permissions::Write;

if perms.contains(Permissions::Read) {
    // ...
}

// Empty flags
let empty_perms = Permissions::none();

// All flags
let all_perms = Permissions::all();
```

Note: Wado's `enum` maps to Component Model's `enum` (simple enumeration), and `variant` maps to Component Model's `variant` (tagged union with payloads). This differs from Rust where `enum` can have payloads.

---

## Object Literals

Object literal syntax supports unquoted keys and shorthand properties.

### Syntax Rules

- Identifier keys: Quotes optional
- Non-identifier keys: Quotes required
- Computed key: `[expr]` syntax (dict only)
- Shorthand: Can omit when variable name matches key

### Struct Initialization

```wado
// Named struct literal
let user = User { name: "Alice", age: 30, active: true };

// Implicit struct literal (requires type annotation)
let user: User = { name: "Alice", age: 30, active: true };

// Shorthand
let name = "Bob";
let age = 25;
let bob: User = { name, age, active: false };

// Computed keys not allowed in structs
```

### Dictionaries

```wado
// String keys
let d: Dict<String, i32> = { x: 10, y: 20 };

// Computed key
let key = "dynamic";
let d: Dict<String, i32> = {
    static_key: 1,
    [key]: 2,
    [get_key()]: 3,
};

// Non-String keys
let nums: Dict<i32, String> = {
    [1]: "one",
    [2]: "two",
};
```

### Access Methods

```wado
// Struct: dot notation
user.name

// Dict: bracket notation
d["key"]
```

## Module System

Wado uses an ESM-like import syntax with `use {...} from "source"`. This aligns with JavaScript/TypeScript conventions, as JavaScript is a primary host environment for Wado.

### Module Source Types

| Source Type   | Syntax                        | Example                              |
| ------------- | ----------------------------- | ------------------------------------ |
| WASI standard | `"wasi:<package>"`            | `"wasi:cli"`, `"wasi:filesystem"`    |
| Core library  | `"core:<module>"`             | `"core:cli"`, `"core:fmt"`           |
| Remote (HTTP) | `"https://..."`               | `"https://example.com/lib.wado"`     |
| Local file    | `"./<path>"` or `"../<path>"` | `"./utils.wado"`, `"../config.wado"` |
| Package       | `"<package-name>"`            | `"parser-lib"`, `"json-utils"`       |

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

## Reactive System

Wado has built-in reactive signals (called "signals" in other frameworks like SolidJS, Svelte 5). The compiler analyzes dependencies at compile-time and generates efficient update code.

### reactive Keyword

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

### observe Function

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
    for [key, value] in vars {
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

The `stores[...]` keyword declares that a function stores reference parameters beyond the function call. This enables compile-time escape analysis and automatic heap promotion.

**Syntax**: `with stores[param1, param2, ...]`

```wado
// Function that stores a reference parameter
fn register(data: &Data) -> Handle with stores[data] {
    registry.push(data);  // Stores the reference
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
fn take_storing(f: Fn(&Data) with stores[0]) { ... }
fn take_pure(f: Fn(&Data) -> Result) { ... }  // cannot store
```

See `docs/adr-2026-01-12-value-semantics-and-stores.md` for detailed design rationale.

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
            result.push(value);
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

A **world** in Wado corresponds directly to the Component Model's `world` concept. A world defines:

1. **Imports**: Which effects and their functions the component requires from the host
2. **Exports**: Which functions the component provides to the host

Worlds are the contract between a Wasm component and its runtime environment.

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

// Declare this component implements the CLI command world
#![world(CliCommand)]

// Implementation
pub fn run() -> Result<(), ()> {
    println("Hello, WASI world!");
    return Ok(());
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

// Select world at compile time
#![world(CliApp)]  // or BrowserApp
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

All Wado types map directly to Component Model (WIT) types. See the [Component Model Mapping](#component-model-mapping) table in the Type System section for the complete mapping reference.

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

Currently, `run` is the only entry point, which confirms the `Command` world defined in wasi:cli.

TBD.

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
