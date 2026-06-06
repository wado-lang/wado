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

The roots, suppression rules, stdlib exclusion, and severity policy
remain owned by the unused-diagnostics WEP and are not duplicated
here.

## Migration Plan

The change touches every file under `elaborator/` and lands incrementally,
with the suite (E2E, WIR golden, LSP query) green at every step, in
dependency order. Stages are guidelines, not PR boundaries.

Stages 1–5 and 7a are DONE:

- **Stages 1–4** — extract `TypeSystem`, `ModuleSemantics`, and per-`AstId`
  `TypeAnnotations` from the `Elaborator` God Object.
- **Stage 5** — split `reify` out as a second walk reading
  `ModuleSemantics.types`; it is the sole TIR source for every module. It
  landed still re-deriving some decisions (types, mangled names); Stage 7
  cleans that.
- **Stage 7a** — routing removed (`module_uses_reify`, the snapshot bypass,
  the combined walk's TIR-output branch).

Remaining:

**Stage 6 — Liveness and DCE.** `liveness::compute(&Semantics)` is
added (see the DCE / Liveness section above), its result stored on
`Semantics` as a non-optional `Liveness`, and reify gates item emission
on `live_items`. A single reachability result feeds both the reify gate
and the user-facing unused diagnostics (per the unused-diagnostics
WEP); `optimize/dce.rs` retires from the diagnostic role but stays as
silent post-monomorphization cleanup. The pass is fail-loud: a
dropped-live item ICEs downstream and is fixed as a graph bug, never
masked by over-approximation. Not started. Independent of Stage 7 —
either may land first.

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
runs the combined walk's TIR stays live as reify's reference. The E2E
suite and WIR golden fixtures carry the equivalence guarantee at every
step.

Each stage keeps `mise run test`, the WIR golden fixtures, and the LSP
query tests green. Performance is not tracked during migration; see
Trade-offs.

### Stage 7-B execution plan

7-B is not a single mechanical edit. The combined walk's TIR is already
dead (reify is the sole producer since Stage 5/7a), so an arm's only
reason to still build a real `TirExpr` is that some _analysis or
diagnostic inside the combined walk_ reads the structure (`.kind`) of
the resolved value. Those readers must move to the AST + recorded facts
before the arm that feeds them can return a placeholder. Hence two
phases:

Phase 1 — port each structural reader off combined-walk TIR. Each is a
behaviour-preserving refactor (the recorded values are unchanged, so
reify's output is byte-identical) verified green on its own. Precedent:
`control_flow.rs` (Stage 5 moved missing-return off the body TIR).

Phase 2 — once no analysis reads resolved TIR structure, convert every
`resolve_*` arm to a placeholder, then change signatures
(`resolve_expr -> TypeId`, `resolve_stmt` records only) file by file,
make `build_tir_from_state` TIR-free, and run LSP through `annotate`
alone. Reify is the only TIR.

The Phase 1 readers, grouped by the analysis to port (the arm each
unblocks in parentheses):

- [x] **assign l-value + ref validity** — `assign_to_target`'s l-value
      match on `target.kind` and `resolve_unary`'s `&mut`-on-primitive-field
      check ported to the AST target shape + recorded dispatch facts
      (`operator_dispatch` / `expression_types`); `Local` and the
      `&mut`-captured-ident deref still read `target.kind` (their resolvers
      are not placeholders yet). (`resolve_field_access`, `resolve_index`,
      and the assign side of `resolve_unary` are now placeholders)
- [x] **null / unknown inference** — `patch_unresolved_null` /
      `NullBreakPatcher` replaced by AST walks
      (`control_flow::collect_unresolved_null_tails` /
      `collect_unresolved_null_breaks`); the TIR-mutating machinery is
      deleted. Required the `expression_types` UNKNOWN-faithfulness change
      (see the note below) so the AST walk sees an unresolved-null branch.
- [x] **block result type** — `crate::tir::block_result_type` callers in the
      `Block` / `If` (no-expected-type inference and post-null checks) arms,
      `with … do`, for-of, and the trailing-match path ported to
      `control_flow::block_result_type` over `expression_types`. The
      if-let-chain / let-chain lowerings still call the TIR version on their
      _synthetic_ blocks (no 1:1 AST block); those readers remain.
- [x] **unary constant folding** — the `-literal` fold and native `Unary`
      construction are deleted; reify owns the fold (it reads
      `expression_types[unary.id]` + the AST). (`resolve_unary` is a
      placeholder)
- [x] **tuple spread** — `resolve_tuple_literal` resolves the elements,
      collects their types (incl. the concrete-tuple-spread inline
      expansion), keeps the spread diagnostic, and returns a placeholder;
      reify's `reify_tuple_literal` owns the actual node / temporary
      construction.
- [x] **struct-literal deferred coercion** — the `value.kind == TupleLiteral`
      gate is read from the AST (a spread-free tuple-literal field), so the
      deferred second pass still records the coercion via
      `try_coerce_tuple_to_sequence`.
- [x] **pattern variant const literals** — the const body AST is classified
      directly (`Number`/`Bool`/`Char` → `Literal` pattern, else
      `ConstantValue`), unblocking `resolve_literal` and `resolve_cast`
      placeholders.
- [x] **for-of `TupleZip`** — `resolve_for_of` no longer reads the resolved
      `iterable.kind == TupleZip`. `TupleZip` is produced only by the tuple
      `.zip()` arm, so an AST shape check (a `.zip()` call) plus the existing
      `type_contains_pack` on the result type is equivalent.
- [x] **if-let-chain / let-chain result type** — ported to the AST. The
      `resolve_if_expr` LetChain arm computes its result type as
      `agree_branch_types(ast_block_result_type(then_block), else_type)
      .unwrap_or(UNIT)`, which is byte-identical to `block_result_type` over the
      synthetic chain block (the per-level `unwrap_or(UNIT)` recursion collapses
      to exactly this for single- and multi-element chains).
      `resolve_let_chain_stmts` is records-only and no longer builds the chain
      `Match` / `If` TIR; the `block_result_type(&TirBlock)` helper is deleted.
- [x] **assign target ident classification** — `resolve_ident` records an
      `AssignPlace` fact (`Local` / `DerefCapture { through_mut_ref }` /
      `Global { name, mutable }`) keyed by the ident's `AstId`;
      `assign_to_target` reads it to validate the l-value and global
      mutability from the AST + fact instead of the resolved `target.kind`.
      The address-taken-local marking the unary / method-call readers did is
      owned by reify (it marks `TirFunction::address_taken_locals` on the TIR
      it emits), so the combined-walk marking was deleted as dead; the
      `&mut`-on-immutable-local diagnostic moved to a read-only `ctx.lookup`
      over the AST target. This unblocked the `resolve_ident` placeholder.

Arms now returning placeholders: range, field-access, index, unary, cast,
tuple-literal, literal, ident, match, struct-literal, anonymous-struct, `?`
(option + result), if-expr (both arms), block, labeled-block, with-handler,
resume (joining binary / call / method-call / operators / coercion / assert /
matches / template / item / module from earlier stages). Still building TIR:
`resolve_block` / `resolve_stmt` with the stmt-level builders (the Phase 2
signature sweep). The combined walk no longer builds any `TirExpr` outside the
statement-level scaffolding.

`adjust_receiver_for_self_kind` (method-call receiver wrapping) reads
`ResolvedType`, not a TIR `kind`, and reify does its own adjustment, so
it needs no port.

Foundational note (discovered porting "block result type"):
`record_expression_type` deliberately drops `ERROR` and
UNKNOWN-containing types, but the combined walk's TIR carries them in
`expr.type_id` (a bare `null` is `Option<UNKNOWN>`). An AST analysis
reading `expression_types` therefore cannot see an unresolved-null
branch and mistypes the block as `Unit`. So the null/unknown handling is
a prerequisite for the block-result-type port: either `expression_types`
must faithfully carry the combined walk's types (including UNKNOWN), or
the null inference must be recorded as an explicit fact. This reorders
Phase 1 so the null/unknown reader is handled before (or with) the
block-result-type reader.

## Status

- [x] **Stages 1–4** — God Object decomposed; `TypeAnnotations` is the
      per-`AstId` fact store, `SymbolKey`-keyed.
- [x] **Stage 5** — reify is the sole TIR source for every module (user /
      stdlib / snapshot), 2692/2692 E2E. It still re-derived some types /
      mangled names; Stage 7 cleans that. The trait default-method
      synthesis path (the combined walk pushing pre-built `TirFunction`s
      onto `pending_default_methods` for reify to drain) was the last
      hold-out and was resolved by a follow-up: combined walk records a
      per-impl `ModuleSemantics` snapshot on `default_method_semantics`,
      and reify's `reify_impl_default_methods` synthesises the
      `TirFunction` from those snapshots — reify is now the sole producer
      of every `TirFunction`, including default methods.
- [x] **Stage 7a** — routing removed; the combined walk survives only as the
      (still TIR-building) `annotate` fact-recorder.
- [ ] **Stage 6** — Liveness / DCE. Design settled (see the DCE /
      Liveness section); implementation not started. Independent of
      Stage 7.
- [x] **Stage 7-A** — reify is mechanical. Every decision-bearing read goes
      through a recorded fact:
  - [x] Function / method signatures — params, return, type params, impl
        type-param scheme, self type, mangled / display names — read from
        recorded facts.
  - [x] Effect / resource op signatures — read from `effect_ops`.
  - [x] Struct / variant type-param defaults — read from `decl_type_params`.
  - [x] Struct field types (including `pub use` re-export recovery) — read
        from `struct_field_types`.
  - [x] Fixed a latent bug the reads exposed: a method generic on an impl that
        binds a concrete trait arg started at the wrong type-param index and
        reached codegen unsubstituted (`trait_method_generic_concrete_trait_arg`).
  - [x] Method-call type args — `MethodDispatch.method_type_args` carries
        the resolved vector verbatim (covers the IndexMut rewrite too).
  - [x] Const types — `sem.decls.associated_constants` stores the resolved
        `TypeId` directly; reify and the combined walk both read it.
  - [x] Let-statement annotated types — `let_annotated_types` carries the
        resolved whole-pattern type for destructuring bindings.
  - [x] Closure param types — read from `local_types` via the binding's
        `AstId`; the expected-fn-type peel survives only for the body's
        return-type forwarding.
  - [x] Cast target types — read from `expression_types[cast.id]`.
  - [x] Free-function call type args — `generic_instantiations.type_args`
        is the single source for both turbofish and inferred forms; reify
        no longer branches on `call.type_args.is_empty()`.
  - [x] Mangled-name class — `ImplFacts.struct_name`,
        `GenericInstantiation.mangled_name` (struct literal / range / anon),
        per-method `MethodNames`, `SequenceCoercionFacts` / `KeyValueCoercionFacts`
        method names, and `FromCallFacts` (?-op + static `T::from`) all
        carry the elaborator-computed strings; reify is a pure read.
  - [x] From-conversion shapes — `DesugarKind::NewtypeFromCollapse` /
        `NewtypeFromUnwrap` / `NewtypeFromWrap` tag the three "no explicit
        impl" `from` paths; the bodyless `impl From<X> for T;` synthesis is
        detected via `from_call_facts[call.id]`. Reify no longer compares
        `type_name(arg)` against the prefix to recognise them.
  - [x] Namespace static methods — `ns::Type::method(x)` records
        `static_method_dispatch` like every other static call; reify's
        ns-arm in-line reconstruction is gone.
  - [x] Static-call / Type::method recovery paths in reify deleted (every
        non-variant static call is dispatched via the recorded
        `static_method_dispatch` early return).
- [x] **Stage 5 completion — trait default-method synthesis moved to
      reify.** Originally, the combined walk's `resolve_module` synthesised
      per-impl `TirFunction`s for trait default methods and pushed them
      onto `pending_default_methods` for reify to drain — making combined-
      walk TIR for those bodies live, not dead. Stage 7-B leaves dropping
      TIR construction in the body walk would corrupt the synthesised
      default-method TIR (proven by an aborted `template.rs` slice that
      trapped Wasm validation on `trait_default` fixtures).
      Resolution: the body walk now snapshots each
      `(impl_block.id, default_method.id)` synthesis as a full
      `ModuleSemantics` on `default_method_semantics`, and reify's
      `reify_impl_default_methods` swaps `self.sem` to that snapshot to
      synthesise the `TirFunction` the same way it processes explicit impl
      methods. Per-impl snapshots avoid the `(trait_module, ast_id)`
      overwrite that happens when the same trait body is synthesised
      across many impls. `pending_default_methods` + the
      `record_pending_default_method` helper are gone. Reify is now the
      sole producer of every `TirFunction` in every emitted `TirModule`.
- [x] **Stage 5 completion — missing-return analysis moved off the
      combined walk's body TIR.** `validate_missing_return`,
      `block_always_exits`, and `find_return_type_in_*` previously
      walked the combined walk's `TirBlock` from `resolve_function` /
      `resolve_method` / `resolve_closure`, which forced Stage 7-B
      leaves to keep producing `TirStmtKind::Return` /
      `TirExprKind::Block` / `LabeledBlock` / `If` / `Match` /
      `Resume` / `WithHandler` shapes the analysis could read.
      Resolution: `elaborator/control_flow.rs` ports the eight walkers
      to operate on the parsed AST, reading
      `expression_types[(module, expr.id)]` for the `type_id == NEVER`
      check the TIR version did on `TirExpr::type_id`. Both phases
      consult it via a small `CtrlFlowCtx { expression_types, module }`
      view — the combined walk through `Elaborator::ctrl_flow_ctx`,
      reify through a direct construction over `self.sem.types`. The
      TIR walkers (~330 lines in `expr.rs`) and the
      `validate_missing_return(TirBlock)` helper are deleted.
- [x] **Stage 7-B — `annotate` builds no TIR; LSP runs `annotate` only.**
  - [x] Every expression arm returns a placeholder — literal / ident / binary
        / unary / call / method-call / static-call / field / index / cast /
        range / tuple-literal / struct + anonymous-struct literal / `match` /
        `?` (option + result) / if-expr (both arms) / block / labeled-block /
        `with` / `resume` / template / matches / assert-capture / closure. The
        `ident` arm was unblocked by the `AssignPlace` fact (`assign_to_target`
        classifies l-values from it + the AST instead of the resolved kind).
  - [x] Every statement is records-only: `resolve_block` / `resolve_stmt` and
        all helpers (let, return, task-return, if, while, C-style for, for-of
        + tuple/variadic/iterator, loop, break, continue, assert,
        labeled-block, let-chain) record facts (types, dispatch, desugar tags,
        `ForOfIteratorInfo`, `tuple_overlays`, local symbols) and build no
        `TirStmt` / `TirBlock`. The if-let-chain result type is computed on the
        AST (`agree_branch_types(ast_block_result_type(then), else)`).
  - [x] `resolve_module` is records-only (returns `Result<(), Bail>`); it
        walks every item for its facts and assembles no `TirModule`. Synthesis
        requests flow through `pending_synthesis_requests`, anon structs
        through `pending_anonymous_structs` — both read by reify.
  - [x] `build_tir_from_state` gates reify (and stdlib-snapshot `TirModule`
        rehydration) on a `build_tir` flag. The LSP engine
        (`wado-lsp::build_semantics` → `semantics_of(.., false)`) runs only the
        body fact-walk and never builds or reads TIR; the batch path, the
        general `semantics()` entry, and kiln options extraction pass `true`.
  - [x] **`resolve_expr -> TypeId`.** Every body-walk resolver returns the
        resolved `TypeId` (the `placeholder` `TirExpr` sentinels are gone).
        The TIR builders that reify shares (`build_tir_method_call`,
        `adjust_receiver_for_self_kind`, `resolve_from_call`, …) keep returning
        `TirExpr` — they are reify's real producers — and the few combined-walk
        sites that still call them read `.type_id` (wrapping `placeholder` only
        where a shared builder needs a `TirExpr` operand). reify (`reify.rs`) is
        untouched and remains the sole TIR producer.

### Landing log

The per-map detail and the gap-by-gap parity history (user 1765 → 2692, then
stdlib) live in git. The one load-bearing rule that remains:

Annotation maps are keyed by `SymbolKey` (`(ModuleSource, AstId)`). Inlined
foreign AST (assoc-const bodies, callee-module default args, trait
default-method bodies) is keyed to its _owning_ module, so a colliding dense
`AstId` in the consumer never overwrites its facts.

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
