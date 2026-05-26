# WEP: Unused Diagnostics

## Context

The Wado compiler today has no equivalent of rustc's `unused_imports`,
`unused_variables`, or `dead_code` lints. Users get no signal when
imports, locals, parameters, or private functions are written but never
consumed. The infrastructure for warnings exists (`Severity::Warning`,
`Logger`, `CompilerHost::emit_diagnostic`), and the analysis substrate
for the policy questions has matured — use→def edges (`Semantics::references`),
local-binding registry (`Semantics::locals`), per-module structural
indices (`AstIndex`), and the DCE reachability pass (`optimize/dce.rs`) —
but no pass turns those into user-facing diagnostics.

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

Add an unused-diagnostics subsystem split across two layers, each
producing `Severity::Warning` diagnostics through the existing
`CompilerHost`. The two layers map cleanly onto rustc's split between
HIR-level `unused_*` lints and `dead_code` reachability:

1. Semantics layer — runs immediately after `semantics_of`, on
   `&Semantics`. Emits `UnusedImport`, `UnusedVariable`,
   `UnusedParameter`. LSP and batch compilation share this pass for
   free.
2. NIR + DCE layer — runs as hooks inside `optimize.rs::run_dce`, on
   each iteration of DCE's fixed-point loop, between reachability
   analysis and removal. Emits `DeadFunction` and `DeadGlobal`. A
   cross-iteration dedup set keyed by `SymbolKey` ensures each item
   is reported at most once.

Both passes are guarded by `CompilerOptions::unused_diagnostics` (on by
default). No package-kind toggle is needed: `export` already names the
complete set of package-external roots for every entry-point kind
(`command`, `service`, `lib`).

### What is in scope (MVP)

| Lint              | Layer          | Code              |
| ----------------- | -------------- | ----------------- |
| `UnusedImport`    | Semantics      | `UnusedImport`    |
| `UnusedVariable`  | Semantics      | `UnusedVariable`  |
| `UnusedParameter` | Semantics      | `UnusedParameter` |
| `DeadFunction`    | NIR (post-DCE) | `DeadFunction`    |
| `DeadGlobal`      | NIR (post-DCE) | `DeadGlobal`      |

### What is out of scope (deferred to follow-up WEPs / PRs)

- `unused_mut`, `unused_type_param`, `unused_assignment`
- `dead_type`, `dead_trait_impl`, `unreachable_pattern`, `dead_closure_functor`
- `#[allow(unused)]` / `#[deny(unused)]` attribute mechanism
- Per-`UseItem::InterfaceFunctions` granularity (function-level inside
  an interface import)
- Workspace-aware multi-package root computation

### Reachability roots (package-external boundary)

The DCE layer treats these as always-reachable:

| Root                                 | Source                                                                              |
| ------------------------------------ | ----------------------------------------------------------------------------------- |
| `is_cm_export`                       | World export wrappers from synthesis (covers `command` / `service` / `lib` exports) |
| `is_export` in `wasm_module_sources` | Raw Wasm exports                                                                    |

This matches the existing DCE root set in `optimize/dce.rs`
(`compute_reachable_from_entries`). No new roots are introduced.
Synthesised functions and globals stay subject to the normal call-graph
rules: a CM binding for an unreachable export, or an auto-derived impl
that nothing references, is still removed by DCE. The synthesis
exclusion only suppresses the diagnostic emission, never the removal.

`is_pub` is never a root. In Wado it denotes package-internal
visibility, never package-external API — that is `export`'s job, and
`export` covers `lib` packages as well as `command` / `service`. An
unreferenced `pub fn` is dead code regardless of the entry-point kind.

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

### Defining-ast-id field

`TirFunction` / `NirFunction` and `TirGlobal` / `NirGlobal` each gain a
`defining_ast_id: Option<AstId>` field. Together with `module_source`,
this forms a `SymbolKey` back to the originating AST node — the
canonical identity already used by `Semantics`, the symbol table, and
the LSP query API. `None` marks synthesised items, which are excluded
from `DeadFunction` / `DeadGlobal` reporting (but still subject to
normal DCE removal).

This is the only structural change required outside the new
`analyze/unused.rs` module.

## Implementation

### Source files

| File                                  | Description                                                                                                                                                                                    |
| ------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `wado-compiler/src/analyze/unused.rs` | New module hosting `check_unused` (Semantics layer), `emit_dead_function_diagnostics`, and `emit_dead_global_diagnostics` (DCE-loop hooks).                                                    |
| `wado-compiler/src/compiler_host.rs`  | Adds `Code::UnusedImport`, `UnusedVariable`, `UnusedParameter`, `DeadFunction`, `DeadGlobal` and their `Display` mappings.                                                                     |
| `wado-compiler/src/logger.rs`         | Adds `Logger::warn_at(code, message, span, file)` for span-bearing warnings.                                                                                                                   |
| `wado-compiler/src/lib.rs`            | Adds `CompilerOptions::unused_diagnostics`. Calls `check_unused` after `semantics_of`. The DCE-layer diagnostic emitters are invoked from `optimize.rs::run_dce`.                              |
| `wado-compiler/src/tir.rs`            | `TirFunction::defining_ast_id: Option<AstId>`, `TirGlobal::defining_ast_id: Option<AstId>`.                                                                                                    |
| `wado-compiler/src/nir.rs`            | `NirFunction::defining_ast_id: Option<AstId>`, `NirGlobal::defining_ast_id: Option<AstId>`.                                                                                                    |
| `wado-compiler/src/optimize/dce.rs`   | Adds `pub(crate) fn unreachable_function_source_keys(&NirPackage, &IndexSet<FunctionId>) -> Vec<SymbolKey>` and `pub(crate) fn unreachable_global_source_keys(&NirPackage) -> Vec<SymbolKey>`. |
| `wado-compiler/src/optimize.rs`       | `run_dce` invokes the diagnostic emitters inside its fixed-point loop, with a cross-iteration dedup set keyed by `SymbolKey`.                                                                  |
| `wado-compiler/src/ast_index.rs`      | Adds `is_param(id)` predicate (1-bit table) so the locals pass can distinguish `UnusedVariable` from `UnusedParameter`.                                                                        |
| `wado-cli`                            | Adds `--no-unused` flag.                                                                                                                                                                       |
| `wado-lsp`                            | Calls `analyze::unused::check_unused` from `Engine::diagnostics`.                                                                                                                              |

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

`optimize.rs::run_dce` runs DCE in a fixed-point loop:
`analyze_project` → `remove_unreachable_functions` →
`remove_unreachable_globals`, repeating until the function set
stabilises. The cascade is real (removing a dead global may rewrite
bodies and orphan a previously-reachable callee), so a single pre-loop
snapshot is not enough.

Strategy: emit diagnostics inside the loop, on each iteration, with a
`reported: IndexSet<SymbolKey>` carried across iterations to dedup.

At the start of each iteration, after `analyze_project` returns the
reachable set:

1. `dce::unreachable_function_source_keys(project, &reachable)` walks
   `project.functions` and returns the set of source `SymbolKey`s for
   functions whose every monomorphisation is unreachable, skipping
   entries with `defining_ast_id == None` (synthesised) and entries in
   stdlib modules. Grouping by `(module_source, defining_ast_id)`
   collapses monomorphisations to a single source-level key.
2. `dce::unreachable_global_source_keys(project)` returns the source
   keys for globals not referenced by any reachable function, with the
   same exclusions.
3. The emitter inserts each new key into `reported` and calls
   `Logger::warn_at` with `Code::DeadFunction` or `Code::DeadGlobal`,
   span from `Semantics::name_span_of(key)` (fallback to the item's
   `span`).

`is_export` / `is_cm_export` items are already roots, so they will not
appear in the unreachable set. The diagnostic emitter does not need
extra checks for them.

The dedup set ensures a function or global is reported at most once
even though it may be observed across multiple iterations
(`analyze_project` is called each iteration; only items the previous
iteration's `remove_unreachable_*` actually removed are gone from
`project.functions` / `project.globals` next time round).

### Pipeline integration

```
parse → bind → desugar → load → analyze → annotate
                                            │
                                            ▼
                                   check_unused
                                  (imports, locals, params)
                                            │
                                            ▼
                              build_tir → monomorphize
                                  → lower → optimize
                                            │
                                            ▼
                              run_dce (fixed-point loop)
                              ┌───────────────────────────┐
                              │ analyze_project           │
                              │   │                       │
                              │   ▼                       │
                              │ emit dead-function /      │
                              │   dead-global diagnostics │
                              │   (dedup across iters)    │
                              │   │                       │
                              │   ▼                       │
                              │ remove_unreachable_*      │
                              │   │                       │
                              │   ▼                       │
                              │ converged? ──no──┐        │
                              │   │ yes          │        │
                              │   ▼              │        │
                              └───┼──────────────┘        │
                                  ▼                       │
                                                          ▼
                                                       codegen
```

`compile_with_options` gates both layers on
`CompilerOptions::unused_diagnostics`. `Engine::diagnostics` (LSP)
calls `check_unused` after `semantics_of` without any extra cost.

### Migration plan

#### Phase 1 — diagnostic plumbing

- [ ] Add `Code::UnusedImport`, `UnusedVariable`, `UnusedParameter`, `DeadFunction`, `DeadGlobal` and their `Display` strings.
- [ ] Add `Logger::warn_at(code, message, span, file)`.
- [ ] Add `CompilerOptions::unused_diagnostics` (default `true`).
- [ ] Add `AstIndex::is_param(id)` and tests.

#### Phase 2 — `defining_ast_id` propagation

- [ ] Add `TirFunction::defining_ast_id: Option<AstId>` and `TirGlobal::defining_ast_id: Option<AstId>`; default to `None`.
- [ ] Set `Some(function.id)` / `Some(global.id)` at every source-authored construction site (in `elaborator`).
- [ ] Add `NirFunction::defining_ast_id` and `NirGlobal::defining_ast_id`; propagate from TIR through `link`, `monomorphize`, `erase`, `lower`. Monomorphisation clones inherit the original `defining_ast_id`.
- [ ] Synthesis sites (`synthesis/cm_binding.rs`, `synthesis/effect_dispatch.rs`, auto-derives, etc.) leave the field as `None`.
- [ ] No behaviour change in this phase — codegen output is bit-identical.

#### Phase 3 — Semantics-layer lints

- [ ] Confirm `elaborator` records `UseItem::Simple.id` as a use-site; patch if missing.
- [ ] Implement `analyze::unused::check_unused` (imports, locals, params).
- [ ] Wire into `lib.rs::compile_with_options` and `wado-lsp::Engine::diagnostics`.
- [ ] Add fixtures under `tests/fixtures/unused_*.wado`; touch `tests/e2e.rs`.

#### Phase 4 — DCE-layer dead-function lint

- [ ] Add `pub(crate) fn unreachable_function_source_keys(&NirPackage, &IndexSet<FunctionId>) -> Vec<SymbolKey>` to `optimize/dce.rs`. Skip entries with `defining_ast_id == None` or stdlib module sources. Group monomorphisations by `(module_source, defining_ast_id)`.
- [ ] Implement `analyze::unused::emit_dead_function_diagnostics` consuming the helper above.
- [ ] Modify `optimize.rs::run_dce` to thread a `reported: IndexSet<SymbolKey>` through the fixed-point loop and invoke the emitter inside it, before the `remove_unreachable_*` calls.
- [ ] Add fixtures under `tests/fixtures/dead_fn_*.wado`.

#### Phase 5 — DCE-layer dead-global lint

- [ ] Add `pub(crate) fn unreachable_global_source_keys(&NirPackage) -> Vec<SymbolKey>` to `optimize/dce.rs`, mirroring the function variant. Use the analysis already performed inside `remove_unreachable_globals` (extract the reachability predicate if needed).
- [ ] Implement `analyze::unused::emit_dead_global_diagnostics`.
- [ ] Wire into `run_dce` next to the dead-function emission, sharing the `reported` dedup set.
- [ ] Add fixtures under `tests/fixtures/dead_global_*.wado`.

#### Phase 6 — CLI wiring

- [ ] `wado-cli`: add `--no-unused` flag for `compile` / `run` / `serve` / `dump`.

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
- LSP "greyed-out unused" works for free because the Semantics layer
  shares a code path between batch compilation and `Engine::diagnostics`.
- DCE keeps doing its silent removal job; the only addition is a
  diagnostic hook in front of removal.
- `defining_ast_id` becomes a reusable back-pointer for future
  diagnostics and tooling (e.g., span-aware optimisation traces).

### Costs

- One new optional field on each of `TirFunction`, `NirFunction`,
  `TirGlobal`, `NirGlobal`. Memory cost is negligible
  (`Option<AstId>` is a `u32` + niche).
- Every TIR construction site for a source-authored function or
  global gains one line to thread the AST id through. The change is
  mechanical but touches several files (`elaborator/item.rs`, closure
  construction, global lowering).
- `optimize.rs::run_dce` gains a `reported: IndexSet<SymbolKey>` set
  threaded through its fixed-point loop. No structural change to the
  loop itself.
- New warnings will fire on every existing Wado source file the first
  time CI runs the new compiler. The MVP defaults `unused_diagnostics`
  to `true`; a one-time cleanup pass on the in-tree examples and
  fixtures is part of Phase 3 / Phase 4 / Phase 5 landing.

### Risks and mitigations

- Risk: synthesised functions accidentally receive `Some(ast_id)` and
  start emitting spurious dead-code warnings. Mitigation:
  `defining_ast_id` defaults to `None`; synthesis sites must
  affirmatively opt in. Tests cover the synthesis paths.
- Risk: a generic function with all monomorphisations unreachable is
  reported once per group, but rare edge cases (specialisation through
  effect dispatch) could leave one monomorph reachable from an
  unreported path. Mitigation: rely on the existing DCE call-graph,
  which already handles these edges for removal.
- Risk: DCE's fixed-point loop interacts with the diagnostic emission
  — a function that becomes dead only after a global is dropped on
  iteration 2 must still be reported, while items already reported on
  iteration 1 must not be repeated. Mitigation: the cross-iteration
  `reported: IndexSet<SymbolKey>` dedup set; tests in
  `dead_global_used_by_dead_fn.wado` and
  `dead_fn_cascade_via_global.wado` exercise the cascade.
- Risk: users coming from Rust expect `pub fn` in a `lib` package to
  be a public API root and may be surprised that it is reported as
  dead. Mitigation: the lint message names the rule
  ("`pub` is package-internal; use `export` to expose at the package
  boundary") and points at [Package Manifest](./wep-2026-02-14-package-manifest.md).
