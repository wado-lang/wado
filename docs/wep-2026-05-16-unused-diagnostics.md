# WEP: Unused Diagnostics

## Context

The Wado compiler today has no equivalent of rustc's `unused_imports`,
`unused_variables`, or `dead_code` lints. Users get no signal when
imports, locals, parameters, or private functions are written but never
consumed. The infrastructure for warnings exists (`Severity::Warning`,
`Logger`, `CompilerHost::emit_diagnostic`), and the elaborator
re-architecture (see
[elaborator rearchitecture](./wep-2026-05-26-elaborator-rearchitecture.md))
provides the analysis substrate this WEP consumes: per-module
`bindings` (use→def edges, local-binding registry), per-module
structural indices (`AstIndex`), and the new `liveness` pass that
runs between `annotate_bodies` and `reify`. The elaborate-time
`liveness` pass is established by this WEP — the rearchitecture WEP
commits only to "there is a pass here, its output lives on
`Semantics`"; the policy that determines what is live, what is
suppressed, and what is reported is owned here.

The reachability roots for "unused" in Wado are not what rustc uses:

- `pub` and `export` both cross the package boundary. `export` additionally
  crosses the CM boundary (`export ⟹ pub`); a `pub`-only item reaches other
  Wado packages natively, with no CM representation (see
  [Visibility](./wep-2026-06-25-visibility-internal-pub-export.md)). Both
  apply uniformly across `command`, `service`, and `lib` entry points: a
  `lib` package exposes its `pub` / `export` items as its public API (see
  [Package Manifest](./wep-2026-02-14-package-manifest.md)).
- `internal` is package-internal visibility. An `internal fn` that no
  caller invokes inside the package is dead code, even in a `lib` package —
  it is not part of the package's public API surface.
- The standard library is a separate package whose `pub` / `export` items
  are visible from user code under special rules. Stdlib must never receive
  user-facing unused warnings.
- Synthesised functions (CM bindings, effect-dispatch wrappers,
  monomorphisation clones, auto-derived impls) are not source-authored
  and must not be reported regardless of reachability.

Without explicit lints, three problems persist: silent dead code
accumulates across refactors, unused imports pollute namespace
resolution and slow type checking, and the LSP cannot offer the
"greyed-out unused" hint that editor users expect.

## Decision

Add an unused-diagnostics subsystem driven entirely by elaborate-time
analysis, producing `Severity::Warning` diagnostics through the
existing `CompilerHost`. Two passes contribute, both consuming
`&Semantics` and running before `reify`:

1. Reference pass — for every user-authored module, reports a binding
   no reference resolves to. Emits `UnusedImport`, `UnusedVariable`,
   `UnusedParameter`. Imports read the import entries that answered
   the module's sites (`Resolutions::used_imports_in`), since the resolver
   is what decides which tier a name lands on.
2. Liveness pass — the elaborate-time DCE pass introduced by
   [elaborator rearchitecture](./wep-2026-05-26-elaborator-rearchitecture.md).
   Computes source-level reachability from world-export roots over
   the cross-module call graph encoded in `bindings` and the dispatch
   facts. Emits `DeadFunction` and `DeadGlobal` for the source items
   the computation does not reach (its `Liveness::dead_items`).

Both passes are guarded by `CompilerOptions::unused_diagnostics` (on
by default). No package-kind toggle is needed: `pub` and `export` already
name the complete set of package-external roots for every entry-point
kind (`command`, `service`, `lib`).

The optimize-time DCE in `optimize/dce.rs` stays in place as a
silent post-monomorphization cleanup — it removes monomorphic
instances and inlined-away code that became unreachable through
specialisation, none of which are user-source-level dead. It no
longer participates in diagnostic emission. The two roles separate
cleanly: source-level "you wrote this and nothing uses it" is
elaborate-time; "this monomorph fell out after inlining" is
optimize-time and silent.

### What is in scope (MVP)

| Lint               | Pass      | Code               |
| ------------------ | --------- | ------------------ |
| `UnusedImport`     | Reference | `UnusedImport`     |
| `UnusedVariable`   | Reference | `UnusedVariable`   |
| `UnusedParameter`  | Reference | `UnusedParameter`  |
| `DeadFunction`     | Liveness  | `DeadFunction`     |
| `DeadGlobal`       | Liveness  | `DeadGlobal`       |
| `TestOnlyFunction` | Liveness  | `TestOnlyFunction` |
| `TestOnlyGlobal`   | Liveness  | `TestOnlyGlobal`   |

### Reachability roots (package-external boundary)

The pass runs two independent reachability closures over the same call graph:

- `E` — the **production** closure, seeded by the roots below.
- `T` — the **test** closure, seeded by `test` blocks only.

| Production root (seeds `E`)               | Source                                                                                                                  |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `pub` / `export` items                    | `Visibility::Public` on a function or global (`export ⟹ pub`); the package-external boundary                            |
| Items satisfying world-export contracts   | Functions whose name and signature satisfy a world export (`command` / `service` / `lib`); identified during `annotate` |
| Items in `wasm_module_sources` re-exports | Bridged Wasm module exports                                                                                             |
| impl methods                              | Seeded live as call-graph intermediaries (method-level dead detection deferred)                                         |
| Declared methods no dispatch fact names   | An `interface` / `resource` operation, or a method carrying a parameter default. See below                              |

Each user-authored free function / global is classified by membership:

- `∈ E` → **live** (no diagnostic).
- `∈ T \ E` → **test-only**: used by tests but not production → `TestOnlyFunction` /
  `TestOnlyGlobal`. Reported in non-test builds only (during `wado test` the
  reaching `test` blocks are the point, so it would be noise).
- `∉ E ∧ ∉ T` → **dead** → `DeadFunction` / `DeadGlobal`.

`test` blocks are world-independent roots of `T`, never `E`: a function reached
only from a test is therefore reported, not silently kept alive — the behaviour
Rust's test-build masks. Reify still gates emission on `live_items = E ∪ T`, so
test-reachable code compiles (in non-test worlds the optimize-time DCE drops it).

The world-export part of the production root set matches the
existing optimize-time DCE root set in `optimize/dce.rs`
(`compute_reachable_from_entries`); the liveness pass restates them at the
source level so reachability can be computed before TIR is emitted.
Synthesised items — CM binding wrappers, effect-dispatch helpers,
monomorphisation clones, auto-derived impls — are not in `Semantics` at all
(they are born during `synthesis` / `monomorphize`) and therefore cannot
appear in either the live set or the unused set. The optimize-time DCE
continues to remove them silently.

`pub` (`Visibility::Public`) _is_ a root, exactly like `export`. The one
divergence from `optimize/dce.rs`: a Wado-to-Wado dependency is today always
consumed from source (see [Package Manifest](./wep-2026-02-14-package-manifest.md)
§"Wado-to-Wado Optimization"), so `compute_reachable_from_entries` does not
root `pub` yet — the consumer's own build supplies real reachability from
its own roots. Preserving `pub` for a standalone published `.wasm` (provider
metadata) is deferred.

`internal` (`Visibility::Internal`) is not a root of the package: it is
package-internal visibility only, so an unreferenced `internal fn` is dead code
regardless of entry-point kind. Deciding that takes every module of the
package, and a compilation loads only the modules its entry reaches. So a
compilation seeds an `internal` item into `E` (not into what reify emits): a
module the compilation did not load may use it, so a report would be a guess.
A private item stays decided, since all its users are in its own file.

### Package analysis

`wado check` over a package loads every member at once, through a synthetic
entry that imports each one (`LivenessScope::Package`). With the whole package
in view, the roots narrow to what the package offers:

- The entries are the members `wado.toml` declares (`lib` and each `[world]`
  entry). Their `pub` / `export` items and their `pub use` re-exports seed `E`.
- A `pub` item of any other member is not a root. It is reachable from another
  package only if an entry re-exports it.
- An `export` function seeds `E` in every member: a member with one is a
  program of its own (a script, an example, a benchmark), compiled as its own
  entry by `wado run`.
- An `internal` item is decided, like a private one.

Production code is what the entries reach by `use`; every other member is test
code. An item of production code in `T \ E` is "only used by tests", the case
Rust's test build hides. An item of test code in `T` reports nothing, since
reaching it from a test is its purpose. Either side reports "never used"
outside `E ∪ T`.

The CLI runs each member's Kiln pipeline apart and folds the indexes into one
keyed by the package's module identities
(`PackageSources::adopt_member_invocations`). An entry's index is the one its
own check already produced, so its generators neither run nor warn twice. One
pipeline over the whole
package would reject two members that generate one schema under different
options, which Gale's descriptor corpus does.

Only the modules that reach a reported member by `use` have their bodies
walked. Any other module can name no candidate, and the impl methods its calls
dispatch to are roots anyway, so its walk would change no verdict. Gale's
generated parsers import only the standard library, and they are most of what
its tests load. The loader goes one step further for a `#![generated]` module
that imports none of the package's code: it skips its function bodies at parse
time, so they cost no AST either.

A single-file compile and the editor keep the compilation rules above, so
`pub` and `internal` items stay roots there.

A declared method carrying a default body is an edge rather than a root, so a
free function that only an unreached default body calls is dead.
[Elaborator Architecture](./wep-2026-05-26-elaborator-rearchitecture.md)
§Liveness says which dispatch fact it rides, why the two kinds above have none,
and what the rule leaves open for a library's `pub trait`.

### Stdlib and dependency exclusion

Only the entry package's own modules (`ModuleSource::is_entry_package`) are
report candidates, in both scopes. The standard library, a dependency, a
remote module, a Kiln redirect and a local module of another package, or of
none where the entry is in one, are someone else's source, so no build of a
consumer reports into them. In package mode a module the package's code reaches
is its own even where `[test].exclude` keeps `wado test` from running it.

### Generated-module exclusion

A module carrying the `#![generated]` inner attribute (Gale parser output,
`wado-from-idl` bindings, and any other machine-emitted source) is excluded
from the lint entirely: it is never hand-edited, so flagging its unused items
is pure noise. Every other lint excludes it the same way, and
`--lint-generated` takes it back in for a generator's author. The module still
seeds the liveness closure, so items it calls stay live. This is the principled
alternative to embedding `#[allow(dead_code)]` in inlined runtime support —
generated parsers carry the marker already, so no attribute is baked into every
emitted file.

### Suppression

- Variables and parameters whose source name begins with `_` are
  silent. This is the same convention rustc uses and is what
  `wado format` is expected to produce when auto-fixing.
- `Wildcard` imports (`use _ from "..."`) are silent — they exist for
  side effects.
- `#[allow(unused_imports)]` on a `use`, or `#![allow(unused_imports)]`
  on the module, waives `UnusedImport`.
- `#[allow(unused_variables)]` waives `UnusedVariable` and
  `UnusedParameter` for every binder inside what it is written on: a `let`,
  a parameter, a function, an `impl`, or the module as
  `#![allow(unused_variables)]`.
- `#[allow(dead_code)]` on a function or global waives its
  `DeadFunction` / `DeadGlobal` / `TestOnlyFunction` / `TestOnlyGlobal`
  lint — for unavoidable unused items (e.g. a library module being built
  up ahead of the export that will reach it). The attribute name matches
  rustc's `dead_code` lint. `#![allow(dead_code)]` as a module inner
  attribute waives the lint for every item in the file — the idiom for
  test-helper files whose functions exist only to back `test` blocks. The
  liveness pass still records the item's call-graph edges; only its
  candidacy for the lint is dropped.

## Implementation

### Source files

| File                                              | Description                                                                                                                                                                                                                                                                          |
| ------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `wado-compiler/src/elaborator/liveness.rs`        | Owned by the elaborator rearchitecture. Computes source-level reachability and returns a `Liveness` value (live `SymbolKey` set, plus per-module unused-item / unused-binding lists).                                                                                                |
| `wado-compiler/src/lib.rs` (`unused_diagnostics`) | Adds `CompilerOptions::unused_diagnostics`. Once the elaborator has produced `Semantics` and its `Liveness`, walks the `Liveness` output and emits `DeadFunction` / `DeadGlobal` / `TestOnly*`, then the reference-pass emitters below. `Lints::reports_in` is the stdlib exclusion. |
| `wado-compiler/src/unused_import.rs`              | The import emitter: `UnusedImport`.                                                                                                                                                                                                                                                  |
| `wado-compiler/src/compiler_host.rs`              | Adds `Code::UnusedImport`, `UnusedVariable`, `UnusedParameter`, `DeadFunction`, `DeadGlobal` and their `Display` mappings.                                                                                                                                                           |
| `wado-compiler/src/logger.rs`                     | Adds `Logger::warn_at(code, message, span, file)` for span-bearing warnings.                                                                                                                                                                                                         |
| `wado-compiler/src/unused_variable.rs`            | The locals and params emitter: walks the AST for binders and assignment targets, and emits `UnusedVariable` / `UnusedParameter`.                                                                                                                                                     |
| `wado-cli`                                        | Adds `--no-lint`, which turns every lint off, these included.                                                                                                                                                                                                                        |
| `wado-lsp`                                        | Invokes the diagnostic emitter from `Engine::diagnostics`. Renders `UnusedImport` / `UnusedVariable` / `UnusedParameter` as `Hint`-severity unused-token decorations as well.                                                                                                        |

### Algorithms

#### Unused imports

`unused_import.rs` walks every file-private `use` in a module a lint reports
in (`Lints::reports_in`), the one predicate every lint and the dead-item
emitter read, so both unused lints answer for the same files: a package check
walks no dependency's bodies. The resolver records, per module, the import
entries, by local name, that answered one of its sites. A name written where no site holds it, which a later
pass reads by name, counts as one: a `#[benign(E)]` argument, and a
world's `import X;` / `export X;`. A
declaration imported twice (`use { a }` and `use m` reaching `m$a`) is
two entries, each used only where it is written. For each `UseItem`:

- `Simple`: reported when its local name (the alias, or the name) is
  unused.
- `Namespace`: used when one of its `ns$member` entries is, or a site spells
  `ns` bare: a case qualifier (`ns::Case`), whose member the elaborator finds
  by the scrutinee's type.
- `InterfaceFunctions`: each unused function is reported. `I::{f}` binds
  `f` alone, so `I` is reported only by a `Simple` item of its own.
- `Wildcard`: skipped (side-effect imports).

A trait has no reference site when only its methods are called: the
import puts the methods in scope. So where one of the module's method
references, as the elaborator records them, reaches a method a trait
declares or one of its `impl`s defines, every import entry that puts
that trait in scope (`Resolutions::imports_admitting`, the tier rule
`decls_in_scope` reads) is used: a name, an alias, or a namespace's
`ns$Trait`. Dispatch decided which declaration answered, so a
same-named method of another type does not count.

An import the resolver did not resolve counts as used, since its error
is reported already. `pub`, `internal` and `export` uses are
re-exports and never reported. Each diagnostic sits at the item's
name.

#### Unused variables and parameters

`unused_variable.rs` walks the AST of every module a lint reports in. The
walk collects each binder with its kind: a parameter of a function with a
body or of a closure is a parameter, and a pattern's name is a variable. It
skips a name starting with `_` and one an `#[allow(unused_variables)]` waives.
It skips the parameters of a method inside a trait, an interface, or an `impl`
with a trait (`ImplBlock::trait_type`), effect handlers included: the
declaration names them for every implementation, and one implementation leaving
one unread is no defect. It skips an `export` function's parameters for the same
reason (`Function::is_export`): the component boundary names them, and a world
such as `wasi:http/service` or `core:kiln/generator` often does. Under rustc's
rule each such implementation renamed
its parameters `_view`, `_gen` and the like, and the renames hid none of the
real defects the lint found elsewhere.
It also collects the names an assignment writes (`Expr::assigned_places`): a
place on the left of `=`, of a destructuring assignment, or of a compound
assignment. A place writes a name directly, or through the fields and indexes
of a value the name holds (`Semantics::written_place`, the rule the `contract`
clause check applies too). A projection stops at a reference, since the write
lands in what the reference points to. A write counts as a read where something
besides the name reads it: an index step whose indexed value reaches a `&mut`,
since the index assignment may write through it, or an affine name
(`Semantics::is_affine`), whose drop may run a `Drop` impl or a resource's
destructor on what was written. A field step only stores into the value.

The elaborator records a use→def edge for every name that reaches a local
(`Semantics::iter_references`). Only edges reaching a candidate binder are
kept. An edge from a write site marks its binder written, and any other edge
marks it read, so `n = n + 1` reads `n`. A binder the elaborator did not
bind as a local (`SymbolKind::Variable`) is a case or a constant the pattern
tests against, and is dropped. Each remaining binder that nothing reads is
reported at its name: "never used" when nothing writes it either, "assigned
to, but never read" otherwise.

An or-pattern binds a name once per alternative, but the elaborator resolves
every use to the first alternative's binder. The walk merges the binders of
one name across the alternatives, and the merged binder is read when any of
them is.

#### Dead functions and globals

The elaborate-time `liveness` pass computes the closure of reachable
source items from the root set (see Reachability roots) over the
source-level call graph. The graph mechanism — node set, the
per-module enclosing-item index, the edge sources
(`ModuleBindings.references` plus the dispatch facts in
`TypeAnnotations`), precise `FunctionRef` resolution, and the
fail-loud treatment of generic trait dispatch — is owned by the
[elaborator rearchitecture](./wep-2026-05-26-elaborator-rearchitecture.md)
(see its DCE / Liveness section). The closure is a single BFS over a
static graph, so there is no fixed-point loop and no per-iteration
dedup.

The emitter walks `Liveness::dead_items` and reports `DeadFunction`
or `DeadGlobal` for each user-authored entry. Stdlib modules and
synthesised items (which are not in `Semantics` at all) never enter
`dead_items`.

Span: `Semantics::name_span_of(key)`, with fallback to the item's
`span` for declarations without a narrow name span.

Globals dropped during optimize-time DCE (e.g., constants inlined
into all callers) do not surface as `DeadGlobal` because they were
live at the source level — that is the intended separation.

### Pipeline integration

```
parse → bind → load → analyze
   ↓
annotate_decls
   ↓
annotate_bodies   (per module, populates ModuleSemantics)
   ↓
liveness          (source-level reachability → Liveness on Semantics)
   ↓
emit unused diagnostics
   ↓                                    ↘
reify                                    LSP terminates here
   ↓
monomorphize → lower → optimize         (optimize-time DCE runs silently)
   ↓
codegen
```

Both the reference pass and the diagnostic emission against `Liveness`
sit immediately after `liveness`. The LSP path consumes the same
emitter (it reads diagnostics from `Engine::diagnostics`), so users
get the warnings in the editor before `reify` runs. Batch
compilation continues into `reify`, which skips items absent from
`Liveness::live_items`.

`compile_with_options` gates the emitter on
`CompilerOptions::unused_diagnostics`. `Engine::diagnostics` does
the same.

### Migration plan

The reference-pass diagnostics (`UnusedImport`, `UnusedVariable`,
`UnusedParameter`) depend only on `ModuleSemantics.bindings`, which
the existing elaborator already populates; they can land before the
elaborator rearchitecture completes. The liveness-pass diagnostics
(`DeadFunction`, `DeadGlobal`) depend on the new `liveness` pass and
land with stage 6 of the rearchitecture (see
[elaborator rearchitecture](./wep-2026-05-26-elaborator-rearchitecture.md)).

#### Phase 1 — diagnostic plumbing

- [x] Add `Code::DeadFunction`, `DeadGlobal` and their `Display` strings.
  (`UnusedImport` / `UnusedVariable` / `UnusedParameter` land with the
  reference pass.)
- [x] Add `Logger::warn_at(code, message, span)`.
- [x] Add `CompilerOptions::unused_diagnostics` (default `true`).

#### Phase 2 — reference pass

- [x] Record each module's used import entries in the resolver.
- [x] Implement the import emitter (`unused_import.rs`), wired through
  `unused_diagnostics`, with `unused_import.wado`.
- [x] Implement the locals and params emitter (`unused_variable.rs`), with
  `unused_variable.wado`.

#### Phase 3 — liveness pass and DeadFunction / DeadGlobal

- [x] Land `elaborator/liveness.rs`: the enclosing-item index (an AST
  id-collector), edge collection over `references`, the BFS, and the
  `Liveness` field on `Semantics`. Free functions and globals are
  precise; every method is seeded live as an intermediary (so the
  free-function reachability stays sound without the operator / `?` /
  for-of dispatch edges).
- [x] Implement the emitter (`DeadFunction`, `DeadGlobal`) consuming
  `Liveness::dead_items`; tests in `tests/integration/unused_diagnostics.rs`.
- [x] Two-closure 3-way classification (`live` / `test-only` / `dead`): test
  blocks seed `T` only, never `E`; `live_items = E ∪ T` (reify gating
  unchanged). Adds `TestOnlyFunction` / `TestOnlyGlobal`, emitted in
  non-test builds. Unit tests in `tests/integration/unused_diagnostics.rs`.
- [x] E2E warning assertions: `warnings_contains` / `warnings_not_contains`
  fixture-spec fields (`compile_capturing_warnings` in the harness);
  `dead_fn_*` / `dead_global_*` fixtures cover dead and test-only.
- [x] Package-wide analysis in `wado check` package mode (see Package
  analysis); members come from `wado test` discovery. Tests in
  `tests/integration/package_unused.rs`.
- [x] LSP wiring: the pure `lint_diagnostics(&Semantics, unused, is_test_world)`
  builder is shared by the batch path and `Engine::diagnostics`, which
  applies it at query time (toggle stays live via
  `Engine::set_unused_diagnostics`) and keeps only entry-document items.
  The LSP runs the command world, so test-only items are reported.
  Diagnostics carry the LSP `Unnecessary` tag. Tests in
  `wado-lsp/tests/diagnostics.rs`.

#### Phase 3b — reify gating (Design B)

Reify gating is the input-shrinking win (`monomorphize` / `lower` /
`optimize` see only the reachable closure). The first attempt gated
inside reify and broke two contracts, both now understood:

1. Diagnostics in dead code. Wado reports errors in unreachable code
   (effect / stores / purity / world-conformance). Those checks run on
   the emitted TIR _after_ reify, so dropping a dead function suppressed
   its error. The fix is structural — produce those diagnostics from
   `Semantics` (AST + recorded facts) so they see the whole program
   regardless of what reify emits. This also lets the LSP surface them
   (it builds no TIR). See the rearchitecture WEP's DCE / Liveness note.
2. A cross-module reachability gap — a dropped-live function the
   source-level graph missed (`cross_module_type_identity` ICE'd at WIR
   build). The graph must be a sound over-approximation of reachable.

Gating is therefore disabled until both are addressed:

- [x] 1b. `check_effects_semantic(&Semantics)` (AST + facts) landed and
  wired into the LSP (`Engine::diagnostics`). Covers free / method /
  static / indirect calls, declared effects, signature resources
  (incl. struct-nested), resource injection, the propagation
  closure, effect-parameter resolution, and `#[benign]`. Batch still
  runs the TIR `check_effects` (see "Batch switch — blocked" below).
- [ ] 1c. Port `check_stores` likewise.
- [ ] 1d. Port `check_default_purity` likewise.
- [ ] 1a. Move the world-export conformance check (`export`-required,
  param / return mismatch) off the gated TIR — read the entry
  module's AST / `Semantics` in `compile_with_options` (where the
  target world is known). Record the world-export root set on
  `Semantics` for the liveness roots.
- 2. [ ] Close the liveness graph's cross-module gaps (foreign-keyed
     `references` from `with_module_perspective`, inlined-foreign-AST,
     namespace imports, test-world roots).
- 3. [ ] Re-enable reify gating on `Liveness::live_items` and validate
     the full E2E suite green (fail-loud: a dropped-live item ICEs).
- [ ] The optimize-time DCE never carried a user-facing diagnostic role,
  so there is nothing to retire — it stays as silent cleanup as
  designed.

Batch switch — landed (Semantics effect check), 3 fixtures still red:

`compile_with_options` now runs `check_effects_semantic` on the whole
program and the TIR `check_effects` is removed. The full E2E suite is at
**2836 / 2842** (6 failures = 3 fixtures × 2 opt levels). Resolved on the
way (each was also an LSP false positive / under-report):

- [x] Stdlib per-module facts (`effect_ops`, `function_effects`,
  `fn_param_types`, …) were not seeded from the snapshot, so the
  propagation closure missed stdlib effect→resource propagation
  (`Stdout → Stream → StreamWritable`). The snapshot now clones each
  stdlib module's `types`.
- [x] Bundled stdlib `.wado` files load as `ModuleSource::Local` with a
  `wasi:` / `core:` scheme path; `is_user_authored` now excludes them.
- [x] Effect identity is non-canonical: `EffectRef::Concrete.module_source`
  reflects the recording module's import perspective (user `with     Stdout` → entry module; stdlib → `wasi:cli`). Each effect is now
  resolved to its declaration where it is written.
- [x] Effect-handler scopes: `with H => … do { … }` grants `H` to the
  do-block body (pushed / popped around the body walk).
- [x] Indirect calls through a function-typed parameter resolve the
  callee via the enclosing function's parameter types.
- [x] Variant-payload-nested resources: a `(module, variant) → payload     types` map feeds the closure and `signature_resources`.
- [x] Default-expression purity was a side effect of the non-canonical
  bug, fixed by canonicalisation (`check_default_purity` stays on
  TIR and fires again).

Async effect checking — resolved with no new rule:

Investigation (temporarily restoring the old TIR check) established that
the `async` failures were **not** a pre/post-synthesis artefact: the new
check runs pre-synthesis and applies the same resource-effect rule the
old check did. `WaitableSet` / `ErrorContext` are simply real
capabilities the failing handlers used (`WaitableSet::new()`,
`ErrorContext::new()`) without holding — true positives. They are not
reachable in the propagation closure from anything an HTTP handler holds
(`WaitableSet` appears in an operation signature only in
`Subtask::join(set: &WaitableSet)`), so the new check correctly flagged
them. The old check passed them only through a resource-specific
leniency.

The accurate failure count with `async` checked is **3 fixtures**
(`cm_waitable_set_new`, `cm_waitable_set_poll`, `cm_error_context`). The
~75 other `task return` handlers pass: they only touch `Future` /
`Response` / `Stream`, held via signature propagation. So no
`task return` rule is needed — the fix is just to declare the capability
the three handlers actually use.

- [x] Remove the interim `async`-skip; `async` bodies are effect-checked.
- [x] Declare the used capability on the three handlers (`with     WaitableSet` / `with ErrorContext`); HTTP handlers accept a `with`
  clause. Full E2E green.

#### Phase 1b design — effect check on `Semantics`

`effect_check.rs` today walks the emitted TIR. The port reimplements
the same logic over the parsed AST plus the facts `annotate` already
records, so it runs after `annotate_bodies` and sees every function —
dead or live — for both batch and LSP. To keep every commit green, land
it incrementally:

- First wire the new checker into the LSP path (`Engine::diagnostics`)
  only; the batch path keeps the TIR-based `check_effects`. No double
  emission (LSP builds no TIR), existing fixtures unchanged, and the LSP
  gains effect diagnostics immediately.
- Once it reaches diagnostic parity (validated against the
  `effect_check_*` fixtures), switch the batch path over and delete the
  TIR-based checker. Reify gating (Phase 3b.3) can then turn on.

Data mapping (TIR read → `Semantics` source):

| TIR read (today)                         | `Semantics` source                                          |
| ---------------------------------------- | ----------------------------------------------------------- |
| `func.effects`                           | `TypeAnnotations.function_effects[fn_key]`                  |
| `func.is_ambient`                        | `#[ambient]` on the AST `Function.attrs`                    |
| `func.task_return_type`                  | `function_task_returns[fn_key]`                             |
| `param.type_id`                          | `fn_param_types[fn_key]`                                    |
| `func.return_type`                       | `fn_return_types[fn_key]`                                   |
| `is_cm_binding` / `is_dispatch_wrapper`  | n/a — synthesised, not in `Semantics`; skip rule disappears |
| struct field / variant payload `TypeId`s | `struct_field_types[struct_key]`, variant decls             |
| resource decls                           | AST `Item::Resource` per module                             |
| call-site `FunctionRef`                  | the dispatch facts (below)                                  |
| `ResolvedType` lookups                   | the `TypeTable` snapshot on `Semantics`                     |

Call-site callee resolution (the body walk visits the AST; each call
kind maps to the fact that names its callee):

| AST call site                 | Fact → callee                                       |
| ----------------------------- | --------------------------------------------------- |
| free call `f()`               | `references[callee_ident_id]` → fn `SymbolKey`      |
| `recv.m()`                    | `method_dispatch[call_id].function_ref`             |
| `T::m()` / `Self::m()`        | `static_method_dispatch[call_id].function_ref`      |
| operator / index              | `operator_dispatch` / `index_assign_dispatch`       |
| `?` / `From`                  | `from_call_facts[expr_id]`                          |
| `for x of e`                  | `for_of_iterator[for_id]` (`into_iter` / `next`)    |
| indirect call (closure value) | `ResolvedType::Function.effects` of the callee type |

Algorithm (unchanged from the TIR version, restated on facts):

1. Build the effect index keyed by `(module, mangled_name)` —
   `mangled_name` from `method_names[method_key].mangled` for methods,
   the source name for free functions — carrying effects, `is_ambient`,
   `benign`, `is_method`, and param `TypeId`s.
2. Build `resource_names`, `struct_fields`, `variant_payloads`, and the
   propagation closure (`build_propagation_closure`) from the same decl
   and type data, now read off `Semantics` rather than `TirModule`.
3. For each user function: `current_effects = expand(declared effects + signature_resources(param/return/task-return types) + benign)`.
4. Walk the AST body; at each call site resolve the callee via the table
   above, compute `get_function_effects` (callee effects + resource
   effect for direct resource methods − benign, with effect-param
   resolution), and report `EffectError` for any effect not in
   `current_effects`.

Stores (1c) and purity (1d) follow the same data mapping; stores reuses
the ref-parameter detection (`ResolvedType::Ref` / `MutRef` over
`fn_param_types`).

#### Phase 4 — CLI wiring and reference pass

- [x] `wado-cli`: `--no-lint`, `--lint` and `--lint-generated` on every
  compiling subcommand, in place of an unused-only `--no-unused`.
- [x] Reference pass: `UnusedImport`.
- [x] Reference pass: `UnusedVariable` / `UnusedParameter`.
- [ ] Method-level dead detection (the dispatch edges; stop seeding every method live).

### Test plan

E2E fixtures (under `tests/fixtures/`):

- `unused_import.wado`
- `unused_var_basic.wado`
- `unused_var_underscore_silent.wado`
- `unused_param_basic.wado`
- `unused_param_underscore_silent.wado`
- `dead_fn_private.wado`
- `dead_fn_export_root.wado`
- `dead_fn_generic.wado`
- `dead_fn_test_world.wado`
- `dead_fn_pub_is_root.wado` (a `pub fn` with no in-package caller is not dead)
- `dead_fn_internal_is_root.wado` (an `internal fn` with no caller in the compilation is not reported)
- `dead_fn_cascade_via_global.wado` (function reachable until a dead global is removed)
- `dead_global_basic.wado`
- `dead_global_used_by_dead_fn.wado` (cascade across iterations)
- `unused_stdlib_no_report.wado`

Each carries the appropriate `stderr_contains` entries in its `__DATA__`
block. `wado-lsp` integration tests cover the Semantics-layer lints
through the diagnostics path.

## Consequences

### Benefits

- Users get immediate, location-precise feedback on dead imports,
  locals, parameters, and private functions, matching the experience
  every rustc user expects.
- LSP "greyed-out unused" works without extra effort because the
  diagnostic emitter consumes the same `Semantics` + `Liveness` that
  batch compilation produces.
- The optimize-time DCE keeps doing its silent removal job. Source-
  level dead code is reported once, at the right phase, with the
  right span; post-optimization specialisation noise stays internal.
- `reify` consumes `Liveness::live_items` to skip dead items,
  reducing the input size of `monomorphize` / `lower` / `optimize`.

### Costs

- New warnings will fire on every existing Wado source file the first
  time CI runs the new compiler. The MVP defaults `unused_diagnostics`
  to `true`; a one-time cleanup pass on the in-tree examples and
  fixtures is part of Phase 2 / Phase 3 landing.
- The reference-pass emitter and the liveness-pass emitter are two
  small additional walks over `Semantics` per compilation. Both are
  linear in the size of the source-level call graph and cheap
  relative to the elaborator itself.

### Risks and mitigations

- Risk: a generic function with all monomorphisations unreachable is
  reported once at the source level, but rare edge cases
  (specialisation through effect dispatch) could leave one
  monomorph reachable from a path that the source-level call graph
  in `bindings` does not see. Mitigation: such cases would also
  break the source-level `bindings` edge model used by goto-def and
  find-references — fix them there. The liveness pass does not
  invent its own graph.
- Risk: the source-level call graph misses a runtime-reachable item
  (e.g. dispatch through a trait object built from a string at
  runtime — not yet a Wado feature, but a future hazard). Mitigation:
  `liveness` only consults edges recorded during `annotate`; adding
  new edge kinds is part of the language feature that introduces
  them.

## Known gaps

- Outside `wado check` package mode, an `internal` item is never reported,
  neither "never used" nor "only used by tests", and neither is what only it
  reaches. A single-file compile and the editor do not load the whole package.
- A namespace re-export (`pub use ns from "./m.wado"`) seeds nothing in package
  mode, so what it offers reports as unused.
- Package mode elaborates every member on one thread, and runs every member's
  Kiln pipeline one at a time. On Gale it takes under twice as long as
  checking the declared entries alone.
- No `unused_mut`, `unused_type_param`, `dead_type`, `dead_trait_impl`,
  `unreachable_pattern` or `dead_closure_functor` lint exists.
- No `unused_assignments` lint reports a value overwritten before anything
  reads it. A read that only feeds the variable's own next value still reads
  it, so `n = n + 1` keeps `n` from `UnusedVariable`, where `n += 1` does not.
- There is no `unused` lint group, and no `#[deny(...)]`.
- Roots are computed per package, never across a workspace.
- `optimize/dce.rs` drops `pub`-only items, which a standalone `.wasm`
  published with provider metadata would need (see Reachability roots).
