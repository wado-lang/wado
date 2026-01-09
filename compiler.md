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

### Core Library

Embedded `.wado` files in `wado-compiler/core/`:

| Module             | File              | Status                                             |
| ------------------ | ----------------- | -------------------------------------------------- |
| `core::prelude`    | `prelude.wado`    | Partial (parser doesn't support generic resources) |
| `core::cli`        | `cli.wado`        | Complete                                           |
| `core::filesystem` | `filesystem.wado` | Complete                                           |

---

## Feature Checklist

### Lexer

- [x] Keywords (`fn`, `let`, `use`, `if`, `while`, `for`, `match`, `return`, etc.)
- [x] Keywords (`pub`, `effect`, `struct`, `enum`, `type`, `impl`, `resource`, `world`)
- [x] Keywords (`record` - legacy, to be removed)
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
- [x] `struct` declarations
- [x] `record` declarations (legacy, to be unified with struct)
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
- [ ] Type checking
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
use core::cli::{println, Stdout};

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
7. **Spec divergence**: Parser still supports `record` keyword separately from `struct` (spec unified them)

---

## Next Steps (Priority Order)

1. **Add `variant` and `flags` keywords** to lexer/parser
2. **Support generic resources** in parser for `prelude.wado`
3. **Add variable support** in codegen (locals, let bindings)
4. **Add control flow** in codegen (if, while)
5. **Type checking** in analyzer
