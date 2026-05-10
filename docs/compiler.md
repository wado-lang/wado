# Wado Compiler

The Wado compiler (`wado-compiler/`) translates `.wado` source into a Wasm component binary. This document gives a high-level overview of the architecture; deeper topics live in their own docs:

- Optimization passes: [optimizer.md](./optimizer.md)
- LSP architecture: [WEP 2026-04-18](./wep-2026-04-18-lsp-architecture.md)
- Language features: [spec.md](./spec.md) and the [WEP index](./CLAUDE.md)

## Pipeline

```
Source (.wado)
  → Lex → Parse → Bind → Desugar          (per module, in loader)
  → Annotate (Analyze + Resolve + lower TIR)
  → Default-purity Check
  → Synthesis (auto-derives, template, From, serde, effect dispatch, CM bindings)
  → Effect Check → Stores Check
  → Link (Package → FlatPackage)
  → Monomorphize → Erase Newtypes & Flags
  → Lower
  → Optimize
  → WIR Build → WIR Optimize → Codegen
  → Wasm component bytes
```

The driver is `compile_after_load` in `src/lib.rs`.

| Phase           | Output          | Module(s)                                        |
| --------------- | --------------- | ------------------------------------------------ |
| Lex / Parse     | AST             | `lexer.rs`, `parser.rs`, `token.rs`, `syntax.rs` |
| Bind            | AST + bindings  | `bind.rs`                                        |
| Desugar         | AST             | `desugar.rs`                                     |
| Loader          | All modules     | `loader.rs`                                      |
| Annotate        | TIR + facts     | `annotate.rs`, `analyze.rs`, `resolver/`         |
| Synthesis       | TIR (extended)  | `synthesis/`                                     |
| Effect / Stores | TIR (validated) | `effect_check.rs`                                |
| Link            | `FlatPackage`   | `link.rs`                                        |
| Monomorphize    | `FlatPackage`   | `monomorphize/`                                  |
| Lower           | `FlatPackage`   | `lower/`                                         |
| Optimize        | `FlatPackage`   | `optimize/`                                      |
| WIR Build       | `WirPackage`    | `wir_build/`                                     |
| WIR Optimize    | `WirPackage`    | `wir_optimize/`                                  |
| Codegen         | Component bytes | `codegen/`                                       |

## Compilation Units and IRs

| Unit                              | Layer                                                                                    |
| --------------------------------- | ---------------------------------------------------------------------------------------- |
| `Module` (`ast.rs`)               | Surface AST. Preserves source-level syntax to support `wado format`.                     |
| `TirModule` (`tir.rs`)            | Typed IR. One per source module after annotate.                                          |
| `Package` (`package.rs`)          | Per-module compilation context, used from synthesis through link.                        |
| `FlatPackage` (`flat_package.rs`) | Flat list of all functions, types, and globals; used from monomorphize through codegen.  |
| `WirPackage` (`wir.rs`)           | Wasm IR — closer to Wasm core instructions, used for emit-time optimization and codegen. |

Codegen consumes `Package`/`WirPackage` without knowledge of earlier phases — the rule that keeps the back end decoupled from the front.

## Frontend (per-module)

The loader runs `lexer → parser → bind → desugar` on every loaded module:

- The lexer extracts the optional `__DATA__` section and tokenizes the rest.
- The parser builds a faithful AST. Compound assigns, comparison chains, struct shorthand, and `&self` parameters are kept verbatim so `wado format` round-trips; sugar is removed in `desugar.rs`.
- `bind.rs` performs local name resolution, scope/mutability checking, and use-before-define detection.
- `desugar.rs` rewrites `x += y` to `x = x + y`, `a < b < c` to a conjunction, for/while loops to explicit loop blocks, and other purely syntactic constructs.

## Annotate (Analyze + Resolve + TIR Lowering)

`annotate_loaded` (`annotate.rs`) is the entry point shared by LSP and batch compilation. It runs `analyze.rs` for symbol-table construction and `resolver/` for type checking; bodies are then lowered into TIR.

The result, `Annotated`, carries the TIR modules plus an `AstIndex` and a use→def map (`(ModuleSource, AstId) → SymbolKey`). This is what makes the architecture LSP-friendly: facts are attached to AST nodes without mutating them, so cross-file navigation, hover, and rename all fall out of the same data the batch compiler uses.

The resolver covers trait selection, generic inference, method dispatch, coercion, and effect typing. All trait calls are resolved statically — by the end of the pipeline every call targets a concrete monomorphized function. There is no runtime vtable.

## Synthesis

`synthesis::synthesize` (`synthesis.rs`) generates synthetic TIR that the user does not write:

| Sub-pass           | File                           | Output                                                                  |
| ------------------ | ------------------------------ | ----------------------------------------------------------------------- |
| Trait auto-derives | `synthesis/traits.rs`          | `Eq`, `Ord`, `Display`, `Inspect` impls for user types                  |
| `From` adapters    | `synthesis/from_synth.rs`      | `From` impls from `impl From<T> for U;` declarations                    |
| Serde              | `synthesis/serde_synth.rs`     | `Serialize` / `Deserialize` for body-less `impl Trait for T;`           |
| Template strings   | `synthesis/template.rs`        | Expands template strings into `Display::fmt` / `Inspect::inspect` calls |
| Effect dispatch    | `synthesis/effect_dispatch.rs` | Per-effect dispatch infrastructure for handler resolution               |
| CM bindings        | `synthesis/cm_binding/`        | Component Model boundary adapters (lift / lower / async export)         |

Synthesized impls are recorded back into the shared `TraitEnv` so subsequent phases query a single source of truth.

## Effect and Stores Checks

`check_effects` and `check_stores` (`effect_check.rs`) run after synthesis and before monomorphize. They validate that every function declares the effects and reference stores it actually requires. Synthesized CM boundary code is exempted.

## Link → Monomorphize → Erase

- `link.rs` merges per-module TIR into a single `FlatPackage`. After link, functions and types are addressed by global indices.
- `monomorphize/` walks call sites, instantiates generic structs and functions with concrete type arguments, and rewrites references. Generic structs are keyed by `(name, ModuleSource)` so same-named generics from different modules coexist. Variadic `TupleSpread` nodes are expanded here. `name.rs` produces stable mangled names (`Box$i32`, `identity$1`, …).
- `type_table.erase_newtypes_and_flags()` then collapses newtypes to their base type and flag types to `u32`. The distinction is needed during monomorphize for trait dispatch but not afterwards.

## Lower

`lower.rs` runs type-driven transformations on the flat package:

| Sub-pass            | File                | What it does                                               |
| ------------------- | ------------------- | ---------------------------------------------------------- |
| Wide-int match      | `lower/wide_int.rs` | i128/u128 match patterns → if-else chains                  |
| Pattern lowering    | `lower/pattern.rs`  | `LetDestructure` / `IfLet` → explicit Let + switch         |
| Global initializers | `lower/globals.rs`  | Extracts non-const initializers into `__initialize_module` |
| Boxing              | `lower/boxing.rs`   | `&primitive` / `&mut primitive` → `Box<T>` struct          |
| Closure             | `lower/closure.rs`  | Closures → functor structs with `__call`                   |
| String collection   | `lower/string.rs`   | Collects literals for the data section                     |
| Value copy          | `lower/value_copy/` | Inserts and synthesizes `$value_copy$T` helpers            |

## Optimize

`optimize/` runs a fixed-point loop of TIR-level passes (inlining, copy propagation, SROA, LICM, DCE, …). See [optimizer.md](./optimizer.md).

## WIR Build

`wir_build/build_wir_package` translates a `FlatPackage` into a `WirPackage` in three stages: register types, collect function signatures, then translate each function body via `FunctionTranslator`. The translator is split across sibling files by concern:

| File                | Concern                                                                           |
| ------------------- | --------------------------------------------------------------------------------- |
| `context.rs`        | `WirContext` — accumulates types, functions, tables                               |
| `component_plan.rs` | `ComponentPlan`: CM-level structure (imports, exports, adapters)                  |
| `translate.rs`      | Driver and dispatch (`translate_expr` / `translate_stmt` / `translate_block`)     |
| `primitive_ops.rs`  | Literals, binary / unary operators, casts, array indexing                         |
| `calls.rs`          | Function-ref resolution, builtin intrinsics, indirect calls, closure-to-canonical |
| `canonical_abi.rs`  | CM canonical ABI: future / stream creation, read / write lowering, result lifting |
| `pattern_match.rs`  | `match` / `if let` / `switch` lowering, variant construct / test / payload        |

Each helper module calls back into `translate.rs` for sub-expression translation; cross-module access uses `pub(super)` on shared fields.

## WIR Optimize

`wir_optimize/` runs Wasm-shape-specific passes that need WIR's lower-level view: peephole, multi-value SROA, init-guard removal, struct elision, array data promotion, parameter / return SROA, nullable-ref folding, constant forwarding, DCE, and final cleanup.

## Codegen

`codegen::emit_wasm` produces the final component bytes:

1. `emit.rs` emits core Wasm bytes from WIR.
2. `component.rs` wraps the core module in a Component Model envelope (imports, exports, adapters, optional WIT bundling, embedded data).
3. `postprocess.rs` adds branch-hint sections and other post-emission custom sections.

Output is validated with `wasmparser` unless `--no-validate` is set.

## Module Loading and Names

### Module Sources

`name.rs::ModuleSource` distinguishes where a module originated:

| Variant      | Origin                                                         |
| ------------ | -------------------------------------------------------------- |
| `Core`       | Embedded core stdlib (`core:prelude`, `core:cli`, …)           |
| `Wasi`       | Embedded WASI bindings (`wasi:cli`, `wasi:io`, …)              |
| `Local`      | Path relative to project root (`./geometry.wado`)              |
| `Remote`     | `http(s)://…` URL, fetched via `host.load_remote()`            |
| `EntryPoint` | The main file being compiled                                   |
| `Redirected` | Module routed through a Kiln invocation index                  |
| `Wasm`       | A `.wat` / `.wasm` asset imported via `use … with { type: … }` |

The loader canonicalizes paths (RFC 3986, project-root-relative with `/` separator) so the same file imported via different paths shares one identity.

### Naming Convention

`name.rs` centralizes mangling so other components do not depend on name shapes:

| Name             | Format                            | Example                             |
| ---------------- | --------------------------------- | ----------------------------------- |
| Method           | `{file}/{Type}::{method}`         | `./geom.wado/Point::sum`            |
| Trait method     | `{file}/{Type}^{Trait}::{method}` | `./geom.wado/Point^Display::fmt`    |
| Effect operation | `{Effect}::{op}`                  | `Stdout::write_via_stream`          |
| WASI canonical   | `wasi:{pkg}/{iface}::{fn}`        | `wasi:cli/stdout::write-via-stream` |
| Mangled generic  | `{Base}$T1$T2…`                   | `Box$i32`, `Pair$i32$String`        |

## Component Model Registries

Three registries collect declarative information from the standard library and feed both the resolver and codegen:

- `WasiRegistry` (`component_model.rs`) — extracts WASI interfaces from `lib/wasi/*.wado`: version pins, async flags, canonical method names, supported types. Codegen drives import generation from this registry; only interfaces whose types are fully supported are imported.
- `WorldRegistry` (`world_registry.rs`) — collects world definitions (e.g., the `Command` world from `wasi/cli.wado`) and provides export signatures.
- `BuiltinRegistry` (`builtin_registry.rs`) — collects function signatures from `lib/core/builtin.wado`. Functions tagged `#[canonical("ns", "name")]` import a CM canonical builtin (`wasi`, `mem`, or `bundled`); untagged builtins compile directly to Wasm instructions.

## Standard Library

The compiler bundles the standard library inside its binary (`stdlib.rs` embeds `lib/`):

- `lib/core/` — `prelude`, `cli`, `collections`, `serde`, `json`, `simd`, `zlib`, `base64`, `url`, `router`, `kiln`, `internal`, `builtin`, `allocator`, plus co-located `_test` modules.
- `lib/wasi/` — Wado bindings for WASI P3 interfaces. Generated from WIT by `wado-from-idl`; regenerate with `mise run update-stdlib-wasi`.

## Bundled Math (`wado-bundled-libm`)

The `wado-bundled-libm/` crate compiles a deterministic libm to `wasm32-unknown-unknown`. The compiler links it as a separate core module inside the produced component, and `core:builtin` exposes its functions via `#[canonical("bundled", …)]`.

## Allocators

Three allocators live in `lib/core/allocator.wado`, each tagged `#[allocator("name")]`. The compiler picks one by setting that function's `export_name` to `"realloc"`:

| Mode       | Default for            | Behaviour                                                               |
| ---------- | ---------------------- | ----------------------------------------------------------------------- |
| `bump`     | CLI                    | Bump pointer with free-rewind; never reclaims general blocks.           |
| `freelist` | HTTP service worlds    | First-fit free list with block splitting; falls back to bump.           |
| `debug`    | Test world / E2E tests | Never reuses freed memory; poisons with `0xFF`. Catches use-after-free. |

`--allocator <name>` overrides the defaults.

## In Progress

- [ ] Variant pattern matching: struct payloads not yet supported (single-payload and tuple-payload work).
- [ ] Function types: parser supports `fn(T) -> U` and closure codegen works, but full first-class function types are incomplete.
- [ ] Stream/Future: resource declarations exist in `core:prelude/types.wado`, but method resolution (`.new()`, `.read()`, `.write()`, `.close()`, `.drop()`) is still hardcoded in `resolver/method_call.rs` rather than driven by the resource declarations.

## Known Limitations

- Implicit struct literals do not work with generic structs: `let b: Box<i32> = { value };` fails. Use `let b: Box<i32> = Box { value };`.
- GC arrays cannot be passed directly to `stream<u8>` — they must be copied to linear memory first ([component-model#525](https://github.com/WebAssembly/component-model/issues/525)).

## Not Yet Implemented

- `?` operator (error propagation)
- Effect handlers
- Reactive signals (source values, derived values, effect blocks)
