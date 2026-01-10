# Wado Compiler Status

This document tracks the current implementation status of the Wado compiler.

## Architecture

The compiler follows a traditional pipeline:

```
Source (.wado) → Lexer → Parser → Analyzer → Codegen → Component Model Wasm
```

### Modules

| Module   | File          | Description                                     |
| -------- | ------------- | ----------------------------------------------- |
| Lexer    | `lexer.rs`    | Tokenizes source code                           |
| Parser   | `parser.rs`   | Recursive descent parser, builds AST            |
| AST      | `ast.rs`      | AST node definitions                            |
| Token    | `token.rs`    | Token types and spans                           |
| Analyzer | `analyze.rs`  | Semantic analysis, symbol table construction    |
| Symbol   | `symbol.rs`   | Symbol table data structures                    |
| Resolver | `resolver.rs` | Module resolution, loads core library           |
| Stdlib   | `stdlib.rs`   | Embedded core library sources                   |
| Codegen  | `codegen.rs`  | Generates Component Model Wasm via wasm-encoder |

### Standard Library

Embedded `.wado` files in `wado-compiler/lib/`:

**Core Library (`core/`):**

| Module            | File              | Status                                             |
| ----------------- | ----------------- | -------------------------------------------------- |
| `core:prelude`    | `prelude.wado`    | Partial (parser doesn't support generic resources) |
| `core:cli`        | `cli.wado`        | Complete                                           |
| `core:filesystem` | `filesystem.wado` | Complete                                           |
| `core:stream`     | `stream.wado`     | Complete                                           |

**WASI Library (`wasi/`):**

| Module            | File              | Status   |
| ----------------- | ----------------- | -------- |
| `wasi:io`         | `io.wado`         | Complete |
| `wasi:cli`        | `cli.wado`        | Complete |
| `wasi:filesystem` | `filesystem.wado` | Complete |

### Type System

**Primitive Layer (`builtin::`):**

The `builtin` namespace provides raw Wasm GC types with no abstraction:

- `builtin::array<T>` - Wasm GC array (no methods)
- `builtin::i31ref` - Wasm GC i31ref (31-bit integer reference)
- Intrinsic functions: `array_new`, `array_len`, `array_get`, `array_set`, `i31ref_new`, `i31ref_get_s`, `i31ref_get_u`, `eqref`, `unreachable`

**Standard Library Types:**

Standard library types wrap builtins with methods:

- `String` - Struct wrapping `builtin::array<u8>` (maps to CM `string`)
- `Array<T>` - Struct wrapping `builtin::array<T>` (maps to CM `list<T>`)

**Single-Field Optimization:**

Structs with exactly one GC field compile directly to that field's Wasm type (zero overhead).

---

## Feature Checklist

### Lexer

- [x] Keywords (`fn`, `let`, `use`, `if`, `while`, `for`, `match`, `return`, etc.)
- [x] Keywords (`pub`, `effect`, `struct`, `enum`, `type`, `impl`, `resource`, `world`)
- [x] Keywords (`async`, `import`, `export`, `with`, `mut`, `reactive`, `move`, `unique`)
- [x] Identifiers
- [x] Integer literals
- [x] Float literals
- [x] String literals (double quotes)
- [x] Template strings (backticks) - parsed as regular strings for now
- [x] Operators (`+`, `-`, `*`, `/`, `%`, `==`, `!=`, `<`, `<=`, `>`, `>=`, `&&`, `||`)
- [x] Punctuation (`(`, `)`, `{`, `}`, `[`, `]`, `,`, `:`, `;`, `::`, `.`, `->`, `=>`, `|`, `&`, `#`, `?`)
- [x] Comments (`//`)
- [ ] Block comments (`/* */`)
- [ ] Doc comments (`///`, `//!`)

### Parser

#### Items

- [x] `use` declarations
- [x] `fn` declarations (with params, return type, effects)
- [x] `pub` modifier
- [x] `effect` declarations
- [x] `struct` declarations (GC struct in Wado, maps to record at CM boundary)
- [x] `enum` declarations (payload-free, CM semantics)
- [x] `type` aliases
- [x] `impl` blocks
- [x] `resource` declarations
- [x] `world` declarations (with imports/exports)
- [x] Attributes (`#[...]`)
- [ ] `variant` declarations (with payloads)
- [ ] `flags` declarations (bit flags)
- [ ] Inner attributes (`#![...]`)
- [ ] Generic parameters on types

#### Statements

- [x] `let` statements (with `mut`, `reactive`, type annotation)
- [x] Expression statements
- [x] `return` statements
- [x] `if` statements
- [x] `while` loops
- [x] `for` loops (with pattern)
- [ ] `match` statements

#### Expressions

- [x] Identifiers
- [x] Integer literals
- [x] Float literals
- [x] String literals
- [x] Boolean literals (`true`, `false`)
- [x] Unit `()`
- [x] Binary operators (arithmetic, comparison, logical)
- [x] Unary operators (`-`, `!`, `&`, `*`)
- [x] Function calls
- [x] Method calls
- [x] Field access
- [x] Index access (`[]`)
- [x] Closures (`|params| expr`)
- [ ] `if` expressions
- [ ] `match` expressions
- [ ] Block expressions
- [ ] Struct literals (`{ field: value }`)
- [ ] Array literals
- [ ] Tuple expressions
- [ ] Range expressions
- [ ] `?` operator (error propagation)
- [ ] Template string interpolation

#### Types

- [x] Named types
- [x] Generic types (`Array<T>`, `Result<T, E>`)
- [x] Reference types (`&T`)
- [x] Tuple types (`(T, U)`)
- [x] Unit type `()`
- [x] Never type `!`
- [ ] Function types
- [ ] Infix tuple syntax `(a, b)` vs `Tuple<A, B>`

#### Patterns

- [x] Identifier patterns
- [x] Wildcard `_`
- [x] Tuple patterns
- [ ] Literal patterns
- [ ] Struct patterns
- [ ] Enum/variant patterns

### Semantic Analysis

- [x] Symbol table construction
- [x] Module resolution (core library)
- [x] Import resolution
- [x] Builtin function detection
- [ ] Simple type checking
- [ ] Generic type checking
- [ ] Type inference
- [ ] Effect checking
- [ ] Borrow checking / move analysis
- [ ] Scope analysis for variables
- [ ] Unused variable warnings

### Code Generation

- [x] Component Model binary output
- [x] WASI P3 imports (`wasi:cli/stdout`, `wasi:cli/types`)
- [x] Stream intrinsics (`stream.new`, `stream.write`, `stream.drop-*`)
- [x] Async task intrinsics (`task.return`, `waitable-set.*`, `subtask.drop`)
- [x] Memory module with string data
- [x] `println` function (core::cli)
- [x] Multiple function calls
- [x] Async function lifting/lowering
- [ ] Variables and locals
- [ ] Control flow (`if`, `while`, `for`)
- [ ] Binary/unary operations
- [ ] User-defined functions
- [ ] Struct construction
- [ ] Enum/variant construction
- [ ] Pattern matching
- [ ] Closures
- [ ] Effect handlers
- [ ] Reactive signals (source values)
- [ ] Reactive signals (derived values)
- [ ] Reactive effect blocks (syntax TBD)
- [ ] Reactive references (`&reactive T`)
- [ ] Multiple modules/files
- [ ] Other WASI interfaces (filesystem, etc.)

### Testing

- [x] Lexer unit tests
- [x] Parser unit tests
- [x] Analyzer unit tests
- [x] Codegen unit tests
- [x] E2E test: hello world (with wasmtime)
- [x] E2E test: multiple println
- [ ] Compile error tests (partial)
- [ ] More E2E tests

---

## Current Capabilities

The compiler can currently:

1. **Parse** basic Wado programs with:
   - `use` imports from `core::cli`
   - `fn main()` with effect declarations
   - `println("...")` calls

2. **Generate** Component Model Wasm that:
   - Imports WASI P3 `wasi:cli/stdout` and `wasi:cli/types`
   - Uses async stream intrinsics for stdout
   - Runs successfully on wasmtime with P3 support

### Example Working Program

```wado
use {println, Stdout} from "core:cli";

fn main() with Stdout {
    println("Hello, world!");
}
```

---

## Known Limitations

1. **Parser doesn't support generic resources**: `resource Stream<T>` in `prelude.wado` fails to parse
2. **No `variant` keyword**: Parser doesn't recognize `variant` declarations (sum types with payloads)
3. **No `flags` keyword**: Parser doesn't recognize `flags` declarations (bit flags)
4. **Template strings not interpolated**: Backtick strings are parsed as plain strings
5. **No type checking**: The analyzer doesn't perform type checking yet
6. **Limited codegen**: Only `println` with string literals works

---

## Reactive Signals Implementation

Wado's `reactive` keyword compiles to efficient Wasm code with minimal runtime overhead.

### Reactive Value Categories

```wado
let reactive mut count = 0;           // Source: mutable reactive state
let reactive doubled = || count * 2;  // Derived: computed from sources
```

**Sources** are mutable reactive values that can be directly assigned.
**Derived** values are computed from other reactive values (sources or other derived).

### Compilation Strategy

The compiler uses static analysis to build a dependency graph at compile-time:

1. **Dependency Analysis**: Track which derived values read which sources
2. **Topological Sort**: Order updates so dependencies are computed before dependents
3. **Inline Update Code**: At each mutation site, generate code to update all affected derived values

**Example transformation:**

```wado
// Source code
let reactive mut count = 0;
let reactive doubled = || count * 2;
let reactive quadrupled = || doubled * 2;

count = 5;  // Mutation site
```

```wat
;; Conceptual WAT output (simplified)
;; Mutation site: count = 5
(local.set $count (i32.const 5))
;; Update doubled (depends on count)
(local.set $doubled (i32.mul (local.get $count) (i32.const 2)))
;; Update quadrupled (depends on doubled)
(local.set $quadrupled (i32.mul (local.get $doubled) (i32.const 2)))
```

### Effect Blocks (TBD)

> **Note**: The syntax for reactive side effects is under discussion. See spec.md for alternatives.

Effect blocks subscribe to reactive values read within them:

```wado
effect {
    println(`Count is {count}`);
}
```

The compiler:

1. Analyzes which reactive values are read inside the effect block
2. Generates a closure for the effect body
3. Inserts calls to this closure after any mutation of its dependencies

### Execution Context

The compiler generates different code depending on the target world:

**CLI World (Synchronous):**

- Updates propagate immediately at each mutation site
- Effect closures are called inline, synchronously
- No event loop or scheduler needed

```wat
;; CLI: Synchronous update at mutation site
(local.set $count (i32.const 5))
(call $effect_0)  ;; Effect runs immediately
;; Next statement executes after effect completes
```

**Event-looped World (Browser/GUI):**

- Updates may be batched within an event handler
- Compiler generates a scheduler that collects mutations and flushes at end of event
- Effect closures are registered with the reactive runtime

```wat
;; Event-loop: Batched updates
(call $reactive_set (local.get $count_ref) (i32.const 5))  ;; Queues update
(call $reactive_set (local.get $count_ref) (i32.const 6))  ;; Queues another
;; At end of event handler:
(call $reactive_flush)  ;; Runs all effects once with final values
```

### Wasm Representation

| Wado Construct            | Wasm Representation                             |
| ------------------------- | ----------------------------------------------- |
| `reactive mut` source     | Local variable + generated update dispatch      |
| `reactive` derived        | Local variable, recomputed on dependency change |
| `effect { ... }`          | Closure called after dependency mutations       |
| `&reactive T` (reference) | Wasm GC struct ref with getter/setter           |

### Dynamic Dependencies (Future)

For cases where dependencies aren't statically known:

```wado
let reactive computed = || {
    if condition {
        return a + b;
    } else {
        return c + d;
    }
};
```

The compiler may generate runtime tracking when static analysis is insufficient. This uses a lightweight subscription mechanism stored in Wasm GC structs.

### Reactive References

Passing reactive values by reference:

```wado
fn increment(counter: &reactive mut i32) {
    *counter += 1;  // Triggers updates in caller's scope
}

let reactive mut count = 0;
let reactive doubled = || count * 2;
increment(&reactive count);  // doubled gets updated
```

This requires the reactive reference to carry update callback information, implemented as a Wasm GC struct containing the value and a reference to the update dispatcher.

---

## Next Steps (Priority Order)

1. **Add `variant` and `flags` keywords** to lexer/parser
2. **Support generic resources** in parser for `prelude.wado`
3. **Add variable support** in codegen (locals, let bindings)
4. **Add control flow** in codegen (if, while)
5. **Type checking** in analyzer
