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

Global variables support all numeric and reference types that Wasm globals support:

- Integers: `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`, `i128`, `u128`
- Floats: `f32`, `f64`
- Boolean: `bool`
- Character: `char`

Note: Reference types (`String`, `Array<T>`, structs) are **not yet supported** as Wasm global initialization for GC types has limitations.

### Initialization

Global initialization follows Wasm's constant expression rules:

**Phase 1 (Current Implementation)**: Literal-only initialization

```wado
global ANSWER: i32 = 42;        // OK: integer literal
global PI: f64 = 3.14159;       // OK: float literal
global FLAG: bool = true;       // OK: boolean literal
```

**Phase 2 (Future)**: Constant expression evaluation

```wado
global DOUBLED: i32 = 21 * 2;   // Future: compile-time arithmetic
global OFFSET: i32 = BASE + 10; // Future: reference other globals
```

**Phase 3 (Future)**: Lazy initialization for complex expressions

For initializers that cannot be expressed as Wasm constant expressions:

```wado
global mut cache: Array<i32> = Array::<i32>::with_capacity(100);
```

This would require:

1. Initialize to a default/null value in Wasm global section
2. Generate a `__init_globals()` function with actual initialization logic
3. Ensure `__init_globals()` is called before any access

The `#[module_init]` attribute (future feature) could allow user-defined initialization order:

```wado
#[module_init]
fn setup() {
    cache = Array::<i32>::with_capacity(100);
}
```

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

1. **Limited initialization**: Complex initialization requires lazy evaluation (not yet implemented)
2. **No reference type globals**: GC type globals need additional work for proper initialization
3. **Cross-module access**: Globals are module-private by default; `pub` enables cross-module access but not Component Model export

### Future Work

1. **Lazy initialization**: Support non-constant initializers with generated init code
2. **Reference type globals**: Support `String`, `Array<T>`, and struct globals
3. **Component Model export**: `export global` syntax for exposing globals at CM boundary
4. **Thread safety**: Consider `global atomic` for thread-safe mutable globals (requires Wasm threads)
