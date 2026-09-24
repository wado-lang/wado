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

The driver is `compile_with_options` in `src/lib.rs`. It loads the modules,
compiles any inline providers, and hands the rest to `compile_after_load`. The
LSP runs the same phases up to liveness and stops there.

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

Link merges the modules into one package. Compile-time parameters (`#[param]`)
then take their `-D` values. Monomorphize instantiates each generic item per
concrete type argument and expands variadic packs. Then newtypes collapse to
their base type and flags to `u32`. Dispatch needed the distinction, and nothing
after it does. Functions nothing reaches are dropped before lower.

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

<<<<<<< HEAD
Each helper module calls back into `translate.rs` for sub-expression translation; cross-module access uses `pub(super)` on shared fields.

Type lookup has two contracts. During registration a type may name one a later phase defines, so `type_id_to_wir_type_pending` yields a placeholder that the final fixup pass re-resolves. Everywhere after registration, `type_id_to_wir_type` treats a miss as a bug and panics: registration and lookup derive their keys through the same `name::wir_*_key` helpers, so they cannot drift apart without one of them being wrong.

CM canonical operations (stream / future read + write, waitable-set, error-context) carry no `wir_build` code: they are lowered entirely in the synthesis phase (`synthesis/cm_binding/`) into ordinary TIR that translates like any other function.

## WIR Optimize

`wir_optimize/` runs Wasm-shape-specific passes that need WIR's lower-level view: peephole, init-guard removal, struct elision, array data promotion, parameter SROA, nullable-ref folding, constant forwarding, DCE, and final cleanup. Tuple- and user-struct ABIs are decided before WIR build by the NIR-level `optimize::multi_value_return` and `optimize::multi_value_param` passes, and variant returns are scalarized into tuples at NIR by `optimize::sroa_variant_return`; what stays here is the result-slot flattening that needs the post-`nullable_ref` shape.

## Codegen

`codegen::emit_wasm` produces the final component bytes:

1. `emit.rs` emits core Wasm bytes from WIR, including the branch-hint section.
2. `component.rs` wraps the core module in a Component Model envelope (imports, exports, adapters, optional WIT bundling, embedded data).
3. `wado-wasm-embed` rewrites an embedded wasm asset's memory definition into an import and prunes it to the exports the component uses — code, and, where the asset carries a `wado.dataref` map, its data segments byte by byte.

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

| Name             | Format                                   | Example                                      |
| ---------------- | ---------------------------------------- | -------------------------------------------- |
| Method           | `{impl}/{decl}/{Type}::{method}`         | `./geom.wado/./geom.wado/Point::sum`         |
| Trait method     | `{impl}/{decl}/{Type}^{Trait}::{method}` | `./geom.wado/./geom.wado/Point^Display::fmt` |
| Effect operation | `{Effect}::{op}`                         | `Stdout::write_via_stream`                   |
| WASI canonical   | `wasi:{pkg}/{iface}::{fn}`               | `wasi:cli/stdout::write-via-stream`          |
| Mangled generic  | `{Base}$T1$T2…`                          | `Box$i32`, `Pair$i32$String`                 |

Every fq name names its subject by the module that declares it, and a type
written into any name goes through `TypeTable::mangle_type_arg_for_generic`. A
simple name alone is never an identity — two modules may declare the same one.

Every name the compiler mints for itself starts with one `$`
(`name::INTERNAL_PREFIX`): a local, a label, a global, a synthesized struct or
function. No Wado identifier holds one, so a minted name collides with nothing an
author wrote, and no source can `break` to a synthesized label. Inside a function
body, the digits that make such a name unique come from
`FunctionContext::fresh_serial`. That serial advances on read, so a desugaring
nested inside another mints names of its own: a tagged template in a hole, a
`for-of` over a `for-of`. An optimizer pass mints a local through
`Engine::alloc_minted_local`, which names it by the index it takes in the same
step. A per-type bridge spells the type's mangle after its
kind (`$value_copy$…`, `$hole_get$…`), and `name::is_type_bridge` recognizes it.

What is one is a `crate::defs::DefId`: a dense index into the whole-program
`DefTable`, built after loading from every module's items. Declaration data
(fields, cases, members, visibility, span) is keyed by it, `ResolvedType`'s
nominal variants carry it, and every trait / effect / resource / impl-target
index in the elaborator is keyed by it — as are the tables a module's own walk
adds to as it goes, so a consumer reads a declaration without knowing which
module it stands in, and a function-local `struct Box` and a module-level one
are two entries rather than one the later insert wins. `FqTypeName` and
`FqTraitName` carry one too, in a `DeclaredHead` whose equality and hashing
read the `DefId` alone, and neither has a constructor that takes a spelling. A
head that names no declaration — a closure environment, an anonymous literal's
shape — is `TypeHead::Shape`, whose rendering _is_ its identity.

A handful of functions still turn a name into a declaration. WEP 2026-08-12
lists them with the reason each survives; adding one to that list is a design
change, not a local convenience.

A method key is `(impl module, declared receiver, trait, method)`, so `{impl}`
and `{decl}` repeat whenever a type is implemented in the module declaring it —
the common case. A receiver with no declaring module (a builtin, a tuple) has no
`{decl}` segment. See WEP 2026-07-29 for why neither segment is removable alone.

The same rule binds names still in their written form. A type name in source is
relative to the module that wrote it, so a **reference site** — not a consumer —
is where an identity is derived, once: `crate::resolve::Resolutions` answers
every site before elaboration begins, keyed by the site's own `AstId`. A written
type, a bound, an `impl` header's trait and target, a struct literal's type
name, a qualified path's segments, a pattern's `Type::` qualifier and a bare
identifier in expression position are all such sites. An `impl`
block's digest (`ImplHeader`) carries its module, its target's `ImplTargetKey`
and its trait's, and the whole-program checks (coherence, orphan rules, sealed
traits, trait-method arity) read the digest instead of re-walking
`loaded_modules`, because a second walk knows no module and can only compare
spellings. A consumer holding a bare name with no site goes through the table's
own scope lookup, so it cannot answer differently from the site. See WEP
2026-08-10.

## Component Model Registries

Three registries collect declarative information from the standard library and feed both the elaborator and codegen:

- `CmInterfaceRegistry` (`component_model.rs`) — extracts WASI interfaces from `lib/wasi/*.wado`: version pins, async flags, canonical method names, supported types. Codegen drives import generation from this registry; only interfaces whose types are fully supported are imported.
- `WorldRegistry` (`world_registry.rs`) — collects world definitions (e.g., the `Command` world from `wasi/cli.wado`) and provides export signatures.
- `BuiltinRegistry` (`builtin_registry.rs`) — collects function signatures from `lib/core/builtin.wado`. Functions tagged `#[canonical("ns", "name")]` import a CM canonical builtin (`wasi`, `mem`, or `bundled`); untagged builtins compile directly to Wasm instructions.

### Canonical intrinsics

`canon future.read` and its siblings are typed — the Component Model instantiates one per `future<T>` — so the core module needs a distinct import per payload type. `CanonicalIntrinsic` (`canonical.rs`) is that identity, and `CmRawCall` carries it from synthesis through TIR and NIR into WIR.

`import_name` renders that identity as the core import name at the end of the path. It is a rendering, not a carrier: nothing parses it back, so it only has to be injective. A payload names a declaration or a structure, and a name carries neither back. So `from_import_name` reads only the payload-less operation a `#[canonical]` annotation states, and refuses anything else.

A payload that classifies as nothing is reported, never defaulted: `classify_future_payload` recognizes the trailers shape structurally and panics otherwise, and `classify_stream_payload` panics rather than falling back to `stream<u8>`. Ask `future_payload_rejection` / `stream_payload_rejection` first so the user gets a diagnostic instead.
||||||| 03599b796
Each helper module calls back into `translate.rs` for sub-expression translation; cross-module access uses `pub(super)` on shared fields.

Type lookup has two contracts. During registration a type may name one a later phase defines, so `type_id_to_wir_type_pending` yields a placeholder that the final fixup pass re-resolves. Everywhere after registration, `type_id_to_wir_type` treats a miss as a bug and panics: registration and lookup derive their keys through the same `name::wir_*_key` helpers, so they cannot drift apart without one of them being wrong.

CM canonical operations (stream / future read + write, waitable-set, error-context) carry no `wir_build` code: they are lowered entirely in the synthesis phase (`synthesis/cm_binding/`) into ordinary TIR that translates like any other function.

## WIR Optimize

`wir_optimize/` runs Wasm-shape-specific passes that need WIR's lower-level view: peephole, init-guard removal, struct elision, array data promotion, parameter SROA, nullable-ref folding, constant forwarding, DCE, and final cleanup. Tuple- and user-struct ABIs are decided before WIR build by the NIR-level `optimize::multi_value_return` and `optimize::multi_value_param` passes, and variant returns are scalarized into tuples at NIR by `optimize::sroa_variant_return`; what stays here is the result-slot flattening that needs the post-`nullable_ref` shape.

## Codegen

`codegen::emit_wasm` produces the final component bytes:

1. `emit.rs` emits core Wasm bytes from WIR, including the branch-hint section.
2. `component.rs` wraps the core module in a Component Model envelope (imports, exports, adapters, optional WIT bundling, embedded data).
3. `wado-wasm-embed` rewrites an embedded wasm asset's memory definition into an import and prunes it to the exports the component uses — code, and, where the asset carries a `wado.dataref` map, its data segments byte by byte.

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

| Name             | Format                                   | Example                                      |
| ---------------- | ---------------------------------------- | -------------------------------------------- |
| Method           | `{impl}/{decl}/{Type}::{method}`         | `./geom.wado/./geom.wado/Point::sum`         |
| Trait method     | `{impl}/{decl}/{Type}^{Trait}::{method}` | `./geom.wado/./geom.wado/Point^Display::fmt` |
| Effect operation | `{Effect}::{op}`                         | `Stdout::write_via_stream`                   |
| WASI canonical   | `wasi:{pkg}/{iface}::{fn}`               | `wasi:cli/stdout::write-via-stream`          |
| Mangled generic  | `{Base}$T1$T2…`                          | `Box$i32`, `Pair$i32$String`                 |

Every fq name names its subject by the module that declares it, and a type
written into any name goes through `TypeTable::mangle_type_arg_for_generic`. A
simple name alone is never an identity — two modules may declare the same one.

Every name the compiler mints for itself starts with one `$`
(`name::INTERNAL_PREFIX`): a local, a label, a global, a synthesized struct or
function. No Wado identifier holds one, so a minted name collides with nothing an
author wrote, and no source can `break` to a synthesized label. Inside a function
body, the digits that make such a name unique come from
`FunctionContext::fresh_serial`. That serial advances on read, so a desugaring
nested inside another mints names of its own: a tagged template in a hole, a
`for-of` over a `for-of`. A per-type bridge spells the type's mangle after its
kind (`$value_copy$…`, `$hole_get$…`), and `name::is_type_bridge` recognizes it.

What is one is a `crate::defs::DefId`: a dense index into the whole-program
`DefTable`, built after loading from every module's items. Declaration data
(fields, cases, members, visibility, span) is keyed by it, `ResolvedType`'s
nominal variants carry it, and every trait / effect / resource / impl-target
index in the elaborator is keyed by it — as are the tables a module's own walk
adds to as it goes, so a consumer reads a declaration without knowing which
module it stands in, and a function-local `struct Box` and a module-level one
are two entries rather than one the later insert wins. `FqTypeName` and
`FqTraitName` carry one too, in a `DeclaredHead` whose equality and hashing
read the `DefId` alone, and neither has a constructor that takes a spelling. A
head that names no declaration — a closure environment, an anonymous literal's
shape — is `TypeHead::Shape`, whose rendering _is_ its identity.

A handful of functions still turn a name into a declaration. WEP 2026-08-12
lists them with the reason each survives; adding one to that list is a design
change, not a local convenience.

A method key is `(impl module, declared receiver, trait, method)`, so `{impl}`
and `{decl}` repeat whenever a type is implemented in the module declaring it —
the common case. A receiver with no declaring module (a builtin, a tuple) has no
`{decl}` segment. See WEP 2026-07-29 for why neither segment is removable alone.

The same rule binds names still in their written form. A type name in source is
relative to the module that wrote it, so a **reference site** — not a consumer —
is where an identity is derived, once: `crate::resolve::Resolutions` answers
every site before elaboration begins, keyed by the site's own `AstId`. A written
type, a bound, an `impl` header's trait and target, a struct literal's type
name, a qualified path's segments, a pattern's `Type::` qualifier and a bare
identifier in expression position are all such sites. An `impl`
block's digest (`ImplHeader`) carries its module, its target's `ImplTargetKey`
and its trait's, and the whole-program checks (coherence, orphan rules, sealed
traits, trait-method arity) read the digest instead of re-walking
`loaded_modules`, because a second walk knows no module and can only compare
spellings. A consumer holding a bare name with no site goes through the table's
own scope lookup, so it cannot answer differently from the site. See WEP
2026-08-10.

## Component Model Registries

Three registries collect declarative information from the standard library and feed both the elaborator and codegen:

- `CmInterfaceRegistry` (`component_model.rs`) — extracts WASI interfaces from `lib/wasi/*.wado`: version pins, async flags, canonical method names, supported types. Codegen drives import generation from this registry; only interfaces whose types are fully supported are imported.
- `WorldRegistry` (`world_registry.rs`) — collects world definitions (e.g., the `Command` world from `wasi/cli.wado`) and provides export signatures.
- `BuiltinRegistry` (`builtin_registry.rs`) — collects function signatures from `lib/core/builtin.wado`. Functions tagged `#[canonical("ns", "name")]` import a CM canonical builtin (`wasi`, `mem`, or `bundled`); untagged builtins compile directly to Wasm instructions.

### Canonical intrinsics

`canon future.read` and its siblings are typed — the Component Model instantiates one per `future<T>` — so the core module needs a distinct import per payload type. `CanonicalIntrinsic` (`canonical.rs`) is that identity, and `CmRawCall` carries it from synthesis through TIR and NIR into WIR.

`import_name` renders that identity as the core import name at the end of the path. It is a rendering, not a carrier: nothing parses it back, so it only has to be injective. A payload names a declaration or a structure, and a name carries neither back. So `from_import_name` reads only the payload-less operation a `#[canonical]` annotation states, and refuses anything else.

A payload that classifies as nothing is reported, never defaulted: `classify_future_payload` recognizes the trailers shape structurally and panics otherwise, and `classify_stream_payload` panics rather than falling back to `stream<u8>`. Ask `future_payload_rejection` / `stream_payload_rejection` first so the user gets a diagnostic instead.
=======
Kiln turns an input file (a schema, a grammar, a Wado dialect) into `.wado`
source. A generator is an ordinary Wado package targeting the
`core:kiln/generator` world. `wado-cli` builds and runs it, and caches the
output by its inputs, options, and the generator's source. The compiler holds
only the pure-data half: the invocations, their order, cache keys, and option
checks. It redirects an import to the generated source. See
[WEP: Kiln](./wep-2026-04-12-kiln.md).
>>>>>>> origin/main

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
