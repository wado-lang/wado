# WEP: World Conformance and Export Syntax

## Context

Wado compiles to WebAssembly Component Model (CM), where a **world** defines the contract between a component and its runtime environment. Currently, Wado has:

1. **`pub` keyword**: Controls visibility between Wado modules (internal to Wado)
2. **Implicit world mapping**: The `run()` function is automatically mapped to `wasi:cli/Command::run`
3. **No explicit world conformance**: No way to declare or verify that a module conforms to a world's requirements

### Problem Statement

To properly support Component Model worlds, we need:

1. **Explicit world conformance declaration**: Verify that a module satisfies a world's requirements (similar to interface implementation in other languages)
2. **CM boundary export**: Generate ABI glue code to expose functions across the Component Model boundary (like `extern "C"` in C/Rust)
3. **Multiple world support**: Allow a single module to conform to multiple worlds
4. **Conflict resolution**: Handle cases where multiple worlds export functions with the same name

### Design Goals

- **No attribute syntax**: Avoid Rust-style `#[...]` attributes which can become chaotic
- **Clear separation of concerns**: Distinguish between Wado module visibility (`pub`), world conformance declaration, and CM boundary export
- **Implicit conformance**: Allow world conformance to be inferred from exports (like Go's interfaces)
- **Align with WIT**: Use `export` keyword consistent with WIT syntax

## Decision

### Visibility and Export

Two orthogonal concepts control accessibility:

| Declaration | From Wado modules | From CM boundary |
|-------------|-------------------|------------------|
| `fn foo()` | ❌ | ❌ |
| `pub fn foo()` | ✅ | ❌ |
| `export fn foo()` | ❌ | ✅ |
| `pub export fn foo()` | ✅ | ✅ |

- **`pub`**: Makes a declaration visible to other Wado modules
- **`export`**: Generates Component Model ABI glue code, making it accessible across CM boundary

### World as First-Class Entity

Worlds are imported like other Wado entities:

```wado
use { Command } from "wasi:cli";
```

### `contract` Declaration

Declares that this module conforms to a specified world:

```wado
use { Command } from "wasi:cli";

contract Command;
```

- One world per line; multiple worlds supported via multiple declarations
- Triggers compile-time verification that all world requirements are satisfied
- **Optional**: If omitted, the runtime environment determines the expected world (e.g., `wado run` expects `wasi:cli/Command`)

### Explicit Export Mapping

When function names don't match world export names, or when exporting to multiple worlds:

```wado
use { Command, HttpServer } from "wasi:cli";

contract Command;
contract HttpServer;

// Explicit mapping to a single world
export(Command::run) pub fn run_cli() { ... }

// Export to multiple worlds
export(Command::run, HttpServer::run) pub fn shared_run() { ... }
```

If signature matches, a single `export fn` can satisfy multiple worlds without explicit mapping.

### Type Export

Types can be exported with the same syntax:

```wado
export struct MyRecord { x: i32 }
export(SomeWorld::MyType) struct AliasedRecord { x: i32 }
```

### World Imports and Effect System

World imports (dependencies the component needs from its host) are not explicitly declared. Instead:

- Use `use` to import capabilities from WASI modules
- Effect system tracks which capabilities a function requires
- **Compile-time check**: Using an effect not provided by the world's imports is a compile error

```wado
use { Command } from "wasi:cli";
use { Stdout } from "wasi:cli";  // Required for println

contract Command;

export fn run() with Stdout {
    println("Hello!");  // OK: Command world imports Stdout
}
```

## Examples

### Simple Case: CLI Application

```wado
use { Command } from "wasi:cli";
use { println, Stdout } from "core:cli";

contract Command;

export fn run() with Stdout {
    println("Hello, World!");
}
```

### Implicit World Conformance

For simple scripts, `contract` can be omitted:

```wado
use { println, Stdout } from "core:cli";

// No contract declaration - runtime determines expected world
// `wado run` expects Command world

export fn run() with Stdout {
    println("Hello!");
}
```

### Multiple Worlds with Name Conflicts

```wado
use { Command, Daemon } from "my:worlds";

contract Command;
contract Daemon;

export(Command::run) pub fn run_cli() {
    println("CLI mode");
}

export(Daemon::run) pub fn run_daemon() {
    loop {
        // Daemon loop
    }
}
```

### Shared Implementation Across Worlds

```wado
use { Command, HttpServer } from "my:worlds";

contract Command;
contract HttpServer;

// Both worlds have compatible `run` - export to both
export(Command::run, HttpServer::run) pub fn run() {
    initialize();
    serve();
}
```

## Keyword Selection Rationale

| Keyword      | Pros                                   | Cons                           | Decision     |
| ------------ | -------------------------------------- | ------------------------------ | ------------ |
| `implements` | Common in OOP languages                | Strong class-level connotation | Rejected     |
| `conforms`   | Clear protocol conformance meaning     | Slightly verbose               | Considered   |
| `confirms`   | Declarative reading                    | Unusual verb form              | Considered   |
| `contract`   | Clear boundary contract semantics      | N/A                            | **Accepted** |

**Why `contract`:**

- "This module satisfies the specified world contract"
- Natural for multiple declarations: `contract A; contract B;`
- Aligns with Component Model terminology
- Works as both noun and verb in singular form

## Consequences

### Positive

- **Explicit world conformance**: Developers can verify their module satisfies world requirements
- **Implicit conformance option**: Simple scripts work without boilerplate (like Go interfaces)
- **Multiple world support**: Natural syntax for conforming to multiple worlds
- **Conflict resolution**: Explicit mapping syntax resolves name conflicts
- **Clear separation**:
  - `pub`: Wado module visibility
  - `export`: CM boundary accessibility
  - `contract`: World conformance verification (optional)
- **Effect system integration**: World import requirements checked at compile time

### Negative

- **More keywords**: Introduces `contract` and extends `export` syntax
- **Three concepts**: Developers must understand `pub`, `export`, and `contract`

## References

- [WIT Reference - Component Model](https://component-model.bytecodealliance.org/design/wit.html)
- [Worlds - Component Model](https://component-model.bytecodealliance.org/design/worlds.html)
