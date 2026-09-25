# Wado Compiler

The Wado compiler (`wado-compiler/`) translates `.wado` source into a Wasm
component. This document is the map: the phases, the IRs between them, and the
rules that hold across them. The WEPs linked below say why each phase has its
shape. Each module's doc says how it works.

- Optimization passes: [optimizer.md](./optimizer.md)
- `wado format` rules: [formatter.md](./formatter.md)
- Language features: [spec.md](./spec.md)

## Pipeline

| Phase                | Output           | Where                                           |
| -------------------- | ---------------- | ----------------------------------------------- |
| Load                 | AST per module   | `loader.rs`, `lexer.rs`, `parser.rs`, `bind.rs` |
| Analyze              | Symbol table     | `analyze.rs`                                    |
| Resolve              | Declarations     | `defs.rs`, `resolve.rs`                         |
| Annotate             | `Semantics`      | `elaborator/`                                   |
| Liveness             | Reachability     | `elaborator/`                                   |
| Reify                | TIR per module   | `elaborator/`                                   |
| Check                | Diagnostics      | `effect_check.rs`, `resource_move_check.rs`     |
| Synthesis            | TIR              | `synthesis/`                                    |
| Link                 | One flat package | `link.rs`                                       |
| Monomorphize / Erase | Concrete TIR     | `monomorphize/`                                 |
| Lower                | NIR              | `lower/`                                        |
| Optimize             | NIR              | `optimize/`                                     |
| WIR Build            | WIR              | `wir_build/`                                    |
| WIR Optimize         | WIR              | `wir_optimize/`                                 |
| Codegen              | Component bytes  | `codegen/`                                      |

<<<<<<< HEAD
| Phase                  | Output          | Module(s)                                        |
| ---------------------- | --------------- | ------------------------------------------------ |
| Lex / Parse            | AST             | `lexer.rs`, `parser.rs`, `token.rs`, `syntax.rs` |
| Bind                   | AST + bindings  | `bind.rs`                                        |
| Loader                 | All modules     | `loader.rs`                                      |
| Analyze                | `SymbolTable`   | `analyze.rs`                                     |
| Resolve                | Declarations    | `defs.rs`, `resolve.rs`                          |
| Annotate               | `Semantics`     | `semantics.rs`, `elaborator/`                    |
| Liveness               | Reachability    | `elaborator/liveness.rs`                         |
| Reify                  | `TirModule`     | `elaborator/reify.rs`                            |
| Effect Check           | (validation)    | `effect_check.rs`                                |
| Synthesis              | TIR (extended)  | `synthesis/`                                     |
| Effect Dispatch (post) | TIR             | `synthesis/effect_dispatch.rs`                   |
| Link                   | `FlatPackage`   | `link.rs`                                        |
| Monomorphize           | `FlatPackage`   | `monomorphize/`                                  |
| Erase Newtypes & Flags | `FlatPackage`   | `tir.rs`                                         |
| Reflect Bridges (post) | `FlatPackage`   | `synthesis/reflect_bridge.rs`                    |
| Pre-lower Prune        | `FlatPackage`   | `prelower_reach.rs`                              |
| Lower                  | `NirPackage`    | `lower/`                                         |
| Optimize               | `NirPackage`    | `optimize/`                                      |
| WIR Build              | `WirPackage`    | `wir_build/`                                     |
| WIR Optimize           | `WirPackage`    | `wir_optimize/`                                  |
| Codegen                | Component bytes | `codegen/`                                       |
||||||| 03599b796
| Phase                  | Output          | Module(s)                                        |
| ---------------------- | --------------- | ------------------------------------------------ |
| Lex / Parse            | AST             | `lexer.rs`, `parser.rs`, `token.rs`, `syntax.rs` |
| Bind                   | AST + bindings  | `bind.rs`                                        |
| Loader                 | All modules     | `loader.rs`                                      |
| Analyze                | `SymbolTable`   | `analyze.rs`                                     |
| Resolve                | Declarations    | `defs.rs`, `resolve.rs`                          |
| Annotate               | `Semantics`     | `semantics.rs`, `elaborator/`                    |
| Liveness               | Reachability    | `elaborator/liveness.rs`                         |
| Reify                  | `TirModule`     | `elaborator/reify.rs`                            |
| Default-purity Check   | (validation)    | `effect_check.rs::check_default_purity`          |
| Synthesis              | TIR (extended)  | `synthesis/`                                     |
| Effect Check           | TIR (validated) | `effect_check.rs`                                |
| Effect Dispatch (post) | TIR             | `synthesis/effect_dispatch.rs`                   |
| Link                   | `FlatPackage`   | `link.rs`                                        |
| Monomorphize           | `FlatPackage`   | `monomorphize/`                                  |
| Erase Newtypes & Flags | `FlatPackage`   | `tir.rs`                                         |
| Reflect Bridges (post) | `FlatPackage`   | `synthesis/reflect_bridge.rs`                    |
| Pre-lower Prune        | `FlatPackage`   | `prelower_reach.rs`                              |
| Lower                  | `NirPackage`    | `lower/`                                         |
| Optimize               | `NirPackage`    | `optimize/`                                      |
| WIR Build              | `WirPackage`    | `wir_build/`                                     |
| WIR Optimize           | `WirPackage`    | `wir_optimize/`                                  |
| Codegen                | Component bytes | `codegen/`                                       |
=======
The driver is `compile_with_options` in `src/lib.rs`. It loads the modules,
compiles any inline providers, and hands the rest to `compile_after_load`. The
LSP runs the same phases up to liveness and stops there.
>>>>>>> origin/main

## IRs

| IR  | What it is                                                                            |
| --- | ------------------------------------------------------------------------------------- |
| AST | Surface syntax, kept as written so `wado format` round-trips.                         |
| TIR | Typed IR. One per module after reify; one flat package of every item after link.      |
| NIR | Normalized IR, what the optimizer rewrites. See [WEP: NIR](./wep-2026-05-11-nir.md).  |
| WIR | Close to Wasm core instructions. See [WEP: WIR Layer](./wep-2026-02-14-wir-layer.md). |

Codegen reads NIR and WIR only, and knows nothing of the phases before them.

## Frontend

The parser builds a faithful AST: compound assignments, comparison chains,
struct shorthand and comments survive as written. Bind resolves local names and
checks scopes, mutability and use-before-define.

The loader reads the entry module and everything it imports: the embedded
standard library, WASI and Web bindings, local files, package dependencies,
remote URLs, Wasm assets, and Kiln output. Each file has one identity however
it was imported. See [WEP: Module Loader](./wep-2026-01-24-module-loader.md).

## Elaboration

The elaborator resolves, infers, and dispatches, then emits TIR
([WEP: Elaborator](./wep-2026-05-26-elaborator-rearchitecture.md)):

- Resolve answers every reference site once, from the module that wrote it. A
  declaration is identified by a whole-program ID, never by its name
  ([WEP: Declaration Identity](./wep-2026-08-12-declaration-identity.md)).
- Annotate covers trait selection, generic inference, method dispatch, coercion,
  and effect typing. Its facts are attached to AST nodes without changing them.
- Liveness decides what reify emits and feeds the unused diagnostics.
- Reify reads the facts back and emits TIR. It infers and decides nothing.

The AST stays as the parser built it. Desugaring (loops, compound assignment,
`assert`, `matches`, templates, namespace prefixes) produces no synthetic AST:
annotate records what the rewrite needs on the source node, and reify emits the
rewritten TIR. That is why hover, go-to-definition, and rename land on the
user's text, from the same facts the batch compiler uses.

Every trait call is resolved statically. By the end of the pipeline each call
targets one concrete function, so there is no vtable
([WEP: Trait Resolution](./wep-2026-09-01-trait-resolution.md),
[WEP: Overload Resolution](./wep-2026-07-31-overload-resolution.md)).

A type parameter is rigid inside the item that declares it: nothing but itself
is assignable to it. An inference variable is flexible: it takes the type the
solver finds. Each use of a generic signature replaces its parameters with fresh
inference variables. No inference variable survives elaboration, and no type
parameter survives monomorphization.

## Checks

Before synthesis, the compiler checks that every function declares the effects
it performs, that default arguments and global initializers are pure, and that
no resource is used after it moved
([WEP: Effect System](./wep-2026-01-27-effect-system-design.md),
[WEP: Ownership Analysis](./wep-2026-05-21-resource-ownership.md)). What a
function retains is not checked: lower infers it
([WEP: Value Semantics](./wep-2026-01-12-value-semantics-and-retention.md)).

## Synthesis

Synthesis generates the TIR the user does not write:

- Derived trait impls. `Eq`, `Ord`, `Default`, and serde are derived only
  where a bound or a use asks for them. `Inspect` holds for every type
  ([WEP: Trait Derivation](./wep-2026-06-25-trait-derivation.md)).
- `From` impls declared by `impl From<T> for U;`.
- Reflection metadata, from which the library derives the rest
  ([WEP: Reflect Derivation](./wep-2026-06-13-reflect-derivation.md)). A
  generic type's bridges are made after monomorphization, one per instance.
- Template strings, expanded into `Display` / `Inspect` calls.
- Effect handlers, desugared into per-effect dispatch
  ([WEP: Effect Handler](./wep-2026-04-11-effect-handler.md)).
- Resource drops. An owned Component Model resource that is still owned at the
  end of its scope gets a `resource.drop`
  ([WEP: Resource Lifecycle](./wep-2026-01-12-resource-lifecycle.md)).
- Component Model boundary adapters: lift, lower, async export, and every
  canonical operation, as ordinary TIR
  ([WEP: CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md)).

## Link and Monomorphize

<<<<<<< HEAD
`synthesis/reflect_bridge.rs` is the one exception to synthesis running before
monomorphize: a generic type's value bridges are named after the concrete
subject and member types, so they exist only per instantiation
([WEP 2026-06-13](./wep-2026-06-13-reflect-derivation.md)).

Bound-driven requests ([WEP 2026-06-25-trait-derivation](./wep-2026-06-25-trait-derivation.md)) originate during Annotate, from two funnels into the shared `TypeTable::bound_driven_synth_requests` set (one per `TypeTable`, project-wide, since each module gets a fresh `Elaborator`):

- A satisfied bound: `Elaborator::type_implements_trait_inner` (`elaborator/trait_query.rs`) records a `(type, trait)` pair whenever a `T: Serialize`/`Deserialize`/`Eq`/`Ord`/`Default` bound is satisfied. `Eq`/`Ord`/serde recurse through members via one walker, `walk_structural_derive_members`, shared with marker validation and reason-chain diagnostics; `Default` instead checks that every field carries a default expression (`auto_derive_default_struct_type`). A direct `S::default()` call records at static-method resolution (`method_call.rs`), since it reaches no bound check.
- A body-less marker: `impl Eq/Ord/Default/Serialize/Deserialize for T;` all validate eligibility immediately at the marker's own span (`Elaborator::record_explicit_derive_request` — a compile error if `T` isn't eligible) and then record into the same set, keyed by the target type's defining module.

`Inspect` is _total_ (WEP): `on_bound_of` classifies it, `type_implements_trait_inner` short-circuits `true` for any type, and generation stays unconditional — so a `T: Inspect` bound always holds and `impl Inspect for T;` markers are accepted, without gating. It records no demand request. `#` selects no trait: it sets `Formatter.alternate`, which each impl reads.

Two passes read the demand set as a snapshot, not a drain (consuming it would starve whichever runs second): `serde_synth::synthesize_serde` claims `Serialize`/`Deserialize`; `traits::synthesize_traits` claims `Eq`/`Ord`/`Default`, gating generation on set membership (`SynthesisCtx::should_synthesize`) instead of its former unconditional sweep.

A body-less marker is itself an `Item::Impl` and lands in `TraitEnv`'s impl indexes like any other, so every gate that means "a real impl already covers this" must count only methodful impls: `SynthesisCtx::has_real_impl` (module-scoped, for `Eq`/`Ord`/`Default` generation dedup), `has_methodful_impl_anywhere` (module-agnostic, so a body-less format marker never suppresses the eager format body while a real `impl Display for String` does), and `Elaborator::has_real_trait_impl_for_type` (module-agnostic — a serde impl legitimately lives outside the type's module, e.g. `core:serde`'s `impl Serialize for i128`).

## Effect Check

`check_semantics` (`effect_check.rs`) runs over `Semantics`, after annotate and before synthesis, so the batch driver and the LSP share it. It checks that every function declares the effects it requires, and that no default expression or global initializer performs an effect its own handlers do not answer. Synthesized code is never checked, since it is not in `Semantics`. What a function retains is not checked here at all: the body states it, and `lower::plan` infers it ([WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md)).

## Effect Dispatch (post-check)

`synthesis::effect_dispatch::synthesize_post_check` runs after synthesis and before link. It desugars `WithHandler` / `Resume` constructs into the per-effect dispatch infrastructure (struct, mut global, dispatch wrappers).

## Link → Monomorphize → Erase

- `link.rs` merges per-module TIR into a single `FlatPackage`. After link, functions and types are addressed by global indices.
- `monomorphize/` walks call sites, instantiates generic structs and functions with concrete type arguments, and rewrites references. Generic structs are keyed by `(name, ModuleSource)`; generic functions are keyed by `(ModuleSource, String)`, where the string is the bare method name for methods and a module-qualified `FreeFunctionName` for free functions. The `ModuleSource` is always the body's home module (where the template is registered in `Package::functions`), so same-named generics from different modules coexist. The dispatch loop panics if a queued generic has no template at its `(module_source, name)` key — every generic must be defined somewhere reachable (e.g. in `core:prelude` for built-in shapes like the tuple, whose reserved base name is `[]`). Variadic `TupleSpread` nodes are expanded here. `name.rs` produces stable mangled names (`Box$i32`, `identity$1`, …).
- `type_table.erase_newtypes_and_flags()` then collapses newtypes to their base type and flag types to `u32`. The distinction is needed during monomorphize for trait dispatch but not afterwards.
||||||| 03599b796
`synthesis/reflect_bridge.rs` is the one exception to synthesis running before
monomorphize: a generic type's value bridges are named after the concrete
subject and member types, so they exist only per instantiation
([WEP 2026-06-13](./wep-2026-06-13-reflect-derivation.md)).

Bound-driven requests ([WEP 2026-06-25-trait-derivation](./wep-2026-06-25-trait-derivation.md)) originate during Annotate, from two funnels into the shared `TypeTable::bound_driven_synth_requests` set (one per `TypeTable`, project-wide, since each module gets a fresh `Elaborator`):

- A satisfied bound: `Elaborator::type_implements_trait_inner` (`elaborator/trait_query.rs`) records a `(type, trait)` pair whenever a `T: Serialize`/`Deserialize`/`Eq`/`Ord`/`Default` bound is satisfied. `Eq`/`Ord`/serde recurse through members via one walker, `walk_structural_derive_members`, shared with marker validation and reason-chain diagnostics; `Default` instead checks that every field carries a default expression (`auto_derive_default_struct_type`). A direct `S::default()` call records at static-method resolution (`method_call.rs`), since it reaches no bound check.
- A body-less marker: `impl Eq/Ord/Default/Serialize/Deserialize for T;` all validate eligibility immediately at the marker's own span (`Elaborator::record_explicit_derive_request` — a compile error if `T` isn't eligible) and then record into the same set, keyed by the target type's defining module.

`Inspect` is _total_ (WEP): `classify_on_bound_trait` classifies it, `type_implements_trait_inner` short-circuits `true` for any type, and generation stays unconditional — so a `T: Inspect` bound always holds and `impl Inspect for T;` markers are accepted, without gating. It records no demand request. `#` selects no trait: it sets `Formatter.alternate`, which each impl reads.

Two passes read the demand set as a snapshot, not a drain (consuming it would starve whichever runs second): `serde_synth::synthesize_serde` claims `Serialize`/`Deserialize`; `traits::synthesize_traits` claims `Eq`/`Ord`/`Default`, gating generation on set membership (`SynthesisCtx::should_synthesize`) instead of its former unconditional sweep.

A body-less marker is itself an `Item::Impl` and lands in `TraitEnv`'s impl indexes like any other, so every gate that means "a real impl already covers this" must count only methodful impls: `SynthesisCtx::has_real_impl` (module-scoped, for `Eq`/`Ord`/`Default` generation dedup), `has_methodful_impl_anywhere` (module-agnostic, so a body-less format marker never suppresses the eager format body while a real `impl Display for String` does), and `Elaborator::has_real_trait_impl_for_type` (module-agnostic — a serde impl legitimately lives outside the type's module, e.g. `core:serde`'s `impl Serialize for i128`).

## Effect Check

`check_effects` (`effect_check.rs`) runs after synthesis and before monomorphize. It validates that every function declares the effects it actually requires. Synthesized CM boundary code is exempted. A separate `check_default_purity` runs earlier, immediately before synthesis, to gate auto-derived `Default::default()` bodies on pure field defaults. What a function retains is not checked here at all: the body states it, and `lower::plan` infers it ([WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md)).

## Effect Dispatch (post-check)

`synthesis::effect_dispatch::synthesize_post_check` runs between the effect check and link. It desugars `WithHandler` / `Resume` constructs into the per-effect dispatch infrastructure (struct, mut global, dispatch wrappers). This pass is split out of the main synthesis stage so that effect-check sees the original `WithHandler` shape and can validate which effects are satisfied locally.

## Link → Monomorphize → Erase

- `link.rs` merges per-module TIR into a single `FlatPackage`. After link, functions and types are addressed by global indices.
- `monomorphize/` walks call sites, instantiates generic structs and functions with concrete type arguments, and rewrites references. Generic structs are keyed by `(name, ModuleSource)`; generic functions are keyed by `(ModuleSource, String)`, where the string is the bare method name for methods and a module-qualified `FreeFunctionName` for free functions. The `ModuleSource` is always the body's home module (where the template is registered in `Package::functions`), so same-named generics from different modules coexist. The dispatch loop panics if a queued generic has no template at its `(module_source, name)` key — every generic must be defined somewhere reachable (e.g. in `core:prelude` for built-in shapes like the tuple, whose reserved base name is `[]`). Variadic `TupleSpread` nodes are expanded here. `name.rs` produces stable mangled names (`Box$i32`, `identity$1`, …).
- `type_table.erase_newtypes_and_flags()` then collapses newtypes to their base type and flag types to `u32`. The distinction is needed during monomorphize for trait dispatch but not afterwards.
=======
Link merges the modules into one package. Compile-time parameters (`#[param]`)
then take their `-D` values. Monomorphize instantiates each generic item per
concrete type argument and expands variadic packs. Then newtypes collapse to
their base type and flags to `u32`. Dispatch needed the distinction, and nothing
after it does. Functions nothing reaches are dropped before lower.
>>>>>>> origin/main

## Lower

Lower turns TIR into NIR in two halves. First the planner decides how each
construct is represented:

- A closure becomes a functor struct
  ([WEP: Closure](./wep-2026-01-16-closure-implementation.md)).
- A reference to a primitive becomes a box.
- Each value consumption becomes a move, a copy, or a share.
- A non-constant global initializer moves into module initialization.

Then the translator folds TIR into NIR in one pass.

## Backend

WIR build translates NIR to WIR and plans the component's imports, exports,
and adapters. WIR optimize runs the Wasm-shaped passes in
[optimizer.md](./optimizer.md). Codegen emits the core module with its branch
hints, wraps it in a component, embeds imported Wasm assets pruned to what the
component uses, and validates the result.

## Component Model

The compiler reads WASI and Web interfaces, worlds, and builtins from their
declarations in the standard library rather than hardcoding them. A function
is imported only if the program calls it and every type in its signature is
supported. A canonical operation such
as `future.read` is typed per payload, so each payload type gets its own core
import. See [WEP: WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md).

## Kiln

Kiln turns an input file (a schema, a grammar, a Wado dialect) into `.wado`
source. A generator is an ordinary Wado package targeting the
`core:kiln/generator` world. `wado-cli` builds and runs it, and caches the
output by its inputs, options, and the generator's source. The compiler holds
only the pure-data half: the invocations, their order, cache keys, and option
checks. It redirects an import to the generated source. See
[WEP: Kiln](./wep-2026-04-12-kiln.md).

## LSP

`wado-lsp/` is a thin layer over the compiler's `Semantics`: each query parses,
loads, and elaborates, then reads the facts at the cursor. The engine performs
no I/O. The caller supplies the host that loads modules, so the engine runs in
VS Code and in the browser alike. That is why `wado-compiler` must build for
`wasm32-unknown-unknown`. See
[WEP: LSP Architecture](./wep-2026-04-18-lsp-architecture.md).

## Standard Library

The standard library is Wado source under `lib/`, embedded in the compiler.
`lib/core/` is written by hand, except `lib/core/kiln/`. `wado-from-idl`
generates that directory, `lib/wasi/`, and `lib/web/`.
Deterministic math is a bundled core Wasm module, linked into the component and
pruned to the functions called
([WEP: Deterministic libm](./wep-2026-01-10-deterministic-libm.md)).

The world selects the allocator, and `--allocator` overrides it.

| Allocator  | Default for             | Behaviour                                            |
| ---------- | ----------------------- | ---------------------------------------------------- |
| `bump`     | CLI and other worlds    | Never reclaims.                                      |
| `freelist` | HTTP service, libraries | Reuses freed blocks.                                 |
| `debug`    | Test world              | Never reuses freed memory, and poisons it with 0xFF. |

## Known Limitations

- A `||` / `&&` chain of thousands of operands overflows the compiler's stack.
- A GC array cannot be passed to `stream<u8>` directly. It is copied to linear
  memory first ([component-model#525](https://github.com/WebAssembly/component-model/issues/525)).
