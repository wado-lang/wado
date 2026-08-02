# WEP: Elaborator Re-architecture — TypeSystem / Annotate / Reify

> Partially superseded (2026-06): this WEP's identity model — body facts
> keyed by `SymbolKey` (`(ModuleSource, AstId)`) and the per-walk
> "module perspective" that fills the module half — was retired as the
> root fix for the recurring cross-module `AstId`-collision class (issue
> #1342). `AstId` is now globally unique (`(AstIdSpace, local)`), so every
> fact map keys by bare `AstId` and `SymbolKey` no longer exists. References
> to `SymbolKey` / `ann_module_override` below are historical. The current
> identity model lives in `docs/compiler.md` and `wado-compiler/AGENTS.md`.

## Context

The `elaborate` phase is the largest and most entangled part of the
compiler. The `wado-compiler/src/elaborator/` tree carries about 33,000
lines spread across two dozen modules, all of which extend the same
`Elaborator<'a, H>` struct in `elaborator.rs`. That struct holds about
35 fields and acts as the single home for type interning, trait
resolution, method dispatch, name resolution, use→def recording, and
AST → TIR construction. Adding a new fact, cache, or registry has no
principled place to land; the default is "another field on
`Elaborator`."

Two structural problems have compounded as the language grew:

### The elaborator is a God Object

Every concern the phase touches has been bolted onto the same struct,
and every module reaches into it via `impl<'a, H> Elaborator<'a, H>`.
The conceptually independent layers — the type system, the per-module
annotation facts, the AST → TIR walk — have no type-level boundary.
Borrow-checker pressure pushes shared state into `Rc<RefCell<…>>` even
where shared mutability is not conceptually required, which masks the
real ownership question of where a fact lives.

A symptom of this is the per-module construction in
`Elaborator::build_tir_from_state`, which rebuilds the full 35-field
struct for each loaded module. The construction site is the canonical
demonstration of the problem: there is no way to introduce a new field
without copying its initialisation into that site, and no way to
remove a field without touching every module that reads from `self`.

### The `annotate` phase name does not match what `annotate` does

`semantics_with_logger` runs the elaborator in two calls:

```
let state = Elaborator::annotate_modules(...);
let tir   = Elaborator::build_tir_from_state(&state, ...);
```

The intent of the split — documented in `orchestration.rs` and
`semantics.rs` — is that `annotate` produces a snapshot the LSP can
query, and `build_tir` is the batch-only extension that emits TIR.
The implementation does not honour that intent. `annotate_modules`
covers only declaration-level information (struct fields, variant
cases, trait impls, decl-interned types). All body-level work —
type inference, name resolution inside expressions, method dispatch,
coercion choice, the desugar-replacement TIR rewrites — happens
inside `build_tir_from_state`, which also emits TIR as a side effect
of the same walk.

The consequence is that the LSP path must run `build_tir_from_state`
even though it discards the resulting `TirModule`s. The comment
`semantics.rs:644` ("Run the full body-level resolve pass so
`state.references` and `state.local_symbols` are populated by the real
elaborator") states this directly. Every editor `didChange` pays for a
full TIR emission whose output is thrown away.

Further, the `Rc<RefCell<…>>` plumbing on `AnnotateState.references`
and `AnnotateState.local_symbols` exists precisely because body walks
mutate them after `annotate_modules` returns. The "phase boundary" is
in the function signatures, not in the data flow.

### LSP needs unused diagnostics; reify needs DCE

A separate but related requirement is that `elaborate` must produce
reachability information. The LSP rendering of unused locals,
imports, and items is currently implemented against an optimize-time
DCE pass (`optimize/dce.rs`); a planned rework
(see `wep-2026-05-16-unused-diagnostics.md`) needs that information
available from `Semantics`, before optimization runs. The same
information lets `reify` skip TIR emission for items that are not
reachable from world exports, reducing the size of the input to
`monomorphize` / `lower` / `optimize`.

There is no clean place for this analysis in the current elaborator,
because the elaborator does not expose a "annotation complete"
checkpoint.

## Decision

Re-architect `elaborate` around three layered types and an explicit
phase order. The God Object disappears; the misleading
`annotate` / `build_tir` split is replaced by a phase order that
matches the work.

### Three load-bearing types

`TypeSystem` — the pipeline-wide type knowledge. Owned by the
batch driver and by `Semantics`; passed by `&mut` reference into
every per-module phase. Holds the `TypeTable` arena, the
`TraitEnv`, the builtin / WASI / world registries, the included-files
map, and the type-system caches (`method_info_cache`,
`indexing_trait_cache`, `trait_check_stack`). Exposes operations that
take only type IDs and names: coercion, inference, type checking,
trait impl lookup, method lookup. It does not know about
`Module`, `AstId`, or `ModuleSource`-keyed per-module state.

The name is honest: this object _is_ the type system, not a "type
context." The naming criterion — "would a new field belong in the
type system itself?" — gates membership and prevents drift back into
God-Object behaviour.

`ModuleSemantics` — per-module semantic facts produced by the
elaborator. One instance per loaded module. Owned by `Semantics` in
an `IndexMap<ModuleSource, ModuleSemantics>`. Built incrementally by
`annotate` and extended by the body walk. Decomposed into four
sub-structs with explicit membership rules:

- `bindings` — `use → def` edges (`references`) and locally
  defined symbols (`local_symbols`). What the LSP reads to answer
  go-to-definition, find-references, and hover-on-local queries.
- `imports` — per-module name resolution context derived from
  `use` declarations: `imported_type_sources`,
  `import_original_names`, `namespace_imports`, `effect_sources`.
- `types` — per-`AstId` type annotations and dispatch decisions
  recorded during the body walk: the `TypeId` of every typed
  expression, the resolved target of each method call, the chosen
  coercion at each conversion site, the desugar kind for each
  TIR-direct rewrite (`assert`, `matches`, comparison chain,
  for-of, while, compound assignment).
- `decls` — module-internal declarations confirmed by
  elaboration: `function_return_types`, `imported_functions`,
  `current_module_globals`, `imported_globals`,
  `associated_constants`, `generic_function_*`,
  `generic_method_*`, `generic_struct_names`,
  `pending_anonymous_structs`.

Each sub-struct admits a new field only when "does this fit the
sub-struct's responsibility?" has a clear yes/no answer. A field
that cannot be placed is a design question, not a default-into-the-
catch-all.

`Reify` — the AST + `ModuleSemantics` → `TirModule` walker.
Mechanical: it reads annotations placed by `annotate` and emits the
corresponding TIR nodes. It does not perform type inference, name
resolution, or method dispatch; those decisions were already made
and recorded in `ModuleSemantics.types`. New types interned during
reify (monomorphic instances created on demand) write through
`TypeSystem`.

### Phase order

```
parse → bind → load → analyze
   ↓
annotate_decls(modules, &mut TypeSystem)
   ↓
annotate_bodies(module, &mut TypeSystem, &mut ModuleSemantics)  ×N
   ↓
liveness(Semantics)
   ↓
reify(module, &TypeSystem, &ModuleSemantics) → TirModule       ×N
   ↓
monomorphize → lower → optimize → codegen
```

The LSP path stops after `liveness` and consumes `Semantics`. The
batch path runs through `reify` and continues into the existing
downstream pipeline.

`annotate_decls` handles the cross-module declaration work that is
already in today's `annotate_modules`. `annotate_bodies` is new in
name only: it is the body walk that today lives inside
`build_tir_from_state`, separated from TIR emission. Its sole output
is a populated `ModuleSemantics`.

`liveness` is a new pass that computes reachability from world-export
roots over the `bindings` edges in every `ModuleSemantics`. The
result is stored on `Semantics` as a `Liveness` value (live item set,
unused-local list, unused-import list). The shape of the analysis and
the policy questions around stdlib exclusion, attribute suppression,
and synthesized items are owned by
[`wep-2026-05-16-unused-diagnostics.md`](./wep-2026-05-16-unused-diagnostics.md);
this WEP commits only to "there is a `liveness` pass between
`annotate_bodies` and `reify`, its result lives on `Semantics`."

`reify` reads `liveness.live_items` and skips items that are
unreachable. The TIR delivered to `monomorphize` is therefore the
reachable closure of the program, not the full source.

### What changes about elaborate's name

`elaborate` survives as the umbrella term and physical directory
name. `wado-compiler/src/elaborator/` continues to host the three
new layers as submodules (`tysys/`, `sem/`, `reify/`). The phase
names exposed in pipeline diagrams and entry points become
`annotate` and `reify`; `elaborate` is the name of the directory and
the conversational name for the group of phases. This matches the
established usage of "elaboration" in PL theory (Coq, Lean, Idris)
for the same kind of work and keeps Wado contributors from needing
a new term.

## Implementation

### Module layout

The top-level split inside `wado-compiler/src/elaborator/` follows
the concerns named in the Decision. One file per concern at this
level; internal splits emerge during migration as each file grows.

```
wado-compiler/src/elaborator.rs    # umbrella
wado-compiler/src/elaborator/
├── tysys.rs       # TypeSystem and its operations
├── sem.rs         # ModuleSemantics (re-exports its sub-structs)
├── sem/
│   ├── bindings.rs
│   ├── imports.rs
│   ├── types.rs
│   └── decls.rs
├── annotate.rs    # annotate_decls + annotate_bodies entry
├── liveness.rs    # cross-module reachability
└── reify.rs       # AST + ModuleSemantics → TirModule
```

The four sub-structs of `ModuleSemantics` each get their own file
because the membership rule (Decision §`ModuleSemantics`) is
file-scoped. Everything else stays as a single file until the
concern visibly demands subdivision; the current 24-module sprawl
under `elaborator/` is the failure mode to avoid.

The crate uses the no-`mod.rs` convention (a `foo.rs` next to a
`foo/` directory), matching the existing `elaborator.rs` +
`elaborator/` layout.

### TypeSystem surface

`TypeSystem` exposes the operations that the rest of the compiler
asks of "the type system":

- Interning — `intern_*`, `make_type_param`, `make_type_pack`,
  registration of associated-type resolutions.
- Coercion — `coerce`, the numeric-literal and tuple-to-sequence
  coercions, struct-to-map coercion. Inputs are `TypeId`s and
  expression contexts; the operation returns a coerced expression or
  rejection.
- Inference — `InferCtx` is constructed from `&mut TypeSystem`
  rather than from an `Elaborator`. Inference results are reported
  back as type substitutions.
- Type checking — `typecheck`, `typecheck_return`.
- Trait queries — `implements`, impl resolution by trait and
  target, blanket-impl resolution, bound checking, associated-type
  binding lookup.
- Method lookup — `lookup_method_info`, indexing-trait impls,
  arithmetic-trait impls, key-value / sequence-literal trait impls,
  trait-method resolution.

Method-lookup and trait-query code that today depends on
`Elaborator.trait_ctx` (the per-impl, per-function scope) splits:
the queries that can be answered from `TypeId`s and decl indices
move to `TypeSystem`; the queries that depend on the current
function's `trait_ctx` move to the `annotate` layer and pass the
scope explicitly.

### ModuleSemantics surface

Each sub-struct has a single-line responsibility and a small,
inspectable API:

- `ModuleBindings::record_reference`,
  `ModuleBindings::record_local_symbol`, plus the
  reference-resolution helpers used by `Semantics::referenced_symbol`
  and `Semantics::references_to_def`.
- `ModuleImports::lookup`, `ModuleImports::canonical_decl_key`, the
  effect-source map, the namespace-alias map.
- `TypeAnnotations::set(ast_id, type_id)`,
  `TypeAnnotations::dispatch_target(ast_id)`,
  `TypeAnnotations::coercion_at(ast_id)`,
  `TypeAnnotations::desugar_kind(ast_id)`. The reify layer is the
  primary consumer; LSP hover may read the type map directly.
- `ModuleDecls::function_return_type(name)`, `generic_*`,
  `imported_global`, `associated_constant`, the
  `pending_anonymous_structs` list.

`ModuleSemantics` is owned by `Semantics`. `annotate_bodies` takes
`&mut ModuleSemantics` for the module it is processing; every other
phase takes `&ModuleSemantics`.

### Reify surface

`reify_module(&Module, &TypeSystem, &ModuleSemantics) → TirModule`.
The walker mirrors the AST shape; each visit method looks up the
corresponding annotation on `ModuleSemantics.types` and emits a
TIR node. No inference, no name resolution, no dispatch decisions.
Monomorphic instances created during reify intern through
`&mut TypeSystem`.

Completeness rule (the contract that makes reify mechanical): every
fact reify needs to emit a node is recorded by `annotate`, keyed by
`AstId`. Reify re-derives only what is _uniquely determined by the AST
alone_ — literal kinds, the syntactic shape of a node (`Index` vs
`Field`) — never anything scope-, inference-, dispatch-, or
mangling-sensitive. Anything that depends on resolution is a recorded
decision, not a re-computation.

Implementation note (the Stage 7 gap — closed): the gap reify once had —
re-running `resolve_type` / `resolve_type_with_self` for type
annotations, `Self`, and impl type args, and re-computing mangled
method / struct names — drifted from `annotate` and was fixed one bug at
a time (`TreeMap<String, V>` self-type indexing, the `&T`-blanket
`&^Inspect` name). Stage 7 closed it structurally: `annotate` records the
resolved types and the impl identity / mangled name (`ann_fn_param_types`,
`ann_fn_return_type`, `ann_decl_type_params`, `ImplFacts::self_type` /
`struct_name`, `ann_method_names`, `GenericInstantiation::mangled_name`),
and reify reads them via `.expect(...)` — a missing fact is a loud panic,
not a silent recompute. Two boundary invariants keep the gap closed by
construction:

- Fail-loud, not fail-safe. Reify does not fall back to recomputation when
  a fact is absent; every decision-bearing read is an `.expect`. The sole
  surviving `resolve_type` call is the global read for a snapshot-rehydrated
  callee module, whose `ModuleSemantics` legitimately carries no
  `current_module_globals` — a documented exception, not a fallback.
- Single source for the projection rule. The "dense, real type params"
  predicate (`!effect && !fn-bound`) that fixes positional monomorph slots
  lives once, as `ast::GenericParam::is_real_type_param`; the annotate walk
  and reify both call it instead of re-spelling the filter (it was inlined
  at ~15 sites, the original drift vector for index-shift bugs).

### DCE / Liveness

The `liveness` pass computes source-level reachability between
`annotate_bodies` and `reify`. Its policy surface — roots, stdlib
exclusion, suppression, severity — is owned by
[`wep-2026-05-16-unused-diagnostics.md`](./wep-2026-05-16-unused-diagnostics.md)
(rewritten to consume this architecture instead of `optimize/dce.rs`).
This WEP owns the pass mechanism: the data it produces, the graph it
walks, and the contract reify holds against it.

#### Result type and ownership

`Liveness` is a non-optional field on `Semantics`, computed by
`semantics_of` immediately after every `ModuleSemantics` is populated
and before reify:

```rust
pub(crate) struct Liveness {
    /// Top-level source items (free functions, impl/trait methods
    /// including defaults, globals) reachable from the export boundary.
    pub live_items: IndexSet<SymbolKey>,
    /// User-authored items absent from `live_items`, in source order.
    /// Consumed by the unused-diagnostics emitter.
    pub dead_items: Vec<SymbolKey>,
}
```

The diagnostic emitter and reify read the same `live_items`. There is
no separate "conservative for reify, precise for diagnostics" set: one
reachability result feeds both consumers. `liveness::compute` runs
regardless of `CompilerOptions::unused_diagnostics`; that flag gates
only whether `dead_items` is emitted as warnings, never the reify gate
(the input-shrinking win is correctness-neutral and always taken).

#### Fail-loud, not fail-safe

`reify` skips any item whose `SymbolKey` is absent from `live_items`.
If the graph is wrong and drops a live item, the missing symbol
surfaces as an ICE in `monomorphize` / `lower` / `codegen` — never as
silently miscompiled output. A dropped-live item is therefore a graph
bug caught by the E2E / WIR golden suite and fixed at the source (a
missing edge kind), per the project's "a compiler bug is always P0"
rule. The pass is built for precision; the loud failure mode is the
safety property that lets it be.

The dual failure — an item that is unused but not reported — is what
this design optimises against. The graph resolves every edge precisely
rather than over-approximating reachability when a target is hard to
resolve; an unresolved edge is a graph bug, not a licence to mark
candidates live.

#### Node set and enclosing-item index

Nodes are the top-level code items of every loaded module: free
functions, impl methods, trait methods (including default bodies), and
globals, each keyed by its defining `SymbolKey`. Stdlib items are
nodes too — user code reaches them — but only user-authored nodes are
eligible for `dead_items` (the stdlib-exclusion policy is the
unused-diagnostics WEP's).

`liveness::compute` builds, locally, an `AstId → enclosing-item
SymbolKey` map per module in one structural walk. A use-site anywhere
in a function/global body — including inside closures, which lower to
synthesised functors later and are not source items — maps to its
enclosing top-level item. `AstIndex` is not extended; the map is
private to the pass.

#### Edges

Each recorded use-site becomes an edge `enclosing-item(use) → target`:

- `ModuleBindings.references` — edges whose def-side is a node
  (free-function calls, global reads, method calls that record a decl
  reference, variant-case construction). Edges to locals fall out
  because their def-side is not in the node set.
- Dispatch facts in `TypeAnnotations` — `method_dispatch`,
  `static_method_dispatch`, `operator_dispatch`,
  `index_assign_dispatch`, `from_call_facts` — resolved through the
  `FunctionRef → defining SymbolKey` resolver reify already uses
  (`find_impl_method_ast_id`). These cover the paths that leave no
  `references` edge: operator overloading, the `?` / `From`
  conversion, `for`-of `into_iter` / `next`, index assignment.

A dispatch fact that cannot be resolved to a defining `SymbolKey` is a
graph bug (fail-loud), not an over-approximation site.

#### Generic trait dispatch

One path is undecidable at the source level: a trait method reached
only through a generic type parameter
(`fn show<T: Display>(x: T) { x.fmt() }`). `annotate` records the edge
to the trait method (`Display::fmt`); the concrete impl
(`Foo^Display::fmt`) is selected during monomorphization, which the
source graph does not run. This is resolved fail-loud: the concrete
impl is left dead at the source level, surfaces as an ICE if actually
instantiated, and the fix is a graph edge — "a trait method reachable
in a generic context makes the corresponding method on every reachable
impl reachable." The edge is added as the rule the language feature
requires; there is no blanket over-approximation.

#### Reachability and reify

Reachability is a single BFS from the root set over the edges — no
fixed-point loop, the node set and edges are static. `live_items` is
the reachable closure; `dead_items` is the user-node complement in
source order.

`reify_module` consults `live_items` in its per-`Item` loop and emits
nothing for an absent item. The TIR handed to `monomorphize` is the
reachable closure of the program, not the full source. `optimize/dce.rs`
stays in place: it removes the post-monomorphization population
(monomorph clones, synthesised CM bindings, effect-dispatch helpers,
inlined-away code) that never passes through `Semantics`. It retires
only from diagnostic emission.

#### Prerequisite: diagnostics come from `Semantics`, not TIR

Gating reify is sound only once every semantic diagnostic that can fire
on dead code is produced from `Semantics` (AST + recorded facts), not
from the emitted TIR. Wado reports errors in unreachable code (the
effect, stores, purity, and world-export-conformance checks), and those
checks historically ran on the `TirModule`s _after_ reify — so dropping
a dead function silently suppressed its error (proven by
`effect_check_missing`, `export_required_for_run`,
`stores_violation_*`). The checks therefore move to the post-`annotate`
analysis layer alongside `liveness`, reading the same facts
(`function_effects`, `effect_ops`, `method_dispatch`, …). This is the
same TIR→AST+facts move Stage 7 made for control-flow / missing-return,
and it lets the LSP surface effect and stores diagnostics too (it builds
no TIR). That move has landed (the effect / stores / purity checks read
`Semantics`, not the emitted TIR), so reify now passes `Some(live_items)`
and gates dead free-function emission; `liveness` also feeds the
`DeadFunction` / `DeadGlobal` diagnostics. The policy is owned by
[`wep-2026-05-16-unused-diagnostics.md`](./wep-2026-05-16-unused-diagnostics.md)
(Phase 3b).

The roots, suppression rules, stdlib exclusion, and severity policy
remain owned by the unused-diagnostics WEP and are not duplicated
here.

## Migration Plan

The change touches every file under `elaborator/` and lands incrementally,
with the suite (E2E, WIR golden, LSP query) green at every step, in
dependency order. Stages are guidelines, not PR boundaries.

Phase 1 (Stages 1–6, 7a, 7-A, 7-B) is DONE: `TypeSystem` / `ModuleSemantics` /
per-`AstId` `TypeAnnotations` extracted from the God Object; `reify` split out
as the sole TIR producer reading recorded facts; the combined `annotate` walk
builds no TIR (every `resolve_*` returns a `TypeId`), so LSP runs `annotate`
alone. The premise the WEP existed to fix — LSP building and discarding TIR —
is resolved. Detail in the Status section below and in git.

Stage 6 — the piece that landed last — closes Phase 1:

Stage 6 — Liveness and DCE. `liveness::compute(&Semantics)` and the
`Liveness` field on `Semantics` have landed (computed in WEP phase order; the
`DeadFunction` / `DeadGlobal` diagnostics consume `dead_items`). The reify gate
is now enabled: `build_tir_from_state` passes `Some(&liveness.live_items)` and
`reify_module` skips dead user free functions (globals stay ungated — an
initializer can trap or have effects; `optimize/dce.rs` removes genuinely pure
dead ones). Both prerequisites landed: (1) the semantic diagnostics that can
fire on dead code (effect / stores / purity) are produced from `Semantics`
rather than the emitted TIR, so dropping a dead function suppresses no
diagnostic; (2) the cross-module graph resolves the dispatch facts that leave
no `references` edge. Independent of Phase 2.

Each stage keeps `mise run test`, the WIR golden fixtures, and the LSP query
tests green.

### Stage 7-B execution plan (done)

7-B landed in two phases. Phase 1 ported every combined-walk analysis that read
resolved-TIR structure onto the AST + recorded facts (assign l-value / ref
validity, null-unknown inference, block result type, unary constant folding,
tuple spread, struct-literal deferred coercion, pattern-variant const literals,
for-of `TupleZip`, if-let-chain result type, assign-target ident classification)
— each behaviour-preserving and green on its own. Phase 2 then converted every
`resolve_*` arm to return a `TypeId`, made `build_tir_from_state` TIR-free, and
routed LSP through `annotate` alone. One foundational ordering constraint
surfaced: `record_expression_type` drops `ERROR`/UNKNOWN types, so the
null/unknown reader had to be ported before the block-result-type reader.

## Status

- [x] Stages 1–4 — God Object decomposed; `TypeAnnotations` is the per-`AstId` fact store.
- [x] Stage 5 — reify is the sole TIR producer (incl. trait default-method synthesis via per-impl `default_method_semantics`); missing-return ported onto the AST (`CtrlFlowCtx`).
- [x] Stage 6 — Liveness / DCE landed; reify gate enabled (dead user free functions skipped, globals ungated); both prerequisites in place.
- [x] Stage 7a — routing removed; the combined walk survives only as the `annotate` fact-recorder.
- [x] Stage 7-A — reify is mechanical: every decision-bearing read goes through a recorded fact.
- [x] Stage 7-B — `annotate` builds no TIR; `resolve_*` return only `TypeId`; LSP runs `annotate` alone; the duplicate combined-walk TIR construction is deleted.

### Landing log

The per-map detail and the gap-by-gap parity history live in git. The identity
model has since moved to globally-unique `AstId` (`SymbolKey` retired — see the
header note); every per-`AstId` fact map keys by bare `AstId`.

## Phase 2 — God Object operation decomposition (2026-06)

Phase 1 (above) split the _data_ out of the `Elaborator` God Object
(`TypeSystem`, `ModuleSemantics`, `reify`). Phase 2 moves the _operations_:
the trait/method-resolution queries still live as `impl Elaborator` methods and
still read the borrowed `loaded_modules` AST map and the per-function
`trait_ctx` scope, so none of them can move to `TypeSystem`. Phase 2 removes
those two couplings so the queries become `impl TypeSystem` operations taking
only type IDs, names, pre-digested decl facts, and an explicit scope.

Three independent tracks; each slice keeps `mise run test` green (E2E 2934/0).

### Done

- [x] Pure type-system ops → `impl TypeSystem`: impl-type matching, type-shape helpers, `inherent_impl_type_args_match` etc. read `self.type_table` via `self.tysys`.
- [x] `TraitEnv` build-time decl digests: `TraitEnv::build` pre-computes `ImplHeader` / `TraitDeclHeader` / `function_type_params` / import scopes, so queries read digests instead of scanning `loaded_modules`.
- [x] Track B Stage A: `AnnotateCtx { trait_ctx, trait_check_stack }` bundles the per-function scope into one `annotate_ctx` field, save/restored by the `TypeParamScope` guard.
- [x] Track B Stage B: `type_implements_trait(_inner)` take explicit `ctx: &AnnotateCtx` (recursion guard + type-param bound check).
- [x] Track B Stage C: every `trait_ctx` query that moves onto `TypeSystem` takes explicit `ctx: &AnnotateCtx` (the trait-impl-lookup cluster, `classify_call_callee`, `infer_variant_type_args`); scope producers keep `&mut self.annotate_ctx`. Criterion: a query moving to `TypeSystem` threads `ctx`, one staying on `Elaborator` reads `self.annotate_ctx`, a producer stays on `Elaborator`.
- [x] Stage C refinement: the `Elaborator`-resident `resolve_type` family reads `self.annotate_ctx` directly (no `ctx` param), removing ~96 `AnnotateCtx` clones and the `#[derive(Clone)]`; `trait_check_stack` is no longer copied anywhere.
- [x] Stage D blocker (option a): the trait-impl-lookup cluster takes an explicit `scope: &TypeLookup` for module-scoped `current_module_source` / imports / struct-and-variant lookups, decoupling it from `self.sem` / `self.type_lookup()`.
- [x] Track B Stage D: the ~14 decoupled cluster methods plus `auto_derive_default_struct_type` moved to `impl TypeSystem`; call sites read `self.tysys.X(ctx, scope, …)`. Trait/type-implements resolution is now callable without an `Elaborator`.
- [x] Track B Stage D: `classify_call_callee`, `infer_variant_type_args`, `binop_operand_requires_trait` moved to `impl TypeSystem`; the rest of `operators.rs` stays on `impl Elaborator` (dispatch/coercion methods calling `resolve_expr` / `record_*` / `self.logger`).
- [x] Track B Stage D: the `resolve_type` family stays on `impl Elaborator` by design (mutates `ModuleSemantics.bindings`, `&mut self` for on-demand interning, reads `self.annotate_ctx` so no clone cost) — a settled member, not pending work.

### Remaining

Superseded: the completion design — the decl-signature digest that removes
the remaining `loaded_modules` consumers, the `Scope` guard unification
(Track B Stage E), and the walker's end state — lives in
[`wep-2026-07-10-elaborator-god-object-dismantlement.md`](./wep-2026-07-10-elaborator-god-object-dismantlement.md).

## Consequences

### Benefits

- The God Object is gone. Every new field has a sub-struct it
  belongs to, and the membership criterion is mechanical.
- `annotate` actually annotates. The phase name matches its
  work, and `Semantics` is genuinely complete after `annotate`
  returns. The LSP no longer pays for thrown-away TIR.
- `reify` is mechanical. It can be tested in isolation against
  hand-built `ModuleSemantics` fixtures, and the type boundary
  ensures it cannot reach for inference state.
- DCE has a place to live. The `liveness` pass slots cleanly
  between `annotate` and `reify`, gives the LSP its unused
  diagnostics, and lets `reify` shrink its output before
  monomorphization sees it.
- `Rc<RefCell<…>>` retreats to where it is genuinely needed.
  The `TypeTable` remains shared (anonymous structs and
  monomorphic instances intern through both `annotate` and
  `reify`); the `references` / `local_symbols` plumbing simplifies
  to `&mut ModuleSemantics`.
- Plain Rust ownership. Borrow-check pressure now reflects the
  conceptual model rather than working around it.

### Trade-offs

- Batch compilation walks bodies twice. Once in
  `annotate_bodies` (the heavy walk that does inference,
  resolution, and dispatch) and once in `reify` (the mechanical
  walk that reads the annotations). The remedy, should one be
  wanted, is to specialise `reify` over the recorded annotations,
  not to merge the phases back. The clean phase boundary is
  the load-bearing decision.
- Bigger up-front design surface. The four sub-structs and
  their membership rules need to be respected when adding fields.
  This is a feature, not a bug — the membership rule replaces
  the current absence of any rule.
- Migration is long. The change touches roughly the entire
  `elaborator/` tree. Stages 4 and 5 are the riskiest because
  they restructure the body walk; the WIR golden fixtures and
  E2E suite carry the equivalence guarantee.

### Risks and mitigations

- `ModuleSemantics` becomes another God Object. Mitigation:
  the four sub-structs are non-optional, and every field added
  during or after migration must justify its sub-struct. Reviews
  reject "just put it on `ModuleSemantics` directly."
- The `annotate` / `reify` split leaks during migration.
  Mitigation: stage 4 introduces the annotation storage _before_
  stage 5 splits the walk. Any annotation reify needs but
  `annotate` does not record is a stage-4 omission, surfaced as a
  panic in stage 5.
- DCE breaks a previously-reachable item. Mitigation: the
  unused-diagnostics WEP owns the reachability rules; stage 6
  switches the source of truth from `optimize/dce.rs` to the
  elaborator-time pass, with the same E2E and WIR fixtures
  validating that no live item is dropped.

## See Also

- [`wep-2026-04-18-lsp-architecture.md`](./wep-2026-04-18-lsp-architecture.md)
  — the LSP path's contract on `Semantics`.
- [`wep-2026-05-16-unused-diagnostics.md`](./wep-2026-05-16-unused-diagnostics.md)
  — the policy and surface for unused diagnostics. To be rewritten
  to consume the elaborator-time `liveness` pass introduced here.
- [`wep-2026-05-11-nir.md`](./wep-2026-05-11-nir.md) — the type
  boundary between TIR and post-lower IR. The boundary this WEP
  introduces between `annotate` and `reify` is the upstream
  counterpart.
