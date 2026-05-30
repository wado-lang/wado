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
      Recording half complete: every Gap (1–6, 9, 11, 12, 13)
      is wired end-to-end. Orchestration scaffold landed —
      `WADO_REIFY=1` env-var opt-in routes
      `build_tir_from_state` through `Reify::reify_module` after
      the existing `resolve_module` walk populates
      `ModuleSemantics`. Reify body walk covers every decl, every
      `Stmt`/`Pattern`/`Literal`/`Ident` variant, every `Expr`
      variant (full Call dispatch, Index, Closure with Gap 4
      capture replay, TryOp, ComparisonChain, CompoundAssign,
      Range, TemplateString, StructLiteral named + anonymous,
      Matches, Resume, LabeledBlock, Spread, impl-block via
      Gap 12, WithHandler via Gap 13, power-assert template
      reconstruction via `ReifyAssertCaptureContext`,
      default-argument padding for free + method calls,
      variant-ctor before `static_method_dispatch`,
      closure-block return inference, `ComparisonChain`
      operator-trait dispatch with Ord wrap, `Self`
      substitution in impl methods). Stdlib bypass keeps
      `Core` / `Wasi` / `Wasm` modules and snapshot construction
      on the production path. Under these annotations the E2E
      suite reaches **2664 / 2678 fixtures passing under
      `WADO_REIFY=1`** at `-O0` + `-O2` (production is 2678/2678,
      so the 7 remaining unique failures are all reify-specific;
      count as of the 2026-05-30 `origin/main` merge). The
      second-half session lifted the count from 1765 via the
      landings below — see `### Stage 5 second-half progress` for
      what changed and the re-triaged remaining clusters.
      See `### Stage 5 handover` below for the original remaining
      work and the gotchas encountered to date.
- [ ] **Stage 6 — Liveness and DCE.**
- [ ] **Stage 7 — Cleanup.**

### Stage 5 second-half progress

Second-half session, starting from the 1765/2664 baseline. All
landings keep production at 2664/2664 (verified by full e2e runs);
every remaining failure is reify-specific.

Landed:

1. **i128/u128 literal coercion + cast replay.** reify never
   consumed the per-`AstId` `coercions` map, so 128-bit literals
   (prelude structs built via `from_u64` / `from_i64` /
   `from_pair`) reached codegen as bare literals and ICE'd. The
   construction is extracted into pure helpers in `coercion.rs`
   (`build_int128_literal_call`, `build_int128_from_pair`,
   `build_int128_from_intermediate`) shared by the elaborator and
   reify; reify replays the recorded `NumericLiteral` coercion and
   the `expr as i128/u128` cast.
2. **FieldAccess type from the struct decl.** reify now types a
   field access from the receiver's struct decl (with generic
   type-arg substitution) rather than `expression_types[field.id]`,
   which collides across template sub-parsers (gotcha #1).
3. **Template interpolation `AstId` collision (parser, root fix).**
   Interpolation sub-expressions were parsed with a fresh `Parser`
   restarting at `AstId(0)`, so multiple interpolations clobbered
   each other's entries in every per-`AstId` map. The sub-parser
   now continues the parent's dense `AstId` space. This single
   change was the largest lift (~+100 fixtures) — it fixes
   mis-typed field accesses and mis-dispatched calls in any
   multi-interpolation template.
4. **Comparison operator-trait wrapping.** reify reproduced only
   the inner `Eq::eq` / `Ord::cmp` dispatch; the source operator's
   wrapping (`!=` → `!eq`, `<` / `>` / `<=` / `>=` → `cmp == Ordering::X`)
   was missing. `ord_bool_from_cmp` is extracted into a shared free
   function and the op-driven wrap applied in `reify_binary`.
   `RefEq` / `RefNotEq` for reference-operand equality is also
   reproduced from operand types.
5. **`Self::AssocType` in impl-method signatures.** reify resolved
   bare `Self` but not the associated-type projection `Self::Output`,
   so `&Self::Output` reified as `&unknown`, breaking the
   call→definition link at WIR build (`[WIR] unresolved`
   `ReadOnlyBox^Index<i32>::index`). reify now resolves
   `Self::AssocType` against the impl's recorded
   `ImplFacts.assoc_type_bindings`. Fixes all `index_trait` fixtures.
6. **Associated-constant patterns and const range bounds.** A nullary
   qualified pattern (`TokenKind::FOO`, `i32::MIN`) was lowered as a
   variant case; it now resolves to the constant's value — user consts
   via `sem.decls.associated_constants`, builtin primitive consts
   (`i32::MIN`/`u8::MAX`/…) via the shared `primitive_assoc_const_to_i128`
   (extracted from `stmt.rs`). Range endpoints resolve const bounds too.
7. **Bare-ident / enum-case pattern disambiguation.** reify_pattern's
   `Ident` arm always produced a binding, so a bare nullary case
   (`None`, `Red`) became a catch-all and an immutable-global pattern
   bound instead of comparing; the `Variant` arm never produced
   `TirPattern::Enum`. Now mirrors `resolve_if_pattern_inner`: known
   enum case → `Enum`, known variant case → nullary `Variant`,
   immutable global → `ConstantValue`, else binding. Largest pattern
   lift (match cluster 166 → 218 passing).
8. **Variant constructor in turbofish form.** `Option::<String>::Some(x)`
   parses as a `StaticMethodCall` whose target is a variant; reify
   emitted an unresolved static call. `reify_static_method_call` now
   detects the variant-case target and emits `VariantConstruct`.
9. **Omitted struct-literal field defaults.** `Config { host }` with
   `port: i32 = 8080` left defaulted fields unset (invalid wasm). reify
   now synthesizes each omitted field's default (generic-substituted
   type) and sorts fields by declaration index, mirroring
   `resolve_struct_literal`.
10. **Impl type params rebuilt from the AST self type.** Generic impls
    whose type params are implicit in the self type
    (`impl … for TagMap<Tag, V>`) reified methods with the param
    resolved to `unknown`. `reify_method` now rebuilds `impl_type_params`
    from the self type, mirroring the elaborator's method emission
    (`item.rs:1370-1462`): each `Named` self-type arg is a positional
    impl type param so monomorph's positional substitution against the
    recorded `type_arg_ids` lines up. Cleared the `from_literal`,
    `index_value`, `sequence_literal`, and `template_string_generic`
    builder fixtures (zero net regressions on a full run).
11. **Async task-return type carried for store inference.** An async
    function's wasm return type is erased to `()`; the recorded
    `function_return_types` held that erased unit, so reify's
    `task_return_type` was unit and the resource-store inference in
    `effect_check` could not see resources in the declared return
    (`missing resource 'Response' required by '__cm_binding__Response_new'`
    on the `wasi:http/service` fixtures). annotate now records the
    declared return on `function_task_returns`; reify reads it.
12. **Static-method dispatch reuse + recorded-type variant ctors.**
    reify rebuilt every `StaticMethodCallExpr` from scratch, losing the
    mangled name / `cm_name` / monomorph info and collapsing turbofish
    targets (`Future::<T>::new`, `Result::<…>::Ok`) to an empty struct
    name (`::new` / `::Ok` unresolved). annotate now records the
    resolved `FunctionRef` on `static_method_dispatch` keyed by the
    static-call AstId and reify reuses it; turbofish variant ctors take
    their variant name + instance type from the call's recorded
    expression type. This was the largest single lift of the session
    (**2038 → 2334**): it cleared the entire CM / HTTP / stream cluster
    and every `Result` / `Option` / `Future` turbofish ctor and static
    call across the suite. Zero net regressions on a full run.
13. **fn_ref: `&fn` callees, generic FuncRef args, address-taken locals.**
    Three reify gaps in the fn-reference cluster (**2334 → 2371**, fn_ref
    e2e 26/34 → 34/34, zero net regressions):
    - A bare-ident or non-ident callee whose type is `&fn` / `&mut fn`
      now auto-derefs to the function value before `IndirectCall`,
      mirroring `build_indirect_call`'s final `deref_to_value`. Detection
      peels refs and the ultimate base type as `as_fn_signature` does.
    - Generic function references (`id::<i32>`,
      `let f: fn(bool)->bool = id`) reified as `FuncRef { type_args: [] }`,
      leaving the name unmangled after monomorphization and tripping the
      `lower::closure` "FuncRef should be wrapped in a Closure" invariant.
      `resolve_func_ref_ident` now records the inferred / turbofish args
      into `generic_instantiations` (which reify already reads).
    - `&x` / `&mut x` now mark the borrowed local address-taken in reify
      (`ctx.address_taken_locals`), so the boxing pass retags the local
      to its box type and mutation through a `&mut fn` out-param
      (`*slot = other_fn`) writes back to the slot instead of a throwaway
      box.
14. **Generic variant / enum types in the static type resolver (P0).**
    `resolve_type_static_with_params` resolved a generic application
    `Name<args…>` only when `Name` was `Option` or a struct; a generic
    variant (`Result<T, E>`) or generic enum fell through to `UNKNOWN`.
    The `Type::Named` arm already checked struct / variant / enum, so the
    generic arm now does too (`make_generic_instance` is name-based).
    This bit reify because `reify_method` re-resolves an impl method's
    declared return type from the AST (unlike `reify_function`, which
    reads the recorded `function_return_types`): a
    `-> Result<(), SerializeError>` signature resolved to `unknown`, so
    the monomorphized instance "still contained a type param" and
    `wir_build::register_methods` silently skipped it, leaving the call
    unresolved at WIR build. Clears serde_serialize_struct / enum / trait.
15. **Type-param scope for body turbofish + match-ergonomics ref
    bindings (serde 28→12, match 27→17, variant 29→13, if_let 6→0).**
    Two coupled gaps in generic trait methods that match on `&self`:
    - Turbofish args in a body (`v.serialize::<S>(s)`) were resolved with
      no type-param scope, so an enclosing param `S` became `unknown` and
      the call read `i32^Serialize::serialize<unknown>`. reify now
      publishes the body's scope (`current_type_param_names`, set in
      `reify_method` / `reify_function`) and `resolve_type` consults it.
    - Variant-pattern payload bindings ignored match ergonomics: matching
      `Some(v)` on `&Option<T>` must bind `v: &T` (forwarding directly to
      a `&self` method), with the `enum_type` / `payload_type` carrying
      the _peeled_ variant. reify previously bound `v: T` (boxing a
      throwaway copy → wrong value) and kept the `&Option<T>` ref as
      `enum_type` (extraction through a ref → null-reference trap). Now
      peels the scrutinee for decl / enum_type / payload and re-wraps only
      the binding in the scrutinee's reference kind, mirroring
      `resolve_if_pattern`'s `RefBinding`. Also fixed result_match /
      result_match_payload / result_if_let_mismatch /
      default_field_variant_payload_ref_match.
16. **Enum-case / nullary-variant ref-peeling (2452 → 2458).** Extends
    the match-ergonomics ref handling to `match &c { Red => … }` (enum
    case on `&Color`) and `if let None = rn` (nullary on `&Option<T>`):
    `scrutinee_enum_case_index`, `scrutinee_has_variant_case`,
    `reify_nullary_variant_case`, and both `TirPattern::Enum`
    constructions now peel references and store the peeled `enum_type`.
17. **Abstract `T::method()` static-call dispatch replay.** A call
    through an abstract type parameter (`T::read()`,
    `T::from_str_range()` inside `fn f<T: Bound>()`) is resolved by
    `resolve_type_param_static_call` into a `Call` whose `method_info`
    carries `is_type_param_receiver = true`, so monomorphization rewrites
    `T` to the concrete type at each instantiation. reify had no
    trait-bound context to reconstruct this and emitted a bare
    `name="T::read"` call that never resolved at WIR build. annotate now
    records the resolved `FunctionRef` on `static_method_dispatch` keyed
    by the CallExpr id (collision-free with StaticMethodCall ids), and
    reify's existing `static_method_dispatch` replay arm picks it up.
18. **Inferred method-level type args on MethodCall nodes.** The
    monomorphizer's instantiation-site collector keys off a `MethodCall`
    node's `type_args` to queue `Struct^Trait::method<Args>` instances.
    reify populated that field only from the syntactic turbofish, so a
    call whose method type params are inferred from argument types
    (`c.transform(42)` infers `T = i32`) reached WIR with empty
    `type_args`, no instance was generated, and the call was unresolved.
    reify now falls back to the inferred args the elaborator baked into
    the recorded `FunctionRef.monomorph_info.method_type_args`.
19. **`Self` / `Self::Assoc` nested in generic / tuple types.**
    `resolve_type_with_self` substituted bare `Self` and a top-level
    `Self::Assoc` projection (and through references), but a projection
    nested inside a generic application or tuple — `Option<Self::Item>`,
    `Result<T, Self::Error>`, `[Self::Item, bool]` — fell to the static
    resolver, which has no self/assoc context and produced `unknown`. An
    impl method `fn get(&self) -> Option<Self::Item>` therefore reified
    its return as `Option<unknown>`; the monomorphized instance "still
    contained a type param" and `wir_build::register_methods` skipped it,
    leaving `Box_<i32>^Container::get` (and `GenericMapIter<…>^Iterator::next`)
    unresolved. reify now rebuilds generic / tuple types from
    self-substituted argument ids when an argument mentions `Self`
    (self-free types keep the proven static path).
20. **String-literal escape decoding (2495 → 2523).** reify emitted
    `StringLiteral` from the raw AST source text, so a literal containing
    an escape (`\"`, `\n`, `\\`, …) reached codegen with the backslash
    intact — `"{\""` serialized as `{\"` instead of `{"`. Decodes via
    `util::unescape_string`, matching the elaborator (expr.rs:403). A
    single broad fix worth +28 fixtures (every reify'd escaped string
    literal across the suite).
21. **Namespace-imported variant ctor + struct static method (2523 →
    2530).** `use ns from "…"; ns::Type::Case(payload)` and
    `ns::Type::method(args)` parse as a `Call` whose ident has two `::`.
    `reify_call`'s top variant-ctor arm only handled the single-`::`
    shape, so the namespaced forms reached the recovery `ERROR`: the
    `let` binding lost its type and — for the static-method case — the
    call established no reachable edge into the namespace module, so
    monomorphization pruned the whole module (`Point::sum` unresolved at
    WIR). Added two arms: a `VariantConstruct` (case from
    `tysys.all_variant_cases[ns_source]`, instance type from the recorded
    expression type) and a static-method `Call` (gated on the type being
    a struct in `ns_source`). Earlier analysis blamed annotate; the real
    fix was reify reaching these `Call` shapes. namespace_import 8/8.
22. **Numeric-literal cast operand re-typed to target width (2530 →
    2539).** A numeric literal under `as Ty` adopts the target's width in
    production (`9007199254740992 as i64` types the literal i64). annotate
    propagates the target to a _direct_ literal cast operand but not
    through a unary `Neg`, so `-9007199254740992 as i64` reified the inner
    literal i32 — codegen's `i32.const` truncated the value
    (`2^53 mod 2^32 == 0`) to 0 before the cast widened. reify's Cast arm
    now reifies a `Number` literal operand (bare or under `Neg`) at the
    target type when the target is an integer. +9 fixtures
    (serde_json_large_int / scientific_notation and other wide-cast
    sites).
23. **Literal-pattern decoding (u128 signedness, char/string escapes,
    null→None).** reify's literal-pattern arm went through
    `ast_literal_to_pattern`, which ignored the scrutinee and always
    emitted `I128` for numerics, read `chars().next()` for chars (so
    `'\n'` → `'\'`), cloned the raw string, and never mapped `null` to a
    `None` case. A `u128` scrutinee compared via `i128::*` — codegen
    emitted the `(ref $u128)` vs `(ref $i128)` mismatch (which had been
    mis-triaged as a deep interning bug) and `parse::<i128>` of a value
    > i128::MAX truncated to 0. The arm now mirrors
    > `resolve_if_pattern_inner` (stmt.rs:1344); the char _expression_
    > literal (`reify_literal`) is fixed the same way (it shared the broken
    > decode). Clears match_literal, match_literal_i128_{guarded,large,
    > side_effect}.
24. **Or-pattern alternative bindings remapped to shared locals.**
    `Num(n) | Neg(n)` gave each alternative its own local for `n` — the
    matched alternative extracted the payload into one slot while the arm
    body read the other. reify now mirrors `resolve_if_pattern_inner`
    (stmt.rs:1798): remap later alternatives' binding locals onto the
    first's and point the arm scope at the first's locals (reusing the
    now-`pub(super)` `collect_pattern_bindings_with_index` /
    `remap_pattern_local`). Clears match_or_pattern_5 / _iflet_1.
25. **Match ergonomics for struct / tuple let-destructure through a ref.**
    `let { x, y } = &p` / `let [a, b] = &t` bind `&field` / `&elem`.
    reify's struct- and tuple-pattern arms read the decl / elements off
    the _unpeeled_ `&Point` / `&[…]` scrutinee, so they resolved nothing
    and the bindings typed `unknown`. Both arms now peel references for
    the lookup and wrap each binding in the scrutinee's reference kind
    via `apply_scrutinee_ref_kind`. Clears match_ergonomics_let_destructure
    (the match cluster is now clean).
26. **Generic resource types in the static resolver (2558 → 2566).**
    `resolve_type_static_with_params`'s `Type::Generic` arm checked
    struct / variant / enum but not resources, so a generic resource
    (`Stream<u8>`, `Future<T>`) resolved to `UNKNOWN`. reify uses this
    resolver for parameter / field types, so `fn consume(rx: Stream<u8>)`
    lost the resource — the effect-check resource-store inference
    (`signature_resources`) saw no `Stream`, and the body's
    `Stream::<u8>::new()` failed with `missing resource 'Stream'`. The arm
    now resolves a generic resource to
    `GenericResource { name, module_source, type_args }` via
    `lookup.resource_type`. +8 fixtures (effect_propagation_signature_*,
    variant_payload, and other generic-resource signature sites). The
    struct-field / nested members (`effect_propagation_struct_field`,
    `_signature_nested`) needed the resolver unification in #27.
27. **Unify `resolve_type_static` onto the type-param-aware resolver
    (2566 → 2570).** `resolve_type_static` and
    `resolve_type_static_with_params` had drifted: the former's
    `Type::Generic` arm handled only `Option` / generic structs, so a
    struct field or effect-op signature typed `Stream<u8>` /
    `Result<T, E>` resolved to `UNKNOWN` (landings #14/#26 fixed only the
    `_with_params` arm). A `Holder { stream: Stream<u8> }` field lost the
    resource, so `build_propagation_closure` did not admit `Stream`
    through an interface op returning `Holder`, and `with MyIO` callers
    failed with `missing resource 'Stream'`. `resolve_type_static` now
    delegates to `resolve_type_static_with_params(…, &[])`. Clears
    effect_propagation_struct_field / _signature_nested; production
    unaffected.
28. **Numeric-literal parsing via shared `util` helpers (2570 → 2578).**
    reify hand-decoded numeric literals keyed on the _target_ type: a
    float target ran `parse::<f64>()` (failing on `0xFF` / `0b1010` →
    0.0), an int target parsed digits directly (failing on scientific
    `1e2` → 0), and neither stripped digit separators (`0x1234_5678` →
    0). Now uses `util::parse_u128_literal` / `util::parse_float_literal`
    keyed on `is_float_only_literal`, converting an integer-form literal
    to f64 for a float target — also applied to the pattern / range-bound
    decoders. Clears coerce_struct_field, number_literal.
29. **Struct field access resolved against the receiver's module.**
    `lookup_struct_field_index` looked the struct up by name only, so two
    same-named structs in different modules (local `Pair` vs imported
    `helper::Pair`) resolved fields against whichever the current module
    saw first — `remote.y` read the wrong field. Now keys on the
    receiver's `(module_source, name)` via `tysys.all_struct_fields`.
    Clears struct_name_conflict.
30. **Realise fn-bound type params to their function type (2579 → 2589).**
    A `<F: fn(...)>` bound is realised eagerly to the bound's function
    type (item.rs:1569), not a `TypeParam` slot. reify treated `F` as a
    regular type param, so a param `f: F` reified to `TypeParam(F)` and
    reached codegen unsubstituted ("unsubstituted TypeParam F"); including
    fn-bound params in the positional scope also shifted the _real_
    params' indices, mis-substituting them. reify_function and
    reify_method now build the type-param scope from real params only
    (dense indices matching the emitted `type_params`) and resolve a
    param/return naming a fn-bound param to the bound's resolved function
    type (`resolve_type_with_fn_bounds`; method bounds resolve with Self /
    assoc bindings in scope). Clears closure_fn_bound_with_{effect_param,
    regular_type_param}, closure_bound_with_multi_effect,
    closure_generic_bound_fn, closure_method_fn_bound.
31. **Carry the impl's trait type args onto `method_info` (2589 → 2595).**
    The effect-dispatch synthesis keys its handler index on
    `(struct, effect_module, base_trait, trait_type_args)` read off each
    method function's `method_info`. reify left `trait_type_args` empty,
    so a generic-effect handler (`impl Future<i32> for MockFuture`) was
    keyed `Future<>` while the `with &mut f do` binding referenced
    `Future<i32>` — no `DispatchPlan` matched and synthesis panicked.
    annotate now records the resolved trait type args on
    `ImplFacts.trait_type_args` (mirroring item.rs:1621) and reify_method
    writes them onto `LocalMethodName.trait_type_args`. Clears
    effect_handler_resource_{future,stream,stream_self_delegation}.
32. **Peel newtypes for the float-literal decision (2595 → 2605).**
    reify_literal chose float vs integer by comparing the recorded type
    to F32/F64 exactly, so a float literal bound to a float _newtype_
    target (`type Meters = f64; let m: Meters = 1000.0`) failed the check,
    took the integer path, and reached codegen as `i32` where `f64` was
    expected. Peel to the ultimate base type before the check. Clears
    newtype_basic, newtype_float_types, newtype_impl,
    newtype_option_pattern (float cluster now clean).
33. **Resolve generic newtypes in the static resolver (2605 → 2607).**
    `resolve_type_static_with_params`'s `Type::Generic` arm did not handle
    a generic newtype (`type MyArray<T> = Array<T>`), so `MyArray<i32>`
    resolved to `UNKNOWN`; reify uses this resolver for let / param types,
    so the newtype's inherited base methods (`arr.len()` →
    `Array<i32>::len`) never resolved and monomorphization couldn't reach
    them. The arm now substitutes the generic newtype's base AST with the
    instantiation's args and builds a `Newtype` over the resolved base
    (mirroring type_resolution.rs:418). Fixes newtype_generic; production
    unaffected.
34. **Replay unary operator-trait dispatch in reify (2607 → 2608).**
    `resolve_unary`'s `Neg` / `BitNot` trait-dispatch path recorded no
    `operator_dispatch` entry, so reify re-emitted a bare `Unary` on a
    struct operand (`-p1` where `p1: Vec2`), which codegen rejects
    (`expected i32, found (ref $T)`). `resolve_unary` now sets
    `pending_operator_ast_id` before `build_trait_op_method_call_on_resolved`
    (mirroring the binary path), and reify's `Unary` arm replays the
    recorded `Neg::neg` / `BitNot::bitnot` method call. Fixes
    newtype_operator_trait; production unaffected.
35. **Forward per-argument expected types for unannotated-param closure
    args (2608 → 2609).** Reify resolved every call argument with
    `expected = None`, so a closure literal passed where the param is a
    `fn`-newtype (`type Reducer = fn(i32, i32) -> i32`) never saw the
    function signature: its unannotated params (`|a, b| ...`) stayed
    UNKNOWN, its functor `__call` lowered with `unknown` param types, and
    the method was dropped before codegen (`fold$__Closure_4` referenced a
    `__Closure_4::__call` that no longer existed). The elaborator now
    records the resolved (type-arg-substituted) param types per call site
    (`sem.types.call_param_types`, keyed by the call `AstId`); reify reads
    them and forwards the matching param type as the expected type, but
    only for closure args that have an unannotated param
    (`arg_is_unannotated_closure`). The restriction keeps effect-
    polymorphic closures (`each(items, |x: i32| { println(...) })` against
    `fn mut(i32) with E`) inferring their effects from the body instead of
    pinning `declared_effects` to the generic effect param. reify_closure
    also peels newtypes before reading the expected fn signature. Fixes
    newtype_closure_coercion; production unaffected.
36. **Replay the production `Call`'s exact type args for static dispatch
    (2609 → 2610).** Reify rebuilt a static / free call's `type_args` from
    `generic_instantiations`, the flat impl+method type-arg list. For a
    static method on a generic struct (`Container::make()` with `let c:
    Container<i32> = …`) the impl arg `i32` rides in
    `function_ref.monomorph_info`, so feeding it back as a method-level
    `type_arg` mangled the call as `Container::make<i32>` while monomorphize
    emitted `Container<i32>::make` — the instance was never found
    (`unresolved Call: Container::make<i32>`). `StaticMethodDispatch` now
    carries the exact `type_args` the production builder put on the `Call`
    (recorded at all four dispatch sites); reify replays it verbatim. Fixes
    infer_static_method_from_lhs; production unaffected.
37. **Coerce `break label: value` against the block's result type in reify
    (2610 → 2611).** Reify resolved every `break label: value` with
    `expected = None`, so a `break label: null` whose `Option<T>` is only
    pinned by a sibling break (`break label: Option::Some(5)`) reached WIR
    as a nullref and failed validation (`expected (ref $type), found
    nullref`). The labeled-block frame now falls back to the block's
    unified result type (`recorded_type`) when the use site supplies no
    expected type, and the `break` stmt resolves its value against the
    matching frame's expected type. Fixes labeled_block_break_null_coercion;
    production unaffected.
38. **ASCII-only test export-name mangling in reify (2611 → 2612).** Reify
    built a test's internal function name with a Unicode-aware
    `is_alphanumeric`, letting multibyte letters (`日`, `é`) survive into the
    Component Model kebab-case export name and crash Wasm validation
    (`export name test-0-日本語のテスト-ok is not a valid extern name`).
    Reify now uses the shared `name::test_name_to_snake` (ASCII-only,
    matching item.rs). Fixes test_unicode_names; production unaffected (it
    already used the helper). ice_array_iter_into_iterator also dropped off
    the failing list (cleared by an earlier landing).
39. **Resolve `fn(...) with E` effects on reified type annotations (2612 →
    2613).** The shared static type resolver has no effect context and
    interned every `fn`-typed annotation with `effects: []`, so a
    `fn`-typed parameter lost its `with` clause and `check_effects` could
    not see that `f: fn() with Stdout` requires `Stdout` at an indirect
    call (`effect_indirect_call_error` compiled instead of reporting
    `missing effect 'Stdout'`). Reify now re-interns function-type
    annotations with their effects (`apply_function_type_effects`,
    covering bare and `&`/`&mut`-wrapped fn types), resolved through
    `reify_effects`. `reify_effects` and reify gained an effect-param scope
    (`current_effect_param_names`, set from `<effect E>` params in
    `reify_function`/`reify_method`) so a param effect resolves to
    `EffectRef::Param`, not a `Concrete` effect — without it effect-generic
    `fn(...) with E` params (closure_effect_generic, effect_1, …) would
    carry `Concrete { E }` and fail to unify with the recorded `Param { E }`
    declared effect. Fixes effect_indirect_call_error; production unaffected.
40. **Mark primitive `&mut self` method receivers address-taken in reify
    (2613 → 2614).** A mutating trait method on a primitive local
    (`x.bump()` where `fn bump(&mut self)` on `i32`) left `x` unchanged
    under reify: the value-copied primitive was never marked address-taken,
    so the boxing pass didn't box it and the mutation through `&mut self`
    was lost. Reify now mirrors `Elaborator::resolve_method_call_with`
    (method_call.rs:517) — when the dispatch is `&mut self` (non-ref-impl)
    on a non-reference primitive local, the receiver local is inserted into
    `address_taken_locals`. Fixes bug_store_load_forward_mut_method_receiver;
    production unaffected.
41. **Peel references and newtypes in reify's field-index lookup (2635 →
    2638).** `lookup_struct_field_index` (reify's field-access index resolver,
    reify.rs:7006) matched only `Struct` / `GenericInstance` / `Ref` / `MutRef`
    receivers and fell to a `(0, field_name, None)` fallback for everything
    else. A field access on a newtype receiver (`loc.y` where
    `loc: Location` and `type Location = Point`) hit the fallback and reified
    every field with `field_index = 0`. The text dumps stayed identical
    (`nir_unparse` prints `FieldAccess` by name and drops the index) and
    `-O0` masked it, but `nir/sroa` keys per-field scalar locals on
    `(local, field_index)` (sroa.rs:1163), so `.y` aliased onto the `.x`
    scalar local — `clone: (10, 20)` became `clone: (10, 10)` at `-O2`.
    `lookup_struct_field_index` now recurses through `Ref` / `MutRef` and a new
    `Newtype` arm (peeling to `base_type`), mirroring the elaborator's
    `lookup_field_type` newtype arm (expr.rs:1500); this also covers chained
    newtypes and `&Location`. Clears the newtype `-O2` cluster
    (newtype_return_type, newtype_chained_method, newtype_method_inheritance);
    production unaffected (the helper is reify-only).
42. **Build reify's tuple-literal type bottom-up from its elements (2638 →
    2648).** reify's `TupleLiteral` arm (reify.rs:2040) stamped the literal
    with the recorded outer `TypeId` while each element carried its own
    independently recorded type. The elaborator's `resolve_tuple_literal`
    instead builds the tuple type bottom-up via `make_tuple(elem_types)`
    (expr.rs:3896), so an outer tuple's element type is, by construction, the
    identical interned id as the inner element's own type. For a nested tuple
    (`nested: [Wrapper, [i32, i32]]`) the two ids print equal but interned
    distinctly, so `nir/sroa` decomposed `nested` into a field-1 scalar local
    typed with one tuple `TypeId` while the inner `nested.1.1` index read the
    other — tripping WIR validation at `-O2` (`expected (ref null $type),
    found i32`). The NIR text was byte-identical, so the divergence was
    invisible in dumps and masked at `-O0`. reify now builds the tuple's
    `TypeId` from its reified element types via `make_tuple`, mirroring
    production. Clears the entire `-O2` `TypeId`-identity cluster (tuple_1,
    tuple_literal_expected_type_in_branch, opt_container_sroa_tuple,
    tuple_name_collision, tuple_name_collision_2 — and the
    cross_module_same_name_fn / infer_lhs_overrides_literal / return_merged /
    wasm_name_conflict_generic_fn sites that shared the same structural-tuple
    interning gap); production unaffected (the arm is reify-only).
43. **Handle spread elements in reify tuple literals (2648 → 2649).** reify's
    `TupleLiteral` arm walked elements with `reify_expr`, which panics on a
    `Spread` (`bare Spread is invalid outside TupleLiteral`) — so any
    `[..rest, b]` / `[a, ..middle, b]` in a variadic function crashed reify
    before WIR build. Ported `resolve_tuple_literal`'s spread handling
    (expr.rs:3797) into a new `reify_tuple_literal`: a direct `TypePack`
    operand → `TypePackExpansion`, a tuple containing a pack → `TupleSpread`
    (monomorphize expands), a concrete tuple → inline `FieldAccess` per
    element (binding a non-trivial operand to a `__spread_N` temporary). Adds
    `type_contains_pack` mirroring expr.rs:3752. Clears variadic_3 and removes
    the spread panic for the rest of the cluster; production unaffected.
44. **Resolve variadic type-pack spreads in the static resolver (2649 →
    2656).** `resolve_type_static_with_params` (the host-agnostic resolver
    reify routes through) had no `Type::TypePackSpread` arm, so a `..T` spread
    fell to `_ => UNKNOWN`. A `[..T]` parameter therefore resolved its element
    to `unknown`, and a generic tuple method (`Tuple<..T>^Eq::eq`)
    monomorphized against `Tuple<unknown>` — never registering at WIR build.
    Added a `TypePackSpread` arm that resolves the pack to a `TypePack` keyed
    by the param's positional index (mirroring the instance resolver's
    `trait_ctx.type_params` lookup, type_resolution.rs:103); the now-exhaustive
    match drops the dead `_ => UNKNOWN` catch-all. Clears variadic_1,
    variadic_2, variadic_trait_bound. Production unaffected (full e2e stays
    2678/2678 — production resolves these spreads through its instance
    resolver, which already handled them).

45. **Mangle variadic-tuple impl methods as `Tuple` in reify (2656 → 2660).**
    `reify_method` derived a method's base struct name from
    `type_name(facts.self_type)` truncated at `<`. For an
    `impl<..T> Trait for [..T]` the self type renders in bracket notation
    (`[..T]`, no `<`), so the method registered as `[..T]^Trait::method`
    instead of the generic `Tuple^Trait::method` production emits (confirmed
    by instrumenting `monomorphize.rs`: reify registered
    `[..T]^Countable::count` vs production's `Tuple^Countable::count`). The
    call site replays annotate's production `FunctionRef`, whose
    `monomorph_info.generic_name` is `Tuple^…`, and the monomorphizer's
    tuple-variadic instantiation path is gated on
    `struct_name == TUPLE_TYPE_NAME` (func_inst.rs:888) — so neither found the
    `[..T]^…` template and WIR build panicked (`unresolved MethodCall:
    Tuple<i32,bool>^Trait::method`). reify_method now uses `TUPLE_TYPE_NAME`
    when the impl self type is a builtin tuple, matching production
    (item.rs:660). Clears variadic_for_of_generic_method and
    variadic_impl_method_type_param; production unaffected (reify_method is
    reify-only). A companion `monomorphize.rs` change keys `generic_functions`
    on `(module_source, name)` so same-named generic tuple methods across
    modules coexist.
46. **Resolve call defaults in the callee module scope in reify (2660 →
    2661).** `reify_pad_args_with_defaults` reified a synthesized
    default-argument expression against the caller's module context, so a
    default referencing a callee-module-private item
    (`paint(c = DEFAULT_VALUE)` with `DEFAULT_VALUE` private to the callee
    module) failed to resolve and reached WIR as "nothing on stack". Mirror
    production's `default_scope_module` (expr.rs:914) by swapping the
    module-context triple (`current_module_source` / `current_module_items` /
    `sem`) to the callee module around the default walk; threads the full
    per-module semantics map onto `Reify` as `all_module_semantics`. Clears
    default_arg_private_item.
47. **Record Index-vs-IndexValue deref explicitly for reify (2661 → 2663).**
    `reify_index` chose whether to wrap an index in an outer `Deref` by
    testing whether `OperatorDispatch.return_type` was a reference — `Index`
    returns `&Output`, `IndexValue` returns `Output`. That misfires when an
    `IndexValue`'s `Output` is itself a reference (`Array<&i32>::index_value`
    → `&i32`): reify saw a `Ref` return and double-dereffed, lowering `*arr[0]`
    to `**arr.index_value(0)` and tripping WIR validation
    (`expected (ref null $type), found i32`). `OperatorDispatch` gains an
    explicit `needs_deref` flag (true only at the `Index` site); reify reads
    it. Clears trait_bound_1. Production unaffected (only reify consumes the
    flag).

#### Re-triaged remaining clusters

Largest first, grouped by cluster. **2664 / 2678** passing under
`WADO_REIFY=1` (fresh full-suite scan on 2026-05-30, after merging
`origin/main`; every fixture `main` added passes under reify). **7 unique
fixtures fail**, all at `-O0` and `-O2`. The localized "missing annotation
channel" gaps are gone; the `-O2`-only `TypeId`-identity cluster (landings
#41 / #42) and the entire variadic type-pack cluster (landings #43–#45) are
cleared; each remaining cluster is feature-level work, not a one-line replay
fix.

##### Variadic type-pack machinery — CLEARED (landings #43–#45)

The six variadic fixtures all pass under reify now. Three coupled gaps were
closed: spread elements in reify tuple literals (#43), `Type::TypePackSpread`
resolution in the static type resolver (#44), and — for the
`impl<..T> Trait for [..T]` shape — mangling the method's base struct name as
`Tuple` rather than the bracket-rendered `[..T]` (#45). The last bit landed
alongside a `monomorphize.rs` change that keys `generic_functions` on
`(module_source, name)` so same-named generic tuple methods from different
modules no longer collide.

##### Compile-time tuple enumeration (2, fail O0+O2)

- `tuple_zip` — reify emits `TupleZip` but monomorphize never expands it
  (panic at `lower/translate.rs:1135`, "should be expanded by monomorphize");
  the receiver tuple-of-tuples type is not in the shape the expansion keys
  on.
- `tuple_for_of` — for-of over a heterogeneous tuple emits invalid Wasm in
  the **test world** only (clean as a CLI program).

##### Remaining singles (5, fail O0+O2)

- `effect_handler_with_do` — codegen-level effect-handler residual.
- `opt_sroa_variant_return_if_descent` — optimizer pass divergence.
- `if_merged` / `for_merged` / `loop_nested` — large multi-feature files;
  `if_merged` traps at runtime, the other two fail test-world assertions.

##### Where to start next

The remaining 11 are all feature-level, not `TypeId`-identity replay fixes.
The two variadic fixtures and the `tuple_zip` / `tuple_for_of` pair all need
monomorphizer-side expansion machinery reproduced in reify (variadic-tuple
impl-method instantiation collection; `TupleZip` / heterogeneous-tuple for-of
enumeration). The remaining singles are independent feature-level gaps. The
`#### Recipe for adding a new reify gap`, `#### Gotchas`, and `#### Endgame`
below still apply unchanged.

### Stage 5 handover

The recording half is complete: every landed gap has an annotation channel
on `sem.types` and reify consumes it. What remains is per-shape parity for
the 7 fixtures above (compile-time tuple enumeration and the remaining
feature-level singles — the `-O2` `TypeId`-identity and variadic type-pack
clusters are cleared), then the final orchestration cut
(`#### Endgame`).

#### Gotchas seen in this session — read before continuing

These are non-obvious traps that cost the previous worker
multiple build cycles each. Every one of them was
production-correct by accident (single-pass walk hid the
underlying issue); reify's two-phase split exposes them.

1. **`expression_types[ast_id]` is unreliable for repeated
   parsed sub-expressions.** Template-string interpolation
   parses each `{expr}` through a fresh sub-`Parser`
   (parser.rs:5175) whose `next_ast_id` restarts at 0, so two
   interpolations like `{g} and n1={n1}` collide on
   `AstId(0)` and the second resolution overwrites the
   first. Always prefer the authoritative storage (local /
   capture / decl) over `expression_types` when one exists.
   Any future sub-parser will hit the same trap. Fixed for
   `reify_ident`'s Local / Capture arms in `cacf2901` —
   audit other `expression_types.get` sites if you see
   "wrong type after second occurrence" symptoms.
2. **Production sometimes `drain`s `sem.decls` collections
   during its body walk.** Reify reads `sem` after that walk,
   so anything drained is gone.
   `pending_anonymous_structs` was the example
   (dcea64f7) — production's
   `Elaborator::resolve_module` now clones instead. Audit
   `pending_*` fields on `ModuleDecls` whenever you find
   reify silently dropping decl-like state.
3. **`LetStmt.is_mut` lives on the stmt, not just the
   pattern.** `let mut x = …;` parses to `LetStmt { is_mut:
   true, pattern: Pattern::Ident(...) }`, not
   `Pattern::MutIdent`. The reify arm previously hardcoded
   `is_mut: false` for the `Ident` pattern. Production never
   hit it because its single walk re-uses the per-pattern
   resolver. The bug was silent at `-O2` (optimizer
   propagated through) and only fired at `-O0` when `&mut x`
   borrows hit wasm validation.
4. **Block-body closures need
   `Elaborator::find_return_type_in_block`.** `|| { return
   "hello"; }` has a body whose tail `type_id` is `NEVER` /
   `UNIT`; the closure's logical return is the returned
   value's type. Use the production helper (closure.rs:276+)
   verbatim — it knows about every divergent shape (`if cond
   { return … }` with no `else`, `match` arms, panic, …).
5. **`Self` doesn't resolve in reify type lookups.**
   Production's `resolve_named_type` consults
   `trait_ctx.self_type` (type_resolution.rs:240); reify
   has no such context. For impl-method param/return types,
   substitute `Self` against the recorded
   `ImplFacts.self_type` before delegating to
   `resolve_type_in_scope`. The current
   `resolve_type_with_self` covers bare `Self` and `&Self` /
   `&mut Self` — extend it (Self inside `Vec<Self>` etc.) if
   future fixtures require.
6. **Variant-constructor detection must run before
   `static_method_dispatch`.** Annotate records every
   call's `FunctionRef` on `static_method_dispatch`
   (call.rs:1146+), including variant ctors like
   `Option::Some(42)`. If reify's
   `static_method_dispatch` arm fires first, the variant ctor
   becomes a `Call` against a function that doesn't exist.
   Variant detection runs first as of `73a177bf`.
7. **`pending_operator_ast_id` side-channel is the only
   way to record operator-trait dispatch on a
   `ComparisonChain`.** Production sets it in
   `resolve_binary` for plain `BinaryExpr`, but
   `desugar_comparison_chain` originally didn't set it for
   the single-comparison path. Reify needs the
   dispatch entry keyed on `chain.id`, not `binary.id`.
   Wired in `c4ec298c`.
8. **Default-argument padding for methods needs
   `param_names` + `param_defaults` on
   `MethodDispatch`.** Free-function padding can lookup
   defaults from the function decl, but method defaults
   come from `MethodInfo` (types.rs:1083+) which reify
   doesn't compute. Carry them through `record_method_dispatch`.
9. **Power-assert needs a reify-specific capture context.**
   Production's `AssertCaptureContext` has private fields
   you can't reach from reify; the channels of the two
   walks shouldn't share state anyway. `ReifyAssertCaptureContext`
   (a separate field on `FunctionContext`) plus a hook at
   `reify_expr`'s top is the clean shape.
10. **Anon struct names re-derive at reify time can
    diverge from the registered name.** Annotate's name
    derivation uses the elaborator-resolved field types;
    reify's may use slightly different reified types (an
    evaporated coercion wrapper, a different cache hit).
    Read the registered `TypeId` from
    `expression_types[struct_lit.id]` and skip the
    re-derivation entirely.

#### Recipe for adding a new reify gap

The shape that has worked consistently:

1. Pick the smallest failing fixture in the category.
2. `WADO_REIFY=1 ./target/release/wado dump
   --tir-monomorphized fixture.wado` and the same without
   `WADO_REIFY=1`. Diff the two. The first divergent line is
   the cut point.
3. Find production's emitting site (the `elaborator/`
   helper that produced the production line). Read what
   state it consults — `trait_ctx`, `pending_*`, scoped
   `Option<…>` channels. Decide whether the state can be
   recorded as a per-`AstId` fact on `sem.types`.
4. Add the annotation struct in
   `elaborator/sem/types.rs`, the record call in the
   production site, and the consume in reify. Mirror an
   existing annotation pair (e.g. `MethodDispatch` or
   `OperatorDispatch`) for the field shape.
5. Run the fixture. If it still fails, dump again — the
   diff is the next gap.
6. Don't pre-derive in reify what annotate has already
   computed. Always prefer reading the recorded fact over
   re-running production logic.

#### Endgame

The dead-code removal (drop the production walk's TIR
emission half + flip `WADO_REIFY` default-on) is the final
cleanup once the residual gaps land and E2E confirms
equivalence. The orchestration scaffold is already in
place — `build_tir_from_state`'s reify branch (orchestration.rs:1136+)
is the single flip site.

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

#### Gap 12: impl-block resolution facts

`Elaborator::resolve_module`'s `Item::Impl` arm
(`elaborator.rs:1139–1430`) and the per-method
`Elaborator::resolve_method` (`item.rs:1331–1600`) together make
five categories of decision the body-walk reify cannot
re-derive without re-running impl-resolution logic — which the
WEP `Reify surface` forbids ("Reify performs no inference, name
resolution, or method dispatch"). The five decisions:

1. The impl's resolved `Self` type, with impl-block type
   parameters interned at the right `TypeParam` indices. For
   non-generic impls this is `Struct { name, module }`; for
   generic impls it's `GenericInstance { name, module, type_args }`
   where `type_args` are the impl's own `TypeParam` ids in
   declaration order. Reify reads this to synthesise `&self` /
   `&mut self` parameter types via `make_ref` / `make_mut_ref`.
2. The trait reference's canonical key `(declaring_module,
   base_trait_name)` and the mangled full name (e.g.
   `Stream<u8>`). The canonical key disambiguates two modules'
   same-named traits in `LocalMethodName::base_trait_module`;
   the mangled name lives on `LocalMethodName::trait_name`.
   Annotate already computes both via
   `Elaborator::canonical_decl_key` + `get_type_name_full`;
   recording them avoids duplicating either helper inside reify.
3. The impl-block's `TirTypeParam` projection (skipping
   concrete-typed positions like `impl<i32, T>`), in
   declaration order. Reify writes this into every method's
   `TirFunction::impl_type_params`. Annotate already produces
   the vec inline in the `Item::Impl` arm.
4. The per-method `is_handler_method` flag, true iff the impl's
   trait reference names an effect (`interface`) declaration.
   Reify writes this onto the method's
   `FunctionContext::in_handler_method` so `resume` validation
   inside the body matches what annotate enforced.
5. The `is_ref_impl` flag, true iff the impl target is
   `&T` / `&mut T`. Method receivers `&self` then have an extra
   `&` layer; this matches Gap 2's per-call `is_ref_impl` on
   `MethodDispatch` but is decided at impl-block scope rather
   than at call-site lookup.

In addition, two `TirModule`-level outputs need a per-module
recording so `reify_module` can produce them without re-running
synthesis:

6. Synthesis requests (`impl Trait for Type;`) the elaborator
   pushes onto `tir_module.synthesis_requests`. Annotate
   records them on
   `ModuleSemantics.decls.pending_synthesis_requests`.
7. Default-method synthesis: when an impl omits methods that
   the trait declares with a default body, the elaborator
   synthesises a `TirFunction` per missing default. Annotate
   records these on
   `ModuleSemantics.decls.pending_default_methods`.

##### Recording shape

- New `TypeAnnotations::impl_facts: IndexMap<AstId,
  ImplFacts>`:
  ```rust
  pub(crate) struct ImplFacts {
      pub(crate) self_type: TypeId,
      pub(crate) trait_name_mangled: Option<String>,
      pub(crate) trait_canonical: Option<(ModuleSource, String)>,
      pub(crate) impl_type_params: Vec<TirTypeParam>,
      pub(crate) assoc_type_bindings: IndexMap<String, TypeId>,
      pub(crate) is_handler_method: bool,
      pub(crate) is_ref_impl: bool,
  }
  ```
- New `ModuleDecls::pending_synthesis_requests: Vec<SynthesisRequest>`.
- New `ModuleDecls::pending_default_methods: Vec<TirFunction>`.

##### Recording sites

- `ImplFacts` is written once per impl block at the end of the
  `Item::Impl` arm's setup phase
  (`elaborator.rs:~1240` — after type-param + assoc-type setup
  and the ref/synth/handler classification), keyed by
  `impl_block.id`.
- `pending_synthesis_requests` is pushed at the existing
  `tir_module.synthesis_requests.push(...)` site, with the
  recording call replacing the direct push so reify_module
  reads from `ModuleDecls` instead of from the elaborator's
  emitted module.
- `pending_default_methods` is pushed at the default-method
  synthesis loop's existing emission site, same shape.

##### Reify consumer

- `reify_impl` reads `impl_facts[impl_block.id]` for the
  full setup; calls `reify_method` per AST `Function`, passing
  the resolved `self_type` + mangled trait name + impl type
  params + is_handler / is_ref flags. No re-resolution of
  the impl target, the trait reference, or the type params.
- `reify_method`'s body walk runs against a `FunctionContext`
  with `in_handler_method` set from the recorded flag, and a
  `trait_ctx.assoc_type_bindings` populated from the recorded
  bindings (so `Self::Output` etc. resolve inside the body
  via the shared `resolve_type_in_scope_with_bindings`).
- `reify_module` reads `pending_synthesis_requests` and
  `pending_default_methods` from `ModuleDecls`, pushing each
  onto the emitted `TirModule`'s `synthesis_requests` /
  function list.

##### Why not just call the elaborator's helpers from reify

The elaborator's setup helpers (`enter_inherited_type_param_scope`,
`canonical_decl_key`, `register_generic_params`) mutate
`self.trait_ctx` and `self.tysys.type_table`. Annotating during
reify would double-write the same `TypeParam` interns and
re-canonicalise the same trait names, producing duplicate
`(module, name)` keys in `trait_env` indices. The recording
pattern keeps `tysys` and `sem` write-once across the two
walks, matching the WEP `Reify surface` constraint that reify
only interns "monomorphic instances created on demand" and
treats trait / impl tables as read-only.

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
