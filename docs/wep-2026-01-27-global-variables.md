# WEP: Global Variables

## Context

Wado needs module-level state for various use cases: configuration values, counters, caches, and singleton patterns. WebAssembly provides a native mechanism for this: **global variables**.

### WebAssembly Globals

Wasm globals are a distinct concept from local variables:

| Aspect         | Local Variables     | Wasm Globals                               |
| -------------- | ------------------- | ------------------------------------------ |
| Scope          | Function-scoped     | Module-scoped                              |
| Lifetime       | Stack frame         | Module lifetime                            |
| Access         | Direct (stack slot) | Indexed (`global.get`/`global.set`)        |
| Initialization | On function entry   | On module instantiation                    |
| Mutability     | Always mutable      | Explicitly declared                        |
| Types          | All Wasm types      | Restricted (no `funcref` in some contexts) |

Wasm globals are initialized with **constant expressions** - a limited subset of Wasm that can be evaluated at instantiation time without executing arbitrary code.

### Keyword Choice: Why `global` Instead of `let` or `static`

Several alternatives were considered:

| Keyword  | Precedent        | Issue                                                   |
| -------- | ---------------- | ------------------------------------------------------- |
| `let`    | JavaScript, Rust | Conflates two fundamentally different concepts          |
| `static` | Rust, C          | Implies memory model semantics that don't apply to Wasm |
| `const`  | Many languages   | Already reserved for compile-time constants             |
| `global` | WebAssembly      | Directly reflects the underlying Wasm concept           |

**Decision**: Use `global` to make the Wasm semantics visible.

**Rationale**:

1. **Wasm-visible design**: Wado's philosophy is that Wasm concepts should be apparent in the source language. A `global` in Wado compiles directly to a Wasm global - no abstraction layers, no hidden complexity.

2. **Semantic distinction**: Local variables and globals have fundamentally different initialization, lifetime, and access semantics. Using `let` for both would hide this distinction, leading to confusion when:
   - Initialization expressions are rejected (non-constant)
   - Performance differs (global access is slower than local)
   - Debugging shows different variable kinds

3. **Teachability**: When learning Wado, understanding that `global` maps to Wasm globals helps developers build accurate mental models of the compilation target.

## Decision

### Syntax

```wado
// Immutable global
global PI: f64 = 3.14159;

// Mutable global
global mut counter: i32 = 0;

// With visibility
pub global VERSION: i32 = 1;
pub global mut state: bool = false;
```

### Supported Types

Global variables support all types:

- Integers: `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`, `i128`, `u128`
- Floats: `f32`, `f64`
- Boolean: `bool`
- Character: `char`
- Object types: `String`, `List<T>`, structs

Object type globals use lazy initialization (see below).

### Initialization

Globals support arbitrary initialization expressions:

#### Constant Initialization

For literals and constant expressions, globals are initialized at Wasm instantiation time:

```wado
global ANSWER: i32 = 42;        // integer literal
global PI: f64 = 3.14159;       // float literal
global FLAG: bool = true;       // boolean literal
global DOUBLED: i32 = 21 * 2;   // arithmetic expression
global NEGATIVE: i32 = -42;     // negation
```

#### Lazy Initialization

For non-constant expressions (function calls, object construction), the compiler uses lazy initialization:

```wado
global mut MESSAGE: String = "Hello, World!";
global mut ITEMS: List<i32> = [1, 2, 3];
global mut ORIGIN: Point = Point { x: 0, y: 0 };
```

Implementation details:

1. Non-constant globals are initialized to a default value (0 for primitives, null for references) in the Wasm global section
2. Wasm declares them as mutable internally (Wado still enforces immutability at the language level for non-`mut` globals)

### Multi-Module Initialization

When a program consists of multiple modules, global initialization is coordinated through a two-level system:

#### Per-Module: `__initialize_module`

Each module with lazy-initialized globals generates a `pub fn __initialize_module()` function:

- Contains initialization statements for that module's non-constant globals only
- Always `pub` so it can be called from the entry module
- Initializers are topologically sorted based on dependencies (globals that depend on other globals are initialized after their dependencies)
- Future: Users will be able to hook into this with `#[module_init]` attribute

#### Entry Module: `__initialize_modules`

The entry module generates a `fn __initialize_modules()` function:

- **Not** `pub` (internal to entry module)
- Uses an initialization flag (`__modules_initialized: bool`) to prevent re-initialization
- Checks flag at start, returns early if already initialized (for handlers like `wasi:http` that may be called multiple times on the same instance)
- Calls each linked module's `__initialize_module` in topological order (modules are initialized before modules that depend on them)
- Sets the flag to `true` after all modules are initialized

Entry point functions (`run`, test functions) call `__initialize_modules()` at start.

#### Initialization Order

##### Within a Module

Global initializers within `__initialize_module` are topologically sorted:

```wado
// These are reordered so B is initialized before A
global A: i32 = B + 1;   // depends on B
global B: i32 = 10;      // no dependencies
// Lowered to: B = 10; A = B + 1;
```

##### Across Modules

Module initialization order follows import dependencies:

```wado
// main.wado
use { helper_global } from "./helper.wado";
global MAIN_GLOBAL: i32 = helper_global + 1;

// helper.wado
pub global helper_global: i32 = compute();
```

`helper.wado`'s `__initialize_module` is called before `main.wado`'s.

#### Edge Cases

1. **Circular dependencies**: Detected at compile time and reported as an error
2. **No lazy globals**: Modules without lazy-initialized globals don't generate `__initialize_module`
3. **Multiple entry points**: Each entry point calls `__initialize_modules`, but the flag ensures initialization happens only once

### Mutability Checking

Assignment to globals is only allowed for `global mut` declarations:

```wado
global CONSTANT: i32 = 42;
global mut variable: i32 = 0;

fn example() {
    variable = 10;    // OK: mutable global
    CONSTANT = 10;    // Error: cannot assign to immutable global
}
```

## Consequences

### Benefits

1. **Direct Wasm mapping**: No runtime overhead - globals compile to exactly what Wasm provides
2. **Predictable semantics**: Developers familiar with Wasm understand the behavior immediately
3. **Clear distinction**: `global` vs `let` makes scope and lifetime obvious at declaration site

### Limitations

1. **Cross-module access**: Globals are module-private by default; `pub` enables cross-module access but not Component Model export
2. **Initialization order**: Lazy-initialized globals depend on entry point execution; accessing before initialization returns default/null values

### Future Work

1. **Component Model export**: `export global` syntax for exposing globals at CM boundary
2. **Thread safety**: Consider `global atomic` for thread-safe mutable globals (requires Wasm threads)
3. ~~**Constant folding optimization**~~: Implemented as `wir_optimize::const_global`. A user-immutable global whose extracted initializer folds to a Wasm constant (scalar, `struct.new`, or `array.new_fixed`) is promoted back to an eager immutable Wasm constant. See [WEP: Constant Object Globalization](./wep-2026-05-31-const-object-globalization.md).
