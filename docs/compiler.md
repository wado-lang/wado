# Wado Compiler Status

This document tracks the current implementation status of the Wado compiler.

## Architecture

The compiler follows a traditional pipeline:

```
Source (.wado) → Lexer → Parser → Analyzer → Codegen → Component Model Wasm
```

### Modules

| Module   | File                  | Description                                     |
| -------- | --------------------- | ----------------------------------------------- |
| Lexer    | `lexer.rs`            | Tokenizes source code                           |
| Parser   | `parser.rs`           | Recursive descent parser, builds AST            |
| AST      | `ast.rs`              | AST node definitions                            |
| Token    | `token.rs`            | Token types and spans                           |
| Analyzer | `analyze.rs`          | Semantic analysis, symbol table construction    |
| Symbol   | `symbol.rs`           | Symbol table data structures                    |
| Resolver | `resolver.rs`         | Module resolution, loads core library           |
| Stdlib   | `stdlib.rs`           | Embedded core library sources                   |
| Codegen  | `codegen.rs`          | Generates Component Model Wasm via wasm-encoder |
| Bundled  | `bundled.rs`          | Loads pre-compiled Wasm builtins (wado-bundled) |
| Postproc | `wasm_postprocess.rs` | Wasm binary transformations                     |

### Bundled Builtins (wado-bundled)

The `wado-bundled` crate provides pre-compiled Wasm functions for operations that are complex to implement in pure Wasm instructions. These are statically linked into the generated component.

**Location:** `wado-bundled/` (compiles to `wasm32-unknown-unknown`)

**Current Functions:**

| Function        | Signature           | Description                          |
| --------------- | ------------------- | ------------------------------------ |
| `f64_to_buffer` | `(f64, i32) -> i32` | Format f64 to buffer, returns length |
| `f32_to_buffer` | `(f32, i32) -> i32` | Format f32 to buffer, returns length |

**Build Process:**

```bash
make update-bundled   # Rebuild wado-bundled.wat from Rust source
make check-bundled    # Verify committed WAT is up-to-date (used in CI)
```

The bundled module is stored as WAT in `wado-compiler/lib/builtins/wado-bundled.wat` for version control visibility. It's parsed at compile time using the `wat` crate.

### Standard Library

Embedded `.wado` files in `wado-compiler/lib/`:

**Core Library (`core/`):**

| Module            | File              | Status                                             |
| ----------------- | ----------------- | -------------------------------------------------- |
| `core:prelude`    | `prelude.wado`    | Partial (parser doesn't support generic resources) |
| `core:cli`        | `cli.wado`        | Complete                                           |
| `core:filesystem` | `filesystem.wado` | Complete                                           |
| `core:stream`     | `stream.wado`     | Complete                                           |
| `core:internals`  | `internals.wado`  | Internal (compiler-generated code support)         |

**WASI Library (`wasi/`):**

| Module            | File              | Status   |
| ----------------- | ----------------- | -------- |
| `wasi:io`         | `io.wado`         | Complete |
| `wasi:cli`        | `cli.wado`        | Complete |
| `wasi:filesystem` | `filesystem.wado` | Complete |

### Type System

**Primitive Layer (`builtin::`):**

The `builtin` namespace provides direct access to Wasm primitives. These types and functions map 1:1 to Wasm instructions with no abstraction. The namespace is always available without import, but is intended primarily for standard library implementation.

**Wasm GC Types:**

```wado
builtin::array<T>    // Wasm GC array (no methods)
builtin::i31ref      // Wasm GC i31ref (31-bit integer reference)
```

**Intrinsic Functions:**

```wado
// Array operations
builtin::array_new<T>(len: i32) -> builtin::array<T>
builtin::array_len<T>(arr: builtin::array<T>) -> i32
builtin::array_get<T>(arr: builtin::array<T>, idx: i32) -> T
builtin::array_set<T>(arr: builtin::array<T>, idx: i32, value: T)
builtin::array_get_u8(arr: builtin::array<u8>, idx: i32) -> i32  // Unsigned byte read

// i31ref operations
builtin::i31ref_new(value: i32) -> builtin::i31ref
builtin::i31ref_get_s(ref: builtin::i31ref) -> i32   // Signed extraction
builtin::i31ref_get_u(ref: builtin::i31ref) -> u32   // Unsigned extraction

// Reference comparison (Wasm ref.eq)
builtin::eqref<T, U>(a: T, b: U) -> bool   // Compare any GC references

// Control
builtin::unreachable() -> !   // Wasm trap instruction

// i64 bit manipulation
builtin::i64_low32(value: i64) -> i32    // Extract low 32 bits
builtin::i64_high32(value: i64) -> i32   // Extract high 32 bits

// i32 operations
builtin::i32_and(a: i32, b: i32) -> i32  // Bitwise AND
builtin::i32_eqz(a: i32) -> i32          // Check if zero (returns 0 or 1)

// Linear memory operations
builtin::memory_store8(addr: i32, value: i32)  // Store byte to memory
builtin::memory_load8_u(addr: i32) -> i32      // Load unsigned byte from memory
builtin::realloc(oldptr: i32, oldsize: i32, align: i32, newsize: i32) -> i32

// Stream intrinsics (Component Model)
builtin::stream_new() -> i64              // Create stream, returns rx|tx packed
builtin::stream_write(tx: i32, ptr: i32, len: i32) -> i32
builtin::stream_drop_writable(tx: i32)
builtin::stream_drop_readable(rx: i32)

// Async task intrinsics (Component Model)
builtin::waitable_set_new() -> i32
builtin::waitable_join(set: i32, subtask: i32)
builtin::waitable_set_wait(set: i32, outptr: i32) -> i32
builtin::subtask_drop(subtask: i32)
```

**Usage in Standard Library:**

```wado
// Standard library uses builtin primitives internally
// In core/string.wado
pub struct String {
    buf: builtin::array<u8>,

    pub fn length(&self) -> i32 {
        return builtin::array_len(self.buf);
    }
}

// In core/prelude.wado
pub fn unreachable() -> ! {
    builtin::unreachable()
}
```

**Standard Library Types:**

Standard library types wrap builtins with methods:

- `String` - Struct wrapping `builtin::array<u8>` (maps to CM `string`)
- `Array<T>` - Struct wrapping `builtin::array<T>` (maps to CM `list<T>`)

**Struct Implementation:**

- Internally: Wasm-GC `struct` type with GC-managed memory
- At CM boundary: Automatically converted to/from `record`
- Enables recursive types, self-referential structures, and efficient field access

**Single-Field Optimization:**

If a struct contains exactly one GC object field (a `builtin::array` or another struct), the compiler skips generating the outer Wasm GC struct. This means wrapper types like `String` and `Array<T>` have **zero runtime overhead**:

```wado
// String wraps builtin::array<u8>
struct String {
    buf: builtin::array<u8>,
    // ... methods
}
// At Wasm level: compiles to just (ref (array u8)), no wrapper struct

// Array<T> wraps builtin::array<T>
struct Array<T> {
    repr: builtin::array<T>,
    // ... methods
}
// At Wasm level: compiles to just (ref (array T)), no wrapper struct
```

This optimization enables ergonomic APIs with methods while maintaining direct Wasm GC representation.

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
- [x] Character literals (single quotes)
- [x] Template strings (backticks with interpolation `{expr}`)
- [x] Operators (`+`, `-`, `*`, `/`, `%`, `==`, `!=`, `<`, `<=`, `>`, `>=`, `&&`, `||`)
- [x] Bitwise operators (`&`, `|`, `^`, `~`, `<<`, `>>`)
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
- [x] `assert` statements (condition check, unreachable on failure)
- [x] `if` statements
- [x] `while` loops
- [x] C-style `for` loops
- [ ] `for-of` loops
- [ ] `match` statements

#### Expressions

- [x] Identifiers
- [x] Integer literals
- [x] Float literals
- [x] String literals
- [x] Character literals
- [x] Boolean literals (`true`, `false`)
- [x] Null literal (`null`)
- [x] Unit `()`
- [x] Template string interpolation (`` `Hello, {name}!` ``)
- [x] Binary operators (arithmetic, comparison, logical, bitwise)
- [x] Unary operators (`-`, `!`, `~`, `&`, `*`)
- [x] Parentheses for grouping `(expr)`
- [x] Function calls
- [x] Method calls
- [x] Field access
- [x] Index access (`[]`)
- [x] Type cast (`as T`) for primitive types
- [x] Closures (`|params| expr`)
- [ ] `if` expressions
- [ ] `match` expressions
- [ ] Block expressions
- [ ] Struct literals (`{ field: value }`)
- [ ] Array literals
- [ ] Tuple expressions
- [ ] Range expressions
- [ ] `?` operator (error propagation)

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
- [x] Template strings (literals, integer interpolation, float interpolation via wado-bundled)
- [x] Variables and locals (`let`, `let mut`)
- [x] Control flow (`if` statements, `while`, `for`)
- [x] Binary/unary operations (arithmetic, comparison, logical, bitwise)
- [x] Type cast (`as T`) for primitive types (i32, i64, f32, f64)
- [x] Assert statements (condition check, unreachable on failure)
- [x] User-defined functions (from core:: modules)
- [ ] Struct construction
- [ ] Enum/variant construction
- [ ] Pattern matching
- [ ] Closures
- [ ] Effect handlers
- [x] Template string type conversion (i32/i64 → string, f32/f64 → string via wado-bundled)
- [ ] Template string format specifiers (`.2f`, `0.3f`, etc.)
- [x] Template string array concatenation
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
- [x] Template string tests (20 comprehensive tests)
- [x] E2E test: hello world (with wasmtime)
- [x] E2E test: multiple println
- [x] E2E test: bitwise operators (`&`, `|`, `^`, `~`, `<<`, `>>`)
- [x] E2E test: parentheses for precedence grouping
- [x] E2E test: float-to-string template interpolation
- [x] E2E test: type cast (`as T`) for primitive types
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
4. **Template strings - mostly implemented**:
   - ✅ Syntax parsing with interpolation `{expr}` works
   - ✅ Format specifiers (`:`) vs scope resolution (`::`) correctly distinguished
   - ✅ Nested template strings supported
   - ✅ Integer interpolation (i32/i64 → string)
   - ✅ Float interpolation (f32/f64 → string via wado-bundled)
   - ✅ String concatenation with GC array copy
   - ❌ Format specifiers (`.2f`, etc.) not implemented in codegen
5. **No type checking**: The analyzer doesn't perform type checking yet
6. **Limited codegen**: Only `println` with string literals works
7. **GC arrays cannot be passed directly to streams**: As of wasmtime v40, `stream<u8>` operations require linear memory. GC arrays must be copied to linear memory before writing to streams. See [component-model#525](https://github.com/WebAssembly/component-model/issues/525)
8. **Non-pub functions from other modules are skipped**: The codegen currently only includes `pub` functions from imported modules (`core::*`). Internal helper functions must be marked `pub` to be included in compilation. This limitation could be addressed later with proper internal dependency tracking.

---

## Template String Interpolation

Wado supports template strings with interpolation using backticks and `{expr}` syntax, similar to JavaScript but with Python-like format specifiers.

### Syntax

```wado
let name = "Alice";
let age = 30;

// Basic interpolation
let greeting = `Hello, {name}!`;

// Complex expressions
let message = `Sum: {a + b * c}`;

// Format specifiers (Python-like)
let pi = 3.14159;
let formatted = `Pi: {pi:.2f}`;        // "Pi: 3.14"
let padded = `Value: {x:0.3f}`;        // Zero padding

// Method calls
let text = `Length: {name.len()}`;

// Nested templates
let outer = `Outer {`Inner {x}`}`;
```

### Implementation Status

#### ✅ Fully Implemented (Lexer & Parser)

**Lexer (`lexer.rs`)**:

- Backtick string tokenization with `TemplateStringLit` token
- Brace depth tracking to handle nested `{}` in interpolations
- String literal tracking inside interpolations
- Escape sequence support (`\n`, `\t`, `\uHHHH`, etc.)
- Nested template string support

**Parser (`parser.rs`)**:

- Template string AST nodes (`TemplateStringExpr`, `TemplatePart`, `FormatSpec`)
- Interpolation expression parsing (any valid expression)
- Format specifier extraction after `:`
- **`:` vs `::` distinction**: Single-character lookahead to differentiate:
  - `:` alone → format specifier start
  - `::` → scope resolution (part of expression)
- Recursive parsing for nested template strings
- Comprehensive error handling

**Test Coverage**:

- 20 comprehensive test cases covering:
  - Basic interpolation
  - Complex expressions
  - Format specifiers
  - Nested templates
  - Edge cases (empty, consecutive interpolations, etc.)
  - Error cases (unterminated, empty interpolation, etc.)

#### ✅ Implemented (Codegen)

**What Works (`codegen.rs`)**:

- String literal parts are collected and embedded in data section
- Interpolation expressions are evaluated
- Template strings produce `ref (array u8)` type
- Integer interpolation (i32/i64 converted to decimal string in linear memory, then copied to GC array)
- Float interpolation (f32/f64 via `wado-bundled` functions using the `ryu` algorithm)
- String concatenation using GC array allocation and `array.copy`

**What's Missing (TODO)**:

1. **Format Specifiers**:

   ```wado
   `{pi:.2f}`       // Decimal precision
   `{x:0.3f}`       // Zero padding
   `{n:d}`          // Integer formatting
   ```

   Format specs are parsed but ignored in codegen.

2. **Boolean to String**:

   ```wado
   `Flag: {true}`   // Need bool → string ("true"/"false")
   ```

### AST Structure

```rust
pub struct TemplateStringExpr {
    pub parts: Vec<TemplatePart>,
    pub span: Span,
}

pub enum TemplatePart {
    String(String),                    // Literal string
    Interpolation {
        expr: Box<Expr>,               // Expression to interpolate
        format: Option<FormatSpec>,    // Optional format spec
    },
}

pub struct FormatSpec {
    pub spec: String,  // e.g., ".2f", "0.3f", "10"
}
```

### Next Steps for Full Implementation

1. **Implement format specifier handling**:
   - Parse format spec (precision, padding, alignment)
   - Pass to appropriate formatting function in wado-bundled

2. **Add boolean to string conversion**:
   - `true` → "true", `false` → "false"

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
