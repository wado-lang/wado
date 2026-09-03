# WEP: Elaborator Architecture — TypeSystem, Signatures, Annotate, Reify

## Context

The `elaborate` phase is the largest and most entangled part of the compiler.
Everything under `wado-compiler/src/elaborator/` extends one
`Elaborator<'a, H>` struct, which was the single home for type interning,
trait resolution, method dispatch, name resolution, use→def recording, and
AST → TIR construction. Adding a fact, cache, or registry had no principled
place to land; the default was "another field on `Elaborator`."

Four problems compounded as the language grew.

### The elaborator was a God Object, in data and in operations

Every concern the phase touches was bolted onto the same struct, and every
module reached into it via `impl<'a, H> Elaborator<'a, H>`. The conceptually
independent layers — the type system, the per-module annotation facts, the
AST → TIR walk — had no type-level boundary. Borrow-checker pressure pushed
shared state into `Rc<RefCell<…>>` even where shared mutability was not
conceptually required, masking the real ownership question of where a fact
lives. The per-module construction site, which rebuilt the whole struct for
each loaded module, was the canonical demonstration: no field could be added
without editing it, and none removed without touching every module reading
from `self`.

The operations were the second half of the same problem. Walker code (the
`resolve_*` recursion that writes `sem` and emits diagnostics) and query code
(method lookup, trait queries, callee-signature lookups) share one
`&mut self`. Nothing but review discipline stopped a query from mutating walk
state, or a walker arm from open-coding a query.

### `annotate` did not annotate

The elaborator ran in two calls, `annotate_modules` then
`build_tir_from_state`. The intent was that `annotate` produces a snapshot the
LSP can query and `build_tir` is the batch-only extension that emits TIR. The
implementation did not honour it: `annotate_modules` covered only
declaration-level information, and all body-level work — inference, name
resolution, dispatch, coercion choice, the desugar rewrites — happened inside
`build_tir_from_state` as a side effect of TIR emission. There was no point at
which the facts were complete, so what the LSP read was a by-product of a
batch-only artefact rather than a phase output with a contract.

### Queries re-resolved foreign declaration ASTs on demand

Roughly 40 `loaded_modules` reads outside reify each fetched a declaration's
AST to re-resolve its signature at the use site: free-function signatures,
impl headers and impl-method signatures, trait-declaration methods, effect ops
and resource statics, globals, data sections, associated-type bounds. No site
outside reify ever read a method _body_ except trait default-method synthesis.
Signatures were the whole coupling.

That re-resolution was the root of everything else. `resolve_type` is a
four-way seam — it interns through `tysys`, records use→def edges into
`sem.bindings`, reads the scope, and logs — so any query calling it needs all
of `Elaborator`, `&mut`. A suppression flag existed so a query re-resolution
would not record non-authoritative edges over the owning module's. A
perspective swap replaced ten fields so `resolve_type` could run under a
foreign module's import context.

### Reachability had no home

`elaborate` must produce reachability information: the LSP renders unused
locals, imports and items (see
[`wep-2026-05-16-unused-diagnostics.md`](./wep-2026-05-16-unused-diagnostics.md)),
and reify emits only the items reachable from world exports, so what reaches
`monomorphize` / `lower` / `optimize` is what the program can run. Neither was
possible while the elaborator exposed no "annotation complete" checkpoint.

## Decision

`elaborate` is four components and an explicit phase order. The boundary
between the components is enforced by types, not review:

```
TypeSystem (+ Signatures)  — pipeline-wide queries; no AST, no sem writes, no logging
ModuleSemantics            — per-module facts
Elaborator                 — the per-module walker: AST in, facts out
Reify                      — facts in, TIR out
```

### Phase order

```
parse → bind → load → analyze
   ↓
annotate_decls   — types → TraitEnv + Signatures → per-module decl facts
   ↓
annotate_bodies  — ×N, the walker; sole output is a populated ModuleSemantics
   ↓
liveness         — reachability over every module's recorded edges
   ↓
reify            — ×N, AST + ModuleSemantics → TirModule
   ↓
monomorphize → lower → optimize → codegen
```

The LSP path stops after `liveness` and consumes `Semantics`; it builds no
TIR. The batch path runs through `reify` into the downstream pipeline.

### TypeSystem — the pipeline-wide type knowledge

Holds the `TypeTable` arena, the `TraitEnv`, the builtin / WASI / world
registries, the included-files map, `Signatures`, and the read-only caches
built once during `annotate_decls`. It exposes the operations the rest of the
compiler asks of "the type system": interning, coercion, inference, type
checking, trait queries, method lookup. It does not know about `Module`,
`AstId`, or `ModuleSource`-keyed per-module state.

The name is the membership rule: "would a new field belong in the type system
itself?" gates admission and prevents drift back into God-Object behaviour.

Query shape is `fn query(&self, ctx: &Scope, scope: &TypeLookup, …) -> …`, and
three rules define the boundary: `TypeSystem` never sees AST, never mutates
`ModuleSemantics`, never logs. Queries return data — including reason chains
([`wep-2026-06-02-diagnostic-reason-chains.md`](./wep-2026-06-02-diagnostic-reason-chains.md))
— and the walker turns them into diagnostics.

### ModuleSemantics — per-module facts

One instance per loaded module, owned by `Semantics` in an
`IndexMap<ModuleSource, ModuleSemantics>`, decomposed into four sub-structs
with explicit membership rules:

- `bindings` — `use → def` edges and locally defined symbols. What the LSP
  reads for go-to-definition, find-references, and hover-on-local.
- `imports` — the per-module name resolution context derived from `use`
  declarations.
- `types` — per-`AstId` type annotations and dispatch decisions recorded
  during the body walk: the `TypeId` of every typed expression, the resolved
  target of each method call, the chosen coercion at each conversion site, the
  desugar kind for each TIR-direct rewrite.
- `decls` — module-internal declarations confirmed by elaboration, including
  the per-module digests `Signatures` is assembled from.

Each sub-struct admits a field only when "does this fit the sub-struct's
responsibility?" has a clear yes/no answer. A field that cannot be placed is a
design question, not a default into the catch-all.

Every fact map keys by bare `AstId`, which is globally unique
(`(AstIdSpace, local)`). That is what lets a fact recorded while walking
foreign AST name its node exactly, whichever module's map it lands in.

A fact has one home, the map the walk wrote it into, and every phase reads it
there. `Semantics`, keyed by bare `AstId` too, holds no second copy: it routes
a fact to the module whose walk recorded it. Routing is per fact kind, because
a node two walks reach splits its kinds across them — a callee's parameter
default is typed at each call site, while its own module records the edges
around it.

### A body fact belongs to a walk, not to a module

One module's walk is not one walk. A tuple `for-of` body is a single source
sub-tree resolved once per element, so one `AstId` inside it carries one fact
per element: a different receiver type, a different method, a different
`From` impl. `sem.types` therefore holds one `BodyFacts` per walk — the
module's own, plus the overlays each tuple `for-of` peeled off it — and a
reader names the walk it wants rather than the module.

The maps a walk records are named once, in `with_body_facts!`
(`sem/types.rs`). The `BodyFacts` struct, the lens `split_off` truncates
back to, the fact count, and reify's `ann_*` accessors are all generated from
that list. A map added to it is peeled per element, swept for inference holes,
and read through the overlays, with no second list to keep in step — which is
the whole point: the copies are what fell out of step, silently and per fact
kind, and each omission was a miscompile of the shape "every element gets the
last element's answer".

Membership: a map whose value can differ between the elements a tuple `for-of`
unrolls belongs to `BodyFacts`. `assign_places` does not (an identifier's
place is the same whichever element binds it), and the declaration-level maps
(`impl_facts`, `function_effects`, `fn_param_types`, …) do not, since a
declaration is walked once.

Reading is total over the walks, at every layer. Reify consults the active
overlay stack innermost-first and then the module's own (`Reify::ann`);
`TypeAnnotations::all` answers every walk's value for one node; `Fact::all`
lifts that through `Semantics`' routing, so a check that must hold for a call
site inside an unrolled body sees each element's dispatch
(`Semantics::method_dispatches_at`) rather than one of them.
`Semantics::fact_at` and the LSP queries keep the singular shape and answer
the first walk's.

### Signatures — every declaration signature is a decl-pass fact

Rule: after `annotate_decls`, no phase re-resolves a declaration signature
from AST. Each signature is resolved exactly once, by the decl pass, in its
own declaration frame, and stored as `TypeId`-level facts:

- Free functions — type params, param types, return type, `is_mut`,
  param-default `ast::Expr`s.
- Impl methods — per-method canonical signatures (params / return /
  `self_kind` / `is_mut` / type params / defaults), plus the owning impl's
  own facts (`ImplSig`).
- Trait-decl methods — signatures plus `Rc<ast::Function>` for default-method
  bodies (the walker's synthesis input).
- Effect ops, resource static methods (including resolved `#[cm]` names),
  global types, associated-type bounds, per-module data sections.

The canonical frame: a signature is resolved under its declaring module's
import scope, with `Self` bound to the impl target, impl and method type
params as `TypeParam` slots, and associated types as the impl's own bindings.
Use sites substitute into that frame and never re-resolve. A signature whose
meaning would depend on the use site cannot exist under this rule; if one is
found, that is a design bug to surface fail-loud, not a licence to re-resolve.

AST survives inside an entry only where the value is irreducibly AST and the
consumer is the walker or reify, never a query: param-default exprs (resolved
per call site under the callee's scope, per
[`wep-2026-04-11-default-arguments.md`](./wep-2026-04-11-default-arguments.md)),
associated-const value exprs, trait default-method bodies.

Placement: one struct in `elaborator/sig.rs`, next to the `DeclSig` /
`MethodSig` / `ImplSig` shapes it stores, held on `TypeSystem` behind an `Rc`
and assembled once from the per-module `ModuleDecls` digests between the decl
and body passes. Keyed by the declaring node's `AstId`, with name-keyed
indices layered on top.

Membership rule: one entry per source declaration, holding what that
declaration _says_ — its signature, or the declaration-level datum it is.
Nothing computed from a use site, and nothing a later phase recomputes.

`Signatures` deliberately does _not_ extend `TraitEnv`. The two are built in
different phases over different alphabets: `TraitEnv::build` runs before any
decl pass and indexes _names_ ("which impls exist, on what receiver, for what
trait"), then freezes behind `Arc`; signatures are `TypeId`-level and can only
exist after the decl pass has interned types. Hanging signatures off
`ImplHeader` would make `TraitEnv` a two-phase build-then-backfill structure
and cost it the immutability its consumers rely on. Two maps under the same
`(ModuleSource, AstId)` key compose just as well at the use site.

### One canonical frame implies one way to leave it

A signature's canonical frame is only enforceable if there is exactly one
operation that instantiates it. The `TypeId`-level primitive is canonical
(`TypeTable::substitute_type_params`, keyed by slot index), but nothing paired
a signature with the slots it is abstract over, so each consumer open-coded
"clone the param types, substitute each, substitute the return".

So a signature is a `DeclSig`: the slots plus the parameter and return types
resolved against them. `DeclSig::instantiate` fills the slots positionally and
is how a use site reads one; inference, which solves _for_ the arguments, is
the one consumer that reads the canonical types directly. `MethodInfo` is not
independently computed — it is exactly
`instantiate(impl_method_sig, receiver_args)`.

An impl block's own declaration facts are its `ImplSig`: its resolved
target, that target's type arguments, its trait reference's arguments, its
`type X = …` bindings, its target's fq name, and which trait declaration it
implements. All resolved in the block's frame, all read back through one
instantiation.

### A frame is abstract over its projections too, and only a use site can fill them

Slots are not the whole of what a declaration frame leaves open. A signature
written against `Self::Item` is abstract over what `Self::Item` _means_, and
that cannot be a declaration fact: `I: IntoIterator<Item = u8>` is written at
the caller. Filling the slots without it yields a projection the use site
cannot resolve — which is exactly why the trait-bound path re-resolved its
callee's AST under a doctored scope instead of instantiating.

So the substitution carries both: `SlotProjections` maps a slot to what the
projections rooted at it stand for, and
`TypeTable::substitute_type_params_with` is the one implementation, with the
slot-only `substitute_type_params` as its empty case.

The use site answers for _every_ associated type the trait declares, not only
the ones its bound names. Rebuilding the recorded projection over the
substituted base recovers its owning trait and bounds but not its bindings,
and those bindings are use-site data — `I::Iter` knowing `Item = u8` comes
from the caller. Leaving the rest to the rebuild produces a projection that
differs from the one the same name resolves to when written in source, and two
spellings of one type that do not intern together are a type error at the use
site.

A projection's own `assoc_type_bindings` are types resolved in the same frame,
so they carry its slots and are substituted with everything else — the rule
every other arm follows.

What a bound's right-hand side denotes is deliberately not resolved to fill a
gap. `Self` there names the bounded type, so answering would mean rebinding
`Self` for the duration — but a frame's `assoc_type_bindings` shadow it, so an
unrelated `impl`'s `type Item = …` answers for a type parameter's, and
recursion through the right-hand side has no fixpoint. An unanswered name
stays abstract.

### Name-keyed facts belong to `TraitEnv`, `TypeId`-level facts to `Signatures`

Both are declaration facts, and the phase that asks decides which structure
can answer. `TraitEnv::build` runs before any decl pass; `Signatures` is
assembled after all of them. So a fact the decl pass needs _about itself_ —
which trait declares `Self::X`, asked while resolving that trait's own method
signatures — can only live on `TraitEnv`, alongside `assoc_type_bound_index`.
Filing it in the digest type-checks and silently answers `None`.

### One place per question

The digest only holds if each question it answers has a single implementation.
Every convergence below was forced by a defect where two of them disagreed:

- Which declaration a name means — from the module that wrote it. A name as
  written and the name a declaration calls itself differ exactly when an alias
  is in play, so a lookup keyed by the wrong one answers with another module's
  same-named type. A reference site is resolved once, by its writing module,
  and the consumers take the answer rather than each deriving one; a
  position-scoped lookup — a trait position where only trait declarations are
  candidates — is what the site answers directly, since it knows the position
  it was written in.
  [`wep-2026-08-12-declaration-identity.md`](./wep-2026-08-12-declaration-identity.md)
  owns the identity model.
- What a projection means in a frame — `frame_projection`, answering from the
  bindings a projection receiver carries and then from the enclosing bounds.
  Three implementations of this question disagreed, and the
  trait-bound path's copy is what fed the AST re-resolution.
- Which target arguments are slots — `TypeSystem::is_impl_target_param`.
- Where a method's own slots start — `MethodSig::method_param_offset`, carried
  on `MethodInfo` rather than recounted from receiver arguments.
- How a frame is entered — `enter_impl_frame` for a block,
  `enter_impl_method_frame` for a method within it.
- How a frame is left — `DeclSig::instantiate` positionally,
  `instantiate_slots` by slot index (a generic, `&`-target, blanket or
  variadic-tuple impl numbers its slots differently and a partially-concrete
  target leaves gaps), `instantiate_call` for a call site that spells the
  declaring block's arguments and the method's own separately, and
  `ImplSig::instantiate` for a block's own bindings.

### Scope — transient walk state with RAII-only mutation

One `Scope` struct (`elaborator/scope.rs`) holds the trait-resolution context
and the default-expression module fallback. Effect parameters live in
`TraitContext` itself: they are declared in a signature's `type_params` list,
so they are generic-scope state and the `TypeParamScope` guard restores them
with the rest of the context. All mutation goes through guards —
`TypeParamScope`, `with_self_type` / `with_self_type_if_known`,
`with_default_scope_module`, `with_foreign_vantage`, one shared field-restore
guard behind the `with_*` helpers. Enforceable by inspection: no `mem::replace`
or manual clone-restore of scope fields outside `scope.rs`.

The rule is about transient walk state, not about this struct: any value saved
for the length of a sub-walk and restored after it needs a guard, so an early
return or a panic cannot leave it swapped. Where that state sits on
`Elaborator` or on `FunctionContext` instead of on `Scope`, the rule is
[not yet met](#transient-walk-state-without-guards).

### Elaborator — the walker

```rust
pub struct Elaborator<'a, H: CompilerHost> {
    tysys: TypeSystem,          // shared handle (+ Signatures)
    sem: ModuleSemantics,       // owned; driver swaps per module
    symbols: &'a SymbolTable,
    logger: &'a Logger<'a, H>,
    current_module_source: ModuleSource,  // set at entry
    entry_module_source: ModuleSource,
    annotate_ctx: Scope,        // guard-managed transient state
    invocations: Rc<InvocationIndex>,
    interner: Rc<RefCell<ModuleSourceInterner>>,
    suppress_reference_recording: bool,  // the argument-classification probe
    infer_holes: InferHoleTable,
    assoc_binding_stack: IndexSet<(TypeId, String)>,
}
```

- No AST map on the struct. No walker arm reads an AST it does not own; the
  whole module's AST reaches the walker as the `&Module` argument its entry
  points already take. Cross-module needs are covered by `Signatures`
  (fallback-module idents, data sections, the declaring node each signature
  carries) and the `Rc`'d trait default bodies. One _declaring-side_ read
  survives: the driver fills each module's imported globals from
  `Signatures::globals` once every decl pass has run, rather than the decl
  pass re-resolving the declaring module's AST under a borrowed perspective.
- Per-call-frame data is data flow, not struct fields: `resolve_method_call_with`
  returns its dispatch outcome, and the operator's source `AstId` is a
  parameter.
- `resolve_type` stays on the walker by design. Inside the walk it is honest:
  interning, authoritative edge recording, and diagnostics are the walker's
  job. What was wrong was queries calling it.
- The suppression flag survives for one caller, and not the one it was built
  for: argument classification walks an argument _speculatively_ to choose
  among overloads, and a probe that records is a probe with a side effect.

### Reify — mechanical

`reify_module(&Module, &TypeSystem, &ModuleSemantics) → TirModule`. The walker
mirrors the AST shape; each visit method looks up the corresponding annotation
on `ModuleSemantics.types` and emits a TIR node. No inference, no name
resolution, no dispatch decisions. Monomorphic instances created during reify
intern through `TypeSystem`.

Completeness rule — the contract that makes reify mechanical: every fact reify
needs to emit a node is recorded by `annotate`, keyed by `AstId`. Reify
re-derives only what is _uniquely determined by the AST alone_ — literal
kinds, the syntactic shape of a node (`Index` vs `Field`) — never anything
scope-, inference-, dispatch-, or mangling-sensitive.

Three boundary invariants keep it that way:

- Fail-loud, not fail-safe. Reify does not fall back to recomputation when a
  fact is absent; every decision-bearing read is an `.expect`. Exactly one
  `resolve_type` call survives: the global read for a snapshot-rehydrated
  callee module, whose `ModuleSemantics` legitimately carries no
  `current_module_globals` — a documented exception, not a fallback.
- Every body fact is read through its `ann_*` accessor, so the walk reify is
  replaying is the walk it reads. A direct `sem.types` read of a
  `BodyFacts` map sees the module's own walk with each tuple `for-of`
  element's entries already peeled out of it, which is not a stale answer but
  a missing one.
- Single source for the projection rule. The "dense, real type params"
  predicate (`!effect && !fn-bound`) that fixes positional monomorph slots
  lives once, as `ast::GenericParam::is_real_type_param`; the annotate walk
  and reify both call it instead of re-spelling the filter.

### Liveness

`liveness` computes source-level reachability between `annotate_bodies` and
`reify`. Its policy surface — roots, stdlib exclusion, suppression, severity —
is owned by
[`wep-2026-05-16-unused-diagnostics.md`](./wep-2026-05-16-unused-diagnostics.md).
This WEP owns the mechanism: the data it produces, the graph it walks, and the
contract reify holds against it.

`Liveness` is a non-optional field on `Semantics`, computed immediately after
every `ModuleSemantics` is populated and before reify. It carries the items
reachable from production roots ∪ tests (reify's gate), the candidates
reachable from neither, and the candidates reachable from tests but not
production. The diagnostic emitter and reify read the same reachability
result; `CompilerOptions::unused_diagnostics` gates only whether the dead set
is emitted as warnings, never the reify gate.

Nodes are the top-level code items of every loaded module — free functions,
impl methods, trait methods including default bodies, and globals — each keyed
by its defining `AstId`. Stdlib items are nodes too, since user code reaches
them, but only user-authored nodes are eligible for the dead set.
`liveness::compute` builds a private `AstId → enclosing-item` map per module in
one structural walk, so a use inside a closure maps to its enclosing top-level
item.

Each recorded use-site becomes an edge `enclosing-item(use) → target`, from
two sources: `bindings.references`, whose def-side is a node (free-function
calls, global reads, variant-case construction, and the method a spelled call
dispatched to), and the dispatch facts in `types` for the sites that spell no
name at all — an overloaded operator, a subscript read or write, a `From`
conversion, a `for-of` iterator, a literal coercing through `From<Array<…>>`,
and a handler binding, whose target is the whole `impl` block the dispatch
wrapper may route any of the effect's operations to. Each such fact carries the
`DefId` dispatch selected, recorded at the same moment as the decision, so the
edge names a declaration rather than re-deriving one from a mangled name.
Reachability is a single BFS from the root set; the node set and edges are
static, so no fixed point is needed.

Fail-loud, not fail-safe: a dispatch fact that cannot be resolved to a
defining `AstId` is a graph bug, not an over-approximation site. If the graph
drops a live item, the missing symbol surfaces as an ICE downstream — never as
silently miscompiled output — and the fix is the missing edge kind. The dual
failure, an item that is unused but not reported, is what this design
optimises against.

Three paths are undecidable at the source level, and each gets a stated rule
rather than a blanket over every trait impl.

A trait method reached through a generic type parameter
(`fn show<T: Display>(x: T) { x.fmt() }`, `C::from_iter(self)`, or a projection
like `S::MapSerializer`): `annotate` records the edge to the trait method, and
the concrete impl is selected during monomorphization, which the source graph
does not run. So reaching a trait method reaches the corresponding method on
every impl of that trait. Broader than "every _reachable_ impl" only because
impl-block reachability is not itself a question this graph answers. It is also
what carries the faces a derive expands into: the synthesised body is TIR this
graph never sees, but the impls it calls are reached through the bound like any
other, from a `T: Serialize` entry point down through `Serializer::begin_seq`.

A generic impl block standing beside one written for a single instantiation of
the same head (`impl<T> Tag for Box_<T>` and `impl Tag for Box_<i32>`):
coherence Rule 1 gives the specific block, and monomorphization settles it by
mangled-name collision. The source spells only the call through the template,
so the template's method gets an edge to the specific block's same-named
method — an edge, not a root: a program that never reaches the template needs
neither block.

A trait whose calls a synthesis pass mints after this one runs: the format
traits behind `${x:spec}`, whose callee is read from a specifier `annotate`
does not interpret, and the reflection faces `reflect_bridge` mints per
monomorphized type. No fact names either, so the methods of a block
implementing one are roots. Membership is
`CompilerItem::dispatched_by_synthesis`, so the registry states the rule once
and a trait added later cannot be forgotten.

Gating reify is sound only because every semantic diagnostic that can fire on
dead code (effect, stores, purity, world-export conformance) is produced from
`Semantics` rather than the emitted TIR. Those checks ran on `TirModule`s
historically, so dropping a dead function silently suppressed its error.
`optimize/dce.rs` stays in place for the post-monomorphization population —
monomorph clones, synthesised CM bindings, effect-dispatch helpers — that
never passes through `Semantics`; it retires only from diagnostic emission.

### Naming

`elaborate` survives as the umbrella term and physical directory name;
`wado-compiler/src/elaborator/` hosts the layers as submodules. The phase
names exposed in pipeline diagrams and entry points are `annotate` and
`reify`. This matches the established use of "elaboration" in PL theory (Coq,
Lean, Idris) for the same kind of work.

### Rejected alternative

Passing a narrow "resolution context" — type table, reference sink, logger,
scope — into the query layer, keeping on-demand foreign-signature resolution.
Rejected: it re-creates the God Object as a parameter bundle. The suppression
gate, the perspective swaps, and the per-use-site re-resolution all stay — it
treats the symptom, and the query layer still cannot be tested without a
walker. The digest removes the cause.

## Implementation

### Module layout

```
wado-compiler/src/elaborator.rs    # umbrella
wado-compiler/src/elaborator/
├── tysys.rs       # TypeSystem and its operations
├── sig.rs         # Signatures, DeclSig / MethodSig / ImplSig
├── scope.rs       # Scope and its RAII guards
├── sem.rs         # ModuleSemantics (re-exports its sub-structs)
├── sem/{bindings,imports,types,decls}.rs
├── liveness.rs    # cross-module reachability
└── reify.rs       # AST + ModuleSemantics → TirModule
```

The four sub-structs of `ModuleSemantics` each get their own file because
their membership rule is file-scoped. The crate uses the no-`mod.rs`
convention.

`sem/types.rs` opens with `with_body_facts!`, the list every per-walk map is
declared in. It is exported, and reify invokes it with its own accessor
macro, so the two files cannot hold different lists: a map is added in one
place and the accessor appears.

### Reading the digest

A consumer reads it via `.expect(…)` — a missing entry is a loud panic, never
a fallback to AST re-resolution. That needs no separate completeness test: the
body walk visits every impl block in every module and `.expect`s the entry, so
the suite fails deterministically at the declaration rather than at whichever
use site reaches it first.

### How the trait-bound path reads the digest

A method reached through a generic bound instantiates the recorded
`MethodSig`, in three parts:

- The trait method's `DeclSig`, with `Self` as slot 0 and the method's own
  parameters after it. The decl pass records it in exactly that frame, so the
  receiver fills slot 0 and nothing else is needed from the declaration.
- What the trait's associated types mean at this use site — from the caller's
  bound (`I: IntoIterator<Item = u8>` answers `I::Item`) or from a
  projection receiver's own bindings. Use-site data, so it enters as
  `SlotProjections`, never as a re-resolution.
- The `ast::TraitBound` lists behind both. Declaration facts, but name-keyed
  and AST-shaped, so they stay on `TraitEnv` — `assoc_type_bound_index` and
  `TraitDeclHeader::assoc_types`.

The query writes no walk state: no scope to enter, no `self_type` to set, no
`assoc_type_bindings` to seed and restore.

### What the dispatch path reads instead of an impl block's AST

Every question `lookup_method_info` asked of the AST is the declaring block's
own `ImplSig` entry, instantiated through the slot map the candidate's shape
implies (`instantiate_slots`, since a blanket, `&`-target or variadic-tuple
block binds its slots from the receiver in a way the target arguments alone do
not say):

- its `type X = …` bindings and its trait reference's arguments — resolved
  once in the block's frame, so a binding naming a type private to the
  declaring module means what the block wrote;
- `Self` — the block's own resolved target, which is also what a concrete
  candidate is matched against;
- the target's fq name and which trait declaration the block implements —
  name-level, but _frame_-level too, so the decl pass is the only phase that
  can answer them without borrowing another module's imports.

Nothing about the answer is call-site-shaped except the slot map, so the query
neither swaps a perspective nor suppresses a recording.

### Invariants, enforceable by inspection

Each is a grep:

| Rule                                                          | Target | Now |
| ------------------------------------------------------------- | ------ | --- |
| Hand-kept copies of the body-fact list                        | 0      | 0   |
| Body-fact reads in reify outside `ann_*`                      | 0      | 1   |
| AST-map reads outside reify / decl pass                       | 0      | 0   |
| Whole-module AST scans outside the decl pass                  | 0      | 0   |
| Reify `resolve_type` call sites                               | 1      | 1   |
| Reify `loaded_modules` / `symbols` reads                      | 0      | 7   |
| `TypeLookup { … }` literals                                   | 1      | 4   |
| Name-keyed AST predicates                                     | 0      | 1   |
| AST-level type-param substitution helpers                     | 0      | 1   |
| `substitute_type_params_by_map` call sites                    | 0      | 5   |
| `mem::replace` / `mem::take` on walk state outside `scope.rs` | 0      | 29  |
| `with_module_perspective_for` call sites                      | 2      | 2   |
| `with_reference_recording_suppressed` call sites              | 1      | 1   |
| Walker signatures taking or returning a TIR node              | 0      | 0   |

The perspective and suppression rows are floors, not zeroes, and each is a
walker frame rather than a query: both perspective swaps are the callee-scope
retry a parameter default needs when the caller cannot name the callee's type
([`wep-2026-04-11-default-arguments.md`](./wep-2026-04-11-default-arguments.md)),
and the surviving suppression is the argument-classification probe. Every row
whose count exceeds its target is a [known gap](#known-gaps).

## Known gaps

Three rules order what is left. Each names a class the measurements below are
instances of, and each gap says what closing it takes.

- One list. A set the compiler must keep in step is written once and
  everything else generated from it; a field-by-field copy is a defect that
  has not fired yet.
- One core. A question with several partial answerers gets one complete
  answerer, and the partial ones are deleted rather than wrapped.
- One home. A fact is recorded where it is decided and read where it is
  recorded; a phase recomputing a fact it could have read is re-deciding.

### Per-element checks read one walk

`BodyFacts` makes every walk's answer reachable, and reify, liveness and the
effect / stores / purity checks read them all. The type-driven half of those
checks does not: `Semantics::expression_type` and `local_type` answer the
first walk, so a check that reasons about a value's type inside a tuple
`for-of` body sees the first element's. A body binding a resource in one
element and a plain value in another is checked against one of them.

Closing it: either a cursor that scopes a `Semantics` query to one
instantiation's walk (`with_element(for_of, k, …)`), which is what reify's
overlay stack already is, or moving the per-element part of those checks into
the annotate walk, where the element is the frame being walked. Which of the
two is a design question, not a mechanical one.

### The hole sweep is still a hand list

`infer_hole.rs::sweep_body_facts` enumerates the `BodyFacts` maps that can
hold a `TypeId` by hand — 16 of the 20 — so a map added to `with_body_facts!`
sweeps only if someone remembers. The four it omits carry none today; nothing
in the type system says so.

Closing it: a `SweepTypeIds` implementation per fact value type and a loop
generated from the same list, so the omission is a missing impl rather than a
silent leak of an unsolved variable into reify.

### One question, several answerers

The same computation is written several times, each copy partial in its own
way. Every row is a defect class, not a style preference: the copies disagree.

| Question                                      | Copies | Where                                                                |
| --------------------------------------------- | ------ | -------------------------------------------------------------------- |
| Check a call's arguments against a signature  | 8      | `call.rs` ×4, `method_call.rs` ×3, `method_lookup.rs`                |
| Pad a call's arguments with declared defaults | 4      | `call.rs`, `method_call.rs` ×2, `reify.rs`                           |
| A receiver's declaration, module, name, args  | 7      | `method_call.rs` ×4, `method_lookup.rs` ×2, `call.rs`                |
| Construct a variant case from a call          | 4      | `call.rs` ×2, `method_call.rs` ×2                                    |
| Dispatch an operator to its trait method      | 4      | `operators.rs`: comparison, arithmetic, shift, unary                 |
| Agree the type of several branches            | 4      | `expr.rs`: `if`, `if let`, `match`, labeled block                    |
| Substitute type parameters                    | 5      | `tir.rs` ×2, `tysys.rs`, `expr.rs`, `type_resolution.rs` (AST-level) |
| Decide a pattern's shape                      | 2      | `resolve_if_pattern_inner`, and the `exh_*` family that mirrors it   |

Two of these are holes rather than duplication. `resolve_static_method_call`
reaches none of the eight argument checks, so a static call's arguments are
never checked against the callee's signature; correctness there rests on
expected-type threading alone. And the four default-padding copies implement
two contradictory rules — reify's method path splices the caller's argument
AST into the callee's default and reifies it under the caller's perspective,
which the free path's own documentation explains must not be done.

Closing it, in order of what unblocks what: `TypeSystem::receiver_shape`
first, since three of the other rows derive a receiver on the way; then one
`check_args(sig, args, spans)` every call path calls, which is what closes
the static-call hole; then the default padding, the variant construction, the
operator ladder and the branch agreement, each a mechanical merge once the
first two exist. The substitution row closes by routing the AST-level helper
and `substitute_type_params_by_map` through
`TypeTable::substitute_type_params_with`, the one implementation this WEP
names, and deleting them; `substitute_type_params_by_map` has one external
caller and is partial by construction wherever its arm list is.

[`wep-2026-09-01-trait-resolution.md`](./wep-2026-09-01-trait-resolution.md)
names "one selection function serving every path" as what its dispatch order
still needs, and states the obstacle as each path holding a different amount
of the call. The receiver query and the argument core are that amount, made
uniform.

### The decl pass answers by scan

The decl pass answers each of its questions with its own scan of every
module's item list. Counted end to end, the module list or a module's items
are walked 34 times between the start of `annotate_modules` and the end of
`build_tir_from_state`, 23 of them over a user module's items; a module's
`use` list alone is walked eight times, once per collector that needs
namespace imports and again per iteration of the newtype fixpoint. So a new
declaration fact defaults to a new scan rather than a place in an existing
walk.

Eight of those passes are not gated on the stdlib snapshot and so re-derive
over all 86 stdlib modules on every compile: `Resolutions`, `TraitEnv`'s
three, liveness, the topological sort, and the `DefTable` seed. The snapshot
carries the `all_*` tables and the `TypeTable` but not these, though
`TypeSystem` holds both behind `Rc` / `Arc` already.

Three duplications sit inside the pass. `collect_function_signatures`
resolves every impl method's return type into a name-keyed map, and
`record_impl_decls` resolves the same methods again into `method_sigs` in the
same pass. The five `generic_*` maps and `function_return_types` on
`ModuleDecls` are `DeclSig` fields rekeyed by name and split, populated
twice, and reassembled at their one consumer. `Signatures` assembly deep-clones
every `MethodSig` / `ImplSig` / `TraitSig` in the program — including each
default-argument AST — where `function_sigs` already proves `Rc` works.

Closing it: one walk the collectors hang off; `TypeLookup::new(…)` and a
hoisted `ModuleDecls::default()` in place of the two 19-field literals and
their eight per-module empty maps; the newtype fixpoint replaced by a
topological pass, since `Resolutions` already resolves each base's head; the
name-keyed projections deleted in favour of reading `DeclSig`; `Rc` on the
remaining signature kinds; and the snapshot seeding `Resolutions` and
`TraitEnv` as it seeds the `all_*` tables.

### Name resolution has four answerers

`Resolutions` is the one answer for what a written name means
([`wep-2026-08-12-declaration-identity.md`](./wep-2026-08-12-declaration-identity.md)),
but three approximations of the same question survive: the
`validate_*_type_names` walkers in `orchestration.rs` (1,231 lines) testing
spellings against a global string set, `reject_unresolved_annotation` (23
sites, silenced after the first error anywhere), and
`TypeSystem::is_known_type_name`. The walkers are the weakest of the four:
they never check a generic head, accept a name declared in an unrelated
module because the set is a global union, and do not scope blocks — all three
of which `Resolutions` answers, at the same `AstId`s.

Closing it: emit the `UnknownType` and `InferPlaceholderNotAllowed`
diagnostics from a scan of `Resolutions` for `Resolution::Unresolved` at
type-position sites, and delete the walkers, `reject_unresolved_annotation`,
and `known_type_names_cache` with them. The golden diffs are diagnostic
ordering.

### Reify re-decides what annotate knew

The completeness rule holds for the facts that exist; what is left is the
facts that do not. Reify still runs a resolver of its own —
`symbol_named`, `case_path`, `lookup_struct_field_index`,
`lookup_free_func_params`, `qualified_owner_decl`, the pattern-side
`scrutinee_*` lookups — and `reify_call` re-derives its callee's shape in
eight sequential probe arms. Those are why `Reify` still carries `symbols`,
`loaded_modules`, `current_type_param_names` and `current_effect_param_names`,
and why 14 `type_lookup()` sites survive in a phase that is meant to resolve
no names.

Each retires against one recorded fact: a `call_shape` on `CallExpr`
(direct / indirect / variant / flags), a `field_access` on `FieldAccessExpr`
(index, name, type), a `case_construct` on every case identifier and pattern
(owner, index, payload type), effects on the written `fn(…) with E` node and
`#[benign]` list, and the callee's parameter defaults on the free-call
dispatch fact as the static path already carries them. The last two also
retire the surviving `resolve_type` call and `apply_function_type_effects`.

The reads that remain are fail-safe where the contract is fail-loud: 84
`unwrap_or*` defaults against 48 `.expect`s. Most are legitimately optional
("this node has no coercion"), but roughly a quarter are decision-bearing — an
unknown field name writes field 0, a missing tuple overlay falls back to the
truncated base map, a malformed literal emits `0` — and each silently changes
emitted TIR. `unescape_checked` is the model for the literal group: the body
walk already rejected the input, so the reify-side read is an `.expect`.

Closing it: the five fact maps, then the fail-safe conversion, then one RAII
`with_module_perspective` for the three hand-rolled module swaps (the
default-argument one restores six values across a loop with an early exit),
and `tir::build` for the pure TIR builders and the attribute extractors that
have no reify content.

### The query layer is only partly moved

Boundaries by type hold for the four components, but the walker carries 660
methods against `TypeSystem`'s 83, and 150 of those touch no walk state at
all — they read `tysys` and nothing else, and are on `Elaborator` because
that is where they were written. The dispatch and call paths
(`method_lookup.rs`, `call.rs`) remain predominantly walker-side even where
they ask no walk-state question, and `collect_trait_method_matches_from_impl`
goes further: it enters a type-param scope, clears two maps and reads them
straight back to build a slot map, using the walker's scope as a scratch
accumulator for a value it could construct directly.

`AnnotateState` still exists, reaching `effect_check.rs` and `semantics.rs`
as a carrier, and the borrowed compilation-unit inputs (symbols, logger,
interner, invocations, entry module) are still five separate fields threaded
identically through `module_elaborator` and `Reify::new` rather than one
`ElabEnv`.

Closing it: move each query that writes no walk state onto `TypeSystem`,
which is also what makes it testable without a walker, with
`typecheck.rs`'s three layers (pure function, `TypeSystem` method returning
data, walker method that emits) as the template; dissolve `AnnotateState`,
with `tysys`, `module_semantics`, `liveness` and `world_registry` landing on
`Semantics` and the rest becoming driver locals or `ElabEnv` fields; and
split `Scope`, whose recursion guards (`trait_check_stack`, `member_edges`)
are query-local frames rather than walk state.

### Transient walk state without guards

`scope.rs` guards what it owns, and nothing else is guarded.
`assoc_binding_stack` is mutated by a hand-written insert / `shift_remove`
pair around a call that can return early;
`with_module_perspective_for` and `with_reference_recording_suppressed`
restore by assignment after the body rather than on drop;
`FunctionContext`'s `for_continue_labels` and `compound_hoist_types` are
saved and restored by hand at six sites, one of them with two restore points
72 lines apart. None of the five is panic-safe, and the invariant row above
counts 29 `mem::replace` / `mem::take` sites on walk state outside
`scope.rs`.

Closing it: `with_scope_field` is the pattern; each of the five becomes a
guard, and `assoc_binding_stack` moves onto `Scope` where the membership rule
puts it.

### The walker's own duplication

Below the dispatch layer the same shapes recur. `resolve_binary_op` (789
lines) contains the operator ladder three times and `resolve_unary` a fourth,
with three byte-identical receiver-name blocks and five identical
`ResolvedTraitMethod` literals between them. `resolve_struct_literal` (544
lines) looks the same declaration up nine times and substitutes through three
different APIs. `resolve_if_expr`'s two condition arms share a seven-line
identical block and a near-identical branch match. The `let` and `if let`
struct-pattern arms duplicate their missing-field check and disagree on
newtype transparency. The exhaustiveness checker (595 lines in `expr.rs`)
needs `TypeSystem`, three environment questions and a diagnostic sink; seven
of its 17 functions already take no `self`, and the `&mut` on the two walker
methods it calls is unearned.

Closing it: one operator dispatch, one `unify_branches`, one struct-pattern
check, one declaration lookup per literal, and `elaborator/exhaustive.rs`.
None of these changes what the language accepts; the e2e corpus is the
verification.

## Consequences

### Benefits

- Boundaries by type. Queries are callable — and testable — without a walker;
  a walker arm cannot open-code a foreign-AST lookup because the map is gone.
  Every new field has a sub-struct it belongs to, and the membership criterion
  is mechanical.
- A body fact is one line in one list, and every reader sees every walk. The
  bug class this removes is omission from a hand-kept copy, whose symptom is
  every element of an unrolled loop getting the last element's answer — a
  miscompile or a check that never fires, per fact kind, silently.
- `annotate` actually annotates. `Semantics` is complete when it returns, so
  the LSP path has a phase output with a contract rather than a by-product of
  TIR emission.
- Each signature is resolved once, in its declaring frame, rather than once per
  use site under whatever context the use site could reconstruct.
- One recorded truth for use→def edges, which removes the "query clobbers the
  owning module's edge" bug class at the root rather than suppressing it.
- Liveness has a place to live: it slots between `annotate` and `reify`, gives
  the LSP its unused diagnostics, and decides what reify emits.
- `Rc<RefCell<…>>` retreats to where it is genuinely needed. Borrow-check
  pressure now reflects the conceptual model rather than working around it.

### Trade-offs

- Batch compilation walks each body twice: `annotate_bodies` for the facts,
  `reify` for the emission. Merging the two back into one walk is not on the
  table — the phase boundary is the load-bearing decision.
- Every declaration's signature is resolved whether or not a use site names it,
  so a declaration nothing reaches must still be resolvable in its own frame.
- Digests hold AST fragments (default exprs, associated-const values, trait
  default bodies), so for those nodes the AST outlives the walk that read it.
- Diagnostic timing shifts: a broken signature errors once at its declaration,
  not at each use.
- A body fact is read per walk, so a consumer that wants one answer for a node
  must say which walk it means. `Semantics::fact_at` answering the first walk
  is a default, not a derivation, and the checks that need every element must
  say so.
- `TypeAnnotations` derefs to its own `BodyFacts`, so a recorder writes
  `types.<map>` whichever group holds the map. The two groups are then
  distinguishable only at the struct definition.
- Bigger up-front design surface. The membership rules must be respected when
  adding fields — which is the point, since they replace the absence of any
  rule.

### Risks and mitigations

- A signature whose meaning secretly depends on use-site context (e.g.
  caller-side associated-type bindings). Surfaces as an `.expect` panic or a
  golden diff; the fix is to widen the canonical frame or add an explicit
  substitution input — never use-site re-resolution.
- `ModuleSemantics` or `Signatures` becomes the new dumping ground. Both
  membership rules are one sentence; reviews reject anything that is not a
  declaration's signature, or that has no sub-struct.
- A body fact filed outside `with_body_facts!` because the placement question
  was skipped. It compiles, and it is wrong only for a node inside an
  unrolled tuple `for-of`, which is the corner the corpus covers thinnest —
  `tuple_for_of_element_facts.wado` and `tuple_for_of_effect_error.wado` are
  the two fixtures a new fact should be exercised against.
- Stdlib-snapshot compatibility: `Signatures` is built in the same pass and
  seeded the same way as the other decl tables; the snapshot round-trip tests
  cover it.

## See Also

- [`wep-2026-04-18-lsp-architecture.md`](./wep-2026-04-18-lsp-architecture.md)
  — the LSP path's contract on `Semantics`.
- [`wep-2026-05-16-unused-diagnostics.md`](./wep-2026-05-16-unused-diagnostics.md)
  — the policy and surface for unused diagnostics, consuming the `liveness`
  pass defined here.
- [`wep-2026-04-11-default-arguments.md`](./wep-2026-04-11-default-arguments.md)
  — the callee-scope contract that keeps param-default exprs AST-shaped.
- [`wep-2026-06-02-diagnostic-reason-chains.md`](./wep-2026-06-02-diagnostic-reason-chains.md)
  — the data-not-diagnostics shape `TypeSystem` queries return.
- [`wep-2026-05-11-nir.md`](./wep-2026-05-11-nir.md) — the type boundary
  between TIR and post-lower IR; the `annotate` / `reify` boundary is its
  upstream counterpart.
