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

- `export` is the only modifier that crosses the package boundary. It
  applies uniformly across `command`, `service`, and `lib` entry
  points: a `lib` package exposes `export` items as its public API
  (see [Package Manifest](./wep-2026-02-14-package-manifest.md)).
- `pub` is package-internal visibility. A `pub fn` that no caller
  invokes inside the package is dead code, even in a `lib` package —
  it is not part of the package's public API surface.
- The standard library is a separate package whose `pub` items are
  visible from user code under special rules. Stdlib must never receive
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

1. Reference pass — walks `ModuleSemantics.bindings` (use→def edges)
   for every user-authored module. Emits `UnusedImport`,
   `UnusedVariable`, `UnusedParameter`. A binding is reported when
   no edge has it as the def-side.
2. Liveness pass — the elaborate-time DCE pass introduced by
   [elaborator rearchitecture](./wep-2026-05-26-elaborator-rearchitecture.md).
   Computes source-level reachability from world-export roots over
   the cross-module call graph encoded in `bindings` and the dispatch
   facts. Emits `DeadFunction` and `DeadGlobal` for the source items
   the computation does not reach (its `Liveness::dead_items`).

Both passes are guarded by `CompilerOptions::unused_diagnostics` (on
by default). No package-kind toggle is needed: `export` already names
the complete set of package-external roots for every entry-point
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

### What is out of scope (deferred to follow-up WEPs / PRs)

- `unused_mut`, `unused_type_param`, `unused_assignment`
- `dead_type`, `dead_trait_impl`, `unreachable_pattern`, `dead_closure_functor`
- `#[allow(unused)]` / `#[deny(unused)]` attribute mechanism
- Per-`UseItem::InterfaceFunctions` granularity (function-level inside
  an interface import)
- Workspace-aware multi-package root computation

### Reachability roots (package-external boundary)

The pass runs two independent reachability closures over the same call graph:

- `E` — the **production** closure, seeded by the roots below.
- `T` — the **test** closure, seeded by `test` blocks only.

| Production root (seeds `E`)               | Source                                                                                                                  |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| Items satisfying world-export contracts   | Functions whose name and signature satisfy a world export (`command` / `service` / `lib`); identified during `annotate` |
| `#[export]`-attributed items              | Raw Wasm exports                                                                                                        |
| Items in `wasm_module_sources` re-exports | Bridged Wasm module exports                                                                                             |
| impl / trait methods                      | Seeded live as call-graph intermediaries (method-level dead detection deferred)                                         |

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

The production root set matches the existing optimize-time DCE root set in
`optimize/dce.rs` (`compute_reachable_from_entries`); the liveness
pass restates them at the source level so reachability can be
computed before TIR is emitted. Synthesised items — CM binding
wrappers, effect-dispatch helpers, monomorphisation clones,
auto-derived impls — are not in `Semantics` at all (they are born
during `synthesis` / `monomorphize`) and therefore cannot appear in
either the live set or the unused set. The optimize-time DCE
continues to remove them silently.

`is_pub` is never a root. In Wado it denotes package-internal
visibility, never package-external API — that is `export`'s job, and
`export` covers `lib` packages as well as `command` / `service`. An
unreferenced `pub fn` is dead code regardless of the entry-point
kind.

### Stdlib exclusion

Functions, imports, and locals whose `ModuleSource` is `Core`, `Wasi`,
`Builtin`, or `Wasm` are excluded from both passes when the entry
module is user-authored. Stdlib never emits unused diagnostics into the
user's build output.

### Suppression

- Variables and parameters whose source name begins with `_` are
  silent. This is the same convention rustc uses and is what
  `wado format` is expected to produce when auto-fixing.
- `Wildcard` imports (`use _ from "..."`) are silent — they exist for
  side effects.
- No attribute-based suppression in MVP; deferred to a follow-up that
  reuses the existing `#[...]` attribute machinery.

## Implementation

### Source files

| File                                              | Description                                                                                                                                                                               |
| ------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `wado-compiler/src/elaborator/liveness.rs`        | Owned by the elaborator rearchitecture. Computes source-level reachability and returns a `Liveness` value (live `SymbolKey` set, plus per-module unused-item / unused-binding lists).     |
| `wado-compiler/src/elaborator/liveness/unused.rs` | This WEP's contribution: walks the `Liveness` output plus `ModuleSemantics.bindings` and emits the five diagnostics. Stdlib exclusion, underscore suppression, and root policy live here. |
| `wado-compiler/src/compiler_host.rs`              | Adds `Code::UnusedImport`, `UnusedVariable`, `UnusedParameter`, `DeadFunction`, `DeadGlobal` and their `Display` mappings.                                                                |
| `wado-compiler/src/logger.rs`                     | Adds `Logger::warn_at(code, message, span, file)` for span-bearing warnings.                                                                                                              |
| `wado-compiler/src/lib.rs`                        | Adds `CompilerOptions::unused_diagnostics`. Invokes the diagnostic emitter once the elaborator has produced `Semantics` and its `Liveness`.                                               |
| `wado-compiler/src/ast_index.rs`                  | Adds `is_param(id)` predicate (1-bit table) so the reference pass can distinguish `UnusedVariable` from `UnusedParameter`.                                                                |
| `wado-cli`                                        | Adds `--no-unused` flag.                                                                                                                                                                  |
| `wado-lsp`                                        | Invokes the diagnostic emitter from `Engine::diagnostics`. Renders `UnusedImport` / `UnusedVariable` / `UnusedParameter` as `Hint`-severity unused-token decorations as well.             |

### Algorithms

#### Unused imports

Walk every `UseDecl` in user-authored modules. For each `UseItem`:

- `Simple { id, .. }` and `Namespace { name, .. }`: report
  `UnusedImport` when no entry in `Semantics::references` has the
  item's use-site `SymbolKey` as its `use_key` and no entry resolves to
  the item's imported `def_key`.
- `Wildcard`: skipped (side-effect imports).
- `InterfaceFunctions`: reported only at the whole-`UseDecl` level for
  MVP; per-function granularity is deferred.

If every item in a single `UseDecl` is unused, emit one diagnostic at
`UseDecl.span`; otherwise emit per-item diagnostics at
`AstIndex::name_span_of(item.id)`.

Prerequisite check: confirm `elaborator` records `UseItem::Simple.id` as
a use-site in `references` during use-decl resolution. If not, add the
single `record_reference_to_decl` call there.

#### Unused variables and parameters

Build a single inverted index from `Semantics::iter_references()`:

```
use_count: IndexMap<SymbolKey, usize>
for (_use_key, def_key) in annotated.iter_references():
    *use_count.entry(def_key).or_insert(0) += 1
```

Walk `Semantics::locals`. For each `(key, sym)` whose kind is
`Variable`:

- Skip if `sym.name` starts with `_`.
- Skip if `use_count.get(&key) > 0`.
- Determine `is_param` via `AstIndex::is_param(key.ast_id)` on the
  module's index. Emit `UnusedParameter` or `UnusedVariable` at
  `sym.span` (which is the `name_span` recorded by
  `record_local_symbol`).

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
- [ ] Add `AstIndex::is_param(id)` and tests (reference pass).

#### Phase 2 — reference pass

- [ ] Confirm the elaborator records `UseItem::Simple.id` as a use-site; patch if missing.
- [ ] Implement the reference-pass emitter (imports, locals, params) against `Semantics`.
- [ ] Wire into `compile_with_options` and `Engine::diagnostics`.
- [ ] Add fixtures under `tests/fixtures/unused_*.wado`; touch `tests/e2e.rs`.

#### Phase 3 — liveness pass and DeadFunction / DeadGlobal

- [x] Land `elaborator/liveness.rs`: the enclosing-item index (an AST
      id-collector), edge collection over `references`, the BFS, and the
      `Liveness` field on `Semantics`. Free functions and globals are
      precise; every method is seeded live as an intermediary (so the
      free-function reachability stays sound without the operator / `?` /
      for-of dispatch edges).
- [x] Implement the emitter (`DeadFunction`, `DeadGlobal`) consuming
      `Liveness::dead_items`; tests in `tests/unused_diagnostics.rs`.
- [x] Two-closure 3-way classification (`live` / `test-only` / `dead`): test
      blocks seed `T` only, never `E`; `live_items = E ∪ T` (reify gating
      unchanged). Adds `TestOnlyFunction` / `TestOnlyGlobal`, emitted in
      non-test builds. Unit tests in `tests/unused_diagnostics.rs`.
- [x] E2E warning assertions: `warnings_contains` / `warnings_not_contains`
      fixture-spec fields (`compile_capturing_warnings` in the harness);
      `dead_fn_*` / `dead_global_*` fixtures cover dead and test-only.
- [ ] Package-wide analysis so `T` is populated outside the entry module
      (load the whole package via `wado test` discovery; `wado check`
      package mode). Until then `test-only` only surfaces for `test` blocks
      in the compiled entry's own module graph.

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
-
  2. [ ] Close the liveness graph's cross-module gaps (foreign-keyed
         `references` from `with_module_perspective`, inlined-foreign-AST,
         namespace imports, test-world roots).
-
  3. [ ] Re-enable reify gating on `Liveness::live_items` and validate
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
      reflects the recording module's import perspective (user `with
      Stdout` → entry module; stdlib → `wasi:cli`). Effects are now
      canonicalised by name through the declaration index before
      comparison / closure lookup.
- [x] Effect-handler scopes: `with H => … do { … }` grants `H` to the
      do-block body (pushed / popped around the body walk).
- [x] Indirect calls through a function-typed parameter resolve the
      callee via the enclosing function's parameter types.
- [x] Variant-payload-nested resources: a `(module, variant) → payload
      types` map feeds the closure and `signature_resources`.
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
- [x] Declare the used capability on the three handlers (`with
      WaitableSet` / `with ErrorContext`); HTTP handlers accept a `with`
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
| `func.benign_effects`                    | `#[benign(E)]` on the AST `Function.attrs`                  |
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
3. For each user function: `current_effects = expand(declared effects +
   signature_resources(param/return/task-return types) + benign)`.
4. Walk the AST body; at each call site resolve the callee via the table
   above, compute `get_function_effects` (callee effects + resource
   effect for direct resource methods − benign, with effect-param
   resolution), and report `EffectError` for any effect not in
   `current_effects`.

Stores (1c) and purity (1d) follow the same data mapping; stores reuses
the ref-parameter detection (`ResolvedType::Ref` / `MutRef` over
`fn_param_types`).

#### Phase 4 — CLI wiring and reference pass

- [ ] `wado-cli`: add `--no-unused` flag for `compile` / `run` / `serve` / `dump`.
- [ ] Reference pass: `UnusedImport` / `UnusedVariable` / `UnusedParameter`.
- [ ] Method-level dead detection (the dispatch edges; stop seeding every method live).

### Test plan

E2E fixtures (under `tests/fixtures/`):

- `unused_import_basic.wado`
- `unused_import_partial.wado`
- `unused_import_wildcard_silent.wado`
- `unused_var_basic.wado`
- `unused_var_underscore_silent.wado`
- `unused_param_basic.wado`
- `unused_param_underscore_silent.wado`
- `dead_fn_private.wado`
- `dead_fn_export_root.wado`
- `dead_fn_generic.wado`
- `dead_fn_test_world.wado`
- `dead_fn_lib_pub_is_dead.wado`
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
- Risk: users coming from Rust expect `pub fn` in a `lib` package to
  be a public API root and may be surprised that it is reported as
  dead. Mitigation: the lint message names the rule
  ("`pub` is package-internal; use `export` to expose at the package
  boundary") and points at [Package Manifest](./wep-2026-02-14-package-manifest.md).
