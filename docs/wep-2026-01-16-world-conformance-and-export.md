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

1. **World conformance**: Verify that a module satisfies a world's requirements (similar to interface implementation in other languages)
2. **CM boundary export**: Generate ABI glue code to expose functions across the Component Model boundary (like `extern "C"` in C/Rust)
3. **Multiple world support**: Allow a single module to conform to multiple worlds
4. **Conflict resolution**: Handle cases where multiple worlds export functions with the same name

### Design Goals

- **No attribute syntax**: Avoid Rust-style `#[...]` attributes which can become chaotic
- **Clear separation of concerns**: Distinguish between Wado module visibility (`pub`) and CM boundary export
- **Implicit conformance**: Infer world conformance from exports (like Go's interfaces)
- **Align with WIT**: Use `export` keyword consistent with WIT syntax

## Decision

### Visibility and Export

Visibility is two orthogonal axes — superseded by [WEP: Visibility —
`internal` / `pub` / `export`](./wep-2026-06-25-visibility-internal-pub-export.md).
Summary: `internal` (package) and `pub` (library) form a scope ladder; `export`
is an additive CM-boundary flag with `export ⟹ pub`. This WEP's original
two-keyword table (`pub` = Wado modules, `export` = CM) is replaced by that
model; the export-mapping syntax below is unaffected.

- **`export`**: Generates Component Model ABI glue code, making the item
  accessible across the CM boundary (and, by `export ⟹ pub`, part of the
  library API).

### World as First-Class Entity

A world is a Wado declaration (`pub world Command { ... }`), imported like other
Wado entities:

```wado
use { Command } from "wasi:cli";
```

### World Selection

A module does not name its world in source. The package's `wado.toml` maps each
world to an entry module, or `--world` selects one for a single file. The
compiler checks conformance against the selected world: the entry point it
exports, the `export` on it, and its signature.

This WEP first proposed a `contract World;` declaration for that purpose. It is
not adopted. The manifest already names the world, and a second place to name it
would need a rule for when the two disagree. A module that serves two worlds is
the entry module of both in the manifest.

### Explicit Export Mapping

When function names don't match world export names, or when exporting to multiple worlds:

```wado
use { Command, Daemon } from "my:worlds";

// Explicit mapping to a single world
export(Command::run) fn run_cli() { ... }

// Export to multiple worlds
export(Command::run, Daemon::run) fn shared_run() { ... }
```

If signature matches, a single `export fn` can satisfy multiple worlds without explicit mapping.

A target in a world other than the selected one exports nothing and is checked
against that world's signature, so a module serving two worlds learns of a
mismatch whichever one it is compiled for. The mapped function is exported only
under the mapped name. A second provider of the same export, mapped or named,
is an error rather than a precedence rule.

### Type Export

Types can be exported with the same syntax:

```wado
export struct MyRecord { x: i32 }
export(SomeWorld::MyType) struct AliasedRecord { x: i32 }
```

### World Imports and Effect System

A program declares its imports through its effects. No effect is performed
without a `with` clause naming it. Unless a handler takes the effect over, that
clause tells the compiler to import the effect's interface. No separate import
declaration exists.

```wado
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("Hello!");  // the component imports the interface behind Stdout
}
```

An effect the selected world does not import is imported all the same, not
rejected. The program has already declared it, and whether the host provides it
is the host's call: a `wasi:cli/command` program performing `wasi:http/types`
operations compiles to a component importing `wasi:http/types`, which a host
that lacks it refuses to instantiate.

## Examples

### CLI Application

```wado
use { println, Stdout } from "core:cli";

// `wado run` selects the Command world
export fn run() with Stdout {
    println("Hello, World!");
}
```

### Multiple Worlds with Name Conflicts

```wado
use { Command, Daemon } from "my:worlds";

export(Command::run) fn run_cli() {
    println("CLI mode");
}

export(Daemon::run) fn run_daemon() {
    loop {
        // Daemon loop
    }
}
```

### Shared Implementation Across Worlds

```wado
use { Command, Daemon } from "my:worlds";

// Both worlds have compatible `run` - export to both
export(Command::run, Daemon::run) fn run() {
    initialize();
    serve();
}
```

## Consequences

### Positive

- **Implicit conformance**: Simple scripts work without boilerplate (like Go interfaces)
- **One place names the world**: the manifest or the command line, never the source as well
- **Conflict resolution**: Explicit mapping syntax resolves name conflicts
- **Clear separation**:
  - `pub`: Wado module visibility
  - `export`: CM boundary accessibility
- **Effect system integration**: the effects a program declares are its imports, with no second declaration

### Negative

- **Extended `export` syntax**: `export(World::name)` adds a form to learn
- **Conformance is not visible in source**: a reader learns a module's world from the manifest

## Known gaps

- Type export, `export(World::Type) struct`, does not parse.
- Only a standard library world can be named in `export(…)`: a world a package
  declares is not registered, so the compiler reports it as one no compilation
  can target.
- `export(…)` is honoured only in the entry module.
- The world and export checks run when the component is built, so `wado check`
  and the language service do not report them, and their errors carry no span.

## References

- [WIT Reference - Component Model](https://component-model.bytecodealliance.org/design/wit.html)
- [Worlds - Component Model](https://component-model.bytecodealliance.org/design/worlds.html)
