# WEP: Elaborator Re-architecture — TypeSystem / Annotate / Reify

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

**`TypeSystem`** — the pipeline-wide type knowledge. Owned by the
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

**`ModuleSemantics`** — per-module semantic facts produced by the
elaborator. One instance per loaded module. Owned by `Semantics` in
an `IndexMap<ModuleSource, ModuleSemantics>`. Built incrementally by
`annotate` and extended by the body walk. Decomposed into four
sub-structs with explicit membership rules:

- **`bindings`** — `use → def` edges (`references`) and locally
  defined symbols (`local_symbols`). What the LSP reads to answer
  go-to-definition, find-references, and hover-on-local queries.
- **`imports`** — per-module name resolution context derived from
  `use` declarations: `imported_type_sources`,
  `import_original_names`, `namespace_imports`, `effect_sources`.
- **`types`** — per-`AstId` type annotations and dispatch decisions
  recorded during the body walk: the `TypeId` of every typed
  expression, the resolved target of each method call, the chosen
  coercion at each conversion site, the desugar kind for each
  TIR-direct rewrite (`assert`, `matches`, comparison chain,
  for-of, while, compound assignment).
- **`decls`** — module-internal declarations confirmed by
  elaboration: `function_return_types`, `imported_functions`,
  `current_module_globals`, `imported_globals`,
  `associated_constants`, `generic_function_*`,
  `generic_method_*`, `generic_struct_names`,
  `pending_anonymous_structs`.

Each sub-struct admits a new field only when "does this fit the
sub-struct's responsibility?" has a clear yes/no answer. A field
that cannot be placed is a design question, not a default-into-the-
catch-all.

**`Reify`** — the AST + `ModuleSemantics` → `TirModule` walker.
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

`elaborate` survives as the **umbrella term and physical directory
name**. `wado-compiler/src/elaborator/` continues to host the three
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

- **Interning** — `intern_*`, `make_type_param`, `make_type_pack`,
  registration of associated-type resolutions.
- **Coercion** — `coerce`, the numeric-literal and tuple-to-sequence
  coercions, struct-to-map coercion. Inputs are `TypeId`s and
  expression contexts; the operation returns a coerced expression or
  rejection.
- **Inference** — `InferCtx` is constructed from `&mut TypeSystem`
  rather than from an `Elaborator`. Inference results are reported
  back as type substitutions.
- **Type checking** — `typecheck`, `typecheck_return`.
- **Trait queries** — `implements`, impl resolution by trait and
  target, blanket-impl resolution, bound checking, associated-type
  binding lookup.
- **Method lookup** — `lookup_method_info`, indexing-trait impls,
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

### DCE / Liveness

The `liveness` pass is documented in
[`wep-2026-05-16-unused-diagnostics.md`](./wep-2026-05-16-unused-diagnostics.md);
that WEP is rewritten to consume the new architecture (its current
content places the analysis in `optimize/dce.rs`, which moves to the
elaborator under this WEP). The contract on the elaborator side is
narrow:

- `liveness::compute(&Semantics) → Liveness` runs after every
  `ModuleSemantics` is fully populated.
- The result is stored on `Semantics` as a new field.
- `reify_all` gates item emission on `liveness.live_items`.
- LSP exposes unused diagnostics by reading `Liveness` directly.

The roots, suppression rules, stdlib exclusion, and severity policy
remain owned by the unused-diagnostics WEP and are not duplicated
here.

## Migration Plan

The change touches every file under `elaborator/`. It must land
incrementally with the test suite (E2E fixtures, WIR golden
fixtures, LSP query tests) green at every step. The migration
proceeds in seven stages, in dependency order. Stages are guidelines,
not commitments to specific PR boundaries.

**Stage 1 — Skeleton.** Introduce `TypeSystem`, `ModuleSemantics`,
and the four sub-structs as empty types alongside the existing
`Elaborator`. No method moves; the existing fields are annotated
with their future destination. The goal is to fix the physical
target before any logic moves.

**Stage 2 — Move pure type-system operations to `TypeSystem`.**
Coercion, inference, type checking, trait queries, and the
type-only portion of method lookup migrate. The remaining
`Elaborator` delegates through a `tysys: TypeSystem` field. The
borrow checker enforces the boundary: methods on `TypeSystem`
cannot reach `current_module_source` or `imported_type_sources`,
which mechanically classifies each moved method.

**Stage 3 — Move per-module state to `ModuleSemantics`.** The
import context, decls, and `use → def` maps migrate into
`ModuleSemantics`. The `Elaborator` becomes a thin wrapper holding
`&mut TypeSystem` and the current `&mut ModuleSemantics`.
`Rc<RefCell<…>>` plumbing that existed only to share these maps
with `AnnotateState` is removed.

**Stage 4 — Introduce explicit per-`AstId` annotation storage.**
`ModuleSemantics.types` (`TypeAnnotations`) is populated by the
existing body walk. TIR emission stays in the same walk for this
stage. The objective is to make the data the future `reify` will
read available, while preserving today's single-pass behaviour.

**Stage 5 — Split `annotate_bodies` from `reify`.** Per-construct
walkers split into a `annotate_*` form that writes
`ModuleSemantics.types` and a `reify_*` form that reads it and
emits TIR. Batch compilation now performs two walks. Equivalence
is established by the E2E suite and the WIR golden fixtures; no
separate TIR-identity check is required because the WIR comparison
already binds the entire downstream pipeline.

**Stage 6 — Liveness and DCE.** `liveness::compute` is added, its
result is stored on `Semantics`, and `reify_all` gates item
emission on it. The user-facing unused diagnostics land per the
unused-diagnostics WEP. `optimize/dce.rs` retires from its current
role as the source of the same information, with the
optimize-time pass either deleted or repurposed per that WEP.

**Stage 7 — Cleanup.** The old `Elaborator` struct and
`AnnotateState` are removed; their last surviving fields move
to their final homes. The pipeline diagram in `CLAUDE.md` and
`docs/compiler.md` is updated. The `elaborator/` directory's
file layout matches the module layout in this WEP.

Each stage keeps `mise run test`, the WIR golden fixtures, and
the LSP query tests green. Performance is not tracked during
migration; see Trade-offs.

## Status

- [x] **Stage 1 — Skeleton.** Empty `TypeSystem` + `ModuleSemantics`
      sub-structs introduced alongside the existing `Elaborator` /
      `AnnotateState`; every existing field annotated with its future
      destination via `// MIGRATION:` markers.
- [x] **Stage 2 — Pure type-system operations on `TypeSystem`.** 15
      pipeline-wide fields (type arena, decl-interned tables, registries,
      included-files map, read-only caches) and five host-agnostic
      helpers (`is_known_type_name`, `is_numeric_literal`,
      `operator_trait_method`, `typecheck`, `typecheck_return`) now live
      on `TypeSystem`. Both `AnnotateState` and `Elaborator` hold one
      `tysys: TypeSystem` (shallow `Rc`/`Arc` clone). Legacy
      `pub fn resolve_module` + `Elaborator::new` removed as dead code.
- [x] **Stage 3 — `ModuleSemantics` population.** The four sub-structs
      hold the per-module state previously flat on `Elaborator`.
      `Elaborator` owns one `sem: ModuleSemantics`;
      `AnnotateState.module_semantics: IndexMap<ModuleSource, ModuleSemantics>`
      replaces the trio of `Rc<RefCell<…>>` maps (`references` /
      `local_symbols` / `local_types`). `build_tir_from_state` takes
      `&mut AnnotateState` and swaps each module's instance around the
      body walk; `semantics_with_logger` flattens back so `Semantics`'s
      flat-map API is unchanged.
- [x] **Stage 4 — Per-`AstId` annotation storage.** `TypeAnnotations`
      now carries four maps populated by the existing body walk:
      `expression_types` (every `resolve_expr` records its resolved
      `TypeId`), `method_dispatch` (each `MethodCallExpr` records the
      resolved `FunctionRef` + `SelfKind`), `coercions` (each successful
      `try_coerce` branch records its `CoercionKind` + target),
      and `desugars` (each TIR-direct rewrite site — `assert`, `matches`,
      comparison chain, for-of, `while`, compound assignment — tags the
      enclosing AST node with its `DesugarKind`). `Semantics` flattens
      every map into a `SymbolKey`-keyed view and exposes stable public
      projection accessors (`expression_type`, `method_dispatch_view`,
      `coercion_view`, `desugar_view`) for tests and the future LSP
      hover path. The stdlib snapshot seeds every map back into
      per-module storage so cached stdlib modules stay consistent.
- [ ] **Stage 5 — `annotate_bodies` / `reify` split.** _In progress._
      Recording half landed: `MethodDispatch.is_ref_impl`,
      `IndexMutMethodCall` / `NewtypeFromCollapse` desugar tags
      (Gaps 2–3, 9), `closure_captures` / `assert_captures` /
      `for_of_iterator` (Gaps 4–6), `generic_instantiations` at every
      generic call / struct-literal / variant-ctor site (Gap 1), and
      `operator_dispatch` end-to-end for binary expressions (Gap 11 —
      Index-side wiring still pending).
      `Reify<'a, H>` introduced in `elaborator/reify.rs` with a
      complete per-Item dispatch surface and a public `Reify::new`
      constructor for the orchestration driver. Decl-only items
      (`Enum`, `Flags`, `Newtype`, `Struct` modulo field defaults,
      `Variant`, `Interface`, `Resource`, `Global`) are concrete.
      Body-walk dispatchers cover `reify_function` / `reify_global` /
      `reify_test_decl` / `reify_block` / `reify_stmt` / `reify_expr`
      / `reify_pattern`. `reify_pattern` is feature-complete
      (`Wildcard`, `Ident`, `MutIdent`, `Literal`, `Tuple`,
      `Variant`, `Or`, `Range`, `Struct`). Body-walk TIR emission
      covers: `Stmt::{Let (all patterns — irrefutable +
      destructuring), Expr, Return, TaskReturn, Break, Continue,
      If (Condition::Expr), Loop, Match, While (Condition::Expr),
      For (Condition::Expr), LabeledBlock}`; `Expr::{Literal (modulo
      Location/Include), Block, Ident (local + globals + assoc
      constants + free function refs + qualified Variant::Case /
      Enum::Case / Flags::Member), TupleLiteral, Cast, Unary,
      FieldAccess, MethodCall, Call (free function + variant ctor),
      Match, Matches, StructLiteral (named), Range, TemplateString,
      If (Condition::Expr), Assign, CompoundAssign (simple lvalues),
      Binary (native + operator-trait dispatch), Resume, LabeledBlock,
      Spread}`. Receiver adjustment is shared via
      `adjust_receiver_for_self_kind_static` so the elaborator and
      reify produce the same `Unary{Ref}` / `Unary{MutRef}` / deref
      wrapping.
      Remaining (each carries a labelled `todo!` with the
      `Elaborator::resolve_*` location it mirrors): `reify_impl`
      (impl-block scaffolding: trait dispatch, synthesis requests,
      ref-type impl unwrapping); non-local idents (`ns::Type::Case`
      namespace path); uninitialised `let x: T;`; field-default
      expressions on `reify_struct`; `Expr::{ComparisonChain,
      StaticMethodCall, Index, Closure, TryOp, WithHandler}` and
      closure-call / indirect-call / qualified-callee shapes of
      `Call`; anonymous-struct literals; `Stmt::{ForOf, Assert}` and
      `If (Condition::LetChain)` / `While (Condition::LetChain)` /
      `For (Condition::LetChain)`; the `Literal::{LocationFile,
      LocationLine, DataSection, IncludeStr, IncludeBytes}`
      host-driven branches; the Index-side wiring of Gap 11; per-arg
      `is_mut` / literal-coercion records for `Call` / `MethodCall`;
      `CompoundAssign` IndexMut-target rewrite.
      Orchestration switch is the last step: rebind
      `build_tir_from_state` so the existing combined walk drives
      annotate_bodies only (and discards its TIR output) while
      `Reify::reify_module` produces the TirModule. The switch is
      gated on the residual `todo!`s being either implemented or
      shown to be unreachable from supported source.
- [ ] **Stage 6 — Liveness and DCE.**
- [ ] **Stage 7 — Cleanup.**

### Design notes (Stages 1–3)

#### TypeSystem membership rule

A field belongs on `TypeSystem` iff the elaborator's body walk queries
it while making a type decision. `world_registry` lives on
`AnnotateState`, not `TypeSystem`: only post-elaborator stages (`link`,
`synthesis`, `optimize/dce`, world-existence validation in `lib.rs`)
read it.

`indexing_trait_cache` / `method_info_cache` are genuine type-system
caches but stay on `Elaborator` until the pipeline-wide cache lifetime
story is decided. `trait_check_stack` is a per-call frame stack (not a
cache); sharing it would either leak stale frames (soundness bug) or
need save/restore that defeats the move — it stays with `trait_ctx`.

#### TypeSystem stays host-agnostic

`TypeSystem` operations return `Result<(), Payload>`, never
`&Logger<H>`. The `<H: CompilerHost>` parameter is confined to a thin
wrapper on `Elaborator` that emits the diagnostic. The pattern in
`typecheck.rs` is the template: pure helper over `&TypeTable` →
`impl TypeSystem` returning payload → `impl Elaborator` calling
`logger.error`. Pure `TypeSystem` helpers enumerate enum variants
exhaustively (no `_ => …`) so a new variant surfaces as a compile
error.

#### Unique-ownership contracts surface at the leak site

`compile_after_load` consumes `Arc<TraitEnv>` and `Rc<BuiltinRegistry>`
out of `state.tysys` and `debug_assert_eq!`s their strong counts. A
stray clone in a later refactor surfaces at the handoff rather than in
a downstream phase (`synthesize` panics on shared `Arc<TraitEnv>`;
shared `Rc<BuiltinRegistry>` silently deep-clones).

#### `Elaborator` owns `sem` by value, not `&mut ModuleSemantics`

The Decision sketch had the elaborator hold `&mut ModuleSemantics`;
the implementation owns `sem: ModuleSemantics` instead. Same goal
(disjoint mutable access per module), lighter shape: no second
lifetime parameter, same `Clone`-by-shallow-Rc handoff as `TypeSystem`
already uses, and the driver iterates a cloned `sorted_sources` while
mutating `state.module_semantics` without borrow conflict. The body
walk is bracketed by `swap_remove(ms)` → `resolve_module` →
`insert(ms, elaborator.sem)`.

#### `Semantics` keeps its flat API

`semantics_with_logger` drains `state.module_semantics.values_mut()`
into the existing flat `references` / `locals` / `local_types` maps,
so the LSP query surface (`referenced_symbol`, `iter_references`,
`local_type_name`, …) is unchanged. Promoting per-module storage onto
`Semantics` itself is a Stage 7 cleanup.

#### Snapshot seeding asserts the loaded-set invariant

The snapshot's flat maps are split by `key.module` into per-module
`ModuleSemantics`. Seeding uses `get_mut` + `debug_assert!` rather
than `entry().or_default()`: an invariant break (snapshot module not
in current `modules.keys()`) surfaces in debug builds instead of
silently creating phantom entries that the flatten would leak into
`Semantics::references` as edges into unloaded modules.

`build_tir_from_state` mirrors the invariant with
`.expect("module_semantics is pre-populated by annotate_modules")`
instead of `unwrap_or_default()` at `swap_remove`.

#### `ModuleBindings` has a transient cross-module exception

`with_module_perspective` swaps `current_module_source` without
swapping `self.sem`, so record calls inside its body tag the use-key
with the foreign module while writing into the outer module's
`sem.bindings`. Today's flatten reconciles by full `SymbolKey`;
Stage 5's reify will need to either extend `with_module_perspective`
to swap `bindings`/`types` too or accept these maps as
flat-store-by-construction. Same note next to the type in
`sem/bindings.rs`.

### Design notes (Stage 4)

#### Recording sits at the choke point, not the outer dispatcher

Self-review surfaced a class of bypasses: when recording lives in the
outer dispatcher (`try_coerce`, `resolve_method_call_with`), every
direct call to a sub-helper (`try_coerce_tuple_to_sequence` from
`resolve_cast` / `resolve_let`, `recoerce_literal_args` after
post-inference type-arg substitution, `try_resolve_index_mut_method_call`
for `container[i].method()`, etc.) silently skips the record. The fix
is to record at the single TIR-construction choke point per kind, not
at the outer dispatcher:

- Coercion: each `try_coerce_*` sub-helper records its `CoercionKind`
  and `expression_types` itself. `try_coerce` no longer wraps the
  numeric / tuple / struct paths with redundant record calls; the
  inline string-newtype and closure-newtype branches still record
  here because they have no sub-helper.
- Method dispatch: `record_method_dispatch` is called by both the
  regular `resolve_method_call_with` path and the IndexMut rewrite in
  `try_resolve_index_mut_method_call`. The MethodNotFound recovery
  branch sets a `method_found = false` flag that gates the record so
  the placeholder MethodInfo doesn't leak into the map as a junk
  dispatch entry.
- Expression types: `record_expression_type` skips writes when the
  resolved type is `ERROR` or still contains `UNKNOWN`. The Null
  literal case (which initially resolves to `Option<UNKNOWN>` and gets
  patched later by `patch_unresolved_null`) is therefore not written,
  matching how reify will need to handle Null via context anyway.
- Desugars: `ForOfIterator` only records after the `IntoIterator`
  trait check passes; `ComparisonChain` only after the empty- and
  single-comparison early returns. Both previously tagged nodes the
  elaborator did not actually desugar.
- Stmt-position match: dispatches to `resolve_match_expr` directly, so
  the stmt arm records `expression_types` explicitly to keep the
  per-AstId map populated for stmt-context matches as well.

Each annotation kind is still written from inside the function that
owns the decision; the choke-point pattern just ensures that "inside"
is the single sub-helper every caller routes through, not the outer
dispatcher one or two of the callers happen to use.

#### Synthetic call sites stay out of the maps

For-of's `.into_iter()` / `.next()` lowerings call
`resolve_method_call_with` with both `method_id: None` (no use→def
edge) and `call_id: None` (no `method_dispatch` entry). The tuple
`.len()` / `.zip()` and static-method-as-instance short-circuits
return before the recording site for the same reason — reify
recognises them from the receiver type alone. For-of's `.enumerate()`
unwrap at the AST level is another instance: the `.enumerate()`
`MethodCallExpr` is consumed by the for-of dispatcher before
`resolve_expr` ever fires on it, so neither `expression_types` nor
`method_dispatch` carry an entry for `mc.id`. Reify re-detects the
pattern by inspecting `for_of.iterable`. The contract is documented on
`MethodDispatch` and enforced by the `call_id` field on
`MethodCallInput`.

#### Recording is idempotent under the assert-capture re-entry

The power-assert path calls `resolve_expr` recursively on the same
`AstId` (with an `in_progress` guard to suppress the capture hook the
second time). The wrapper records `(ast_id, type_id)` on both calls,
but both writes carry the same value, so the final map state is
correct. Coercion and dispatch sites do not re-enter on the same id.

#### Public projection accessors return strings, not `pub(crate)` types

`MethodDispatch`, `CoercionChoice`, and `DesugarKind` all live in the
`pub(crate) mod sem` namespace and embed `pub(crate)` TIR types
(`FunctionRef`'s `MethodInfo`), so a `pub fn …_view` accessor that
returned them by reference would leak `pub(crate)` types to the API.
The Stage 4 accessors instead return small public projections —
`(name, module, self_kind_str)`, `(kind_str, target_type)`, the
variant name as a `String` — that are sufficient for the testability
contract while keeping the full structures internal until reify or LSP
lands a real consumer.

#### `#[allow(dead_code)]` is the load-bearing TODO

The new field bodies on `MethodDispatch` / `CoercionChoice`,
`CoercionKind`'s variants, and the `pub(crate)` `method_dispatch_at`
accessor are tagged `#[allow(dead_code)]` because reify (Stage 5) is
the consumer. Removing those allows when Stage 5 lands gives a
mechanical "what data is actually consumed" audit; any field that
stays unread by then is a Stage 4 over-record.

### Design notes (Stage 5)

Stage 5 is the cut from "annotate also emits TIR" to "annotate
populates `ModuleSemantics`, reify reads it and emits TIR." Stage 4
covered the four obviously-needed maps (`expression_types`,
`method_dispatch`, `coercions`, `desugars`); the body walk also makes
several other TIR-shaping decisions that the current code captures
implicitly inside the emitted `TirExpr`/`TirStmt` shape. These
sub-sections enumerate each remaining gap, name the new
`ModuleSemantics` field, pin the recording site, and pin the reify
consumer. The list is the design contract Stage 5 implements; no
gap is left to be re-derived inside reify, because the WEP's
Decision §`Reify` insists that reify perform no inference, name
resolution, or dispatch decisions.

#### Gap inventory ground rules

Each gap is described by four facts. Implementations that skip any
of the four are not Stage 5.

- A name for the decision the body walk makes.
- The `ModuleSemantics` field that records it, with a concrete type
  sketch and the sub-struct it belongs on
  (`bindings` / `imports` / `types` / `decls`).
- The recording site in today's code — file + the choke-point
  helper, per the Stage 4 §`Recording sits at the choke point`
  pattern.
- The reify consumer — which TIR-construction site reads the field.

New fields live on `TypeAnnotations` unless the data is plainly a
declaration fact (then `ModuleDecls`) or a binding edge (then
`ModuleBindings`).

#### Gap 1: generic instantiation type arguments

`Elaborator::infer_fn_type_args` (`call.rs:440–580`),
`infer_static_method_type_args` and `infer_variant_type_args`
(`expr.rs:973–1051`), and the struct-literal path
`infer_struct_type_args` (`expr.rs:3422–3520`) decide concrete
`TypeId`s for the generic parameters at each call / construction
site. The decision flows into `FunctionRef::monomorph_info`,
`TirExprKind::Call::type_args`, the variant-ctor TIR shape, and the
mangled `TirExprKind::StructLiteral::struct_name`. No
`ModuleSemantics` field carries it today.

- Field: `TypeAnnotations::generic_instantiations:
  IndexMap<AstId, GenericInstantiation>`, with
  `GenericInstantiation { type_args: Vec<TypeId>, instance_type:
  TypeId }`. `instance_type` is the `make_generic_instance` /
  `make_struct` / `make_variant` result; recording it saves the
  same lookup at reify time and pins the mangled-name input.
- Recording sites: each `infer_*` return path, plus the explicit
  `type_args` branch (`fn f::<i32, T>(x)`). The recording helper
  matches the Stage 4 choke-point pattern: one
  `record_generic_instantiation(ast_id, type_args, instance_type)`
  on `Elaborator`, called from `resolve_call`, `resolve_struct_literal`,
  `resolve_variant_ctor`, and `infer_static_method_type_args`.
- Reify consumer: `reify_call`, `reify_struct_literal`,
  `reify_variant_ctor`. Each reads `generic_instantiations[ast_id]`
  and emits `TirExprKind::Call { type_args, … }` /
  `TirExprKind::StructLiteral { struct_type, struct_name, … }` /
  `TirVariantConstruct` directly.

#### Gap 2: receiver adjustment for self-kind

`adjust_receiver_for_self_kind` (`method_lookup.rs:1596–1700`)
decides whether to insert `Unary { Ref }`, `Unary { MutRef }`, a
`deref_to_value` chain, or nothing, based on the receiver's
resolved type, the dispatched `SelfKind`, and the impl's
ref-receiver flag. Today this is implicit in the TIR shape; reify
needs to know whether to wrap. `MethodDispatch.self_kind` alone is
not enough — the ref-impl flag determines an _additional_ layer
(e.g. `&&T` for `&self` on `impl Trait for &T`).

- Field: extend `MethodDispatch` (in `sem/types.rs`) with
  `is_ref_impl: bool`. The wrap depth is then fully derivable from
  `self_kind` × `is_ref_impl` × the receiver's resolved type
  (which reify already has from `expression_types`).
- Recording site: `lookup_method_info` records the `is_ref_impl`
  flag on its result; `record_method_dispatch` passes it through.
- Reify consumer: `reify_method_call` constructs the receiver
  TIR, then routes it through a `reify_receiver_adjustment` helper
  that mirrors today's `adjust_receiver_for_self_kind` — purely
  mechanical because the inputs are all on hand.

#### Gap 3: IndexMut rewrite of `container[i].method()`

`try_resolve_index_mut_method_call`
(`method_lookup.rs:3390–3455`) rewrites `container[i].method()`
into a TIR shape that materialises a `let __index_mut_val = …;`
local and dispatches the method through it. Today the rewrite
fabricates both the `TirStmt::Let` and the `TirExprKind::Local`
that follows. The rewrite is a method-call decision that
`MethodDispatch` already covers (`IndexMut::index_mut` is the
dispatched method), but reify also needs to know that the call
expanded — not contracted — and to thread the synthesised local
through.

- Field: tag the `MethodCallExpr`'s `AstId` with a
  `DesugarKind::IndexMutMethodCall` variant (the existing
  `desugars` map; the variant is new). The receiver-side
  `IndexExpr` keeps its own `expression_types` entry so reify
  emits the `__index_mut_val` initialiser type correctly.
- Recording site: `try_resolve_index_mut_method_call` calls
  `record_desugar(method_call_ast_id, IndexMutMethodCall)` once
  per successful rewrite (alongside its existing
  `record_method_dispatch`).
- Reify consumer: `reify_method_call` checks `desugars` for the
  call's id; on hit it follows the IndexMut expansion path
  instead of the plain method-call path, synthesising
  `__index_mut_val` through the per-function context (gap 7).

#### Gap 4: closure capture analysis

`resolve_closure` (`closure.rs:127–250`) runs
`collect_mutated_vars` to decide which outer bindings need
`&mut T` capture, materialises a `let __ref_<v> = &mut <v>;` per
mut-captured binding in the _outer_ scope, opens a closure scope
with `deref_overrides`, and finally collects the capture list from
`closure_ctx.get_captures()`. Today every step is a side effect of
the body walk; reify cannot reproduce the capture list without
running the same scan + scope plumbing.

- Field: `TypeAnnotations::closure_captures: IndexMap<AstId,
  ClosureCaptureInfo>`, with
  ```rust
  pub(crate) struct ClosureCaptureInfo {
      // Outer locals captured by mutating reference. Each entry
      // names the original binding and the synthesised `__ref_*`
      // binding that proxies it. `outer_index` is the outer
      // function's local-table index at annotate time; reify
      // recomputes the same index from its own walk (see Gap 7).
      pub(crate) mut_captures: Vec<MutCapture>,
      // Final list of captures the closure surfaces, in the order
      // `closure_ctx.get_captures()` produces them. Each entry is
      // (name, kind, type_id); kind is Value / RefDeref.
      pub(crate) captures: Vec<CaptureEntry>,
      // True when any capture is mutating — drives the
      // `fn mut(...)` vs `fn(...)` choice at the closure type.
      pub(crate) is_mutating: bool,
  }
  ```
- Recording site: `resolve_closure` records the info on the
  closure's `AstId` once Step 6 produces the final capture list.
  The `mut_captures` list is filled in Step 2 just before
  `ctx.address_taken_locals.insert`.
- Reify consumer: `reify_closure` re-materialises the
  `let __ref_<v> = &mut <v>;` statements from `mut_captures`,
  opens a fresh `FunctionContext::new_closure` with the same
  `deref_overrides`, walks the body via `reify_expr` (which
  consumes `expression_types` / `coercions` as usual), and emits
  the `TirCapture` list from `captures`. The `is_mutating` flag
  decides the closure type's `fn mut` vs `fn` tag.

#### Gap 5: assert capture-slot mapping

`desugar_assert` (`assert.rs:62–250`) scans the condition with
`CaptureScanner` to decide which sub-expressions become
`let __vK = …;` bindings, then resolves the condition with a
side-channel hook (`FunctionContext::assert_capture_ctx`) that
captures sub-expressions as they are walked. Today the
slot↔`AstId` map and the `__vK` local indices both live on
`AssertCaptureContext` and dissolve after the assert lowers.

- Field: `TypeAnnotations::assert_captures: IndexMap<AstId,
  AssertCaptureInfo>` keyed by the `AssertStmt`'s `AstId`, with
  ```rust
  pub(crate) struct AssertCaptureInfo {
      // Sub-expression AstIds the scanner flagged, in inner-first
      // order. Each slot index (0..n) maps to one entry; the
      // `__vK` local name follows the slot index.
      pub(crate) slots: Vec<AssertSlot>,
      // Subset of slots whose AST node survived resolution
      // (cf. the `emitted` flag in `desugar_assert`). Slots
      // outside this set produce no `let __vK = …;` binding —
      // template interpolation skips them.
      pub(crate) emitted_slot_indices: Vec<u32>,
  }
  pub(crate) struct AssertSlot {
      pub(crate) ast_id: AstId,
      pub(crate) capture_label: String,  // user-facing label in
                                         // the panic template
  }
  ```
- Recording site: `desugar_assert` records the info just after
  `ctx.assert_capture_ctx.take()` returns, with `slots` /
  `emitted_slot_indices` derived from the `emitted_lets` it has
  just produced.
- Reify consumer: `reify_assert` (a new helper invoked when
  `desugars[stmt.id] == Assert`) walks the condition AST,
  consults `assert_captures[stmt.id].slots` to decide which
  sub-expressions get a `let __vK = …;` binding, threads the
  surviving slot indices into the panic template, and emits the
  guard `if !__cond { panic(…) }` directly.

#### Gap 6: for-of iterator method selection

`resolve_for_of` (`stmt.rs:2107–2200`) classifies the iterable as
`ForOfTuple` / `ForOfVariadic` / `ForOfIterator`; Stage 4 already
tags this on the `ForOfStmt`'s `AstId` via `DesugarKind`. The
remaining decision the elaborator makes silently is _which_
`.into_iter()` and `.next()` implementations the iterator path
picks. `resolve_iterator_for_of` synthesises both calls through
`resolve_method_call_with(method_id: None, call_id: None)`, so
neither call leaves an entry in `method_dispatch`. Reify needs
the dispatch result to emit the same calls.

- Field: `TypeAnnotations::for_of_iterator: IndexMap<AstId,
  ForOfIteratorInfo>` keyed by the `ForOfStmt`'s `AstId`, with
  the `FunctionRef` for `into_iter` and `next` (each carrying its
  own `SelfKind` / monomorph info), plus the iterator's
  `Item` associated type as a `TypeId`.
- Recording site: `resolve_iterator_for_of` records the info
  immediately before it builds the synthetic method calls — once
  per `ForOfStmt`, after the `IntoIterator` trait check passes.
- Reify consumer: `reify_for_of` reads
  `for_of_iterator[stmt.id]` to construct the same
  `TirExprKind::MethodCall { function_ref: into_iter, … }` and
  the loop body's `next` call without re-dispatching.

#### Gap 7: per-function local-frame walk-order invariant

`FunctionContext::locals` (`types.rs:1131–1202`) is the function-
wide local-table built incrementally by `add_local`. Every
`TirExprKind::Local::index`, every `TirStmtKind::Let::local_index`,
the `outer_index` on `TirCapture`, and the `local_types` on
`TirFunction` / `TirGlobal` are stable references into this
vector. Serialising the vector into `ModuleSemantics` would either
duplicate the entire per-function frame state or break the
source-of-truth invariant — neither is acceptable.

Stage 5 keeps `FunctionContext` ephemeral and instead requires
annotate and reify to agree on **walk order**:

- The body-walk visit order is the source of truth.
- Reify mirrors annotate's walk order one-for-one: every `let`,
  every pattern binding, every synthetic local
  (`__assert_K` / `__for_N_body` / `__ref_v` / `__index_mut_val`
  / `__tuple_for_of_N` / `__cond`) is added at the same logical
  point in both passes.
- The synthetic-local naming counters
  (`FunctionContext::next_assert_id`, `next_loop_id`, the
  per-closure ref counter) move with the walk; reify maintains
  its own counters that increment in lockstep with annotate's by
  walking the same nodes in the same order.

This is the invariant that lets `TirCapture::outer_index`,
`TirExprKind::Local::index`, and similar fields remain
non-recorded. The unit-test contract for Stage 5 is that for any
function `f`, the `Vec<TirLocal>` annotate would have emitted
equals the `Vec<TirLocal>` reify does emit. The WIR golden
fixtures bind this transitively, but a focused reify-only test
(see §`Equivalence validation`) makes regressions easy to
diagnose.

The single ordering hazard worth calling out separately: the
closure capture pre-pass (Gap 4) materialises `__ref_<v>` locals
in the _outer_ function's frame, before the closure body is
walked. Reify must add those locals at the same point —
specifically, immediately before `reify_expr` recurses into the
closure body. The recorded `closure_captures` info names the
locals in order; reify replays the `add_local` calls in that
order.

#### Gap 8: struct-literal deferred field coercion

`resolve_struct_literal` (`expr.rs:3422–3520`) performs a
two-pass coercion for generic struct literals: the first pass
resolves field values with the unsubstituted `TypeParam` field
types, and the second pass re-runs `try_coerce_tuple_to_sequence`
once concrete `type_args` are known. Stage 4's per-`AstId`
`coercions` map records each successful coercion at its AST
node, so the second-pass coercion _is_ already recorded on the
field-value's `AstId`. The remaining concern is ordering:

- The second-pass coercion's `coercions[field_value_ast_id]`
  entry overwrites the first-pass entry. Stage 4 already
  guarantees idempotence under `try_coerce` re-entry; the same
  property carries through here because the second pass only
  fires when the first-pass result didn't match the substituted
  field type, and the recording site is the `try_coerce_*`
  sub-helper either way.

No new field is needed. The contract is documented here so a
future review doesn't insist on a `deferred_coercions` map: the
existing `coercions` map is the right place, the choke-point
recording pattern keeps it correct.

#### Gap 9: newtype `T::from(T_val)` reflexive collapse

When the elaborator sees `Newtype::from(x)` and `x` is already of
the newtype's base type, it collapses the call to `x` itself
(`expr.rs:1920–1970`). The outer `Call` AST node evaporates —
its `expression_types` entry is recorded against the _inner_
expression, not the call site. Reify would otherwise emit a
spurious `TirExprKind::Call` that the elaborator never did.

- Field: tag the outer call's `AstId` with
  `DesugarKind::NewtypeFromCollapse` (the existing `desugars`
  map; the variant is new).
- Recording site: the collapse branch in the newtype-ctor call
  path records `record_desugar(call.id, NewtypeFromCollapse)`
  alongside its existing argument resolution.
- Reify consumer: `reify_call` checks `desugars` first; on
  `NewtypeFromCollapse` it emits the inner argument's TIR
  directly (the inner `expression_types` entry already names the
  right type) and skips the call construction entirely.

#### Gap 11: operator dispatch to a trait method

`Elaborator::build_binary_op_tir` (`operators.rs:126`+) and the
matching path for `IndexExpr` lower an operator to either a native
[`TirExprKind::Binary`] / [`TirExprKind::Index`] or to a
[`TirExprKind::MethodCall`] against the operator trait
(`Add::add`, `Eq::eq`, `Index::index`, …) — the decision is made
by the receiver type, not the AST. The method-dispatch branch
constructs the [`TirExprKind::MethodCall`] through
[`Elaborator::build_tir_method_call`] with a hand-built
[`crate::tir::FunctionRef`] rather than routing through
`resolve_method_call_with`, so no
`TypeAnnotations::method_dispatch` entry is left under the AST id
of the [`crate::ast::BinaryExpr`] / [`crate::ast::IndexExpr`].
Reify cannot tell native vs. method dispatch apart without an
annotation.

- Field: `TypeAnnotations::operator_dispatch:
  IndexMap<AstId, OperatorDispatch>`, with
  ```rust
  pub(crate) struct OperatorDispatch {
      pub(crate) function_ref: FunctionRef,
      pub(crate) self_kind: ast::SelfKind,
      // Per-argument flag: `true` when the operator's trait parameter is
      // declared as `&T` / `&mut T` and reify must wrap the argument
      // in a `Unary { Ref }` / `Unary { MutRef }` before passing it.
      // Indexed in the order the elaborator's argument-walk produces
      // (LHS-first for binary; the lone index for `IndexExpr`).
      pub(crate) arg_ref_wraps: Vec<bool>,
      pub(crate) return_type: TypeId,
  }
  ```
- Recording site: `Elaborator::build_trait_op_method_call_on_resolved`
  (`operators.rs:1446`+) and the IndexExpr operator-dispatch path.
  Each call site already computes the inputs above
  (`resolved.self_kind`, the `wrap_flags` vector, `resolved.return_type`,
  `FunctionRef` from `ResolvedTraitMethod`) — the recording is one
  call at the top of the helper, just before
  [`Elaborator::build_tir_method_call`].
- Reify consumer: `reify_expr` for [`ast::Expr::Binary`] /
  [`ast::Expr::Index`] checks `operator_dispatch[id]` first; on hit
  it emits the same `MethodCall` TIR (sharing the receiver-adjustment
  and arg-wrap helpers with `reify_method_call`); on miss it emits
  the native [`TirExprKind::Binary`] / [`TirExprKind::Index`].
- Why not reuse `method_dispatch`: `MethodDispatch` carries an
  `is_ref_impl: bool` flag that is meaningful only for receiver-
  adjustment off a real method-call receiver. Operator dispatch
  uses `is_ref_impl = false` and additionally needs per-argument
  ref-wrap flags that `MethodDispatch` does not carry. Splitting
  into `operator_dispatch` keeps each map's invariants clean.

#### Gap 10: stmt-position match and other dispatch shortcuts

`resolve_stmt` dispatches a stmt-position `Expr::Match` directly
to `resolve_match_expr` (documented in Stage 4 §`Recording sits
at the choke point`); the stmt arm records `expression_types`
explicitly. Reify mirrors the same dispatch: `reify_stmt` on a
stmt-position match calls `reify_match_expr` and wraps the
result in `TirStmtKind::Expr`. No new field; the design contract
is the dispatch parity itself, called out here so a future
refactor doesn't reintroduce the "stmt-position match has no
expr-position twin" asymmetry.

#### Synthetic call sites stay annotation-free by design

Three call shapes evaporate during annotate before a
`MethodCallExpr` is ever resolved, and so leave no
`expression_types` / `method_dispatch` entry. Reify re-detects
each from the AST shape and the receiver type:

- `.enumerate()` inside a for-of head — unwrapped at
  `stmt.rs:2130–2135`. Reify reads `for_of.iterable` directly.
- `tuple.len()` / `tuple.zip(...)` — short-circuited at the
  receiver-type level in `method_call.rs`. Reify recognises
  tuple-typed receivers and emits `TirExprKind::TupleLen` /
  `TirExprKind::TupleZip`.
- Static-method-as-instance error (`T::method(x)` written with
  instance syntax) — the elaborator emits a diagnostic and the
  call's `expression_types` resolves to `ERROR`; reify reads
  the absence of an entry as "the call failed, drop the
  enclosing TIR construction."

These are not gaps in Stage 5; they are the dual of the
synthetic-call recording contract on `MethodDispatch` from
Stage 4. Listed here because every "reify needs to know X"
review question circles back to one of them.

#### Reify pipeline structure

`reify_module(module: &Module, tysys: &mut TypeSystem,
sem: &ModuleSemantics, symbols: &SymbolTable, …) -> TirModule`
mirrors `Elaborator::resolve_module` in dispatch shape:

- The per-Item loop pattern-matches on `Item::*` exactly as
  `resolve_module` does. Decl-only items (`Enum`, `Flags`,
  `Newtype`, `Variant`, `Effect`, `Resource`, `Struct`) dispatch
  into `reify_enum_decl` / `reify_struct` / `reify_variant_decl`
  / `reify_effect_decl` / `reify_resource_decl`. Each reads
  decl-interned types from `TypeSystem.all_*` and produces TIR
  without consulting `TypeAnnotations`.
- Function / impl-method / test / global bodies dispatch into
  `reify_function` / `reify_method` / `reify_test_decl` /
  `reify_global`. Each builds a fresh `FunctionContext` and
  walks the AST via `reify_block` / `reify_stmt` / `reify_expr`
  / `reify_pattern`.
- `reify_expr` consults `ModuleSemantics.types`:
  - `expression_types[id]` → `TirExpr::type_id`
  - `method_dispatch[id]` → dispatch target + `self_kind` +
    `is_ref_impl` (Gap 2)
  - `coercions[id]` → coercion wrapper to emit around the raw
    expression
  - `desugars[id]` → which expansion path to take (assert /
    matches / for-of / while / compound-assign / comparison
    chain / IndexMut method call / newtype-from collapse)
  - `generic_instantiations[id]` → `type_args` for
    call / struct / variant constructions
  - `closure_captures[id]` → closure capture list and
    `__ref_*` materialisation
  - `assert_captures[id]` → assert slot map
  - `for_of_iterator[id]` → for-of iterator dispatch target

Reify never re-runs inference, never looks at
`TypeSystem.trait_env`'s impl tables, and never mutates
`TypeTable` except to intern new monomorphic instances that
arise during reify itself (e.g. a mangled `Container<i32>`
first reached here). The Decision §`Reify surface` constraint
holds: "Monomorphic instances created during reify intern
through `&mut TypeSystem`."

#### `TypeSystem` ownership across the two passes

`Elaborator::annotate_bodies` takes `&mut TypeSystem` because it
interns new types during inference. `reify_module` takes
`&mut TypeSystem` too — not because reify re-runs inference, but
because reify still needs to intern monomorphic struct/variant
instances that the post-substitution paths reach for the first
time (see the `make_generic_instance` calls in `resolve_call`
and `resolve_struct_literal` that the recorded
`generic_instantiations.instance_type` already covers — but the
mangled-name registry on the `TypeSystem` may still need new
entries when the body walk uses a less-specific type and reify
crystallises a more-specific one). The `Decision §Reify surface`
note already calls this out; the contract is sharpened to "reify
may intern but not query trait/impl tables for resolution
decisions."

#### Equivalence validation

The WIR golden fixtures and E2E suite carry the full equivalence
guarantee, as the WEP's §`Migration Plan` Stage 5 entry already
states. Stage 5 adds one targeted developer-only assertion that
makes regressions easier to diagnose without spending the cost
on every compile:

- A `#[cfg(test)]` helper in `wado-compiler/tests/` that, for
  selected fixtures, runs both `annotate_bodies → reify` and
  the legacy combined walk (kept under a feature flag during
  the migration window) and asserts the resulting
  `TirModule`s compare equal under a structural eq that
  ignores `Span`. The helper retires the moment `optimize/dce`
  retires from its current role (Stage 6); the WIR golden
  fixtures + E2E suite remain the long-term contract.

#### Out of scope for Stage 5

- The full removal of the combined walk. That is Stage 7
  cleanup, gated on Stage 6's liveness pass.
- Recording the trait-impl-selection rationale (which blanket
  impl won, which bound check succeeded). Reify reads the
  recorded `FunctionRef` and `is_ref_impl` flag and trusts
  them; the rationale survives only as the absence of
  ambiguity at the recorded dispatch target.
- Performance optimisation of the two-walk pipeline. The
  trade-off is taken as decided in the WEP's §`Trade-offs`.

## Consequences

### Benefits

- **The God Object is gone.** Every new field has a sub-struct it
  belongs to, and the membership criterion is mechanical.
- **`annotate` actually annotates.** The phase name matches its
  work, and `Semantics` is genuinely complete after `annotate`
  returns. The LSP no longer pays for thrown-away TIR.
- **`reify` is mechanical.** It can be tested in isolation against
  hand-built `ModuleSemantics` fixtures, and the type boundary
  ensures it cannot reach for inference state.
- **DCE has a place to live.** The `liveness` pass slots cleanly
  between `annotate` and `reify`, gives the LSP its unused
  diagnostics, and lets `reify` shrink its output before
  monomorphization sees it.
- **`Rc<RefCell<…>>` retreats to where it is genuinely needed.**
  The `TypeTable` remains shared (anonymous structs and
  monomorphic instances intern through both `annotate` and
  `reify`); the `references` / `local_symbols` plumbing simplifies
  to `&mut ModuleSemantics`.
- **Plain Rust ownership.** Borrow-check pressure now reflects the
  conceptual model rather than working around it.

### Trade-offs

- **Batch compilation walks bodies twice.** Once in
  `annotate_bodies` (the heavy walk that does inference,
  resolution, and dispatch) and once in `reify` (the mechanical
  walk that reads the annotations). Performance is not optimised
  during the migration; if a workload regresses unacceptably, the
  remedy is to specialise `reify` over the recorded annotations,
  not to merge the phases back. The clean phase boundary is
  the load-bearing decision.
- **Bigger up-front design surface.** The four sub-structs and
  their membership rules need to be respected when adding fields.
  This is a feature, not a bug — the membership rule replaces
  the current absence of any rule.
- **Migration is long.** The change touches roughly the entire
  `elaborator/` tree. Stages 4 and 5 are the riskiest because
  they restructure the body walk; the WIR golden fixtures and
  E2E suite carry the equivalence guarantee.

### Risks and mitigations

- **`ModuleSemantics` becomes another God Object.** Mitigation:
  the four sub-structs are non-optional, and every field added
  during or after migration must justify its sub-struct. Reviews
  reject "just put it on `ModuleSemantics` directly."
- **The `annotate` / `reify` split leaks during migration.**
  Mitigation: stage 4 introduces the annotation storage _before_
  stage 5 splits the walk. Any annotation reify needs but
  `annotate` does not record is a stage-4 omission, surfaced as a
  panic in stage 5.
- **DCE breaks a previously-reachable item.** Mitigation: the
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
