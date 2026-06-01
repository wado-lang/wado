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

Completeness rule (the contract that makes reify mechanical): every
fact reify needs to emit a node is recorded by `annotate`, keyed by
`AstId`. Reify re-derives only what is _uniquely determined by the AST
alone_ — literal kinds, the syntactic shape of a node (`Index` vs
`Field`) — never anything scope-, inference-, dispatch-, or
mangling-sensitive. Anything that depends on resolution is a recorded
decision, not a re-computation.

Implementation note (the Stage 7 gap): the _current_ reify violates
this rule in two places — it re-runs `resolve_type` /
`resolve_type_with_self` for type annotations, `Self`, and impl type
args, and it re-computes mangled method / struct names. Both are
decisions (they depend on the impl's positional type-param indexing and
on name mangling), and both have drifted from `annotate` and been fixed
one bug at a time (`TreeMap<String, V>` self-type indexing, the
`&T`-blanket `&^Inspect` name). Stage 7 closes the gap structurally:
`annotate` records the resolved types and the impl identity / mangled
name, and reify reads them. Once reify re-derives nothing
decision-bearing it cannot drift — the parity-bug class disappears by
construction.

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

**Stage 5 — Split `annotate_bodies` from `reify`.** Reify is built as
a second walk that reads `ModuleSemantics.types` and emits TIR; it
became the sole TIR source for every module at **2692 / 2692** E2E.
DONE — but it landed re-deriving the decisions it should read (types,
mangled names), so the split is functional, not yet _clean_; Stage 7
finishes it. (Routing cleanup — `module_uses_reify` removal — is the
already-landed Stage 7a; see the Landing log.)

**Stage 6 — Liveness and DCE.** `liveness::compute(&Semantics)` is
added, its result stored on `Semantics`, and reify gates item emission
on it. The user-facing unused diagnostics land per the
unused-diagnostics WEP; `optimize/dce.rs` retires from that role. Not
started. Independent of Stage 7 — either may land first.

**Stage 7 — Make reify mechanical, then `annotate` TIR-free.** This is
the structural completion of the annotate/reify split, in two sub-steps
gated only on Stage 5 (now done). The premise the whole WEP exists to
fix — that LSP builds and discards TIR just to obtain `Semantics` — is
satisfied at the end of 7-B.

- **7-A — Reify becomes mechanical (incremental, low-risk).** Close the
  completeness-rule gap one decision at a time: have `annotate` record
  what reify re-derives (resolved types per `AstId`, impl identity /
  mangled name) and switch reify from re-computation to a fact read.
  Each step keeps E2E green; reify's output is unchanged, only its
  source. When reify re-derives nothing decision-bearing it depends on
  `&Semantics` alone — the two-walk parity-bug class is gone.
- **7-B — `annotate` stops building TIR.** Strip TIR construction from
  the combined walk (`resolve_*` returns resolved types + records facts,
  no `TirExpr`), file by file (`expr.rs` → `stmt.rs` → `item.rs` → …),
  keeping every `record_*`. After 7-A the contract is pinned (the facts
  reify reads), so the target is exact: record the contract, drop the
  TIR. LSP then runs `annotate` only — no TIR built or discarded. The
  old `Elaborator` / `AnnotateState` TIR-emission halves and the
  duplicate TIR construction are deleted; the pipeline diagrams in
  `CLAUDE.md` / `docs/compiler.md` and the `elaborator/` file layout are
  updated to match.

Order is 7-A → 7-B: 7-A is incremental and de-risks 7-B, and while 7-A
runs the combined walk's TIR stays live as reify's reference. Migration
guard: a temporary structural diff between the combined walk's TIR and
reify's TIR detects any drift before 7-B removes the former.

Each stage keeps `mise run test`, the WIR golden fixtures, and the LSP
query tests green. Performance is not tracked during migration; see
Trade-offs.

## Status

- [x] **Stages 1–4 — Skeleton → `TypeSystem` → `ModuleSemantics` →
      per-`AstId` `TypeAnnotations`.** The God Object is decomposed; the
      20-map fact store exists and is `SymbolKey`-keyed. See the Landing
      log.
- [x] **Stage 5 — `annotate_bodies` / `reify` split.** Reify is the sole
      TIR source for every module (user / stdlib / snapshot) at **2692 /
      2692** E2E with no env var. Functionally complete; reify still
      re-derives types and mangled names (closed in Stage 7). See the
      Landing log.
- [x] **Stage 7a — Routing.** `module_uses_reify` and its stdlib/snapshot
      bypass, `stdlib_snapshot::is_building`, and the combined walk's
      TIR-output branch are removed; reify produces the final TIR for
      every module. The combined walk survives only as the (still
      TIR-building) `annotate` fact-recorder.
- [ ] **Stage 6 — Liveness and DCE.** Not started. Independent of
      Stage 7.
- [ ] **Stage 7 — Mechanical reify, then TIR-free `annotate`.** 7-A
      (reify reads recorded types / mangled names instead of re-deriving)
      → 7-B (`annotate` stops building TIR; LSP runs `annotate` only;
      `Elaborator` / `AnnotateState` TIR halves deleted). See the
      Migration Plan.

### Landing log (Stages 1–5, 7a — DONE)

Stages 1–4 (skeleton → `TypeSystem` → `ModuleSemantics` → per-`AstId`
`TypeAnnotations`) landed as designed; see git history for the field-by-field
moves. Net result: `Elaborator` is no longer a God Object — pipeline-wide type
knowledge lives on `TypeSystem`, per-module facts on `ModuleSemantics`'s four
sub-structs (`bindings` / `imports` / `types` / `decls`), and `TypeAnnotations`
carries 20 per-`AstId` fact maps (`expression_types`, `method_dispatch`,
`operator_dispatch`, `static_method_dispatch`, `index_assign_dispatch`,
`coercions`, `sequence_coercions`, `key_value_coercions`, `desugars`,
`generic_instantiations`, `for_of_iterator`, `closure_captures`,
`assert_captures`, `impl_facts`, `handler_bindings`, `function_effects`,
`function_task_returns`, `call_param_types`, `local_types`, `tuple_overlays`).

Stage 5 (annotate/reify split) reached **2692 / 2692 E2E** with reify the sole
TIR source for every module — user, stdlib, and the stdlib snapshot — with no
env var (`WADO_FORCE_REIFY=1` and the live path are now identical).

Canonical invariant: annotation maps are keyed by `SymbolKey`
(`(ModuleSource, AstId)`). Inlined foreign AST (assoc-const bodies,
callee-module default args, trait default-method bodies) is keyed to its
_owning_ module, so a colliding dense `AstId` in the consumer never overwrites
its facts.

Recurring gap shapes cleared during user-module parity (1765 → 2692): per-`AstId`
collisions (template interpolation continuing the parent id space; then
`SymbolKey` keying); reify reading recorded facts it had ignored (i128/u128
coercions, comparison operator-trait wrapping, `Self::Assoc` projection via
`ImplFacts`, generic-instantiation args, closure captures, IndexMut / effect
bindings); pattern & dispatch disambiguation; compile-time tuple-for-of
per-element overlays; trailing-value control flow; method-call result typing
from the recorded return type.

Stdlib parity (this track) cleared the remaining stdlib-shaped gaps, each a case
of reify re-deriving a decision instead of reading it:

- foreign-AST facts keyed to the owning module (const / default-method bodies);
- reference impls mangled by base struct `&` / `&mut` (blanket `&T: Inspect`);
- operator-overloaded compound-assignment dispatch (`u128 /= …`);
- impl-method self type resolved under reify's positional impl-param indexing,
  including the leading ref of a reference impl (`TreeMap<String, V>`, `&T`);
- tuple-binding destructuring in variadic for-of (`Eq for [..T]`).

Stage 7a (routing): `module_uses_reify` and its stdlib/snapshot bypass, the
`stdlib_snapshot::is_building` predicate, and the combined walk's TIR-output
branch are removed. The combined walk's returned `TirModule` is now discarded
for every module; reify produces the final TIR. The combined walk survives only
as the (still TIR-building) `annotate` fact-recorder reify reads from — which is
exactly what Stage 7 (below) finishes removing.

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
