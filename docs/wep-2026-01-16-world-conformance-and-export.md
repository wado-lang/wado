# WEP: World Conformance and Export Syntax

**Status**: Accepted

**Date**: 2026-01-16

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
4. **Conflict resolution**: Handle cases where multiple worlds export functions with the same name but different signatures or semantics

### Design Goals

- **No attribute syntax**: Avoid Rust-style `#[...]` attributes which can become chaotic
- **Clear separation of concerns**: Distinguish between Wado module visibility (`pub`), world conformance declaration, and CM boundary export
- **Explicit over implicit**: Make world conformance explicit rather than relying solely on duck typing (unlike Go)
- **Align with WIT**: Use `export` keyword consistent with WIT syntax

## Decision

We introduce two new language constructs:

### 1. `contract` Keyword - World Conformance Declaration

```wado
contract World;
```

- Declares that this module conforms to the specified world
- One world per line; multiple worlds supported via multiple declarations
- Triggers compile-time verification that all world requirements are satisfied
- Does NOT generate code by itself; purely declarative/verification

**Multiple worlds example:**

```wado
contract Command;
contract HttpHandler;
```

### 2. `export` Keyword - CM Boundary Export

```wado
export pub fn function_name() { ... }
```

- `export`: Generates Component Model ABI glue code
- `pub`: Controls Wado module visibility (both required)
- Automatically maps to world export if function name matches

**Explicit mapping for name conflicts:**

```wado
export(World::export_name) pub fn wado_function_name() { ... }
```

### Complete Examples

#### Simple Case: Single World

```wado
use {println} from "core:cli";

contract Command;

export pub fn run() -> Result<(), ()> {
    println("Hello, World!");
}
```

#### Complex Case: Multiple Worlds with Name Conflicts

```wado
contract Command;
contract Daemon;

// Same function name, different worlds
export(Command::run) pub fn run_cli() -> Result<(), ()> {
    println("CLI mode");
    return Ok(());
}

export(Daemon::run) pub fn run_daemon() {
    loop {
        // Daemon loop
    }
}
```

#### Library with Multiple Exports

```wado
contract MathService;

export pub fn add(a: i32, b: i32) -> i32 {
    return a + b;
}

export pub fn multiply(a: i32, b: i32) -> i32 {
    return a * b;
}
```

## Keyword Selection Rationale

We evaluated multiple keyword options:

| Keyword      | Pros                                   | Cons                           | Decision     |
| ------------ | -------------------------------------- | ------------------------------ | ------------ |
| `implements` | Most common in OOP languages           | Strong class-level connotation | Rejected     |
| `conforms`   | Clear protocol conformance meaning     | Slightly verbose               | Considered   |
| `satisfies`  | TypeScript-like, emphasizes validation | Value-level connotation        | Rejected     |
| `entrypoint` | Emphasizes CM connection point         | Implies singularity            | Rejected     |
| `contract`   | Clear boundary contract semantics      | N/A                            | **Accepted** |

**Why `contract`:**

- Emphasizes the contract between component and runtime
- Natural for multiple declarations: `contract A; contract B;`
- Aligns with Component Model terminology
- Distinct from class-based `implements`

## Consequences

### Positive

- **Explicit world conformance**: Developers can verify their module satisfies world requirements
- **Multiple world support**: Natural syntax for conforming to multiple worlds
- **Conflict resolution**: Explicit mapping syntax resolves name/signature conflicts
- **Three-layer separation**:
  - `pub`: Wado module visibility
  - `contract`: World conformance verification
  - `export`: CM boundary ABI generation
- **No attribute chaos**: Clean keyword-based syntax instead of attributes
- **Gradual complexity**: Simple cases remain simple; complex cases have escape hatches

### Negative

- **More keywords**: Introduces `contract` and extends `export` syntax
- **Verbosity**: Requires both `export` and `pub` for exported public functions
- **Learning curve**: Developers must understand three visibility concepts

### Migration Path

- **Current code**: `pub fn run()` continues to work with implicit Command world mapping
- **New code**: Should use `contract Command; export pub fn run()` for clarity
- **Future**: Consider deprecating implicit world mapping in favor of explicit `contract`

### Implementation Requirements

1. **Parser**: Support `contract World;` declarations and `export(World::fn)` syntax
2. **AST**: Add `ContractDecl` and extend `Function` with optional world mapping
3. **Semantic analysis**: Verify world requirements are satisfied by exported functions
4. **Code generation**: Generate CM ABI glue code for `export` functions with correct world bindings
5. **Error messages**: Clear diagnostics for missing exports, signature mismatches, and name conflicts

### Open Questions

1. **Default world**: Should there be a default world if no `contract` is declared? (Propose: error, require explicit `contract`)
2. **Library components**: How to handle components with no exports? (Propose: allow, but warn)
3. **World import syntax**: Should worlds themselves be importable? (Future work)
4. **Wildcard exports**: Should `export *` syntax be supported for forwarding? (TBD)

## Related ADRs

- [Target WASI P3 Only](./wep-2025-01-11-wasi-p3-only.md) - WASI world context
- [Value Semantics and Reference Captures](./wep-2026-01-12-value-semantics-and-captures.md) - Related to export semantics

## References

- [WIT Reference - Component Model](https://component-model.bytecodealliance.org/design/wit.html)
- [Worlds - Component Model](https://component-model.bytecodealliance.org/design/worlds.html)
- [TypeScript satisfies operator](https://www.typescriptlang.org/docs/handbook/release-notes/typescript-4-9.html)
- [Swift Protocols](https://docs.swift.org/swift-book/documentation/the-swift-programming-language/protocols/)
- [Elixir Protocols and Behaviours](https://elixir-lang.org/getting-started/protocols.html)
