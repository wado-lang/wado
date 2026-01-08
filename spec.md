# Wado Language Specification

Wado is a new programming language targeting Wasm/WASI -- Wasm in plain sight.

## Overview

| Item | Description |
|------|-------------|
| Name | Wado |
| Extension | `.wado` |
| Target | Wasm/WASI only |
| Paradigm | Reactive, Effect System |

## Design Philosophy

- **Wasm/WASI only**: Design optimized for runtime constraints
- **No macros**: Prioritizes tooling compatibility (formatter, syntax highlighter)
- **Explicit imports**: All dependencies are explicit
- **Colorless async**: Eliminates async/await "color" problem via Stack Switching
- **Effect System**: Side effect tracking and control, swappable via Handlers

---

## Memory Model

### Core Principles

- **Wasm-GC based**: Garbage collection delegated to runtime
- **Lifetime inference**: No explicit lifetime annotations required
- **Explicit move**: Ownership transfer only when explicitly stated

### Move Syntax

```rust
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

```rust
// Enforce unique ownership
let unique handle = open_file("data.txt");
let other = handle;       // Error: unique cannot be implicitly copied
let other = move handle;  // OK: explicit move
```

---

## Type System

### Component Model Mapping

All Wado types map directly to WebAssembly Component Model types:

| Wado Type | Component Model Type | Notes |
|--------------|---------------------|-------|
| `bool` | `bool` | Boolean |
| `char` | `char` | Unicode scalar value |
| `string` | `string` | UTF-8 string |
| `i8`, `i16`, `i32`, `i64` | `s8`, `s16`, `s32`, `s64` | Signed integers |
| `u8`, `u16`, `u32`, `u64` | `u8`, `u16`, `u32`, `u64` | Unsigned integers |
| `i128`, `u128` | - | Wasm wide-arithmetic proposal |
| `f32`, `f64` | `f32`, `f64` | Floating point (32-bit, 64-bit) |
| `List<T>` | `list<T>` | Dynamic list (UpperCamel in Wado) |
| `Dict<K, V>` | - | Wado extension, not in Component Model |
| `Tuple<T1, T2, ...>` | `tuple<T1, T2, ...>` | Tuple types (UpperCamel in Wado) |
| `Option<T>` | `option<T>` | Optional value (UpperCamel in Wado) |
| `Result<T, E>` | `result<T, E>` | Result type (UpperCamel in Wado) |
| `record { ... }` | `record { ... }` | Record type (component model primitive) |
| `struct { ... }` | - | GC struct (Wasm-GC, not Component Model) |
| `variant { ... }` | `variant { ... }` | Variant/sum type with payloads |
| `resource` | `resource` | Resource handle |
| `Stream<T>` | `stream<T>` | Component Model async stream (UpperCamel in Wado) |
| `Future<T>` | `future<T>` | Component Model async future (UpperCamel in Wado) |

**Type Naming Convention:**
- Built-in primitive types use lowercase: `bool`, `char`, `string`, `i32`, `f64`
- Generic container types use UpperCamelCase: `List<T>`, `Dict<K, V>`, `Tuple<T1, T2, ...>`, `Option<T>`, `Result<T, E>`, `Stream<T>`, `Future<T>`
- User-defined types follow UpperCamelCase convention

### The Prelude

The **prelude** (`core::prelude`) is automatically imported into every module, providing access to fundamental types without requiring explicit imports:

**Automatically Available:**
- `Option<T>` and its variants: `Some(x)`, `None`
- `Result<T, E>` and its variants: `Ok(x)`, `Err(e)`
- `Stream<T>` - Component Model async stream
- `Future<T>` - Component Model async future
- `Pollable` - WASI I/O polling resource

**Disabling the Prelude:**
```rust
#![no_prelude]  // At the top of a module

// Now you must explicitly import everything
use core::prelude::{Option, Result, Stream};
```

### Primitive Types (No Import Required)

```rust
// Numeric
i8, i16, i32, i64, i128
u8, u16, u32, u64, u128
f32, f64

// Basic
bool
char
string

// Collections (UpperCamelCase)
List<T>           // Component Model List<T>
Dict<K, V>        // Extension, not in Component Model
Tuple<T1, T2, ...> // Component Model tuple<T1, T2, ...>

// Language core (UpperCamelCase)
Option<T>    // Some(x), None
Result<T, E> // Ok(x), Err(e)
Reactive<T>  // Reactive value
```

### String Literals

**Regular strings** use double quotes:
```rust
let name = "Alice";
let path = "path/to/file.txt";
```

**Template strings** (interpolation) use backticks:
```rust
let name = "Alice";
let greeting = `Hello, {name}!`;  // "Hello, Alice!"

let count = 42;
let message = `Count: {count}`;   // "Count: 42"

// With formatting
let pi = 3.14159;
let formatted = `Pi: {pi:0.2f}`;  // "Pi: 3.14"
```

### Literal Types

```rust
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

### Records and Structs

**Records** (Component Model primitive):
```rust
// Record type (maps to Component Model record)
record User {
    name: string,
    age: i32,
    active: bool,
}

// Inline record type
type UserData = record {
    name: string,
    age: i32,
};
```

**Structs** (Wasm-GC):
```rust
// GC struct (Wasm-GC feature, not Component Model)
struct Node {
    value: i32,
    next: Option<Node>,
}
```

**Note**: For Component Model interfaces, use `record`. For internal data structures with GC, use `struct`.

---

## Object Literals

### Syntax Rules

- Identifier keys: Quotes optional
- Non-identifier keys: Quotes required
- Computed key: `[expr]` syntax (dict only)
- Shorthand: Can omit when variable name matches key

### Struct Initialization

```rust
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

```rust
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

```rust
// Struct: dot notation
user.name

// Dict: bracket notation
d["key"]
```

---

## Module System

### Import Rules

- Always use curly braces
- Wildcards prohibited
- All imports must be explicit (except the prelude)

```rust
use core::cli::{println, eprintln};
use core::dom::{window, document};
use core::fmt::{format};
use core::collections::{vec, set};

// Prohibited patterns
use core::cli::*;        // Wildcard
use core::cli::println;  // No curly braces
```

**Exception: The Prelude**

The `core::prelude` module is automatically imported into every module, making `Option`, `Result`, `Stream`, `Future`, and `Pollable` available without explicit imports. To opt out, use `#![no_prelude]`.

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

```rust
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

```rust
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

```rust
effect {
    console.log("Count changed:", count);
}
```

### JSX Integration

```rust
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

```rust
// No async keyword needed
fn fetch_user(id: i32) -> Result<User, HttpError> with Http {
    let response = Http.get("users/{id}")?;
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

```rust
effect Console {
    fn print(msg: string);
    fn read_line() -> string;
}

effect Http {
    fn get(url: string) -> Response;
    fn post(url: string, body: string) -> Response;
}

effect FileSystem {
    fn read(path: string) -> Result<List<u8>, IoError>;
    fn write(path: string, data: List<u8>) -> Result<(), IoError>;
    fn exists(path: string) -> bool;
}

effect Dom {
    fn query(selector: string) -> Option<Element>;
    fn create_element(tag: string) -> Element;
}
```

**2. Methods with effect requirements**:

```rust
// Methods can declare required effects
impl TcpStream {
    fn read(&mut self, buffer: &mut List<u8>) -> Result<i32, IoError> with Network;
    fn write(&mut self, data: &List<u8>) -> Result<i32, IoError> with Network;
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

```rust
// Declare effects used with `with`
fn greet(name: string) with Console {
    Console.print("Hello, {name}!");
}

// Multiple effects
fn download_and_save(url: string, path: string) with Http, FileSystem {
    let data = Http.get(url).body;
    FileSystem.write(path, data);
}

// No effects = pure function
fn add(a: i32, b: i32) -> i32 {
    return a + b;
}
```

### Effect Propagation

- Local functions: Inferred
- pub functions: Must be explicit

```rust
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

```rust
use core::handlers::{WasiConsole, WasiFileSystem, WasiHttp, BrowserDom};

fn main() {
    with Console => WasiConsole, FileSystem => WasiFileSystem, Http => WasiHttp {
        app();
    }
}
```

#### Inline Handler

```rust
with handler Console {
    print(msg) => actual_print(msg),
    read_line() => actual_read(),
} {
    greet("Alice");
}
```

#### Named Handler

```rust
handler MockConsole for Console {
    let mut output: List<string> = [];

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

```rust
effect Generator<T> {
    fn yield(value: T);
}

fn range(start: i32, end: i32) with Generator<i32> {
    let mut i = start;
    while i < end {
        Generator.yield(i);
        i += 1;
    }
}

fn collect_all() -> List<i32> {
    let mut result: List<i32> = [];

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

```rust
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

```rust
world WorldName {
    import EffectName {
        function_name_1,
        function_name_2,
    }

    import AnotherEffect {
        function_name_3,
    }

    export fn exported_function(arg: Type) -> ReturnType;
}

// Declare which world this component implements
#![world(WorldName)]
```

### WASI CLI World Example

The standard WASI CLI `command` world in Wado syntax:

```rust
// Based on wasi:cli@0.2.x command world
// Effect definitions are in core::cli (see cli.wado)

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

    // Entry point: maps to WIT's "run: func() -> result"
    export fn run() -> Result<(), ()>;
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

```rust
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
- **Versioning**: Version information (`@0.3.0-rc-2025-09-16`) is specified in the effect definitions (e.g., `cli.wado`), not in the world declaration

---

## Error Handling

### Unrecoverable Errors (Wasm Exceptions)

```rust
panic("Fatal error");      // Immediate termination
assert(condition);         // Condition check, panic on failure
unreachable();             // Unreachable code
```

These cannot be caught; the program terminates.

### Recoverable Errors (Result Type)

```rust
fn parse_int(s: string) -> Result<i32, ParseError> {
    // ...
}

fn read_config(path: string) -> Result<Config, ConfigError> with FileSystem {
    let content = FileSystem.read(path)
        .map_err(|e| ConfigError.Io(e))?;
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

```rust
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

| WIT Type | Wado Type | Notes |
|----------|-----------|-------|
| `stream<u8>` | `Stream<u8>` | First-class async stream |
| `future<T>` | `Future<T>` | First-class async future |
| `result<T, E>` | `Result<T, E>` | Error handling |
| `result` | `Result<(), ()>` | Unit result (no payload) |
| `option<T>` | `Option<T>` | Optional value |
| `list<T>` | `List<T>` | Dynamic list |
| `tuple<A, B>` | `Tuple<A, B>` | Tuple types |
| `string` | `string` | UTF-8 string |
| `enum { a, b }` | `enum { A, B }` | Variants use UpperCamelCase in Wado |

### WASI P3 CLI Interfaces

Wado effects map to WASI P3 interfaces:

| Wado Effect | WASI Interface | Key Functions |
|-------------|----------------|---------------|
| `Stdout` | `wasi:cli/stdout` | `write-via-stream(stream<u8>)` |
| `Stderr` | `wasi:cli/stderr` | `write-via-stream(stream<u8>)` |
| `Stdin` | `wasi:cli/stdin` | `read-via-stream() -> tuple<stream<u8>, future<...>>` |
| `Environment` | `wasi:cli/environment` | `get-arguments()`, `get-environment()` |
| `Exit` | `wasi:cli/exit` | `exit(result)`, `exit-with-code(u8)` |

### Async Functions in WASI P3

WASI P3 uses `async func` in WIT for non-blocking operations. In Wado, these are handled transparently via stack switching (colorless async):

```wit
// WIT definition
write-via-stream: async func(data: stream<u8>) -> result<_, error-code>;
```

```rust
// Wado usage - no async keyword needed
fn println(message: string) with Stdout {
    let stream = string_to_stream(`{message}\n`);
    Stdout.write_via_stream(stream);  // Colorless async
}
```

### Entry Points

```rust
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

```rust
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

```rust
// Restrict plugin capabilities
let plugin = load_plugin("transform.wasm");
plugin.grant(FileSystem);  // Allow
plugin.deny(Http);         // Deny
```
