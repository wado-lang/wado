# WEP: World Conformance and Export Syntax

## Context

Wado compiles to WebAssembly Component Model (CM), where a **world** defines the contract between a component and its runtime environment.

Worlds fall into two categories (Wado terminology; the CM treats all worlds uniformly):

- **Hosted world**: A world that a runtime knows how to instantiate and drive (e.g., `wasi:cli/command`, `wasi:http/service`). The runtime provides all imports and invokes exports according to a defined lifecycle.
- **Library world**: A world that defines a component's public API for composition with other components, rather than for direct execution by a runtime.

Currently, Wado has:

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

Visibility is two orthogonal axes — superseded by [WEP: Visibility —
`internal` / `pub` / `export`](./wep-2026-06-25-visibility-internal-pub-export.md).
Summary: `internal` (package) and `pub` (library) form a scope ladder; `export`
is an additive CM-boundary flag with `export ⟹ pub`. This WEP's original
two-keyword table (`pub` = Wado modules, `export` = CM) is replaced by that
model; the `contract` and export-mapping syntax below is unaffected.

- **`export`**: Generates Component Model ABI glue code, making the item
  accessible across the CM boundary (and, by `export ⟹ pub`, part of the
  library API).

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

| Keyword      | Pros                               | Cons                           | Decision     |
| ------------ | ---------------------------------- | ------------------------------ | ------------ |
| `implements` | Common in OOP languages            | Strong class-level connotation | Rejected     |
| `conforms`   | Clear protocol conformance meaning | Slightly verbose               | Considered   |
| `confirms`   | Declarative reading                | Unusual verb form              | Considered   |
| `contract`   | Clear boundary contract semantics  | N/A                            | **Accepted** |

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
