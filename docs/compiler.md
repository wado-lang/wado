# Wado Compiler

This document describes the Wado compiler architecture and implementation status.

## Compiler Architecture

The compiler follows a multi-phase pipeline:

```
Source (.wado) → Lexer → Parser → Bind → Load → Analyze → Resolve → Effect Check → Synthesis → Monomorphize → Post-Mono Synthesis → Lower → Optimize → WIR Build → WIR Optimize → Codegen
```

### Compilation Pipeline

| Phase        | Input         | Output          | Description                                               |
| ------------ | ------------- | --------------- | --------------------------------------------------------- |
| Lexer        | Source        | Tokens          | Tokenize, extract `__DATA__` section                      |
| Parser       | Tokens        | AST             | Build abstract syntax tree                                |
| Bind         | AST           | AST (validated) | Local name resolution, scope/mutability checking          |
| Load         | AST           | All modules     | Load dependencies; each module: parse → bind → desugar    |
| Analyze      | All modules   | Symbol table    | Build symbol table, validate imports                      |
| Resolve      | AST + Symbols | Project         | Type resolution, produce Project                          |
| Effect Check | Project       | Project         | Validate function effect requirements                     |
| Synthesis    | Project       | Project         | Enum traits, CM adapter synthesis                         |
| Monomorphize | Project       | Project         | Instantiate generics with concrete types                  |
| Post-Mono    | Project       | Project         | Template expansion, inspect debug output synthesis        |
| Lower        | Project       | Project         | Closure, i128 match, global init, string literal lowering |
| Optimize     | Project       | Project         | Inlining, copy-prop, LICM, DCE, post-opt rewrite          |
| WIR Build    | Project       | WirModule       | Planning + TIR → WIR (Wasm IR) translation                |
| WIR Optimize | WirModule     | WirModule       | Multi-value SROA, array data promotion, peephole          |
| Codegen      | WirModule     | Wasm bytes      | WIR emission to core Wasm + Component Model wrapping      |

**Note:** The Desugar phase is integrated into the Load phase. Each module goes through the same frontend pipeline: `lexer → parser → bind → desugar`.

### Modules

| Module          | File                                 | Description                                                 |
| --------------- | ------------------------------------ | ----------------------------------------------------------- |
| Lexer           | `lexer.rs`                           | Tokenizes source code, extracts `__DATA__` section          |
| Parser          | `parser.rs`                          | Recursive descent parser, builds AST                        |
| AST             | `ast.rs`                             | AST node definitions, `Module::data_section()` API          |
| Token           | `token.rs`                           | Token types and spans                                       |
| Syntax          | `syntax.rs`                          | Syntax definitions (keywords, operators)                    |
| Comment         | `comment.rs`                         | Comment collection and CommentMap for formatting            |
| Bind            | `bind.rs`                            | Local name binding, scope analysis, mutability check        |
| Loader          | `loader.rs`                          | Module loading, dependency resolution                       |
| Desugar         | `desugar.rs`                         | AST transformations (compound assign, etc.)                 |
| EffectCheck     | `effect_check.rs`                    | Validates effect requirements for function calls            |
| Unparser        | `unparse.rs`                         | Converts AST/TIR back to source code                        |
| Analyzer        | `analyze.rs`                         | Semantic analysis, symbol table construction                |
| Symbol          | `symbol.rs`                          | Symbol table data structures                                |
| Name            | `name.rs`                            | Name mangling utilities for methods and symbols             |
| Resolver        | `resolver.rs`                        | Type resolution, AST to TIR, produces Project (`resolver/`) |
| TIR             | `tir.rs`                             | Typed Intermediate Representation                           |
| Synthesis       | `synthesis.rs`                       | Unified synthesis phase (`synthesis/`)                      |
| SynthCommon     | `synthesis/common.rs`                | Shared TIR builders for synthesis phases                    |
| SynthTraits     | `synthesis/traits.rs`                | Auto-derived Eq/Ord/Display/Inspect for types               |
| SynthTemplate   | `synthesis/template.rs`              | Template string expansion (pre-monomorphize)                |
| SynthInspect    | `synthesis/inspect.rs`               | Inspect debug output synthesis (type→TIR)                   |
| SynthCmAdapter  | `synthesis/cm_adapter.rs`            | CM boundary adapter synthesis (TIR functions)               |
| CmAbi           | `cm_abi.rs`                          | Canonical ABI layout computation                            |
| Monomorphize    | `monomorphize.rs`                    | Generic type/function instantiation (Project→Project)       |
| Lower           | `lower.rs`                           | Closure, i128 match, global init, string lowering           |
| Project         | `project.rs`                         | Project: compilation context passed through pipeline        |
| Optimize        | `optimize.rs`                        | Optimization coordinator (`optimize/`)                      |
| ConstFold       | `optimize/const_fold.rs`             | Constant folding for integer/float arithmetic               |
| ConstProp       | `optimize/const_prop.rs`             | Constant propagation for immutable globals                  |
| ConstGlobal     | `optimize/const_global_promotion.rs` | Promote runtime globals to compile-time constants           |
| DCE             | `optimize/dce.rs`                    | Dead code elimination via reachability analysis             |
| Inline          | `optimize/inline.rs`                 | Function inlining for small, pure functions                 |
| RefElim         | `optimize/ref_elim.rs`               | Reference elimination after inlining                        |
| CopyProp        | `optimize/copy_prop.rs`              | Copy propagation for trivial bindings                       |
| SROA            | `optimize/sroa.rs`                   | Scalar replacement of aggregates (struct/tuple elim)        |
| LICM            | `optimize/licm.rs`                   | Loop-invariant code motion                                  |
| Rewrite         | `optimize/rewrite.rs`                | Select lowering, move insertion, block simplification       |
| WasmPlan        | `wasm_plan.rs`                       | `ComponentPlan` types and `build_component_plan`            |
| Stdlib          | `stdlib.rs`                          | Embedded core library sources                               |
| CompilerHost    | `compiler_host.rs`                   | I/O abstraction for the compiler                            |
| Logger          | `logger.rs`                          | Diagnostic logging with timestamps                          |
| ComponentModel  | `component_model.rs`                 | WASI import registry and CM ABI type support                |
| BuiltinRegistry | `builtin_registry.rs`                | Builtin function registry from `core:builtin`               |
| WorldRegistry   | `world_registry.rs`                  | World definitions registry for export signatures            |
| WIR             | `wir.rs`                             | Wasm IR data structures                                     |
| WIR Unparse     | `wir_unparse.rs`                     | WIR → pseudo-Wado source code for debugging                 |
| WIR Build       | `wir_build.rs`                       | Planning + TIR→WIR translation (`wir_build/`)               |
| WIR Optimize    | `wir_optimize.rs`                    | WIR-level optimizations (multi-value SROA, etc.)            |
| Codegen         | `codegen.rs`                         | WIR→Wasm emission + Component Model wrapping (`codegen/`)   |
| Bundled         | `bundled.rs`                         | Loads pre-compiled Wasm builtins (wado-bundled-libm)        |

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
wado dump --tir --unparse file.wado    # Show TIR before monomorphization
wado dump --lower --unparse file.wado  # Show TIR after monomorphization and lowering
```

**Output Characteristics:**

- `--tir`: Shows generic types as-is (e.g., `Box<T>`)
- `--lower`: Shows monomorphized type names (e.g., `Box$i32` instead of `Box<T>`)
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

The `monomorphize.rs` module is a dedicated compilation phase that instantiates generic structs and functions with concrete types. It runs after type resolution and before the lower phase.

**Process:**

1. **Collect generic definitions**: Gather all generic struct and function definitions from all modules
2. **Find instantiation sites**: Scan for `GenericInstance` types and generic function calls
3. **Instantiate structs**: Create concrete struct definitions with substituted field types
4. **Instantiate functions**: Create concrete function definitions with substituted types
5. **Rewrite types**: Replace all `GenericInstance` type references with concrete struct types
6. **Rewrite calls**: Replace generic function calls with calls to monomorphized functions
7. **Transitive instantiation**: Iteratively process new instantiations created during monomorphization

**Cross-Module Support:**

The monomorphizer supports cross-module generic function instantiation. Generic functions defined in one module (e.g., `Array` methods from prelude) can be instantiated when used in another module. This is achieved by collecting all generic functions from all modules before processing.

**Supported Features:**

| Feature                        | Example                                  | Status |
| ------------------------------ | ---------------------------------------- | ------ |
| Single type parameter          | `Box<i32>`                               | ✅     |
| Multiple type parameters       | `Pair<i32, String>`                      | ✅     |
| Nested generics                | `Box<Box<i32>>`                          | ✅     |
| Generics in Array              | `Array<Pair<i32, String>>`               | ✅     |
| Struct type parameters         | `Box<Point>`                             | ✅     |
| Impl on specialization         | `impl Box<i32> { fn get() }`             | ✅     |
| Generic functions              | `fn identity<T>(x: T) -> T`              | ✅     |
| Generic methods                | `impl T { fn foo<U>(&self) }`            | ✅     |
| Generic trait methods          | `trait T { fn f<D>(&self, d: D) }`       | ✅     |
| Static trait methods with args | `i32::deserialize::<MockDeserializer>()` | ✅     |

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

### Optimizer

See [optimizer.md](./optimizer.md).

### Standard Library

Embedded `.wado` files in `wado-compiler/lib/`:

**Core Library (`core/`):**

| Module                         | File                      | Description                                        |
| ------------------------------ | ------------------------- | -------------------------------------------------- |
| `core:prelude`                 | `prelude.wado`            | Auto-imported re-exports from prelude sub-modules  |
| `core:prelude/traits.wado`     | `prelude/traits.wado`     | Trait definitions (Eq, Ord, Iterator, etc.)        |
| `core:prelude/types.wado`      | `prelude/types.wado`      | Core types (Option, Result, Stream, Future)        |
| `core:prelude/string.wado`     | `prelude/string.wado`     | String type and string iterators                   |
| `core:prelude/array.wado`      | `prelude/array.wado`      | Array type and array iterators                     |
| `core:prelude/int128.wado`     | `prelude/int128.wado`     | u128/i128 types (re-exported from prelude)         |
| `core:prelude/primitives.wado` | `prelude/primitives.wado` | Primitive type trait implementations               |
| `core:prelude/format.wado`     | `prelude/format.wado`     | Format traits (Display, Formatter)                 |
| `core:prelude/fpfmt.wado`      | `prelude/fpfmt.wado`      | Float-to-string formatting (pure Wado)             |
| `core:cli`                     | `cli.wado`                | CLI output (println, eprintln, etc.)               |
| `core:collections`             | `collections.wado`        | TreeMap and other collections                      |
| `core:zlib`                    | `zlib.wado`               | Compression (zlib/deflate)                         |
| `core:base64`                  | `base64.wado`             | Base64 encoding/decoding (RFC 4648)                |
| `core:internal`                | `internal.wado`           | Compiler-generated code support, panic/unreachable |
| `core:builtin`                 | `builtin.wado`            | Compiler intrinsics with `#[canonical(...)]` attrs |

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
make test-wado   # runs all *_test.wado files
cargo run --bin wado -- test wado-compiler/lib/core/zlib_test.wado  # run one file
```

Test names can contain any characters (parentheses, dashes, etc.) — the compiler sanitizes them into valid kebab-case CM export names.

The `#[expect_trap]` and `#[TODO]` attributes mark tests as expected to trap. The compiler encodes this in the export name prefix:

```
test-0-simple            # normal test export
test-trap-1-panics       # #[expect_trap]: passes when body traps
test-todo-2-wip          # #[TODO]: passes when body traps; distinct failure message when it doesn't
```

Both `wado test` and the e2e test runner detect these prefixes and handle pass/fail accordingly.

### WASI Registry

The `WasiRegistry` module (`component_model.rs`) collects WASI import information from `lib/wasi/*.wado` files and provides it to the code generator for dynamic Component Model generation.

**Purpose:**

- Extract WASI version strings from `#[wasi(...)]` attributes (e.g., `0.3.0-rc-2025-09-16`)
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
2. Parse enum variants from `#[wasi(...)]` annotated enums in wasi/\*.wado
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

**CM Adapter expansion:** During the CM Adapter phase, `task return expr` is expanded in-place to a sequence of TIR that:

1. Lowers the Wado value to flat CM ABI values (using `synthesize_lower_to_flat`)
2. Calls `builtin::task_return(0, flat0, flat1, ...)` — the `0` is the Ok discriminant

This expansion is performed by `expand_task_returns_in_func` in `cm_adapter_gen.rs`, which walks the function body and replaces each `TirStmtKind::TaskReturn` with the expanded sequence.

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

| Function              | Wasm Instruction      | Category    |
| --------------------- | --------------------- | ----------- |
| `i32_and`             | `i32.and`             | i32 ops     |
| `i32_eqz`             | `i32.eqz`             | i32 ops     |
| `i32_clz`             | `i32.clz`             | i32 ops     |
| `i64_clz`             | `i64.clz`             | i64 ops     |
| `array_len`           | `array.len`           | Array       |
| `array_get_u8`        | `array.get_u $type`   | Array       |
| `array_set_u8`        | `array.set $type`     | Array       |
| `memory_store8`       | `i32.store8`          | Memory      |
| `memory_load8_u`      | `i32.load8_u`         | Memory      |
| `unreachable`         | `unreachable`         | Control     |
| `f32_abs`             | `f32.abs`             | Float math  |
| `f64_abs`             | `f64.abs`             | Float math  |
| `f32_ceil`            | `f32.ceil`            | Float math  |
| `f64_ceil`            | `f64.ceil`            | Float math  |
| `f32_floor`           | `f32.floor`           | Float math  |
| `f64_floor`           | `f64.floor`           | Float math  |
| `f32_trunc`           | `f32.trunc`           | Float math  |
| `f64_trunc`           | `f64.trunc`           | Float math  |
| `f32_nearest`         | `f32.nearest`         | Float math  |
| `f64_nearest`         | `f64.nearest`         | Float math  |
| `f32_sqrt`            | `f32.sqrt`            | Float math  |
| `f64_sqrt`            | `f64.sqrt`            | Float math  |
| `f32_min`             | `f32.min`             | Float math  |
| `f64_min`             | `f64.min`             | Float math  |
| `f32_max`             | `f32.max`             | Float math  |
| `f64_max`             | `f64.max`             | Float math  |
| `f32_copysign`        | `f32.copysign`        | Float math  |
| `f64_copysign`        | `f64.copysign`        | Float math  |
| `i64_add128`          | `i64.add128`          | Wide arith  |
| `i64_sub128`          | `i64.sub128`          | Wide arith  |
| `i64_mul_wide_u`      | `i64.mul_wide_u`      | Wide arith  |
| `i64_mul_wide_s`      | `i64.mul_wide_s`      | Wide arith  |
| `i64_reinterpret_f64` | `i64.reinterpret_f64` | Reinterpret |
| `f64_reinterpret_i64` | `f64.reinterpret_i64` | Reinterpret |
| `i32_reinterpret_f32` | `i32.reinterpret_f32` | Reinterpret |
| `f32_reinterpret_i32` | `f32.reinterpret_i32` | Reinterpret |

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

**Namespace Validation:**

```rust
pub enum ModuleSource {
    Core { name: String },      // core:prelude, core:cli
    Wasi { interface: String }, // wasi:cli, wasi:io
    Local { path: String },     // ./module.wado, ../lib.wado
    Remote { url: String },     // https://example.com/lib.wado
    EntryPoint,                 // The main entry module
}
```

**Resolution Rules:**

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

For-of loops are desugared to use `IntoIterator` and `Iterator` traits:

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

- **Cross-module monomorphization**: Generic stdlib methods (like `ArrayIter::collect` calling `Array::append`) may encounter type table ID mismatches when called from user code. Workaround: Use direct builtin calls in stdlib generic functions instead of method calls.

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

### 128-bit Integer Types (i128/u128)

See [WEP: 128-bit Integer Types](./wep-2026-01-24-i128-u128-types.md).

### Template String Interpolation

Wado supports template strings with interpolation using backticks and `{expr}` syntax, similar to JavaScript but with Python-like format specifiers.

**Syntax:**

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

**Lexer (`lexer.rs`):**

- Backtick string tokenization with `TemplateStringLit` token
- Brace depth tracking to handle nested `{}` in interpolations
- String literal tracking inside interpolations
- Escape sequence support (`\n`, `\t`, `\uHHHH`, `\{`, `\}`, etc.)
- `\{` and `\}` produce literal braces without triggering interpolation (emitted as `{{`/`}}` for the parser's doubling rule)
- Nested template string support

**Parser (`parser.rs`):**

- Template string AST nodes (`TemplateStringExpr`, `TemplatePart`, `FormatSpec`)
- Interpolation expression parsing (any valid expression)
- Format specifier extraction after `:`
- **`:` vs `::` distinction**: Single-character lookahead to differentiate:
  - `:` alone → format specifier start
  - `::` → scope resolution (part of expression)
- Recursive parsing for nested template strings
- Comprehensive error handling

**AST Structure:**

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

**Template Expansion (pre-monomorphize, `synthesis/template.rs`):**

Template strings are expanded before monomorphization. The resolver emits `TirExprKind::TemplateString` nodes without expansion; the synthesis phase replaces each with a `__tmpl` labeled block containing:

- `String::with_capacity(N)` to allocate a buffer
- `String::append(literal)` for literal parts
- `Formatter` construction with format spec fields
- Trait method calls to `Display::fmt` or `Inspect::inspect` based on the concrete type
- Direct `Formatter::write_str` for optimized paths (e.g., String append, closure source text)

Template expansion emits generic trait method calls that the monomorphizer resolves to concrete implementations.

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

### Reactive Signals

Wado's `reactive` keyword compiles to efficient Wasm code with minimal runtime overhead.

**Reactive Value Categories:**

```wado
let reactive mut count = 0;           // Source: mutable reactive state
let reactive doubled = || count * 2;  // Derived: computed from sources
```

**Sources** are mutable reactive values that can be directly assigned.
**Derived** values are computed from other reactive values (sources or other derived).

**Compilation Strategy:**

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

**Effect Blocks (TBD):**

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

**Execution Context:**

The compiler generates different code depending on the target hosted world:

CLI hosted world (`wasi:cli/command` — synchronous):

- Updates propagate immediately at each mutation site
- Effect closures are called inline, synchronously
- No event loop or scheduler needed

```wat
;; CLI: Synchronous update at mutation site
(local.set $count (i32.const 5))
(call $effect_0)  ;; Effect runs immediately
;; Next statement executes after effect completes
```

Event-looped hosted world (Browser/GUI — future):

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

**Wasm Representation:**

| Wado Construct            | Wasm Representation                             |
| ------------------------- | ----------------------------------------------- |
| `reactive mut` source     | Local variable + generated update dispatch      |
| `reactive` derived        | Local variable, recomputed on dependency change |
| `effect { ... }`          | Closure called after dependency mutations       |
| `&reactive T` (reference) | Wasm GC struct ref with getter/setter           |

**Dynamic Dependencies (Future):**

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

**Reactive References:**

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

### Value Semantics

Wado uses value semantics for composite types: assignment creates a copy rather than sharing references. This matches the behavior users expect from languages like Swift and differs from reference semantics in languages like JavaScript.

**Supported Types:**

| Type        | Copy Strategy                                     |
| ----------- | ------------------------------------------------- |
| Struct      | Field-by-field copy via `struct.get`/`struct.new` |
| Array<T>    | Element-by-element copy with loop                 |
| Tuple       | Same as struct (implemented as Wasm GC struct)    |
| String      | Element copy with `ArrayGetU` (packed i8)         |
| Option<T>   | Conditional copy if non-null                      |
| Result<T,E> | Not yet implemented (codegen blocked)             |
| Variant     | Copy tag + all fields                             |

**Reference Types:**

Reference types (`&T`, `&mut T`) do **not** have value semantics - they share the underlying value. This is intentional: references provide a way to share data when needed.

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

The resolver (`resolver.rs`) handles auto-deref in `resolve_method_call()`:

1. Check if receiver type is `Ref(T)` or `MutRef(T)`
2. If so, insert a `TirUnaryOp::Deref` expression
3. Repeat until receiver is not a reference type
4. Proceed with normal method resolution on the dereferenced type

**Supported Cases:**

| Receiver Type | Auto-Deref | Example                                      |
| ------------- | ---------- | -------------------------------------------- |
| `&T`          | ✅         | `(&point).sum()` → `(*&point).sum()`         |
| `&mut T`      | ✅         | `(&mut point).sum()` → `(*&mut point).sum()` |
| `&&T`         | ✅         | Double deref applied                         |
| `&&&T`        | ✅         | Triple deref applied                         |
| `&Box<T>`     | ✅         | Generic struct methods work                  |
| `&String`     | ✅         | String methods like `.len()` work            |
| `&Array<T>`   | ❌         | See Known Limitations                        |

### String Equality

String equality (`==` and `!=`) uses value semantics - comparing the actual string contents rather than reference identity.

**Desugaring:**

The resolver desugars string comparisons to calls to `core::internal::string_eq`:

```wado
// Source
a == b
a != b

// Desugared to
core::internal::string_eq(&a, &b)
!core::internal::string_eq(&a, &b)
```

**Implementation:**

The `string_eq` function in `lib/core/internal.wado`:

```wado
pub fn string_eq(a: &String, b: &String) -> bool {
    let len_a = a.len();
    let len_b = b.len();
    if len_a != len_b {
        return false;
    }
    for (let mut i = 0; i < len_a; i += 1) {
        if a.get(i) != b.get(i) {
            return false;
        }
    }
    return true;
}
```

Key design decisions:

- Takes `&String` parameters to avoid copying strings
- Uses auto-dereference so `a.len()` and `a.get(i)` work on references
- Byte-by-byte comparison (UTF-8 safe since equal strings have identical byte sequences)

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

## Implemented

### Parser

#### Items

- `use` declarations
- `fn` declarations (with params, return type, effects)
- `pub` modifier
- `effect` declarations
- `struct` declarations
- `impl` blocks
- `trait` declarations (static dispatch, default method implementations)
- `impl Trait for Type` (trait implementations)
- Associated types in traits (`type Output;` and `type Output = T;`)
- `enum` declarations (payload-free, CM semantics, match/if let/matches, auto-derived Display/Eq/Ord, impl blocks)
- `global` declarations (module-level Wasm globals)
- `type` declarations (newtypes)
- `resource` declarations
- `world` declarations (with imports/exports)
- Attributes (`#[...]`)
- `variant` declarations (with payloads, construction, if let pattern matching with tuple destructuring)
- Generic parameters on structs (monomorphization)

#### Statements

- `let` statements (with `mut`, `reactive`, type annotation)
- Expression statements
- `return` statements
- `assert` statements with optional message (power-assert style error messages)
- `if` statements
- `while` loops
- C-style `for` loops
- `loop` loops
- `for-of` loops

#### Expressions

- Identifiers
- Integer literals
- Float literals
- String literals
- Character literals
- Boolean literals (`true`, `false`)
- Null literal (`null`)
- Unit `()`
- Template string interpolation (`` `Hello, {name}!` ``)
- Compile-time location literals (`#file`, `#line`, `#function`)
- Binary operators (arithmetic, comparison, logical, bitwise)
- Comparison chaining (`a < b < c` → `a < b && b < c`)
- Unary operators (`-`, `!`, `~`, `&`, `*`)
- Parentheses for grouping `(expr)`
- Function calls
- Method calls
- Static method calls (`Point::origin()`)
- Static method calls on generic types (`Array::<i32>::with_capacity()`)
- Field access
- Index access (`[]`)
- Type cast (`as T`) for primitive types
- Closures (`|params| expr`)
- `if` expressions (`let x = if cond { a } else { b }`)
- Block expressions / Labeled blocks (`scope: { ... }`)
- Struct literals (`Point { x: 1, y: 2 }`)
- Implicit struct literals with type context (`let p: Point = { x: 1, y: 2 }`)
- Tuple literals (`[1, 2, 3]` creates `[i32, i32, i32]`)
- Array literals (`[1, 2, 3] as Array<i32>` or via type coercion)

#### Types

- Named types
- Generic types (`Array<T>`, `Result<T, E>`)
- Reference types (`&T`)
- Tuple types (`[T, U]`)
- Unit type `()`
- Never type `!`

#### Patterns

- Identifier patterns
- Mutable identifier patterns (`if let Some(mut x) = ...`, `match { Ok(mut v) => ... }`)
- Wildcard `_`
- Tuple patterns
- Variant patterns in if let (`if let Circle(r) = shape`, `if let Rect([w, h]) = shape`)
- Option variant patterns (`if let Some(x) = ...`, `if let None = ...`)

### Semantic Analysis

- Symbol table construction
- Module resolution (core library)
- Import resolution
- Builtin function detection
- Effect checking (validates function calls have required effects)
- Scope analysis for variables
- Immutable variable reassignment detection

### Code Generation

- Component Model binary output
- WASI P3 imports (`wasi:cli/stdout`, `wasi:cli/types`)
- Stream/Future intrinsics (`stream.*`, `future.*`)
- Async task intrinsics (`task.return`, `waitable-set.*`, `subtask.drop`)
- Memory module with string data
- `println` function (core::cli)
- Multiple function calls
- Async function lifting/lowering
- Template strings (literals, integer interpolation, float interpolation, format specifiers)
- Variables and locals (`let`, `let mut`)
- Global variables (`global`, `global mut`) with Wasm global section
- Control flow (`if` statements, `if let init`, `while`, `for`, `loop`, `for-of`)
- Binary/unary operations (arithmetic, comparison, logical, bitwise)
- Type cast (`as T`) for primitive types (i32, i64, f32, f64)
- Assert statements with power-assert style error messages (intermediate values, value caching)
- User-defined functions (from core:: modules)
- Branch hinting (`builtin::likely`, `builtin::unlikely`)
- WASI clocks (`wasi:clocks/monotonic-clock`, `now()`)
- Mixed-type arithmetic (i64 vs i32 literal promotion)
- Struct construction and field access
- Tuple construction and index access
- Array construction, index access, and iteration
- Reference types (`&T`, `&mut T`) for primitives, structs, tuples, and arrays
- Dereference operator (`*ref`)
- Auto-dereference for method calls (`ref.method()` auto-derefs to `(*ref).method()`)
- String equality (`==`, `!=`) with value semantics (desugared to `string_eq(&a, &b)`)
- Generic structs with monomorphization (`Box<T>`, `Pair<A, B>`)
- Generic struct instantiation with type inference
- Generic struct field access
- Impl blocks on generic struct specializations (`impl Box<i32>`)
- Nested generic types (`Box<Box<i32>>`, `Array<Pair<i32, String>>`)
- Generic structs as function parameters and return types
- Generic functions with explicit turbofish syntax (`identity::<i32>(x)`)
- Generic methods with explicit turbofish syntax (`obj.method::<T, U>(...)`)
- Double generics with mixed types (`Container<T>.combine::<U, V>()` where T, U, V are different)
- Variant construction (`Option::<T>::Some(x)`, `Color::Red`, `Shape::Circle(r)`)
- Option pattern matching (`if let Some(x) = ...`)
- Custom variant pattern matching (`if let Circle(r) = shape`, `if let Rect([w, h]) = shape`)
- Closures (pure and capturing, `&mut ||` for mutable captures)
- Template string type conversion (i8/i16/i32/i64/u8/u16/u32/u64/bool/char → string, f32/f64 → string)
- Value semantics for structs (field-by-field copy on assignment)
- Value semantics for arrays (element-by-element copy on assignment)
- Value semantics for tuples (field-by-field copy on assignment)
- Value semantics for strings (element-by-element copy on assignment)
- Value semantics for Option<T> (conditional copy of inner value)
- Value semantics for Variant (copy tag + all fields)
- Template string array concatenation

---

## In Progress

### Partial Implementations

- **Variant pattern matching**: Single-payload and tuple-payload cases work (`if let Circle(r) = shape`, `if let Rect([w, h]) = shape`). Struct payloads not yet supported. See [WEP: Variant Payload Design](./wep-2026-01-25-variant-payload-design.md).
- **Function types**: Parser supports `fn(T) -> U` syntax, closure codegen works (both pure and capturing), but full function type support is incomplete.
- **Stream/Future resource migration**: Resource declarations (`resource Stream<T>`, `resource Future<T>`, etc.) exist in `core:prelude/types.wado`, but method resolution (`.new()`, `.read()`, `.write()`, `.close()`, `.drop()`) is still hardcoded in the resolver (`method_call.rs`) rather than being driven by the resource declarations. The low-level canonical builtins in `builtin.wado` (`stream_new`, `stream_read`, `future_write`, etc.) remain the actual backing implementation.

### Known Limitations

1. **Parser doesn't support generic resources**: `resource Stream<T>` in `prelude.wado` fails to parse
2. **Implicit struct literals don't work with generic structs**: `let b: Box<i32> = { value };` fails. Use explicit form: `let b: Box<i32> = Box { value };`
3. **No type checking**: The analyzer doesn't perform type checking yet
4. **GC arrays cannot be passed directly to streams**: As of wasmtime v40, `stream<u8>` operations require linear memory. GC arrays must be copied to linear memory before writing to streams. See [component-model#525](https://github.com/WebAssembly/component-model/issues/525)
5. **Non-pub functions from other modules are skipped**: The codegen currently only includes `pub` functions from imported modules (`core::*`). Internal helper functions must be marked `pub` to be included in compilation. This limitation could be addressed later with proper internal dependency tracking.

---

## Not Yet Implemented

- Range expressions
- `?` operator (error propagation)
- Type inference
- Effect handlers
- Reactive signals (source values, derived values, effect blocks)
- JSX
- Generic function/method type inference
