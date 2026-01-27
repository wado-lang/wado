# Wado Compiler

This document describes the Wado compiler architecture and implementation status.

## Compiler Architecture

The compiler follows a multi-phase pipeline:

```
Source (.wado) → Lexer → Parser → Bind → Desugar → Load → Analyze → Resolve → Effect Check → Monomorphize → Lower → Optimize → Codegen
                           ↓                                           ↓                           ↓          ↓
                       Unparser                                  TIR (Typed IR)              TIR Unparser  TIR Unparser
                           ↓                                                                      ↓              ↓
                   Formatted Source                                                       Pseudo-Wado Source (pre/post lower)
```

### Compilation Pipeline

| Phase        | Input         | Output       | Description                              |
| ------------ | ------------- | ------------ | ---------------------------------------- |
| Lexer        | Source        | Tokens       | Tokenize, extract `__DATA__` section     |
| Parser       | Tokens        | AST          | Build abstract syntax tree               |
| Bind         | AST           | Bind info    | Local name resolution, scope tracking    |
| Desugar      | AST           | AST          | Transform syntactic sugar                |
| Load         | AST           | All modules  | Load all dependencies recursively        |
| Analyze      | All modules   | Symbol table | Build symbol table, validate imports     |
| Resolve      | AST + Symbols | Project      | Type resolution, produce Project         |
| Effect Check | Project       | Errors       | Validate function effect requirements    |
| Monomorphize | Project       | Project      | Instantiate generics with concrete types |
| Lower        | Project       | Project      | String literal collection                |
| Optimize     | Project       | Project      | DCE, usage analysis, feature flags       |
| Codegen      | Project       | Wasm bytes   | Generate Component Model Wasm            |

### Modules

| Module           | File                    | Description                                           |
| ---------------- | ----------------------- | ----------------------------------------------------- |
| Lexer            | `lexer.rs`              | Tokenizes source code, extracts `__DATA__` section    |
| Parser           | `parser.rs`             | Recursive descent parser, builds AST                  |
| AST              | `ast.rs`                | AST node definitions, `Module::data_section()` API    |
| Token            | `token.rs`              | Token types and spans                                 |
| Comment          | `comment.rs`            | Comment collection and CommentMap for formatting      |
| Bind             | `bind.rs`               | Local name binding, scope analysis, mutability check  |
| Loader           | `loader.rs`             | Module loading, dependency resolution                 |
| Desugar          | `desugar.rs`            | AST transformations (compound assign, etc.)           |
| EffectCheck      | `effect_check.rs`       | Validates effect requirements for function calls      |
| Unparser         | `unparse.rs`            | Converts AST/TIR back to source code                  |
| Analyzer         | `analyze.rs`            | Semantic analysis, symbol table construction          |
| Symbol           | `symbol.rs`             | Symbol table data structures                          |
| Name             | `name.rs`               | Name mangling utilities for methods and symbols       |
| ModuleLoader     | `module_loader.rs`      | Module path resolution, loads core library            |
| Resolver         | `resolver.rs`           | Type resolution, AST to TIR, produces Project         |
| TIR              | `tir.rs`                | Typed Intermediate Representation                     |
| Monomorphize     | `monomorphize.rs`       | Generic type/function instantiation (Project→Project) |
| Lower            | `lower.rs`              | String literal collection (Project→Project)           |
| Project          | `project.rs`            | Project: compilation context passed through pipeline  |
| Optimize         | `optimize.rs`           | Optimization coordinator, dispatches to sub-modules   |
| OptimizeDCE      | `optimize_dce.rs`       | Dead code elimination via reachability analysis       |
| OptimizeInline   | `optimize_inline.rs`    | Function inlining for small, pure functions           |
| OptimizeRefElim  | `optimize_ref_elim.rs`  | Reference elimination after inlining                  |
| OptimizeCopyProp | `optimize_copy_prop.rs` | Copy propagation for trivial bindings                 |
| OptimizeLICM     | `optimize_licm.rs`      | Loop-invariant code motion                            |
| OptimizeMove     | `optimize_move.rs`      | Move insertion for fresh values, copy type collection |
| Stdlib           | `stdlib.rs`             | Embedded core library sources                         |
| WasiRegistry     | `wasi_registry.rs`      | WASI import registry, type alias resolution           |
| BuiltinRegistry  | `builtin_registry.rs`   | Builtin function registry from `core:builtin`         |
| WorldRegistry    | `world_registry.rs`     | World definitions registry for export signatures      |
| WasmBuilder      | `wasm_builder.rs`       | Wasm index tracking utilities                         |
| Codegen          | `codegen.rs`            | Generates Component Model Wasm via wasm-encoder       |
| Bundled          | `bundled.rs`            | Loads pre-compiled Wasm builtins (wado-bundled)       |
| Postproc         | `wasm_postprocess.rs`   | Wasm binary transformations                           |

---

## Module Details

### Parser and Desugar Separation

The parser preserves source syntax literally to enable accurate formatting via the unparser. Syntactic sugar is transformed in the desugar pass before codegen.

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

| Feature                  | Example                       | Status |
| ------------------------ | ----------------------------- | ------ |
| Single type parameter    | `Box<i32>`                    | ✅     |
| Multiple type parameters | `Pair<i32, String>`           | ✅     |
| Nested generics          | `Box<Box<i32>>`               | ✅     |
| Generics in Array        | `Array<Pair<i32, String>>`    | ✅     |
| Struct type parameters   | `Box<Point>`                  | ✅     |
| Impl on specialization   | `impl Box<i32> { fn get() }`  | ✅     |
| Generic functions        | `fn identity<T>(x: T) -> T`   | ✅     |
| Generic methods          | `impl T { fn foo<U>(&self) }` | ✅     |

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

The `optimize.rs` module coordinates multiple optimization passes on TIR. The optimizer follows the ownership transfer pattern: `optimize(project: Project, opt_level: OptLevel) -> Project`.

**Optimization Passes:**

| Module                | File                    | Description                                            |
| --------------------- | ----------------------- | ------------------------------------------------------ |
| DCE                   | `optimize_dce.rs`       | Dead code elimination via reachability analysis        |
| Function Inlining     | `optimize_inline.rs`    | Inline small, pure functions                           |
| Reference Elimination | `optimize_ref_elim.rs`  | Eliminate unnecessary `&local` bindings after inline   |
| Copy Propagation      | `optimize_copy_prop.rs` | Propagate trivial copies like `let x = y`              |
| LICM                  | `optimize_licm.rs`      | Hoist loop-invariant field accesses                    |
| Move Insertion        | `optimize_move.rs`      | Avoid copies for fresh values (literals, call results) |

**Optimization Order:**

For `-O2` and `-Os`:

1. **Inlining** → inline small functions
2. **Reference Elimination** → clean up `&local` bindings from inlining
3. **Copy Propagation** → eliminate trivial copies
4. **LICM** → hoist loop-invariant code
5. **DCE Analysis** → determine reachable functions
6. **DCE Removal** → remove unreachable functions
7. **Move Insertion** → mark fresh values as movable (all optimization levels)
8. **Value Copy Collection** → collect types needing copy support for codegen

**Usage Analysis Fields (populated in Project):**

| Field                 | Type                               | Description                                 |
| --------------------- | ---------------------------------- | ------------------------------------------- |
| `reachable_functions` | `HashSet<FunctionId>`              | Functions reachable from entry point (DCE)  |
| `all_reachable`       | `bool`                             | When true, DCE is disabled                  |
| `used_effects`        | `HashSet<WasiEffect>`              | WASI effects used (Stdout, Stderr, etc.)    |
| `used_wasi_functions` | `HashSet<String>`                  | WASI functions called                       |
| `used_builtins`       | `HashSet<CanonBuiltin>`            | Canonical builtins used (stream ops, etc.)  |
| `used_box_primitives` | `HashSet<PrimitiveType>`           | Primitives needing box types for references |
| `strip_names`         | `bool`                             | Whether to strip debug name sections        |
| `needed_copy_types`   | `HashMap<String, HashSet<TypeId>>` | Types requiring value copy per function     |

**CLI Control:**

| Flag  | Effect                                                      |
| ----- | ----------------------------------------------------------- |
| `-O0` | No optimizations, includes all functions/features           |
| `-O1` | DCE only, keeps debug names                                 |
| `-O2` | Full optimizations (inline, ref-elim, copy-prop, LICM, DCE) |
| `-Os` | Full optimizations + strips debug name sections             |

### Standard Library

Embedded `.wado` files in `wado-compiler/lib/`:

**Core Library (`core/`):**

| Module                | File                  | Description                                        |
| --------------------- | --------------------- | -------------------------------------------------- |
| `core:prelude`        | `prelude.wado`        | Auto-imported types and functions                  |
| `core:cli`            | `cli.wado`            | CLI output (println, eprintln, etc.)               |
| `core:clocks`         | `clocks.wado`         | MonotonicClock, now()                              |
| `core:filesystem`     | `filesystem.wado`     | Filesystem operations                              |
| `core:stream`         | `stream.wado`         | Stream utilities                                   |
| `core:collections`    | `collections.wado`    | TreeMap and other collections                      |
| `core:prelude/int128` | `prelude/int128.wado` | u128/i128 types (re-exported from prelude)         |
| `core:internal`       | `internal.wado`       | Compiler-generated code support                    |
| `core:builtin`        | `builtin.wado`        | Compiler intrinsics with `#[canonical(...)]` attrs |

**WASI Library (`wasi/`):**

| Module            | File              | Description      |
| ----------------- | ----------------- | ---------------- |
| `wasi:io`         | `io.wado`         | I/O interfaces   |
| `wasi:cli`        | `cli.wado`        | CLI interfaces   |
| `wasi:clocks`     | `clocks.wado`     | Clock interfaces |
| `wasi:filesystem` | `filesystem.wado` | FS interfaces    |

### WASI Registry

The `WasiRegistry` module (`wasi_registry.rs`) collects WASI import information from `lib/wasi/*.wado` files and provides it to the code generator for dynamic Component Model generation.

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
| Type aliases         | `Instant` → `u64`, `Duration` → `u64` resolved from wasi/\*.wado |
| Function signatures  | Params and return types parsed from effect methods               |
| Supported interfaces | Dynamically filtered based on type support                       |

**Dynamic Interface Filtering:**

Instead of a hardcoded whitelist, interfaces are included based on type support:

- Only interfaces where ALL functions have supported types are imported
- Supported param types: primitives (`i32`, `u64`, `bool`, `char`, `String`, etc.), `Stream<T>`
- Supported return types: same as params plus `Result<T, E>`
- Type aliases are resolved before filtering (e.g., `Instant` → `u64`)
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

### Builtin Registry

The `BuiltinRegistry` module (`builtin_registry.rs`) collects function signatures from `lib/core/builtin.wado` and provides type information for code generation.

**The `#[canonical("...")]` Attribute:**

Builtins in `builtin.wado` are divided into two categories:

1. **Canonical builtins** - Functions with `#[canonical("name")]` attribute are imported as Component Model canonical built-ins
2. **Instruction builtins** - Functions without the attribute compile directly to Wasm instructions

```wado
// Canonical builtin - imported as CM function "stream-new"
#[canonical("stream-new")]
fn stream_new() -> i64;

// Instruction builtin - compiles to Wasm i32.and instruction
fn i32_and(a: i32, b: i32) -> i32;
```

**Canonical Builtins (12 functions):**

| Wado Name              | Canonical Name         | Category         |
| ---------------------- | ---------------------- | ---------------- |
| `stream_new`           | `stream-new`           | Stream           |
| `stream_write`         | `stream-write`         | Stream           |
| `stream_drop_writable` | `stream-drop-writable` | Stream           |
| `stream_drop_readable` | `stream-drop-readable` | Stream           |
| `task_return`          | `task-return`          | Async task       |
| `waitable_set_new`     | `waitable-set-new`     | Async task       |
| `waitable_join`        | `waitable-join`        | Async task       |
| `waitable_set_wait`    | `waitable-set-wait`    | Async task       |
| `subtask_drop`         | `subtask-drop`         | Async task       |
| `realloc`              | `realloc`              | Memory (bundled) |
| `f64_to_buffer`        | `f64_to_buffer`        | Float (bundled)  |
| `f32_to_buffer`        | `f32_to_buffer`        | Float (bundled)  |

**Instruction Builtins:**

| Function         | Wasm Instruction         | Category |
| ---------------- | ------------------------ | -------- |
| `i32_and`        | `i32.and`                | i32 ops  |
| `i32_eqz`        | `i32.eqz`                | i32 ops  |
| `array_len`      | `array.len`              | Array    |
| `array_get_u8`   | `array.get_u $type`      | Array    |
| `array_set_u8`   | `array.set $type`        | Array    |
| `string_new`     | `array.new_default`      | String   |
| `memory_store8`  | `i32.store8`             | Memory   |
| `memory_load8_u` | `i32.load8_u`            | Memory   |
| `effect_wait`    | (effect synchronization) | Effects  |
| `unreachable`    | `unreachable`            | Control  |

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

The module loader validates module paths before loading:

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

**Method Resolution Priority:**

1. **Inherent methods** (methods in `impl Type { }`) take priority over trait methods
2. **Trait methods** (methods in `impl Trait for Type { }`) are used when no inherent method matches
3. If multiple traits define the same method name, it's currently a compile error (disambiguation syntax not yet implemented)

**Advantages of Static Dispatch:**

- **Zero runtime overhead**: No vtable lookup
- **Inlining possible**: Optimizer can inline trait methods
- **Dead code elimination**: Unused trait implementations are removed

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
Ord::lt(&a, &b)

// a <= b desugars to:
Ord::lt(&a, &b) || Eq::eq(&a, &b)

// a > b desugars to:
Ord::lt(&b, &a)

// a >= b desugars to:
Ord::lt(&b, &a) || Eq::eq(&a, &b)
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

// Stream intrinsics (Component Model)
builtin::stream_new() -> i64              // Create stream, returns rx|tx packed
                                          // Extract: rx = handles as i32, tx = (handles >> 32) as i32
builtin::stream_write(tx: i32, ptr: i32, len: i32) -> i32
builtin::stream_drop_writable(tx: i32)
builtin::stream_drop_readable(rx: i32)

// Async task intrinsics (Component Model)
builtin::waitable_set_new() -> i32
builtin::waitable_join(set: i32, subtask: i32)
builtin::waitable_set_wait(set: i32, outptr: i32) -> i32
builtin::subtask_drop(subtask: i32)

// Effect synchronization
builtin::effect_wait()                // Wait for all pending effects to complete

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

Unlike primitive types (`i32`, `i64`, etc.), Wado's 128-bit integers are implemented as **structs** in `core:prelude/int128` and re-exported from the prelude, making them available without explicit imports:

```wado
pub struct u128 {
    low: u64,
    high: u64,
}

pub struct i128 {
    low: u64,
    high: i64,
}
```

**Why structs?**

1. **Wasm has no 128-bit primitive**: Only 32/64-bit integers are native
2. **Most operations need software implementation**: Division, shifts, bitwise ops
3. **Leverages operator overloading**: Uses `Add`, `Sub`, `Eq`, `Ord` traits
4. **Component Model compatibility**: WIT has no 128-bit types; struct maps to record

**Available operations:**

| Category   | Operations                                                     |
| ---------- | -------------------------------------------------------------- |
| Arithmetic | `+`, `-` (via `Add`/`Sub` traits), `mul_u64()` method          |
| Comparison | `==`, `!=`, `<`, `<=`, `>`, `>=` (via `Eq`/`Ord` traits)       |
| Negation   | Unary `-` for i128 (via `Neg` trait)                           |
| Conversion | `from_u64()`, `from_i64()`, `from_string()`, `low()`, `high()` |

**Wasm Wide Arithmetic instructions** are used for efficient add/sub:

- `builtin::i64_add128` → `i64.add128`
- `builtin::i64_sub128` → `i64.sub128`
- `builtin::i64_mul_wide_u` → `i64.mul_wide_u`

See [WEP: 128-bit Integer Types](./wep-2026-01-24-i128-u128-types.md) for full design rationale.

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
- Escape sequence support (`\n`, `\t`, `\uHHHH`, etc.)
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

**Codegen (`codegen.rs`):**

- String literal parts are collected and embedded in data section
- Interpolation expressions are evaluated
- Template strings produce `ref (array u8)` type
- Integer interpolation (signed i8/i16/i32/i64 and unsigned u8/u16/u32/u64 converted to decimal string)
- Float interpolation (f32/f64 via `wado-bundled` functions using the `ryu` algorithm)
- String concatenation using GC array allocation and `array.copy`

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

The compiler generates different code depending on the target world:

CLI World (Synchronous):

- Updates propagate immediately at each mutation site
- Effect closures are called inline, synchronously
- No event loop or scheduler needed

```wat
;; CLI: Synchronous update at mutation site
(local.set $count (i32.const 5))
(call $effect_0)  ;; Effect runs immediately
;; Next statement executes after effect completes
```

Event-looped World (Browser/GUI):

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
- `trait` declarations (static dispatch)
- `impl Trait for Type` (trait implementations)
- Associated types in traits (`type Output;` and `type Output = T;`)
- `enum` declarations (payload-free, CM semantics)
- `global` declarations (module-level Wasm globals)
- `type` aliases
- `resource` declarations
- `world` declarations (with imports/exports)
- Attributes (`#[...]`)
- `variant` declarations (with payloads, construction, if let pattern matching for single-payload cases)
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
- Wildcard `_`
- Tuple patterns
- Variant patterns in if let (single-payload: `if let Circle(r) = shape`)
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
- Stream intrinsics (`stream.new`, `stream.write`, `stream.drop-*`)
- Async task intrinsics (`task.return`, `waitable-set.*`, `subtask.drop`)
- Memory module with string data
- `println` function (core::cli)
- Multiple function calls
- Async function lifting/lowering
- Template strings (literals, integer interpolation, float interpolation via wado-bundled)
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
- Custom variant pattern matching (single-payload: `if let Circle(r) = shape`)
- Closures - pure (no captures)
- Template string type conversion (i8/i16/i32/i64/u8/u16/u32/u64/bool/char → string, f32/f64 → string via wado-bundled)
- Value semantics for structs (field-by-field copy on assignment)
- Value semantics for arrays (element-by-element copy on assignment)
- Value semantics for tuples (field-by-field copy on assignment)
- Value semantics for strings (element-by-element copy on assignment)
- Value semantics for Option<T> (conditional copy of inner value)
- Value semantics for Variant (copy tag + all fields)
- Template string array concatenation

### Current Capabilities

The compiler can currently:

1. **Parse** basic Wado programs with:
   - `use` imports from `core::cli`
   - `fn main()` with effect declarations
   - `println("...")` calls

2. **Generate** Component Model Wasm that:
   - Imports WASI P3 `wasi:cli/stdout` and `wasi:cli/types`
   - Uses async stream intrinsics for stdout
   - Runs successfully on wasmtime with P3 support

**Example Working Program:**

```wado
use {println, Stdout} from "core:cli";

fn run() with Stdout {
    println("Hello, world!");
}
```

---

## In Progress

### Partial Implementations

- **Template strings**: Syntax and basic interpolation work. Format specifiers (`.2f`, `0.3f`, etc.) are parsed but not implemented in codegen.
- **Variant pattern matching**: Single-payload cases work (`if let Circle(r) = shape`). Tuple/struct payloads not yet supported. See [WEP: Variant Payload Design](./wep-2026-01-25-variant-payload-design.md).
- **`core:prelude`**: Partial (parser doesn't support generic resources)
- **Function types**: Parser supports `Fn(T) -> U` syntax, basic closure codegen works, but full function type support is incomplete.

### Known Limitations

1. **Parser doesn't support generic resources**: `resource Stream<T>` in `prelude.wado` fails to parse
2. **No `flags` keyword**: Parser doesn't recognize `flags` declarations (bit flags). This prevents `wasi:filesystem` from being loaded by `build_wasi_registry_from_stdlib()` since it contains `flags` declarations.
3. **Implicit struct literals don't work with generic structs**: `let b: Box<i32> = { value };` fails. Use explicit form: `let b: Box<i32> = Box { value };`
4. **No type checking**: The analyzer doesn't perform type checking yet
5. **GC arrays cannot be passed directly to streams**: As of wasmtime v40, `stream<u8>` operations require linear memory. GC arrays must be copied to linear memory before writing to streams. See [component-model#525](https://github.com/WebAssembly/component-model/issues/525)
6. **Non-pub functions from other modules are skipped**: The codegen currently only includes `pub` functions from imported modules (`core::*`). Internal helper functions must be marked `pub` to be included in compilation. This limitation could be addressed later with proper internal dependency tracking.
7. **Auto-deref doesn't work on `&Array<T>`**: Method calls like `arr_ref.len()` where `arr_ref: &Array<i32>` fail with "unknown function: Array<i32>::len". This is due to how Array methods are resolved with monomorphized type names after auto-deref. Workaround: dereference explicitly `(*arr_ref).len()`.

---

## Not Yet Implemented

### Parser

- `#[data]` attribute for data section injection
- `flags` declarations (bit flags)
- Inner attributes (`#![...]`)
- `match` statements

### Expressions

- `match` expressions
- Range expressions
- `?` operator (error propagation)

### Patterns

- Literal patterns
- Struct patterns

### Semantic Analysis

- Simple type checking
- Generic type checking
- Type inference
- Borrow checking / move analysis
- Unused variable warnings

### Code Generation

- Custom variant pattern matching (tuple/struct payloads, see WEP)
- Match expressions (see WEP)
- Closures - with captures (see WEP)
- Effect handlers
- Template string format specifiers (`.2f`, etc.)
- Value semantics for Result<T, E> (blocked on Result codegen)
- String UTF-8 validation (reject invalid byte sequences at construction)
- Reactive signals (source values)
- Reactive signals (derived values)
- Reactive effect blocks (syntax TBD)
- Reactive references (`&reactive T`)
- Multiple modules/files
- Other WASI interfaces (filesystem, etc.)
- Generic function/method type inference
