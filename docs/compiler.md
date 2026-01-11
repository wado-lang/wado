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

// i31ref operations
builtin::i31ref_new(value: i32) -> builtin::i31ref
builtin::i31ref_get_s(ref: builtin::i31ref) -> i32   // Signed extraction
builtin::i31ref_get_u(ref: builtin::i31ref) -> u32   // Unsigned extraction

// Reference comparison (Wasm ref.eq)
builtin::eqref<T, U>(a: T, b: U) -> bool   // Compare any GC references

// Control
builtin::unreachable() -> !   // Wasm trap instruction
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
- [x] `if` statements
- [x] `while` loops
- [x] `for` loops (with pattern)
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
- [x] Bitwise operators (`&`, `|`, `^`, `<<`, `>>`)
- [x] Unary operators (`-`, `!`, `~`, `&`, `*`)
- [x] Operator precedence (Rust-style, bitwise > comparison)
- [x] Comparison chaining (`a < b < c`, parser support only)
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
- [ ] Comparison chaining validation (same-direction only, no `!=` chaining)

### Code Generation

- [x] Component Model binary output
- [x] WASI P3 imports (`wasi:cli/stdout`, `wasi:cli/types`)
- [x] Stream intrinsics (`stream.new`, `stream.write`, `stream.drop-*`)
- [x] Async task intrinsics (`task.return`, `waitable-set.*`, `subtask.drop`)
- [x] Memory module with string data
- [x] `println` function (core::cli)
- [x] Multiple function calls
- [x] Async function lifting/lowering
- [x] Template strings (partial - literals only, no type conversion/formatting)
- [x] Binary/unary operations (arithmetic, comparison, logical, bitwise)
- [x] Bitwise operators (`&`, `|`, `^`, `<<`, `>>`, `~`)
- [ ] Variables and locals (partial - only in specific contexts)
- [ ] Control flow (`if`, `while`, `for`)
- [ ] User-defined functions (partial - basic support)
- [ ] Struct construction
- [ ] Enum/variant construction
- [ ] Pattern matching
- [ ] Closures
- [ ] Effect handlers
- [ ] Template string type conversion (i32/f64 → string)
- [ ] Template string format specifiers (`.2f`, `0.3f`, etc.)
- [ ] Template string array concatenation
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
- [x] E2E test: operator precedence (bitwise operators)
- [ ] E2E test: comparison chaining (needs semantic validation)
- [ ] Compile error tests (partial)
- [ ] Template string E2E tests (runtime execution)
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
4. **Template strings - partial implementation**:
   - ✅ Syntax parsing with interpolation `{expr}` works
   - ✅ Format specifiers (`:`) vs scope resolution (`::`) correctly distinguished
   - ✅ Nested template strings supported
   - ❌ No type conversion (i32/f64 → string) in codegen
   - ❌ Format specifiers (`.2f`, etc.) not implemented in codegen
   - ❌ String concatenation uses placeholder implementation
5. **No type checking**: The analyzer doesn't perform type checking yet
6. **Limited codegen**: Only `println` with string literals works

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

#### ⚠️ Partial Implementation (Codegen)

**What Works (`codegen.rs`)**:

- String literal parts are collected and embedded in data section
- Interpolation expressions are evaluated
- Template strings recognized as producing `ref (array u8)` type
- Basic structure for concatenation (locals allocated)

**What's Missing (TODO)**:

1. **Type-to-String Conversion**:

   ```wado
   `Count: {42}`    // Need i32 → string
   `Pi: {3.14}`     // Need f64 → string
   `Flag: {true}`   // Need bool → string
   ```

   Currently assumes all interpolated expressions are already strings.

2. **Format Specifiers**:

   ```wado
   `{pi:.2f}`       // Decimal precision
   `{x:0.3f}`       // Zero padding
   `{n:d}`          // Integer formatting
   ```

   Format specs are parsed but ignored in codegen.

3. **String Concatenation**:
   Currently uses placeholder that only keeps the last part:

   ```rust
   // TODO: Implement proper array concatenation
   // Current: just overwrites with each part (incorrect)
   func.instruction(&Instruction::LocalSet(result_local));
   ```

   Proper implementation needs:
   - Calculate total length of all parts
   - Allocate new GC array with total length
   - Copy each part into correct offset using `array.copy`

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

1. **Add `to_string()` intrinsics** for primitive types:

   ```wado
   builtin::i32_to_string(value: i32) -> builtin::array<u8>
   builtin::f64_to_string(value: f64) -> builtin::array<u8>
   builtin::bool_to_string(value: bool) -> builtin::array<u8>
   ```

2. **Implement format specifier handling**:
   - Parse format spec (precision, padding, alignment)
   - Pass to appropriate formatting intrinsic

3. **Implement efficient array concatenation**:

   ```wat
   ;; Calculate total length
   (i32.add (array.len $part1) (i32.add (array.len $part2) ...))
   ;; Allocate result array
   (array.new_default $array_u8 (local.get $total_len))
   ;; Copy each part
   (array.copy $array_u8 $array_u8
     (local.get $result)  ;; dest
     (i32.const 0)        ;; dest offset
     (local.get $part1)   ;; src
     (i32.const 0)        ;; src offset
     (array.len $part1))  ;; length
   ;; Repeat for each part at appropriate offset
   ```

4. **Add E2E tests** for runtime execution with wasmtime

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

## Operator Precedence Implementation

### Status: ✅ Implemented (2026-01-11)

Wado implements Rust-style operator precedence, which fixes C's historical design flaw where bitwise operators have lower precedence than comparison operators.

### Implementation Details

**Tokens (`token.rs`)**:
- `LShift` (`<<`) - Left shift
- `RShift` (`>>`) - Right shift
- `Tilde` (`~`) - Bitwise NOT
- `Caret` (`^`) - Bitwise XOR
- Note: `Ampersand` (`&`) and `Pipe` (`|`) were already present for references and closures

**Lexer (`lexer.rs`)**:
- Split `<<` and `>>` from `<` and `>` with two-character lookahead
- Added `~` and `^` tokenization
- Special handling for `>>` in generic types (e.g., `Array<Tuple<String, String>>`)

**AST (`ast.rs`)**:
- Added `BinaryOp`: `BitAnd`, `BitOr`, `BitXor`, `LShift`, `RShift`
- Added `UnaryOp`: `BitNot` (`~`)

**Parser (`parser.rs`)**:

Precedence chain (highest to lowest):
```
parse_expr
  → parse_assignment_expr
    → parse_or_expr (||)
      → parse_and_expr (&&)
        → parse_equality_expr (==, !=)
          → parse_comparison_expr (<, <=, >, >=)
            → parse_bitor_expr (|)
              → parse_bitxor_expr (^)
                → parse_bitand_expr (&)
                  → parse_shift_expr (<<, >>)
                    → parse_additive_expr (+, -)
                      → parse_multiplicative_expr (*, /, %)
                        → parse_unary_expr (!, ~, -, &, *)
                          → parse_postfix_expr
```

Key features:
- **Bitwise operators have higher precedence than comparison**: `flags & mask == expected` correctly parses as `(flags & mask) == expected`
- **Comparison chaining supported**: `a < b < c` parses as left-associative chain
- **`>>` token splitting**: `expect_gt()` helper splits `>>` into two `>` tokens for nested generics

**Codegen (`codegen.rs`)**:
- Maps bitwise operators to Wasm instructions:
  - `BitAnd` → `i32.and`
  - `BitOr` → `i32.or`
  - `BitXor` → `i32.xor`
  - `LShift` → `i32.shl`
  - `RShift` → `i32.shr_s` (arithmetic right shift)
  - `BitNot` → `i32.const -1` + `i32.xor`
- Unary minus for integers: `i32.const 0` - value

**Tests (`tests/e2e.rs`, `tests/fixtures/`)**:
- ✅ `operator_precedence_bitwise.wado` - All bitwise operators and precedence
- ⚠️ `operator_precedence_comparison_chaining.wado` - Parser support, needs semantic validation
- ⚠️ `operator_precedence_comprehensive.wado` - Complex precedence tests, some need chaining validation

### Test Results

```bash
cargo test --test e2e test_operator_precedence_bitwise
# ✅ PASSED - All bitwise operator precedence tests pass
```

Example working code:
```wado
let flags = 0b1010;
let mask = 0b0010;
let expected = 0b0010;

// Correctly parses as (flags & mask) == expected
if flags & mask == expected {
    println("bitwise and precedence works");  // ✅ Prints
}
```

### Remaining Work

**Semantic Analysis (TODO)**:

Comparison chaining is currently parsed but not validated. Need to add semantic checks:

1. **Same-direction validation**: Reject mixed chains like `a < b > c`
2. **`!=` chaining rejection**: `a != b != c` should be a semantic error
3. **Type consistency**: All operands in chain should have compatible types

Example invalid code that currently parses:
```wado
if a < b > c {  // Should be semantic error: mixed directions
    // ...
}

if a != b != c {  // Should be semantic error: != chaining not allowed
    // ...
}
```

### Design Documentation

- **Research**: `docs/operator-precedence-research.md` - Comprehensive analysis across languages
- **ADR**: `docs/adr-2026-01-11-operator-precedence.md` - Architectural decision record
- **Spec**: `spec.md` - User-facing specification with examples

### Key Design Decisions

1. **Follow Rust's model**: Bitwise > Comparison (fixes C's flaw)
2. **No `++`/`--` operators**: Avoid undefined behavior, use `+=`/`-=`
3. **No `**` power operator**: Use explicit `pow()` function
4. **Mathematical comparison chaining**: Similar to Python, with stricter validation
5. **Arithmetic right shift**: `>>` for signed integers uses `i32.shr_s`

---

## Next Steps (Priority Order)

1. **Comparison chaining semantic validation** - Add validation for same-direction chains and reject `!=` chaining
2. **Add `variant` and `flags` keywords** to lexer/parser
3. **Support generic resources** in parser for `prelude.wado`
4. **Add variable support** in codegen (locals, let bindings)
5. **Add control flow** in codegen (if, while)
6. **Type checking** in analyzer
