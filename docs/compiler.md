# Wado Compiler

This document describes the Wado compiler architecture and implementation status.

## Compiler Architecture

The compiler follows a multi-phase pipeline:

```
Source (.wado) → Lexer → Parser → Bind → Load → Analyze → Resolve → Synthesis → Effect Check → Stores Check → Erase Newtypes/Flags → Link → Monomorphize → Lower → Optimize → WIR Build → WIR Optimize → Codegen
```

### Compilation Pipeline

| Phase        | Input         | Output          | Description                                               |
| ------------ | ------------- | --------------- | --------------------------------------------------------- |
| Lexer        | Source        | Tokens          | Tokenize, extract `__DATA__` section                      |
| Parser       | Tokens        | AST             | Build abstract syntax tree                                |
| Bind         | AST           | AST (validated) | Local name resolution, scope/mutability checking          |
| Load         | AST           | All modules     | Load dependencies; each module: parse → bind → desugar    |
| Analyze      | All modules   | Symbol table    | Build symbol table, validate imports                      |
| Resolve      | AST + Symbols | Package         | Type resolution, produce Package                          |
| Synthesis    | Package       | Package         | Eq/Ord/serde/CM binding/template/inspect synthesis        |
| Effect Check | Package       | Package         | Validate function effect requirements                     |
| Stores Check | Package       | Package         | Validate reference storage declarations                   |
| Erase Types  | Package       | Package         | Erase newtypes and flags to base types                    |
| Link         | Package       | FlatPackage     | Merge per-module TIR into flat lists                      |
| Monomorphize | FlatPackage   | FlatPackage     | Instantiate generics with concrete types                  |
| Lower        | FlatPackage   | FlatPackage     | Closure, i128 match, global init, string literal lowering |
| Optimize     | FlatPackage   | FlatPackage     | Inlining, copy-prop, LICM, DCE, post-opt rewrite          |
| WIR Build    | FlatPackage   | WirPackage      | Planning + TIR → WIR (Wasm IR) translation                |
| WIR Optimize | WirPackage    | WirPackage      | Multi-value SROA, array data promotion, peephole          |
| Codegen      | WirPackage    | Wasm bytes      | WIR emission to core Wasm + Component Model wrapping      |

**Note:** The Desugar phase is integrated into the Load phase. Each module goes through the same frontend pipeline: `lexer → parser → bind → desugar`.

### Modules

| Module          | File                                 | Description                                                                                              |
| --------------- | ------------------------------------ | -------------------------------------------------------------------------------------------------------- |
| Lexer           | `lexer.rs`                           | Tokenizes source code, extracts `__DATA__` section                                                       |
| Parser          | `parser.rs`                          | Recursive descent parser, builds AST                                                                     |
| AST             | `ast.rs`                             | AST node definitions, `Module::data_section()` API                                                       |
| Token           | `token.rs`                           | Token types and spans                                                                                    |
| Syntax          | `syntax.rs`                          | Syntax definitions (keywords, operators)                                                                 |
| Comment         | `comment.rs`                         | Comment collection and CommentMap for formatting                                                         |
| Bind            | `bind.rs`                            | Local name binding, scope analysis, mutability check                                                     |
| Loader          | `loader.rs`                          | Module loading, dependency resolution                                                                    |
| Desugar         | `desugar.rs`                         | AST transformations (compound assign, etc.)                                                              |
| EffectCheck     | `effect_check.rs`                    | Validates effect requirements and stores declarations                                                    |
| Unparser        | `unparse.rs`                         | Converts AST/TIR back to source code                                                                     |
| Analyzer        | `analyze.rs`                         | Semantic analysis, symbol table construction                                                             |
| Symbol          | `symbol.rs`                          | Symbol table data structures                                                                             |
| Name            | `name.rs`                            | Name mangling utilities for methods and symbols                                                          |
| Resolver        | `resolver.rs`                        | Type resolution, AST to TIR, produces Package (`resolver/`)                                              |
| TIR             | `tir.rs`                             | Typed Intermediate Representation                                                                        |
| Synthesis       | `synthesis.rs`                       | Unified synthesis phase (`synthesis/`)                                                                   |
| SynthCommon     | `synthesis/common.rs`                | Shared TIR builders for synthesis phases                                                                 |
| SynthSerde      | `synthesis/serde_synth.rs`           | Synthesized Serialize/Deserialize for structs                                                            |
| SynthTraits     | `synthesis/traits.rs`                | Auto-derived Eq/Ord/Display/Inspect for types                                                            |
| SynthTemplate   | `synthesis/template.rs`              | Template string expansion                                                                                |
| SynthFrom       | `synthesis/from_synth.rs`            | From trait synthesis                                                                                     |
| SynthCmBinding  | `synthesis/cm_binding.rs`            | CM boundary adapter synthesis (TIR functions)                                                            |
| CmAbi           | `cm_abi.rs`                          | Canonical ABI layout computation                                                                         |
| Monomorphize    | `monomorphize.rs`                    | Generic type/function instantiation (FlatPackage→FlatPackage)                                            |
| Lower           | `lower.rs`                           | Lowering coordinator (`lower/`)                                                                          |
| LowerWideInt    | `lower/wide_int.rs`                  | i128/u128 match pattern → if-else chains                                                                 |
| LowerPattern    | `lower/pattern.rs`                   | LetPattern/IfPattern → explicit statements + switch                                                      |
| LowerGlobals    | `lower/globals.rs`                   | Global initializer extraction + `__initialize_modules`                                                   |
| LowerBoxing     | `lower/boxing.rs`                    | `&primitive` → `Box<T>` struct lowering                                                                  |
| LowerClosure    | `lower/closure.rs`                   | Closure → functor struct with `__call` methods                                                           |
| LowerString     | `lower/string.rs`                    | String/bytes literal collection for data section                                                         |
| Package         | `package.rs`                         | Package: per-module compilation context (resolve → lower)                                                |
| FlatPackage     | `flat_package.rs`                    | FlatPackage: flat compilation context (link → codegen)                                                   |
| Link            | `link.rs`                            | Merges per-module Package into flat FlatPackage                                                          |
| Optimize        | `optimize.rs`                        | Optimization coordinator (`optimize/`)                                                                   |
| ConstFolding    | `optimize/const_folding.rs`          | Constant folding for integer/float arithmetic                                                            |
| ConstProp       | `optimize/const_propagation.rs`      | Constant propagation for immutable globals                                                               |
| ConstGlobal     | `optimize/const_global_promotion.rs` | Promote runtime globals to compile-time constants                                                        |
| ConstBranch     | `optimize/const_branch_prune.rs`     | Dead branch elimination for known-false conditions                                                       |
| DCE             | `optimize/dce.rs`                    | Dead code elimination via reachability analysis                                                          |
| Inline          | `optimize/inline.rs`                 | Function inlining for small, pure functions                                                              |
| RefElim         | `optimize/ref_elim.rs`               | Reference elimination after inlining                                                                     |
| CopyProp        | `optimize/copy_prop.rs`              | Copy propagation for trivial bindings                                                                    |
| SROA            | `optimize/sroa.rs`                   | Scalar replacement of aggregates (struct/tuple elim)                                                     |
| LICM            | `optimize/licm.rs`                   | Loop-invariant code motion                                                                               |
| SelectLower     | `optimize/select_lowering.rs`        | if/else → Wasm `select` instruction                                                                      |
| FieldScalarize  | `optimize/field_scalarize.rs`        | Hot field scalarization from GC structs                                                                  |
| BlockFusion     | `optimize/labeled_block_fusion.rs`   | Labeled block fusion                                                                                     |
| StoreLoadFwd    | `optimize/store_load_forward.rs`     | Store-load forwarding for literal values                                                                 |
| CondImplication | `optimize/condition_implication.rs`  | Condition implication from dominating guards                                                             |
| TmplHoist       | `optimize/tmpl_hoist.rs`             | Template buffer hoisting out of loops                                                                    |
| ComponentPlan   | `wir_build/component_plan.rs`        | `ComponentPlan` types and `build_component_plan`                                                         |
| WIR Translate   | `wir_build/translate.rs` + siblings  | Function-body TIR→WIR: driver + helpers (primitive_ops, calls, canonical_abi, pattern_match)             |
| Stdlib          | `stdlib.rs`                          | Embedded core library sources                                                                            |
| CompilerHost    | `compiler_host.rs`                   | I/O abstraction for the compiler                                                                         |
| Logger          | `logger.rs`                          | Diagnostic logging with timestamps                                                                       |
| ComponentModel  | `component_model.rs`                 | WASI import registry and CM ABI type support                                                             |
| BuiltinRegistry | `builtin_registry.rs`                | Builtin function registry from `core:builtin`                                                            |
| Doc             | `doc.rs`                             | Documentation generation from AST                                                                        |
| HashMap         | `hashmap.rs`                         | Deterministic `IndexMap`/`IndexSet` type aliases                                                         |
| TirVisitor      | `tir_visitor.rs`                     | TIR visitor traits (`TirMutVisitor`/`TirRefVisitor`/`TirOptVisitor`) + utilities                         |
| WorldRegistry   | `world_registry.rs`                  | World definitions registry for export signatures                                                         |
| WIR             | `wir.rs`                             | Wasm IR data structures                                                                                  |
| WIR Unparse     | `wir_unparse.rs`                     | WIR → pseudo-Wado source code for debugging                                                              |
| WIR Build       | `wir_build.rs`                       | Planning + TIR→WIR translation (`wir_build/`)                                                            |
| WIR Optimize    | `wir_optimize.rs`                    | WIR-level optimizations (multi-value SROA, etc.)                                                         |
| Codegen         | `codegen.rs`                         | WIR→Wasm emission + Component Model wrapping (`codegen/`)                                                |
| Bundled         | `bundled.rs`                         | Loads pre-compiled Wasm builtins (wado-bundled-libm)                                                     |

---

## Module Details

### Parser and Desugar Separation

The parser preserves source syntax literally to enable accurate formatting via the unparser. Syntactic sugar is transformed in the desugar pass, which runs during module loading (not as a separate top-level phase).

| Construct              | Parser Output           | Desugar Output                  |
| ---------------------- | ----------------------- | ------------------------------- |
| `x += y`               | `CompoundAssignExpr`    | `AssignExpr` with `BinaryExpr`  |
| `a < b < c`            | `ComparisonChainExpr`   | `BinaryExpr` chain with `&&`    |
| `&self`                | `Param` with `SelfKind` | (preserved, handled in codegen) |
| `{ x }` (struct field) | `is_shorthand: true`    | (preserved for formatting)      |

This separation ensures:

- `wado format` outputs the original syntax (e.g., `x += 1` not `x = x + 1`)
- Codegen receives simplified AST without syntactic variants

### TIR Unparser

The `unparse.rs` module also provides a TIR unparser that converts Typed IR back to pseudo-Wado source code. This is useful for debugging the monomorphization and lowering phases.

**Usage:**

```sh
wado dump --tir-resolved file.wado   # Show TIR before monomorphization
wado dump --tir-lowered file.wado    # Show TIR after monomorphization and lowering
wado dump --tir file.wado            # Show final TIR (after optimization)
```

**Output Characteristics:**

- `--tir-resolved`: Shows generic types as-is (e.g., `Box<T>`)
- `--tir-lowered`: Shows monomorphized type names (e.g., `Box$i32` instead of `Box<T>`)
- Includes fully qualified function calls (e.g., `core::cli::println`)
- Preserves the `__DATA__` section if present
- Output is pseudo-Wado (not compilable due to mangled names)

**Example Output:**

```wado
struct Box$i32 {
    value: i32,
}

fn run() with Stdout {
    let b: Box$i32 = Box$i32 { value: 42 };
    core::cli::println(core::internal::string_concat("value: ", b.value.to_string()));
}
```

### Bundled Library (wado-bundled-libm)

The `wado-bundled-libm` crate provides pre-compiled Wasm math functions (deterministic libm). These are statically linked into the generated component.

**Location:** `wado-bundled-libm/` (compiles to `wasm32-unknown-unknown`)

Float-to-string formatting was previously a bundled Wasm module but is now implemented in pure Wado (`core:prelude/fpfmt.wado`).

### Monomorphization

The `monomorphize.rs` module is a dedicated compilation phase that instantiates generic structs and functions with concrete types. It runs after link (on a FlatPackage) and before the lower phase.

**Process:**

1. **Collect generic definitions**: Gather all generic struct and function definitions from all modules
2. **Find instantiation sites**: Scan for `GenericInstance` types and generic function calls
3. **Instantiate structs**: Create concrete struct definitions with substituted field types
4. **Instantiate functions**: Create concrete function definitions with substituted types
5. **Rewrite types**: Replace all `GenericInstance` type references with concrete struct types
6. **Rewrite calls**: Replace generic function calls with calls to monomorphized functions
7. **Transitive instantiation**: Iteratively process new instantiations created during monomorphization

**Cross-Module Support:**

The monomorphizer operates on the flat function/struct lists after the link phase. Generic functions defined in one module (e.g., `Array` methods from prelude) can be instantiated when used in another module. Generic structs are keyed by `(name, module_source)` to allow same-named generics from different modules to coexist.

**Name Mangling:**

```
// Struct types
Box<i32>           → Box$i32
Pair<i32, String>  → Pair$i32$String
Box<Box<i32>>      → Box$Box$i32

// Generic functions (suffix is unique instantiation ID)
identity::<i32>    → identity$1
identity::<i64>    → identity$2

// Generic methods
Container::transform::<i32, i64> → Container::transform$1
```

**Variadic Type Packs:**

During monomorphization, `TirExprKind::TupleSpread` nodes (generated by the resolver for `[..expr]` inside variadic functions) are expanded into individual `FieldAccess` elements. The tuple literal's type is rebuilt from the expanded element types. Non-variadic spread expressions (concrete tuples) are expanded at resolve time; the resolver introduces temporary let-bindings for non-trivial expressions to ensure single evaluation.

### WIR Build

The `wir_build/` directory translates a linked `FlatPackage` into a `WirPackage`. The pass runs in three steps driven from `wir_build.rs`: register types, collect function signatures, then translate each function body. Function-body translation is implemented as a `FunctionTranslator` whose `impl` blocks are split across sibling files by concern:

| File                          | Concern                                                                                                                |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `wir_build/context.rs`        | `WirContext` — accumulates types, functions, and tables during the pass                                                |
| `wir_build/types.rs`          | Register TIR types as WIR type definitions                                                                             |
| `wir_build/functions.rs`      | Collect and register function signatures                                                                               |
| `wir_build/component_plan.rs` | `ComponentPlan` types and `build_component_plan`                                                                       |
| `wir_build/translate.rs`      | Driver: `FunctionTranslator` struct, main dispatch (`translate_expr`/`translate_stmt`/`translate_block`), core helpers |
| `wir_build/primitive_ops.rs`  | Primitive-level operations: literals, binary / unary operators, casts, array indexing                                  |
| `wir_build/calls.rs`          | Call sites: function-ref resolution, builtin intrinsics, indirect calls, closure-to-canonical                          |
| `wir_build/canonical_abi.rs`  | Component Model canonical ABI: future / stream creation, read / write lowering, result lifting                         |
| `wir_build/pattern_match.rs`  | `match` / `if let` / `switch` lowering, pattern conditions and bindings, variant construct / test / payload            |

Cross-module access uses `pub(super)` on shared fields and methods. `translate.rs` owns the struct and the top-level dispatch; each helper module handles one class of TIR expression and calls back into `translate.rs` for sub-expression translation.

### Optimizer

See [optimizer.md](./optimizer.md).

### Standard Library

Embedded `.wado` files in `wado-compiler/lib/`:

**Core Library (`core/`):**

| Module                        | File                     | Description                                        |
| ----------------------------- | ------------------------ | -------------------------------------------------- |
| `core:prelude`                | `prelude.wado`           | Auto-imported re-exports from prelude sub-modules  |
| `core:prelude/traits.wado`    | `prelude/traits.wado`    | Trait definitions (Eq, Ord, Iterator, etc.)        |
| `core:prelude/types.wado`     | `prelude/types.wado`     | Core types (Option, Result, Stream, Future)        |
| `core:prelude/string.wado`    | `prelude/string.wado`    | String type and string iterators                   |
| `core:prelude/array.wado`     | `prelude/array.wado`     | Array type and array iterators                     |
| `core:prelude/int128.wado`    | `prelude/int128.wado`    | u128/i128 types (re-exported from prelude)         |
| `core:prelude/primitive.wado` | `prelude/primitive.wado` | Primitive type trait implementations               |
| `core:prelude/format.wado`    | `prelude/format.wado`    | Format traits (Display, Formatter)                 |
| `core:prelude/fpfmt.wado`     | `prelude/fpfmt.wado`     | Float-to-string formatting (pure Wado)             |
| `core:prelude/tuple.wado`     | `prelude/tuple.wado`     | Tuple trait implementations                        |
| `core:cli`                    | `cli.wado`               | CLI output (println, eprintln, etc.)               |
| `core:collections`            | `collections.wado`       | TreeMap and other collections                      |
| `core:serde`                  | `serde.wado`             | Serialization/deserialization traits               |
| `core:json`                   | `json.wado`              | JSON serialization/deserialization                 |
| `core:json_nsd`               | `json_nsd.wado`          | JSON non-self-describing deserializer              |
| `core:json_value`             | `json_value.wado`        | JSON value type representation                     |
| `core:simd`                   | `simd.wado`              | SIMD v128 operations                               |
| `core:zlib`                   | `zlib.wado`              | Compression (zlib/deflate)                         |
| `core:base64`                 | `base64.wado`            | Base64 encoding/decoding (RFC 4648)                |
| `core:internal`               | `internal.wado`          | Compiler-generated code support, panic/unreachable |
| `core:builtin`                | `builtin.wado`           | Compiler intrinsics with `#[canonical(...)]` attrs |

**WASI Library (`wasi/`):**

| Module            | File              | Description       |
| ----------------- | ----------------- | ----------------- |
| `wasi:cli`        | `cli.wado`        | CLI interfaces    |
| `wasi:clocks`     | `clocks.wado`     | Clock interfaces  |
| `wasi:filesystem` | `filesystem.wado` | FS interfaces     |
| `wasi:http`       | `http.wado`       | HTTP interfaces   |
| `wasi:random`     | `random.wado`     | Random interfaces |
| `wasi:sockets`    | `sockets.wado`    | Socket interfaces |

### Standard Library Tests

Stdlib tests are co-located with their source as `*_test.wado` files (e.g., `lib/core/zlib_test.wado`). They use Wado's `test` declaration syntax and run via `wado test`:

```sh
mise run test-wado   # runs all *_test.wado files
cargo run --bin wado -- test wado-compiler/lib/core/zlib_test.wado  # run one file
```

Test names can contain any characters (parentheses, dashes, etc.) — the compiler sanitizes them into valid kebab-case CM export names.

The `#[expect_trap]` and `#[TODO]` attributes mark tests as expected to trap. The compiler encodes this in the export name prefix:

```
test-0-simple            # normal test export
test-trap-1-panics       # #[expect_trap]: passes when body traps
test-todo-2-wip          # #[TODO]: passes when body traps; distinct failure message when it doesn't
test-tm5000-0-slow       # #[timeout_ms(5000)]: custom timeout in ms
test-trap-tm500-1-panic  # combined: expect_trap + custom timeout
```

The `tm{N}` segment encodes a custom timeout in milliseconds (default: 1000ms). It appears after the `test-`/`test-trap-`/`test-todo-` prefix.

Both `wado test` and the e2e test runner detect these prefixes and handle pass/fail accordingly.

Test functions use the same async wrapper as `run()`, ensuring compatibility with WASI P3's async model. Each test properly completes its async task before reporting results.

### WASI Registry

The `WasiRegistry` module (`component_model.rs`) collects WASI import information from `lib/wasi/*.wado` files and provides it to the code generator for dynamic Component Model generation.

**Purpose:**

- Extract WASI version strings from `#[cm(...)]` attributes (e.g., `0.3.0-rc-2025-09-16`)
- Map effect methods to function names using a unified naming scheme
- Track which WASI interfaces are used for conditional import generation

**Naming Convention:**

The registry uses a **unified naming scheme** across both component-level and core module-level code:

| Format                                       | Example                             |
| -------------------------------------------- | ----------------------------------- |
| `wasi:{package}/{EffectName}::{method_name}` | `wasi:cli/Stdout::write_via_stream` |

This naming scheme:

- Uses `wasi:` prefix for clarity
- Includes package for uniqueness across packages (e.g., `cli`, `clocks`)
- Uses Wado effect/method names (not WIT interface/function names)
- Uses `::` as method separator (Wado convention)

The registry provides `build_local_alias_name()` utility function and `resolve()` method for name resolution.

**What's Dynamic (from registry):**

| Item                 | Example                                                          |
| -------------------- | ---------------------------------------------------------------- |
| Version strings      | `wasi:cli/stdout@0.3.0-rc-2025-09-16`                            |
| Import paths         | Built via `format!("wasi:cli/stdout@{}", cli_version)`           |
| Function async flag  | `is_async` from effect method definition                         |
| Interface presence   | `has_interface("monotonic-clock")` for conditional codegen       |
| Local alias names    | `build_local_alias_name("cli", "Stdout", "write_via_stream")`    |
| WASI type resolution | `Instant` → `u64`, `Duration` → `u64` resolved from wasi/\*.wado |
| Function signatures  | Params and return types parsed from effect methods               |
| Supported interfaces | Dynamically filtered based on type support                       |

**Dynamic Interface Filtering:**

Instead of a hardcoded whitelist, interfaces are included based on type support:

- Only interfaces where ALL functions have supported types are imported
- Supported param types: primitives (`i32`, `u64`, `bool`, `char`, `String`, etc.), `Stream<T>`
- Supported return types: same as params plus `Result<T, E>`
- WASI newtypes are resolved to base types before filtering (e.g., `Instant` → `u64`)
- The "run" interface is skipped (it defines exports, not imports; needed for Command world)

**What's Still Hardcoded (TODO):**

| Item                       | Location                                  | Reason                                |
| -------------------------- | ----------------------------------------- | ------------------------------------- |
| `error-code` enum variants | `["io", "illegal-byte-sequence", "pipe"]` | Registry only tracks effect functions |

**Future Work:**

To fully eliminate hardcoded CM structures, the registry would need to:

1. Track WASI types (enums, resources) in addition to effect functions
2. Parse enum variants from `#[cm(...)]` annotated enums in wasi/\*.wado
3. Generate CM type definitions dynamically from parsed definitions

### Async Export Functions (`export async fn`)

Wado HTTP handlers use `export async fn` to opt into the Component Model async calling convention. The `async` modifier is significant — it changes the entire adapter generation strategy.

#### Why `async` Is Required for HTTP Handlers

Without `async`, the compiler generates a synchronous CM export adapter: it calls the user function, receives the return value, lowers it to flat CM ABI values, and returns them to the CM runtime. The function lifetime is tied to the return value.

For HTTP handlers, the return type is `Result<Response, ErrorCode>`. A `Response` contains a `FutureWritable<Result<Option<Trailers>, ErrorCode>>` — a writable future handle that the caller must fulfill **after** the response headers are sent. With a sync adapter, the function would return before the trailers future is resolved, and there would be no opportunity to write to it.

With `async`, the CM runtime allows the function to remain alive after delivering its result. The adapter generated for `export async fn` has two key differences:

1. The Wasm-level function signature uses the async calling convention: flat params with no outptr, and the function returns nothing (result delivery is via `task.return`).
2. The adapter only lifts the incoming parameters, then calls the user function directly — it does not handle the return value. The user's `task return` statement inside the function body drives result delivery.

```wado
// Synchronous (sync adapter wraps return):
export fn get_version() -> String { return "1.0"; }

// Async (task return drives delivery; function can continue after):
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    // ...build response with trailers future...
    task return Result::<Response, ErrorCode>::Ok(response);
    // function continues here; fulfills trailers future
    trailers_tx.write(Ok(null));
}
```

#### `task return` Syntax

`task return expr;` is a statement that calls the CM `task.return` instruction. It delivers the function's result to the CM runtime without ending the function.

**Rationale:** Regular `return` terminates the Wasm function. If an HTTP handler used `return response`, the function would exit before it could fulfill any outstanding futures (e.g., trailers). `task return` separates result delivery from function termination, keeping the function alive so it can perform cleanup and fulfill futures.

**Type checking:** The `task return` expression is type-checked against the declared return type of the surrounding `export async fn`. Regular `return` is forbidden in `async` function bodies — using it would terminate the Wasm function without notifying the CM runtime.

**CM Binding expansion:** During the CM Binding phase, `task return expr` is expanded in-place to a sequence of TIR that:

1. Lowers the Wado value to flat CM ABI values (using `synthesize_lower_to_flat`)
2. Calls `builtin::task_return(0, flat0, flat1, ...)` — the `0` is the Ok discriminant

This expansion is performed by `expand_task_returns_in_func` in `cm_binding_gen.rs`, which walks the function body and replaces each `TirStmtKind::TaskReturn` with the expanded sequence.

### Builtin Registry

The `BuiltinRegistry` module (`builtin_registry.rs`) collects function signatures from `lib/core/builtin.wado` and provides type information for code generation.

**The `#[canonical("...")]` Attribute:**

Builtins in `builtin.wado` are divided into two categories:

1. **Canonical builtins** - Functions with `#[canonical("namespace", "name")]` attribute are imported as Component Model canonical built-ins
2. **Instruction builtins** - Functions without the attribute compile directly to Wasm instructions

```wado
// Canonical builtin - imported as CM function "stream-new"
#[canonical("wasi", "stream-new")]
fn stream_new() -> i64;

// Instruction builtin - compiles to Wasm i32.and instruction
fn i32_and(a: i32, b: i32) -> i32;
```

**Canonical Builtins:**

Builtins with `#[canonical("namespace", "name")]` are imported as CM canonical built-ins. The namespace determines the import source: `"wasi"` for CM canonical builtins, `"mem"` for memory operations, `"bundled"` for wado-bundled-libm.

| Wado Name              | Namespace | Canonical Name         | Category    |
| ---------------------- | --------- | ---------------------- | ----------- |
| `stream_new`           | `wasi`    | `stream-new`           | Stream      |
| `stream_read`          | `wasi`    | `stream-read`          | Stream      |
| `stream_write`         | `wasi`    | `stream-write`         | Stream      |
| `stream_drop_writable` | `wasi`    | `stream-drop-writable` | Stream      |
| `stream_drop_readable` | `wasi`    | `stream-drop-readable` | Stream      |
| `future_new`           | `wasi`    | `future-new`           | Future      |
| `future_write`         | `wasi`    | `future-write`         | Future      |
| `future_drop_writable` | `wasi`    | `future-drop-writable` | Future      |
| `future_drop_readable` | `wasi`    | `future-drop-readable` | Future      |
| `task_return`          | `wasi`    | `task-return`          | Async task  |
| `waitable_set_new`     | `wasi`    | `waitable-set-new`     | Async task  |
| `waitable_join`        | `wasi`    | `waitable-join`        | Async task  |
| `waitable_set_wait`    | `wasi`    | `waitable-set-wait`    | Async task  |
| `subtask_drop`         | `wasi`    | `subtask-drop`         | Async task  |
| `realloc`              | `mem`     | `realloc`              | Memory      |
| `libm_sin`, etc.       | `bundled` | `libm_sin`, etc.       | Math (libm) |

**Instruction Builtins:**

Functions without the `#[canonical]` attribute compile directly to Wasm instructions (e.g., `i32_and` → `i32.and`, `f64_sqrt` → `f64.sqrt`). See `lib/core/builtin.wado` for the full list. Categories include: i32/i64 ops, array ops, linear memory ops, float math, wide arithmetic (i128), reinterpret casts, and control flow.

**Registry Usage:**

The `BuiltinRegistry` is used by both codegen and resolver:

- **Codegen**: Uses the registry to look up canonical names for imported builtins
- **Resolver**: Uses the registry to look up return types for builtin function calls, eliminating the need for hardcoded type mappings

### World Registry

The `WorldRegistry` module (`world_registry.rs`) collects world definitions from `lib/wasi/*.wado` and provides export signature information for code generation.

**Purpose:**

- Extract world definitions (e.g., `Command` world from `wasi/cli.wado`)
- Provide export function signatures for component generation
- Derive the `run` function signature from world exports instead of hardcoding

**Usage:**

```rust
// Get the run export signature from Command world
if let Some(run_export) = world_registry.get_export("Command", "run") {
    let params = world_export_to_core_params(run_export);
    let results = world_export_to_core_results(run_export);
}
```

### Name Mangling

The `name.rs` module centralizes all naming and mangling logic for the compiler. It provides utilities for building and parsing mangled names for methods, effect operations, and module-qualified symbols.

**Naming Conventions:**

| Name Type               | Format                                                 | Example                              |
| ----------------------- | ------------------------------------------------------ | ------------------------------------ |
| Simple method           | `{struct_name}::{method_name}`                         | `Point::sum`                         |
| Full method             | `{filename}/{struct_name}::{method_name}`              | `./geometry.wado/Point::sum`         |
| Trait method            | `{filename}/{struct_name}^{trait_name}::{method_name}` | `./geometry.wado/Point^Display::fmt` |
| Effect operation        | `{effect_name}::{operation_name}`                      | `Stdout::write_via_stream`           |
| WASI qualified          | `wasi:{package}/{interface}::{function}`               | `wasi:cli/stdout::write-via-stream`  |
| Module-qualified struct | `{module_path}::{struct_name}`                         | `./geometry.wado::Point`             |
| Core internal           | `core::internal::{name}`                               | `core::internal::log_stdout`         |

**Utility Functions:**

| Function              | Description                    | Example                              |
| --------------------- | ------------------------------ | ------------------------------------ |
| `mangle_generic_name` | Build monomorphized type name  | `("Box", ["i32"])` → `"Box<i32>"`    |
| `strip_type_params`   | Extract base name from generic | `"IndexValue<i32>"` → `"IndexValue"` |
| `extract_local_name`  | Strip module path prefix       | `"./main.wado/Point"` → `"Point"`    |

### ModuleSource

The `ModuleSource` enum in `name.rs` provides a structured representation of where a module comes from.

```rust
pub enum ModuleSource {
    Core { name: String },      // core:prelude, core:cli, etc.
    Wasi { interface: String }, // wasi:cli, wasi:io, etc.
    Local { path: String },     // ./geometry.wado, ../lib.wado
    EntryPoint,                 // The main entry module
}
```

### Module Path Canonicalization

The `name.rs` module also provides path canonicalization utilities to ensure the same file imported via different paths resolves to the same module identity.

**Design:**

- Uses URI path normalization (RFC 3986)
- Always uses `/` separator (platform-agnostic, even on Windows)
- Canonical paths are project-root-relative (prefixed with `./`)
- Special prefixes (`core:`, `wasi:`, `http://`, `https://`) pass through unchanged

**Examples:**

| Input Path                       | Canonical Output                 |
| -------------------------------- | -------------------------------- |
| `./geometry.wado`                | `./geometry.wado`                |
| `./sub/../geometry.wado`         | `./geometry.wado`                |
| `./sub/./file.wado`              | `./sub/file.wado`                |
| `core:cli`                       | `core:cli`                       |
| `http://localhost:8080/lib.wado` | `http://localhost:8080/lib.wado` |

**Relative Import Resolution:**

When resolving relative imports, the path is resolved against the importing module's path:

| From Module       | Import Source     | Resolved Path      |
| ----------------- | ----------------- | ------------------ |
| `./main.wado`     | `./geometry.wado` | `./geometry.wado`  |
| `./sub/main.wado` | `./utils.wado`    | `./sub/utils.wado` |
| `./sub/main.wado` | `../lib.wado`     | `./lib.wado`       |

**Validation:**

The analyzer validates module paths before loading to provide better error messages for invalid paths. Paths must be valid URI references per RFC 3986.

### Module Loader

The module loader loads all modules and applies the frontend pipeline to each:

**Frontend Pipeline (per module):**

1. **Lexer**: Source → Tokens
2. **Parser**: Tokens → AST
3. **Bind**: Validate local scopes, detect use-before-define and duplicate definitions
4. **Desugar**: Transform syntactic sugar (compound assignment, comparison chains, loops)

**Resolution Rules** (based on `ModuleSource`, see [ModuleSource](#modulesource)):

1. `core:*` → `ModuleSource::Core` → embedded stdlib
2. `wasi:*` → `ModuleSource::Wasi` → embedded stdlib
3. `http://` or `https://` → `ModuleSource::Remote` → host.load_remote()
4. `./` or `../` → `ModuleSource::Local` → host.load_source()
5. Unknown `xxx:` → Error: `unknown module namespace`
6. Other → Error: `invalid module path`

### Trait Static Dispatch

Wado traits use **static dispatch** (also known as "static resolution" or "monomorphization"). All trait method calls are resolved at compile time to concrete implementations. There is no runtime vtable or dynamic dispatch.

**How It Works:**

1. When a trait method is called (e.g., `person.greet()`), the resolver looks up the concrete type of the receiver
2. The resolver finds the matching `impl Trait for Type` block
3. The method call is lowered to a static function call with a mangled name: `Type^Trait::method`

**Example Lowering:**

```wado
// Source code
trait Greet {
    fn greet(&self) -> String;
}

impl Greet for Person {
    fn greet(&self) -> String {
        return `Hello, {self.name}!`;
    }
}

let p = Person { name: "Alice" };
println(p.greet());
```

```wado
// Lowered TIR (pseudo-Wado)
fn "Person^Greet::greet"(self: Person) -> String {
    return core::internal::string_concat("Hello, ", self.name, "!");
}

let p = Person { name: "Alice" };
println("Person^Greet::greet"(p));
```

**Static Trait Method Calls (no `&self`):**

Traits can define static methods (no `self` parameter). These are called using `Type::method()` syntax:

```wado
trait Deserialize {
    fn deserialize<D: Deserializer>(d: &mut D) -> Result<Self, Error>;
}
impl Deserialize for i32 {
    fn deserialize<D: Deserializer>(d: &mut D) -> Result<i32, Error> { ... }
}

// Call site: Type::method::<TypeArg>(args)
let result = i32::deserialize::<JsonDeserializer>(&mut d);
```

The resolver uses `find_static_method_trait` to detect when a static call targets a trait method and produces the mangled name `i32^Deserialize::deserialize`. Method-level type arguments (e.g., `<JsonDeserializer>`) generate `monomorph_info` for the monomorphizer to create a concrete instantiation.

**Method Resolution Priority:**

1. **Inherent methods** (methods in `impl Type { }`) take priority over trait methods
2. **Trait methods** (methods in `impl Trait for Type { }`) are used when no inherent method matches
3. If multiple traits define the same method name, it's currently a compile error (disambiguation syntax not yet implemented)

**Advantages of Static Dispatch:**

- **Zero runtime overhead**: No vtable lookup
- **Inlining possible**: Optimizer can inline trait methods
- **Dead code elimination**: Unused trait implementations are removed

### Orphan Rule Enforcement

Orphan rule checking runs inside `TraitEnv::build()` in `resolver/trait_env.rs`, immediately before per-module resolution begins. At that point all modules (stdlib + user files) are loaded, so the full set of trait and type declarations is available.

**Phase placement:**

```
LoadModules → TraitEnv::build()  ← orphan check here
                │
                ├── build impl_index / decl_index / blanket_impl_index
                ├── build type_decl_index  (struct/variant/enum/flags/newtype → ModuleSource)
                └── check_all_orphan_rules() → Vec<TypeError::OrphanViolation>
                         │
                         └── emitted via logger before per-module resolution starts
```

**Implementation:**

`TraitEnv::build()` returns `(Arc<TraitEnv>, Vec<TypeError>)`. The caller (`resolve_all_modules` in `orchestration.rs`) emits each `OrphanViolation` through the logger immediately after the call.

The check skips any impl block whose containing module is `Core`, `Wasi`, or `Remote` — the standard library and remote packages are trusted to write any impl they need. Only `EntryPoint` and `Local` modules are checked.

For each local impl block with a foreign trait, `check_orphan_rfc2451` walks the sequence `[self_type, trait_arg_1, …]` left-to-right and classifies each position via `classify_position`:

| `PositionKind`       | Meaning                                                            |
| -------------------- | ------------------------------------------------------------------ |
| `LocalType`          | Outermost type constructor is defined in a local module            |
| `ForeignType`        | Outermost type constructor is foreign (or a tuple / function type) |
| `UncoveredTypeParam` | The position is a bare `impl<T>` type parameter                    |

`&T` and `&mut T` are fundamental: `classify_position` recurses into the inner type, so `&LocalType` yields `LocalType`.

The sequence walk returns `true` (allowed) as soon as a `LocalType` is found with no `UncoveredTypeParam` seen at any earlier position. If an `UncoveredTypeParam` is reached before any `LocalType`, or the sequence is exhausted without finding a `LocalType`, the check returns `false` and an `OrphanViolation` error is produced.

**Error code:** `Code::OrphanRule` → diagnostic string `"ORPHAN_RULE"`.

### Default Trait Methods

Trait methods can have default implementations (a body in the trait declaration). When a type implements the trait but omits a method with a default body, the compiler synthesizes the method in the impl block using the default body.

**Resolution:**

1. During impl block processing, the resolver collects explicitly provided method names
2. For each default method in the trait not provided by the impl, the resolver calls `resolve_method` with the default method's AST, treating it as if it were written in the impl block
3. `Self` resolves to the implementing type, so `self.method()` calls in default bodies dispatch to the concrete type's methods

**Method Call Lookup:**

When `find_trait_method_for_type` searches for a method:

1. First checks methods explicitly in the impl block
2. If not found, checks the trait declaration for a default method with that name

### Associated Types

Traits can declare associated types using `type Name;` syntax. Implementors bind these types using `type Name = ConcreteType;`.

**AST Representation:**

```rust
// In trait declarations
struct AssociatedTypeDecl {
    name: String,
    span: Span,
}

// In impl blocks
struct AssociatedTypeBinding {
    name: String,
    ty: Type,
    span: Span,
}
```

**Resolution:**

When resolving `Self::TypeName` in trait methods:

1. The resolver maintains `current_associated_type_bindings: HashMap<String, TypeId>`
2. Before resolving methods in a trait impl, bindings are collected from the impl block
3. `Self::TypeName` is parsed as `Type::NamespacedGeneric { namespace: "Self", name: "TypeName" }`
4. Resolution looks up the type name in the current bindings

**Example:**

```wado
trait Container {
    type Item;
    fn get(&self) -> Self::Item;
}

impl Container for IntBox {
    type Item = i32;  // Binding: "Item" -> i32
    fn get(&self) -> Self::Item {  // Self::Item resolves to i32
        return self.value;
    }
}
```

### Newtype Semantics

`type T = U` creates a **newtype** - a distinct type that shares representation with its base type but is not interchangeable.

**Key Properties:**

- `T` and `U` are distinct types (no implicit conversion)
- `T` inherits methods, operators, and traits from `U`
- Explicit `as` cast required to convert between `T` and `U`
- Zero runtime cost (same Wasm representation)

**Method Signature Substitution:**

When calling an inherited method on a newtype, the method signature is substituted:

```wado
type Location = Point;

impl Point {
    fn distance(&self, other: &Point) -> f64 { ... }
}

let loc1: Location = ...;
let loc2: Location = ...;
loc1.distance(&loc2);  // Parameters expect &Location, not &Point
```

The resolver substitutes all occurrences of the base type with the newtype in:

- Parameter types (including `&BaseType` → `&Newtype`)
- Return type

**Static Methods and Traits:**

Newtypes inherit static methods and trait implementations from their base type:

```wado
Location::origin()  // Calls Point::origin()
loc.describe()      // Calls Point's Describable::describe()
```

**Chained Newtypes:**

Newtypes can chain: `type C = B; type B = A;` - the resolver traces back to the ultimate base type for method lookup.

See [WEP: Newtype Semantics](./wep-2026-01-29-newtype-semantics.md) for full specification.

### Iterator Trait Resolution

The compiler resolves iterator traits (`Iterator`, `IntoIterator`, `FromIterator`) using the same static dispatch mechanism as other traits.

**For-Of Loop Compilation:**

For-of loops over tuples are expanded at compile time in the resolver (one copy of the body per element, each typed independently). This enables heterogeneous iteration with per-element trait dispatch. `break`, `continue`, and `return` are compile errors inside tuple for-of.

For-of loops over non-tuple types are desugared to use `IntoIterator` and `Iterator` traits:

```wado
// Source
for let item of collection {
    body(item);
}

// Desugars to (conceptually)
{
    let mut __iter = IntoIterator::into_iter(&collection);
    loop {
        match Iterator::next(&mut __iter) {
            Some(__item) => {
                let item = __item;
                body(item);
            }
            None => break,
        }
    }
}
```

**Resolution Process:**

1. **Type lookup**: Get the type of `collection`
2. **IntoIterator lookup**: Find `impl IntoIterator for CollectionType`
3. **Iter type extraction**: Get `Self::Iter` associated type
4. **Iterator lookup**: Find `impl Iterator for IterType`
5. **Item type extraction**: Get `Self::Item` associated type for the loop binding

**Known Limitations:**

- **Cross-module monomorphization**: Generic stdlib methods (like `ArrayIter::collect` calling `Array::push`) may encounter type table ID mismatches when called from user code. Workaround: Use direct builtin calls in stdlib generic functions instead of method calls.

### Builtin Comparison Traits

The compiler desugars comparison operators to trait method calls:

**Eq Trait (Equality):**

```wado
// a == b desugars to:
Eq::eq(&a, &b)

// a != b desugars to:
!Eq::eq(&a, &b)
```

**Ord Trait (Ordering):**

```wado
// a < b desugars to:
Ord::cmp(&a, &b) == Ordering::Less

// a > b desugars to:
Ord::cmp(&a, &b) == Ordering::Greater

// a <= b desugars to:
Ord::cmp(&a, &b) != Ordering::Greater

// a >= b desugars to:
Ord::cmp(&a, &b) != Ordering::Less
```

**Resolution:**

1. For primitive types (`i32`, `f64`, etc.), the compiler generates direct Wasm comparison instructions
2. For `String` and `Array<T>`, the resolver looks up the trait implementation in prelude
3. For user-defined types, the resolver finds `impl Eq for Type` or `impl Ord for Type`

### Indexing Traits

Index expressions desugar to trait method calls:

```wado
// arr[i] (read) desugars to:
IndexValue::index_value(&arr, i)
// or Index::index(&arr, i) for reference-type elements

// arr[i] = value (write) desugars to:
IndexAssign::index_assign(&mut arr, i, value)
```

**Design Note:** `IndexValue` returns by value because Wasm GC's `array.get` copies elements. For primitive arrays, you cannot get a reference to an element. `Index` is only used for containers of reference-type elements.

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

// i32 operations
builtin::i32_and(a: i32, b: i32) -> i32  // Bitwise AND
builtin::i32_eqz(a: i32) -> i32          // Check if zero (returns 0 or 1)

// Linear memory operations
builtin::memory_store8(addr: i32, value: i32)  // Store byte to memory
builtin::memory_load8_u(addr: i32) -> i32      // Load unsigned byte from memory
builtin::realloc(oldptr: i32, oldsize: i32, align: i32, newsize: i32) -> i32

// Stream/Future intrinsics (Component Model)
// These are low-level i32 handle operations used internally by the resolver.
// User code accesses Stream<T>/Future<T> resource types from core:prelude/types.wado.
// NOTE: Migration from builtin-based to resource-based is incomplete.
// Resource declarations exist in types.wado but method resolution (.new(), .read(),
// .write(), .close(), .drop()) is still hardcoded in the resolver (method_call.rs)
// rather than being driven by the resource declarations.
builtin::stream_new() -> i64              // Create stream, returns rx|tx packed
                                          // Extract: rx = handles as i32, tx = (handles >> 32) as i32
builtin::stream_read(rx: i32, ptr: i32, len: i32) -> i32
builtin::stream_write(tx: i32, ptr: i32, len: i32) -> i32
builtin::stream_drop_writable(tx: i32)
builtin::stream_drop_readable(rx: i32)
builtin::future_new() -> i64             // Create future, returns rx|tx packed
builtin::future_write(tx: i32, ptr: i32) -> i32
builtin::future_drop_writable(tx: i32)
builtin::future_drop_readable(rx: i32)

// Async task intrinsics (Component Model)
builtin::waitable_set_new() -> i32
builtin::waitable_join(set: i32, subtask: i32)
builtin::waitable_set_wait(set: i32, outptr: i32) -> i32
builtin::subtask_drop(subtask: i32)

// Branch hinting (Wasm branch hinting proposal)
builtin::likely(cond: bool) -> bool    // Hint: branch is usually taken
builtin::unlikely(cond: bool) -> bool  // Hint: branch is rarely taken
```

**Branch Hinting:**

`builtin::likely()` and `builtin::unlikely()` generate WebAssembly branch hints via the `metadata.code.branch_hint` custom section. These hints help the Wasm runtime optimize branch prediction.

```wado
// Hint that this condition is usually true
if builtin::likely(x > 0) {
    // fast path
}

// Hint that this condition is rarely true (error path)
if builtin::unlikely(x < 0) {
    // error handling
}
```

To inspect generated branch hints:

```sh
cargo run --bin wado -- compile --wat-to-stdout file.wado | grep branch_hint
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

### Generic Type Inference

Generic function, method, struct, variant, and closure calls all share a single inference engine in `resolver/infer.rs`. Callers build an `InferCtx`, feed it argument and return-type constraints, and ask it to solve for a list of `TypeId`s in declaration order.

Constraints are collected in three confidence tiers, and solved in that order so stronger information always wins:

1. **Strong (argument-derived)** — `InferCtx::add(expected, actual)` unifies the declaration parameter type (which may contain `TypeParam` holes) against a concrete argument type immediately. The first typed argument to mention a type parameter pins its binding via `IndexMap::or_insert`, so later arguments cannot overwrite it.
2. **Weak (literal-number)** — `InferCtx::add_deferred(expected, actual)` queues constraints whose "actual" type comes from an integer or float literal. These run only after every strong constraint, so `fn two<T>(x: T, y: T); two(1, 2 as u8)` picks `T = u8` instead of locking on the literal's default type.
3. **Weaker (expected-return)** — `InferCtx::add_expected_return(decl_return, expected)` queues a back-inference constraint derived from the caller's expected type (e.g. `let x: i32 = foo();`). It runs last so a use-site annotation only fills type parameters that the arguments left unbound.

The structural unifier `unify()` handles `TypeParam`, tuple/variadic `TypePack` splices, same-named `GenericInstance`s, `Array<K>` against homogeneous tuple literals, `BuiltinArray<T>`, `Ref<T>`, `MutRef<T>`, and `Function` types. It never fails: an unmatched shape is silently skipped so a caller can try the next argument or fall through to a fallback.

`InferCtx` exposes three solve methods. `solve()` returns the final type arguments, falling back to the original `TypeParam TypeId` for unbound parameters. `solve_with_phantoms()` additionally reports whether every parameter resolved to a concrete type, which callers use to detect phantom-only parameters. `solve_with_bindings()` returns the raw bindings map alongside the inferred vec; callers that may forward a caller-scope type parameter into an inner generic call (e.g. `outer<T>(g: &mut T) { inner(g); }`) use the map to tell "no inference happened" from "bound to the same interned `TypeId`".

Because `TypeParam` ids are interned by `(name, index)`, two generic functions that both declare a `T` at index `0` share the same `TypeId`. The resolver relies on this interning during forwarding: `inner`'s `T` is "bound" to `outer`'s `T` (same id), and monomorphization later substitutes them to the caller's concrete type. To support same-module forward references (`outer` defined before `inner` in the same file), `resolver.rs` runs a pre-pass that calls `precompute_generic_function_cache` for every generic function in the module before any body is resolved. All implicit generic forwarding is therefore handled in the resolver; the monomorphizer only consumes calls whose `type_args` are already populated.

### 128-bit Integer Types (i128/u128)

See [WEP: 128-bit Integer Types](./wep-2026-01-24-i128-u128-types.md).

### Template String Interpolation

Template strings use backticks with `{expr}` syntax and Python-like format specifiers (e.g., `{pi:.2f}`). The compilation pipeline:

1. **Lexer**: Tokenizes backtick strings with brace depth tracking, nested template support, and escape sequences (`\{`, `\}`)
2. **Parser**: Builds `TemplateStringExpr` with `TemplatePart::String` and `TemplatePart::Interpolation` nodes. Distinguishes `:` (format spec) from `::` (scope resolution) via lookahead
3. **Synthesis** (`synthesis/template.rs`): Expands each template string into a `__tmpl` labeled block that allocates a `String` buffer, appends literal parts, and calls `Display::fmt` or `Inspect::inspect` for interpolated expressions. Emits generic trait calls that the monomorphizer resolves to concrete implementations

### Serde Synthesis (`synthesis/serde_synth.rs`)

The synthesis phase generates `Serialize` and `Deserialize` trait implementations for structs that use `impl Serialize for Type;` or `impl Deserialize for Type;` (semicolon instead of block body).

**How it works:**

1. The resolver detects `impl Trait for Type;` declarations and records them as `SynthesisRequest` entries in the TIR module.
2. `serde_synth.rs` processes each request, inspects the struct's fields, and generates complete TIR method bodies.
3. For `Serialize`: generates a `serialize` method that calls `begin_struct`, then `field` for each field (with `snake_case` → `camelCase` name conversion), then `end`.
4. For `Deserialize`: generates a `deserialize` static method with a field lookup function, golden-mask bitmask tracking for required fields, and a loop that processes fields in any order.
5. A lookup function (`_typename_field_lookup`) is generated alongside each `Deserialize` impl to map camelCase JSON keys to field indices.

**Generated Deserialize pattern (golden mask):**

- Each field gets one bit in a `u32` bitmask (supports up to 32 fields).
- Unknown fields are skipped via `skip()`.
- After the loop, a single `seen & mask != mask` check verifies all required fields are present.
- Missing fields produce a `DeserializeError` with `MissingField` kind.

### Inspect/Display Synthesis (`synthesis/traits.rs`)

The synthesis phase auto-generates `Inspect` and `Display` trait implementations for all types that need them. `Inspect` is always generated; `Display` is generated as a fallback (delegating to `Inspect`) only for types without a user-provided `Display` impl.

**How it works:**

1. Template expansion (`synthesis/template.rs`) encounters `{expr:?}` or `{expr}` and emits calls to `Inspect::inspect` or `Display::fmt`.
2. `synthesis/traits.rs` scans all types in the project and generates `Inspect` trait impls — field access for structs, match arms for variants/enums, loops for arrays, etc.
3. For types without a user-provided `Display` impl, a fallback `Display::fmt` is generated that delegates to `Inspect::inspect`.
4. The monomorphizer resolves all generic trait calls to these concrete implementations.
5. The generated TIR flows through the rest of the pipeline (lower → optimize → codegen).

Each distinct type gets a dedicated `__inspect$TypeName` function generated once and called from all use sites. The `InspectRegistry` deduplicates these across the module.

### String Literal Data Segments

String literals are stored in Wasm **passive data segments**. This allows direct initialization of GC arrays using `array.init_data`, which is more efficient than loading from linear memory.

### The `assert` Statement

`assert` behaves like a power-assert, which shows source conditions, collects intermediate values, and prints them if the assertion fails.

**Basic Assert:**

`assert x > 0;` is compiled into:

```wado
if builtin::unlikely(!condition) {
    panic(`Assertion failed:\ncondition: x > 0\nx: {x}`);
}
```

**Assert with Custom Message:**

`assert x > 0, "x must be checked elsewhere";` is compiled into:

```wado
if builtin::unlikely(!condition) {
    panic(`Assertion failed: x must be checked elsewhere\ncondition: x > 0\nx: {x}`);
}
```

**Intermediate Values:**

Each intermediate value is collected and printed if the assertion fails.

`assert x + y > 0;` is compiled into:

```wado
if builtin::unlikely(!condition) {
    panic(`Assertion failed:\ncondition: x + y > 0\nx: {x}\ny: {y}\nx + y: {x + y}`);
}
```

**Value Caching for Side-Effect Safety:**

When the condition contains function calls with side effects, values are cached in Wasm locals to ensure each function is called exactly once:

```wado
assert get_value() > 10;
```

The compiler:

1. Extracts all "interesting" sub-expressions (identifiers, function calls, binary expressions)
2. Evaluates each sub-expression once and stores the result in a local variable
3. Evaluates the condition using cached local values
4. On failure, builds the error message using cached values (no re-evaluation)

This ensures that `get_value()` is called only once, not twice (once for caching and once for condition evaluation).

### Value Semantics

Wado uses value semantics for composite types: assignment creates a copy. Structs and tuples are copied field-by-field via `struct.get`/`struct.new`. Arrays and strings are copied element-by-element. Option and variant types conditionally copy their payloads. Reference types (`&T`, `&mut T`) do **not** have value semantics — they share the underlying value.

### Auto-Dereference for Method Calls

When calling a method on a reference type, the compiler automatically inserts dereference operations to reach the underlying value type.

**How It Works:**

```wado
let p = Point { x: 10, y: 20 };
let p_ref = &p;
let sum = p_ref.sum();  // Auto-derefs: (*p_ref).sum()

let p_ref2 = &p_ref;
let sum2 = p_ref2.sum();  // Double auto-deref: (**p_ref2).sum()
```

The resolver handles auto-deref in `resolve_method_call()`: it repeatedly inserts `TirUnaryOp::Deref` expressions until the receiver is not a reference type, then proceeds with normal method resolution. Works for `&T`, `&mut T`, and multi-level references (`&&T`, `&&&T`).

### Global Variables

Global variables compile to WebAssembly globals with two initialization strategies:

| Category | Condition                                    | Strategy                                                  |
| -------- | -------------------------------------------- | --------------------------------------------------------- |
| Constant | Primitive type with Wasm constant expression | Direct initialization in Wasm global section              |
| Lazy     | Object types or non-constant expressions     | Null/zero default, initialized in `__initialize_module()` |

**Module Initialization:**

- Each module with lazy globals generates `pub fn __initialize_module()`
- Entry module generates `fn __initialize_modules()` which calls all modules' initializers
- Initialization order: topologically sorted by dependencies (within module and across modules)
- Re-initialization prevented via flag check

### Match Expression

Match expressions are lowered to a series of pattern checks with branching:

**Lowering Strategy:**

| Pattern Type | Lowering                                                |
| ------------ | ------------------------------------------------------- |
| Variant      | `br_on_cast_fail` to test discriminant, extract payload |
| Literal      | Equality check with `br_if`                             |
| Wildcard `_` | No check (always matches)                               |
| Or pattern   | Chain of checks with shared arm body                    |
| Guard `&&`   | Pattern check followed by guard expression check        |

**Codegen to Wasm:**

For dense integer patterns (e.g., enum discriminants), the codegen emits `br_table` for O(1) dispatch:

```wat
;; match color { Red => 0, Green => 1, Blue => 2 }
(block $arm2
  (block $arm1
    (block $arm0
      (br_table $arm0 $arm1 $arm2 (local.get $color)))
    (i32.const 0)  ;; Red
    (br $end))
  (i32.const 1)    ;; Green
  (br $end))
(i32.const 2)      ;; Blue
```

For variant patterns, `br_on_cast_fail` tests the discriminant and extracts the payload in one instruction.

**Exhaustiveness:**

Checked during analysis phase. Non-exhaustive patterns are compile errors.

---

### Linear Memory and `realloc`

The Component Model requires each core module to export a `realloc` function. The CM runtime calls `realloc` whenever it needs guest-side linear memory — for example, `stream.read` copies bytes from the host into a guest buffer allocated via `realloc`, and string lifting/lowering also goes through it. Because CM operations can allocate significant amounts of memory (e.g., reading a large HTTP response body in a loop), the `realloc` implementation must be robust.

The allocator is implemented in Wado itself in `lib/core/allocator.wado`, using the `#![wasm_module("mem")]` attribute to compile it into a separate core module. The module exports a `realloc` function (via `#[export_name("realloc")]`) and a mutable global for the heap pointer.

The compiler extracts `#![wasm_module("mem")]` items during WIR construction and emits them as a standalone core module. This "mem" module is instantiated as part of the component, and its `realloc` and linear memory exports are shared with the main GC core module. See the [spec section on the "mem" core module](./spec.md#the-mem-core-module) for the full component structure.

The main core module accesses `realloc` and linear memory through imports, declared via `#[canonical("mem", "realloc")]` in `core:builtin`. Internal functions like `memory_to_gc_array` and `gc_array_to_memory` in `core:internal` use these builtins to copy data between GC arrays and linear memory.

#### Allocator Selection

The compiler supports multiple allocator implementations, each tagged with `#[allocator("name")]` in `lib/core/allocator.wado`. The compiler selects one by setting its `export_name` to `"realloc"` and clearing the others.

Available allocators:

- **`bump`** (default for CLI): Simple bump allocator with free-rewind optimization. Never frees memory except the most recent allocation. Suitable for short-lived processes.
- **`freelist`** (default for HTTP): Free-list allocator that reclaims freed memory via a singly-linked free list with first-fit search and block splitting. Falls back to bump allocation when no suitable free block exists. Suitable for long-living processes.
- **`debug`**: Never reuses freed memory and poisons freed regions with `0xFF` bytes. Useful for detecting use-after-free bugs.

Selection rules:

1. CLI `--allocator <name>` overrides everything.
2. Test world (`--world test` / `wado test`) defaults to `"debug"`.
3. HTTP service world (`wasi:http/service`) defaults to `"freelist"`.
4. E2E tests (`cargo test`) default to `"debug"` unless the fixture specifies `"allocator"` in its `__DATA__` JSON.
5. Otherwise, defaults to `"bump"`.

---

## In Progress

### Partial Implementations

- **Variant pattern matching**: Single-payload and tuple-payload cases work (`if let Circle(r) = shape`, `if let Rect([w, h]) = shape`). Struct payloads not yet supported. See [WEP: Variant Payload Design](./wep-2026-01-25-variant-payload-design.md).
- **Function types**: Parser supports `fn(T) -> U` syntax, closure codegen works (both pure and capturing), but full function type support is incomplete.
- **Stream/Future resource migration**: Resource declarations (`resource Stream<T>`, `resource Future<T>`, etc.) exist in `core:prelude/types.wado`, but method resolution (`.new()`, `.read()`, `.write()`, `.close()`, `.drop()`) is still hardcoded in the resolver (`method_call.rs`) rather than being driven by the resource declarations. The low-level canonical builtins in `builtin.wado` (`stream_new`, `stream_read`, `future_write`, etc.) remain the actual backing implementation.

### Known Limitations

1. **Implicit struct literals don't work with generic structs**: `let b: Box<i32> = { value };` fails. Use explicit form: `let b: Box<i32> = Box { value };`
2. **GC arrays cannot be passed directly to streams**: `stream<u8>` operations require linear memory. GC arrays must be copied to linear memory before writing to streams. See [component-model#525](https://github.com/WebAssembly/component-model/issues/525)

---

## Not Yet Implemented

- `?` operator (error propagation)
- Effect handlers
- Reactive signals (source values, derived values, effect blocks)
- JSX
