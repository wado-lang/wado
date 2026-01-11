# Wado Language Specification

Wado is a new programming language targeting Wasm/WASI -- Wasm in plain sight.

## Overview

| Item      | Description             |
| --------- | ----------------------- |
| Name      | Wado                    |
| Extension | `.wado`                 |
| Target    | Wasm/WASI only          |
| Paradigm  | Reactive, Effect System |

## Design Philosophy

- **Wasm only**: Zero abstraction to Wasm
- **Explicit imports**: All dependencies are explicit
- **Colorless async**: Eliminates async/await "color" problem via Wasm Stack Switching
- **Effect System**: Side effect tracking and control, swappable via Handlers

---

## Lexical Structure

### Whitespace

The lexer recognizes exactly four whitespace characters:

| Code Point | Name                 |
| ---------- | -------------------- |
| `\u0020`   | Space                |
| `\u000A`   | Line Feed (LF)       |
| `\u000D`   | Carriage Return (CR) |
| `\u0009`   | Tab                  |

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

Block comments do not nest. The lexer discards all comments.

### Identifiers

Identifiers match the pattern `[a-zA-Z_][a-zA-Z0-9_]*`:

```wado
foo
_private
camelCase
PascalCase
SCREAMING_CASE
name123
```

Identifiers are case-sensitive. Unicode letters are not permitted in identifiers.

---

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

### unique Modifier (Unique Ownership)

```wado
// Enforce unique ownership
let unique handle = open_file("data.txt");
let other = handle;       // Error: unique cannot be implicitly copied
let other = move handle;  // OK: explicit move
```

---

## Type System

### Component Model Mapping

All Wado types map directly to WebAssembly Component Model types:

| Wado Type                 | Component Model Type                 | Notes                                        |
| ------------------------- | ------------------------------------ | -------------------------------------------- |
| `bool`                    | `bool`                               | Boolean                                      |
| `char`                    | `char`                               | Unicode scalar value                         |
| `i8`, `i16`, `i32`, `i64` | `s8`, `s16`, `s32`, `s64`            | Signed integers                              |
| `u8`, `u16`, `u32`, `u64` | `u8`, `u16`, `u32`, `u64`            | Unsigned integers                            |
| `i128`, `u128`            | `tuple<s64, s64>`, `tuple<u64, u64>` | As tuple at CM boundary                      |
| `f32`, `f64`              | `f32`, `f64`                         | Floating point (32-bit, 64-bit)              |
| `f16`                     | -                                    | TODO: Wasm half-precision proposal (Phase 1) |
| `String`                  | `string`                             | UTF-8 string type                            |
| `Array<T>`                | `list<T>`                            | Dynamic array type                           |
| `Dict<K, V>`              | `list<tuple<K, V>>`                  | As list of tuples at CM boundary             |
| `Tuple<T1, T2, ...>`      | `tuple<T1, T2, ...>`                 | Tuple types                                  |
| `Option<T>`               | `option<T>`                          | Optional value                               |
| `Result<T, E>`            | `result<T, E>`                       | Result type                                  |
| `struct { ... }`          | `record { ... }`                     | GC struct in Wado, record at CM boundary     |
| `enum { ... }`            | `enum { ... }`                       | Enumeration without payloads                 |
| `variant { ... }`         | `variant { ... }`                    | Variant/sum type with payloads               |
| `flags { ... }`           | `flags { ... }`                      | Bit flags (maps to u8/u16/u32/u64)           |
| `resource`                | `resource`                           | Resource handle                              |
| `Stream<T>`               | `stream<T>`                          | Component Model async stream                 |
| `Future<T>`               | `future<T>`                          | Component Model async future                 |

**Type Naming Convention:**

- Wasm primitive types use lowercase: `bool`, `char`, `i32`, `f64`, etc.
- Standard library types use UpperCamelCase: `String`, `Array<T>`, `Option<T>`, `Result<T, E>`, etc.
- User-defined types follow UpperCamelCase convention

### The Prelude

The **prelude** (`core:prelude`) is automatically imported into every module, providing access to fundamental types without requiring explicit imports:

**Automatically Available:**

- `String` - UTF-8 string type
- `Array<T>` - Dynamic array type
- `Option<T>` and its variants: `Some(x)`, `None` (also accessible via `null` keyword)
- `Result<T, E>` and its variants: `Ok(x)`, `Err(e)`
- `Stream<T>` - Component Model async stream
- `Future<T>` - Component Model async future
- `Pollable` - WASI I/O polling resource

**Disabling the Prelude:**

```wado
#![no_prelude]  // At the top of a module

// Now you must explicitly import everything
use {String, Array, Option, Result, Stream} from "core:prelude";
```

### Built-in Types (No Import Required)

**Wasm primitive types** (lowercase):

```wado
// Numeric
i8, i16, i32, i64, i128
u8, u16, u32, u64, u128
f32, f64

// Basic
bool
char
```

**Standard library types** (UpperCamelCase, defined in `core/`):

```wado
String             // UTF-8 string type
Array<T>           // Dynamic array type
Dict<K, V>         // As list<tuple<K, V>> at CM boundary
Tuple<T1, T2, ...> // Component Model tuple<T1, T2, ...>
Reactive<T>        // Reactive value
```

**Prelude types** (from `core:prelude`, auto-imported):

```wado
Option<T>    // Some(x), None (also accessible via `null` keyword)
Result<T, E> // Ok(x), Err(e)
Stream<T>    // Component Model async stream
Future<T>    // Component Model async future
```

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

**Note:** `null` is a language keyword, while `None` is an identifier from the prelude (`Option::None`). They compile to the same value.

#### Character Literals

Character literals use single quotes and represent a Unicode scalar value. While internally represented as a 32-bit value (like `u32`), `char` is a distinct type with Unicode semantics—similar to how `String` differs from `Array<u8>`:

```wado
let letter = 'A';
let digit = '9';
let unicode = '\u0041';  // Unicode escape (same as 'A')
let emoji = '😀';        // Direct Unicode character
let newline = '\n';
```

**Escape sequences** (shared with string literals, except `\'` instead of `\"`):

| Escape   | Character                  |
| -------- | -------------------------- |
| `\'`     | Single quote               |
| `\\`     | Backslash                  |
| `\/`     | Forward slash              |
| `\b`     | Backspace                  |
| `\f`     | Form feed                  |
| `\n`     | Newline                    |
| `\r`     | Carriage return            |
| `\t`     | Tab                        |
| `\uHHHH` | Unicode BMP (4 hex digits) |
| `\u{H+}` | Unicode full range         |

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

**Type suffixes** (optional):

```wado
let byte: i8 = 127i8;
let long: i64 = 9_223_372_036_854_775_807i64;
let unsigned: u32 = 4_294_967_295u32;
```

#### Floating-Point Literals

```wado
let pi = 3.14159;
let with_separator = 1_000_000.5;
let scientific = 6.022e23;         // 6.022 × 10²³
let negative_exp = 1.6e-19;        // 1.6 × 10⁻¹⁹
let explicit_positive = 2.5e+10;
```

**Type suffixes** (optional):

```wado
let single: f32 = 3.14f32;
let double: f64 = 3.14159265358979f64;
```

#### String Literals

String literals create `String` values.

**Regular strings** use double quotes:

```wado
let name = "Alice";           // Type: String
let path = "path/to/file.txt";
let escaped = "Line 1\nLine 2\tTabbed";
```

**Escape sequences:**

| Escape   | Character                  |
| -------- | -------------------------- |
| `\"`     | Double quote               |
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

#### Array Literals

Array literals create `Array<T>` values.

```wado
let numbers = [1, 2, 3, 4, 5];  // Type: Array<i32>
let empty: Array<i32> = [];
let mixed = [1, 2, 3,];          // Trailing comma allowed
```

#### Tuple Literals

```wado
let pair = (1, "hello");
let triple: Tuple<i32, String, bool> = (42, "answer", true);
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

Tagged template literals provide a general mechanism for compile-time computation, avoiding the need for built-in syntax for each use case. This aligns with Wado's philosophy of minimal built-ins and explicit dependencies. See `docs/adr-2026-01-10-tagged-template-literals.md` for detailed design decisions.

**Future Extensions:**

Interpolation support may be added in future versions:

```wado
// Future: interpolation syntax (not yet implemented)
let id = 42;
let query = sql`SELECT * FROM users WHERE id = ${id}`;
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
let user: User = { name: "Alice", age: 30, active: true };

// With quoted keys
let user: User = { "name": "Alice", "age": 30, "active": true };

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

---

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
- Use `::` for effect function access: `Effect::{func1, func2}`

```wado
// Valid patterns
use {println, eprintln} from "core:cli";        // Named import
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";
use utils from "./utils.wado";                   // Namespace import

// Prohibited patterns
use * from "core:cli";           // Wildcard not allowed
use println from "core:cli";     // Named items require curly braces
```

### Calling Effect Functions

Effect functions use `::` syntax:

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
- `::` → effect functions and namespace access (`Stdout::write_via_stream()`)

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

The prelude is automatically imported into every module, making `Option`, `Result`, `Stream`, `Future`, and `Pollable` available without explicit imports. To opt out, use `#![no_prelude]`.

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
println("hello");
panic("error");
unreachable();
```

## The `assert` Keyword

The `assert` keyword is used to assert that a condition is true. If the condition is false, the program will panic with the power-assert style message.

```wado
// if x is not greater than 0, the program will panic, printing x (if x is a struct, the fields of the struct will be printed).
assert x > 0;
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
    let (users, posts) = join(
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

**1. Effect interfaces** (for free functions):

```wado
effect Console {
    fn print(msg: String);
    fn read_line() -> String;
}

effect Http {
    fn get(url: String) -> Response;
    fn post(url: String, body: String) -> Response;
}

effect FileSystem {
    fn read(path: String) -> Result<Array<u8>, IoError>;
    fn write(path: String, data: Array<u8>) -> Result<(), IoError>;
    fn exists(path: String) -> bool;
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
// Declare effects used with `with`
fn greet(name: String) with Console {
    Console::print(`Hello, {name}!`);
}

// Multiple effects
fn download_and_save(url: String, path: String) with Http, FileSystem {
    let data = Http::get(url).body;
    FileSystem::write(path, data);
}

// No effects = pure function
fn add(a: i32, b: i32) -> i32 {
    return a + b;
}
```

### Importing Effect Functions

To avoid the verbosity of `Effect::function()` calls, you can explicitly import effect functions:

```wado
// Import effect functions
use {Stdout::{write_via_stream}} from "wasi:cli";
use {Environment::{get_environment, get_arguments}} from "wasi:cli";

pub fn println(message: String) with Stdout {
    let stream = string_to_stream(`{message}\n`);
    write_via_stream(stream);  // No need for Stdout:: prefix
}

pub fn env(name: String) -> Option<String> with Environment {
    let vars = get_environment();  // No need for Environment:: prefix
    for (key, value) in vars {
        if key == name {
            return Some(value);
        }
    }
    return None;
}
```

**Import Rules:**

- Effect functions use `::` syntax: `use {Effect::{func1, func2}} from "..."`
- Multiple functions can be imported: `Effect::{func1, func2, func3}`
- Function renaming is supported: `use {func as renamed} from "..."`
- Wildcards are prohibited: `use {Effect::{*}}` is not allowed
- The `with` declaration is still required for effect tracking

**Name Resolution:**

- Imported effect functions can be called directly without the `Effect::` prefix
- If a function name is ambiguous, use the fully qualified `Effect::function()` syntax
- Non-imported effect functions must always use the `Effect::function()` syntax

```wado
// Example with name collision handling
use {Stdout::{write_via_stream}} from "wasi:cli";
use {Stderr::{write_via_stream as stderr_write}} from "wasi:cli";

pub fn log(message: String) with Stdout, Stderr {
    write_via_stream(stdout_stream);  // Calls Stdout::write_via_stream
    stderr_write(stderr_stream);      // Calls Stderr::write_via_stream (renamed)
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

### Handlers

#### Built-in Handlers

```wado
use {WasiConsole, WasiFileSystem, WasiHttp, BrowserDom} from "core:handlers";

fn main() {
    with Console => WasiConsole, FileSystem => WasiFileSystem, Http => WasiHttp {
        app();
    }
}
```

#### Inline Handler

```wado
with handler Console {
    print(msg) => actual_print(msg),
    read_line() => actual_read(),
} {
    greet("Alice");
}
```

#### Named Handler

```wado
handler MockConsole for Console {
    let mut output: Array<String> = [];

    print(msg) => {
        output.push(msg);
    },
    read_line() => "mocked input",
}

// Usage
fn test() {
    with Console => MockConsole {
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
    with Console => WasiConsole, Http => WasiHttp, FileSystem => WasiFileSystem {
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

## Operator Precedence

Wado follows Rust's operator precedence model, which fixes C's historical design flaw where bitwise operators have lower precedence than comparison operators.

### Precedence Table

From highest to lowest precedence:

| Level | Operators                        | Associativity | Description            |
| ----- | -------------------------------- | ------------- | ---------------------- |
| 1     | `::`, `.`, `()`, `[]`            | Left-to-right | Paths, field, call     |
| 2     | `?`                              | N/A           | Error propagation      |
| 3     | `!`, `~`, `-`, `*`, `&`          | Right-to-left | Unary operators        |
| 4     | `as`                             | Left-to-right | Type cast              |
| 5     | `*`, `/`, `%`                    | Left-to-right | Multiplicative         |
| 6     | `+`, `-`                         | Left-to-right | Additive               |
| 7     | `<<`, `>>`                       | Left-to-right | Bitwise shift          |
| 8     | `&`                              | Left-to-right | Bitwise AND            |
| 9     | `^`                              | Left-to-right | Bitwise XOR            |
| 10    | `\|`                             | Left-to-right | Bitwise OR             |
| 11    | `==`, `!=`, `<`, `>`, `<=`, `>=` | Special       | Comparison (chainable) |
| 12    | `&&`                             | Left-to-right | Logical AND            |
| 13    | `\|\|`                           | Left-to-right | Logical OR             |
| 14    | `..`, `..=`                      | N/A           | Range operators        |
| 15    | `=`, `+=`, `-=`                  | Right-to-left | Assignment             |

### Key Features

**✅ Bitwise operators have higher precedence than comparison**

This fixes C's counterintuitive behavior:

```wado
// Wado (correct)
if flags & mask == expected {  // Parses as: (flags & mask) == expected ✅
    // ...
}

// C (incorrect)
if (flags & mask == expected)  // Parses as: flags & (mask == expected) ❌
```

**✅ Comparison chaining**

Mathematical comparison chaining is supported with validation:

```wado
// Valid: same-direction chains
if a < b < c {           // OK: (a < b) AND (b < c)
    println("ascending");
}

if 0 <= x <= 100 {       // OK: range check
    println("in range");
}

if a == b == c {         // OK: all equal
    println("all equal");
}

// Invalid: mixed directions (semantic error)
if a < b > c {           // ERROR: mixed directions
}

if a != b != c {         // ERROR: != chaining not allowed
}
```

**❌ No `++`/`--` operators**

Use `+=` and `-=` instead to avoid undefined behavior:

```wado
count += 1;  // ✅ Clear and unambiguous
count -= 1;  // ✅ Clear and unambiguous

count++;     // ❌ Compile error
++count;     // ❌ Compile error
```

**❌ No `**` power operator\*\*

Use explicit `pow()` function:

```wado
let result = pow(x, 2);  // ✅ Explicit
let result = x ** 2;     // ❌ Compile error
```

### Bitwise Operators

| Operator | Name        | Example  | Description                |
| -------- | ----------- | -------- | -------------------------- |
| `&`      | Bitwise AND | `a & b`  | Bitwise AND                |
| `\|`     | Bitwise OR  | `a \| b` | Bitwise OR                 |
| `^`      | Bitwise XOR | `a ^ b`  | Bitwise exclusive OR       |
| `~`      | Bitwise NOT | `~a`     | Bitwise complement (unary) |
| `<<`     | Left shift  | `a << 2` | Left shift by 2 bits       |
| `>>`     | Right shift | `a >> 2` | Arithmetic right shift     |

All bitwise operators work on integer types (`i8`, `i16`, `i32`, `i64`, `i128`, `u8`, `u16`, `u32`, `u64`, `u128`).

### Design Rationale

See `docs/operator-precedence-research.md` for detailed analysis of operator precedence across languages, and `docs/adr-2026-01-11-operator-precedence.md` for the architectural decision record.

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

### WASI P3 Type Mapping

| WIT Type        | Wado Type        | Notes                               |
| --------------- | ---------------- | ----------------------------------- |
| `stream<u8>`    | `Stream<u8>`     | First-class async stream            |
| `future<T>`     | `Future<T>`      | First-class async future            |
| `result<T, E>`  | `Result<T, E>`   | Error handling                      |
| `result`        | `Result<(), ()>` | Unit result (no payload)            |
| `option<T>`     | `Option<T>`      | Optional value                      |
| `list<T>`       | `Array<T>`       | Dynamic list                        |
| `tuple<A, B>`   | `Tuple<A, B>`    | Tuple types                         |
| `string`        | `String`         | UTF-8 string (struct in Wado)       |
| `enum { a, b }` | `enum { A, B }`  | Variants use UpperCamelCase in Wado |

### WASI P3 CLI Interfaces

Wado effects map to WASI P3 interfaces:

| Wado Effect   | WASI Interface         | Key Functions                                         |
| ------------- | ---------------------- | ----------------------------------------------------- |
| `Stdout`      | `wasi:cli/stdout`      | `write-via-stream(stream<u8>)`                        |
| `Stderr`      | `wasi:cli/stderr`      | `write-via-stream(stream<u8>)`                        |
| `Stdin`       | `wasi:cli/stdin`       | `read-via-stream() -> tuple<stream<u8>, future<...>>` |
| `Environment` | `wasi:cli/environment` | `get-arguments()`, `get-environment()`                |
| `Exit`        | `wasi:cli/exit`        | `exit(result)`, `exit-with-code(u8)`                  |

### Async Functions in WASI P3

WASI P3 uses `async func` in WIT for non-blocking operations. In Wado, these are handled transparently via stack switching (colorless async):

```wit
// WIT definition has async keyword
write-via-stream: async func(data: stream<u8>) -> result<_, error-code>;
```

```wado
// Wado has no async keyword in effect declarations or function implementations.
fn println(message: String) with Stdout {
    let stream = string_to_stream(`{message}\n`);
    Stdout::write_via_stream(stream);  // Colorless async
}
```

### Entry Points

Entry points are integrated in World system.

Currently, `run` is the only entry point, which confirms the `Command` world defined in wasi:cli.

TBD.

### Attribute Syntax for WASI Linking

Use `#[wasi(...)]` attributes to link Wado definitions to WASI interfaces:

```wado
// Link an effect to a WASI interface
pub effect Stdout {
    #[wasi("wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream")]
    fn write_via_stream(data: Stream<u8>) -> Result<(), ErrorCode>;
}

// Link a resource to a WASI resource
#[wasi("wasi:cli/terminal-output@0.3.0-rc-2025-09-16")]
resource TerminalOutput;

// Link an enum to a WASI enum
// Wado uses UpperCamelCase for variants
pub enum ErrorCode {  // Maps to WIT: enum error-code
    Io,               // Maps to WIT: io
    IllegalByteSequence,  // Maps to WIT: illegal-byte-sequence
    Pipe,             // Maps to WIT: pipe
}
```

### The Wasm Loader (Security Model for external plugins)

Effect declaration = Wasm import = WASI capability:

```wado
use {load_wasm} from "core:wasm";

// Restrict plugin capabilities with world and effect declarations
world Transform {
    // ...

    export fn execute(input: String) with FileSystem -> String;
}

let plugin = load_wasm<Transform>("./transform.wasm");

let output = plugin.execute(input);
```

TBD.
