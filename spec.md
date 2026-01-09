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
- **No macros**: Prioritizes tooling compatibility (formatter, syntax highlighter)
- **Explicit imports**: All dependencies are explicit
- **Colorless async**: Eliminates async/await "color" problem via Wasm Stack Switching
- **Effect System**: Side effect tracking and control, swappable via Handlers

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
use(a);             // Compile error

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

| Wado Type                 | Component Model Type                 | Notes                                             |
| ------------------------- | ------------------------------------ | ------------------------------------------------- |
| `bool`                    | `bool`                               | Boolean                                           |
| `char`                    | `char`                               | Unicode scalar value                              |
| `string`                  | `string`                             | UTF-8 string                                      |
| `i8`, `i16`, `i32`, `i64` | `s8`, `s16`, `s32`, `s64`            | Signed integers                                   |
| `u8`, `u16`, `u32`, `u64` | `u8`, `u16`, `u32`, `u64`            | Unsigned integers                                 |
| `i128`, `u128`            | `tuple<s64, s64>`, `tuple<u64, u64>` | As tuple at CM boundary                           |
| `f32`, `f64`              | `f32`, `f64`                         | Floating point (32-bit, 64-bit)                   |
| `f16`                     | -                                    | TODO: Wasm half-precision proposal (Phase 1)      |
| `Array<T>`                | `list<T>`                            | GC array in Wado, list at CM boundary             |
| `Dict<K, V>`              | `list<tuple<K, V>>`                  | As list of tuples at CM boundary                  |
| `Tuple<T1, T2, ...>`      | `tuple<T1, T2, ...>`                 | Tuple types (UpperCamel in Wado)                  |
| `Option<T>`               | `option<T>`                          | Optional value (UpperCamel in Wado)               |
| `Result<T, E>`            | `result<T, E>`                       | Result type (UpperCamel in Wado)                  |
| `struct { ... }`          | `record { ... }`                     | GC struct in Wado, record at CM boundary          |
| `enum { ... }`            | `enum { ... }`                       | Enumeration without payloads                      |
| `variant { ... }`         | `variant { ... }`                    | Variant/sum type with payloads                    |
| `flags { ... }`           | `flags { ... }`                      | Bit flags (maps to u8/u16/u32/u64)                |
| `resource`                | `resource`                           | Resource handle                                   |
| `Stream<T>`               | `stream<T>`                          | Component Model async stream (UpperCamel in Wado) |
| `Future<T>`               | `future<T>`                          | Component Model async future (UpperCamel in Wado) |

**Type Naming Convention:**

- Built-in primitive types use lowercase: `bool`, `char`, `string`, `i32`, `f64`
- Generic container types use UpperCamelCase: `Array<T>`, `Dict<K, V>`, `Tuple<T1, T2, ...>`, `Option<T>`, `Result<T, E>`, `Stream<T>`, `Future<T>`
- User-defined types follow UpperCamelCase convention

### The Prelude

The **prelude** (`core:prelude`) is automatically imported into every module, providing access to fundamental types without requiring explicit imports:

**Automatically Available:**

- `Option<T>` and its variants: `Some(x)`, `None`
- `Result<T, E>` and its variants: `Ok(x)`, `Err(e)`
- `Stream<T>` - Component Model async stream
- `Future<T>` - Component Model async future
- `Pollable` - WASI I/O polling resource

**Disabling the Prelude:**

```wado
#![no_prelude]  // At the top of a module

// Now you must explicitly import everything
use {Option, Result, Stream} from "core:prelude";
```

### Primitive Types (No Import Required)

```wado
// Numeric
i8, i16, i32, i64, i128
u8, u16, u32, u64, u128
f32, f64

// Basic
bool
char
string

// Collections (UpperCamelCase)
Array<T>          // GC array in Wado, list at CM boundary
Dict<K, V>        // As list<tuple<K, V>> at CM boundary
Tuple<T1, T2, ...> // Component Model tuple<T1, T2, ...>

// Language core (UpperCamelCase)
Option<T>    // Some(x), None
Result<T, E> // Ok(x), Err(e)
Reactive<T>  // Reactive value
```

### String Literals

**Regular strings** use double quotes:

```wado
let name = "Alice";
let path = "path/to/file.txt";
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

### Literal Types

```wado
// String literal types
type Direction = "north" | "south" | "east" | "west";

// Numeric literal types
type Digit = 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9;

// Object literal types
type Status = {
    status: "loading" | "success" | "error",
    message: string,
};
```

### Structs

Wado uses `struct` for structured data types. Internally they are implemented as Wasm-GC structs, and automatically converted to Component Model `record` at component boundaries.

```wado
// Struct definition
struct User {
    name: string,
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
    name: string,
    age: i32,
};
```

**Implementation Notes:**

- Internally: Wasm-GC `struct` type with GC-managed memory
- At CM boundary: Automatically converted to/from `record`
- Enables recursive types, self-referential structures, and efficient field access

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
let none = Permissions::none();

// All flags
let all = Permissions::all();
```

Note: Wado's `enum` maps to Component Model's `enum` (simple enumeration), and `variant` maps to Component Model's `variant` (tagged union with payloads). This differs from Rust where `enum` can have payloads.

---

## Object Literals

Object literal syntax is compatible with JSON.

### Syntax Rules

- Identifier keys: Quotes optional
- Non-identifier keys: Quotes required
- Computed key: `[expr]` syntax (dict only)
- Shorthand: Can omit when variable name matches key

### Struct Initialization

```wado
let user: User = { name: "Alice", age: 30, active: true };

// With quotes (JSON compatible)
let user: User = { "name": "Alice", "age": 30, "active": true };

// Shorthand
let name = "Bob";
let age = 25;
let bob: User = { name, age, active: false };

// Computed keys not allowed in structs
```

### Dictionaries

```wado
// str keys
let d: Dict<string, i32> = { x: 10, y: 20 };

// Computed key
let key = "dynamic";
let d: Dict<string, i32> = {
    static_key: 1,
    [key]: 2,
    [get_key()]: 3,
};

// Non-str keys
let nums: Dict<i32, string> = {
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

| Source Type       | Syntax                              | Example                                        |
| ----------------- | ----------------------------------- | ---------------------------------------------- |
| WASI standard     | `"wasi:<package>"`                  | `"wasi:cli"`, `"wasi:filesystem"`              |
| Core library      | `"core:<module>"`                   | `"core:cli"`, `"core:fmt"`                     |
| Remote (HTTP)     | `"https://..."`                     | `"https://example.com/lib.wado"`               |
| Local file        | `"./<path>"` or `"../<path>"`       | `"./utils.wado"`, `"../config.wado"`           |
| Package           | `"<package-name>"`                  | `"parser-lib"`, `"json-utils"`                 |

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
use {config} from "https://example.com/data.json" with { type: "json" };

// 4. Local files (relative path, extension required)
use {Helper} from "./utils.wado";
use {Config} from "../config.wado";

// 5. Package dependencies (name only)
use {Parser} from "parser-lib";
```

### Import Attributes (`with`)

Use `with { ... }` to specify import metadata:

```wado
// Version specification
use {Stdout} from "wasi:cli" with { version: "0.3.0" };

// Type hint for non-code imports
use {config} from "https://example.com/data.json" with { type: "json" };

// Future: integrity hash for security
use {Parser} from "parser-lib" with { integrity: "sha384-..." };

// Multiple attributes
use {ApiClient} from "https://example.com/api.wado" with {
    integrity: "sha384-...",
    version: "1.0.0",
};
```

### Import Rules

- Always use curly braces: `use {x} from "..."`
- Wildcards prohibited: `use {*} from "..."` is not allowed
- All imports must be explicit (except the prelude)
- Use `::` for effect function access: `Effect::{func1, func2}`

```wado
// Valid patterns
use {println, eprintln} from "core:cli";
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";

// Prohibited patterns
use * from "core:cli";           // Wildcard not allowed
use println from "core:cli";     // Missing curly braces
```

### Calling Effect Functions

Effect functions use `::` syntax (not `.`):

```wado
use {Stdout, Stdout::{write_via_stream}} from "wasi:cli";

fn example() with Stdout {
    // With import - direct call
    write_via_stream(stream);

    // Fully qualified - always works
    Stdout::write_via_stream(stream);
}
```

**Notation distinction:**

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

**Exception: The Prelude**

The prelude is automatically imported into every module, making `Option`, `Result`, `Stream`, `Future`, and `Pollable` available without explicit imports. To opt out, use `#![no_prelude]`.

### core Module Structure (Proposal)

```
core
├── prelude     # Automatically imported (Option, Result, Stream, Future, Pollable)
├── cli         # WASI CLI (println, eprintln, args, env, exit, ...)
├── filesystem  # WASI Filesystem (open, read, write, stat, ...)
├── net         # Network
├── dom         # DOM API
├── fmt         # format, etc.
├── collections # vec, set, etc.
├── iter        # Iterators
├── json        # parse, stringify
├── math        # sin, cos, sqrt, ...
└── test        # assert_eq, ...
```

### Built-in Functions

Built-in functions provided instead of macros:

```wado
vec(1, 2, 3);             // Rust: vec![1, 2, 3]
println("hello");         // Rust: println!("hello")
panic("error");           // Rust: panic!("error")
assert(x > 0);            // Rust: assert!(x > 0)
dbg(value);               // Rust: dbg!(value)
todo();                   // Rust: todo!()
unreachable();            // Rust: unreachable!()
```

Note: Use string interpolation with backticks instead of `format()`: `` `x = {x}` `` or `` `x = {x:0.3f}` ``

---

## Reactive System

### reactive Keyword

```wado
// Source (mutable reactive value)
let reactive mut count = 0;

// Derived (computed value)
let reactive doubled = || count * 2;

// Read and write
let x = count;      // Read
count = 5;          // Write (change propagates)

// Pass reactive reference
some_function(&reactive count);
```

### Effect Block

```wado
effect {
    console.log("Count changed:", count);
}
```

### JSX Integration

```wado
fn Counter() -> Element with Dom {
    let reactive mut count = 0;

    return <button onclick={|_| count += 1}>
        {count}
    </button>;
}
```

`Reactive` is built into the language; no `with` declaration required.

---

## Concurrency Model

### Stack Switching Based (Colorless)

```wado
// No async keyword needed in function implementations
fn fetch_user(id: i32) -> Result<User, HttpError> with Http {
    let response = Http::get("users/{id}")?;  // Even if Http::get is async in WIT
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

**Important:** While function implementations are colorless (no `async` keyword), effect and world declarations must use `async` to accurately match WIT's `async func` signatures. This enables proper Component Model compilation while maintaining colorless async in user code.

---

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
    fn print(msg: string);
    fn read_line() -> string;
}

effect Http {
    // async keyword required when corresponding to WIT's "async func"
    async fn get(url: string) -> Response;
    async fn post(url: string, body: string) -> Response;
}

effect FileSystem {
    async fn read(path: string) -> Result<Array<u8>, IoError>;
    async fn write(path: string, data: Array<u8>) -> Result<(), IoError>;
    fn exists(path: string) -> bool;  // Synchronous
}

effect Dom {
    fn query(selector: string) -> Option<Element>;
    fn create_element(tag: string) -> Element;
}
```

**Note on async in Effect Declarations:**

- Effect declarations use the `async` keyword to match WIT's `async func` signatures
- Function _implementations_ don't use `async` (colorless async via stack switching)
- This separation allows accurate WIT mapping while maintaining colorless async in code

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
fn listen(addr: string) -> Result<TcpListener, IoError> with Network;
```

This approach makes effect requirements explicit and visible in method signatures, maintaining consistency with the language's design philosophy of being clear and explicit.

### Effect Declaration in Functions

```wado
// Declare effects used with `with`
fn greet(name: string) with Console {
    Console::print("Hello, {name}!");
}

// Multiple effects
fn download_and_save(url: string, path: string) with Http, FileSystem {
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

pub fn println(message: string) with Stdout {
    let stream = string_to_stream(`{message}\n`);
    write_via_stream(stream);  // No need for Stdout:: prefix
}

pub fn env(name: string) -> Option<string> with Environment {
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

pub fn log(message: string) with Stdout, Stderr {
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
    let mut output: Array<string> = [];

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

---

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

    // Use async when exporting functions that map to WIT's "async func"
    export async fn exported_function(arg: Type) -> ReturnType;
    export fn synchronous_function() -> i32;
}

```

> **TBD: Component/Module Structure**
> The relationship between files, modules, and components is still under discussion. The intended design is "1 file = 1 module, 1 component = multiple modules", but the exact syntax for declaring which modules compose a component has not been finalized.

**Note:** The `async` keyword in world export/import declarations indicates correspondence with WIT's `async func`. Function implementations remain colorless (no `async` keyword needed).

### WASI CLI World Example

The standard WASI CLI `command` world in Wado syntax:

```wado
// Based on wasi:cli@0.3.0-rc-2025-09-16 command world
// Effect definitions are in "core:cli" (see cli.wado)

world CliCommand {
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
    // The async keyword is required in world declarations to match WIT signatures.
    // Function implementations don't need async (colorless async via stack switching).
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

    export fn mount(root: string);
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
- **async keyword in declarations**: Effect and world declarations use `async` to match WIT's `async func`, but function implementations don't (colorless async via stack switching)
- **Versioning**: Version information (`@0.3.0-rc-2025-09-16`) is specified in the effect definitions (e.g., `cli.wado`), not in the world declaration

---

## Error Handling

### Unrecoverable Errors (Wasm Exceptions)

```wado
panic("Fatal error");      // Immediate termination
assert(condition);         // Condition check, panic on failure
unreachable();             // Unreachable code
```

These cannot be caught; the program terminates.

### Recoverable Errors (Result Type)

```wado
fn parse_int(s: string) -> Result<i32, ParseError> {
    // ...
}

fn read_config(path: string) -> Result<Config, ConfigError> with FileSystem {
    let content = FileSystem::read(path)
        .map_err(|e| ConfigError::Io(e))?;
    let config = parse_config(content)?;
    return Ok(config);
}

// Handle with pattern matching
match result {
    Ok(value) => use(value),
    Err(e) => handle_error(e),
}
```

---

## JSX

Built into the language, no macros needed:

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

---

## WASI / Browser Support

Wado targets **WASI Preview 3** (0.3.0-rc-2025-09-16), which introduces native `stream<T>` and `future<T>` types that map directly to Wado's `Stream<T>` and `Future<T>`.

> **Implementation Status**: The compiler currently generates WASI 0.2.x compatible code. WASI P3 with native `stream`/`future` types is pending Component Model async feature stabilization in wasmtime.

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
| `string`        | `string`         | UTF-8 string                        |
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
// WIT definition
write-via-stream: async func(data: stream<u8>) -> result<_, error-code>;
```

```wado
// Wado usage - no async keyword needed
fn println(message: string) with Stdout {
    let stream = string_to_stream(`{message}\n`);
    Stdout::write_via_stream(stream);  // Colorless async
}
```

### Entry Points

```wado
// For WASI CLI
fn main() with Stdout {
    println("Hello, world!");
}

// For browser
fn main() with Dom {
    mount(App, "#root");
}
```

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

### Security Model (Plugin System)

Effect declaration = Wasm import = WASI capability:

```wado
// Restrict plugin capabilities
let plugin = load_plugin("transform.wasm");
plugin.grant(FileSystem);  // Allow
plugin.deny(Http);         // Deny
```
