# WEP: Unused Diagnostics

## Context

The Wado compiler today has no equivalent of rustc's `unused_imports`,
`unused_variables`, or `dead_code` lints. Users get no signal when
imports, locals, parameters, or private functions are written but never
consumed. The infrastructure for warnings exists (`Severity::Warning`,
`Logger`, `CompilerHost::emit_diagnostic`), and the analysis substrate
for the policy questions has matured — use→def edges (`Annotated::references`),
local-binding registry (`Annotated::locals`), per-module structural
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

1. Annotated layer — runs immediately after `annotate_loaded`, on
   `&Annotated`. Emits `UnusedImport`, `UnusedVariable`,
   `UnusedParameter`. LSP and batch compilation share this pass for
   free.
2. NIR + DCE layer — runs as a hook inside `optimize/dce.rs`,
   after reachability is computed but before unreachable functions are
   removed. Emits `DeadFunction`.

Both passes are guarded by `CompilerOptions::unused_diagnostics` (on by
default). No package-kind toggle is needed: `export` already names the
complete set of package-external roots for every entry-point kind
(`command`, `service`, `lib`).

### What is in scope (MVP)

| Lint              | Layer          | Code              |
| ----------------- | -------------- | ----------------- |
| `UnusedImport`    | Annotated      | `UnusedImport`    |
| `UnusedVariable`  | Annotated      | `UnusedVariable`  |
| `UnusedParameter` | Annotated      | `UnusedParameter` |
| `DeadFunction`    | NIR (post-DCE) | `DeadFunction`    |

### What is out of scope (deferred to follow-up WEPs / PRs)

- `unused_mut`, `unused_type_param`, `unused_assignment`
- `dead_type`, `dead_trait_impl`, `unreachable_pattern`
- `#[allow(unused)]` / `#[deny(unused)]` attribute mechanism
- Per-`UseItem::InterfaceFunctions` granularity (function-level inside
  an interface import)
- Workspace-aware multi-package root computation

### Reachability roots (package-external boundary)

The DCE layer treats these as always-reachable:

| Root                                 | Source                                                                                 |
| ------------------------------------ | -------------------------------------------------------------------------------------- |
| `is_cm_export`                       | World export wrappers from synthesis (covers `command` / `service` / `lib` exports)    |
| `is_export` in `wasm_module_sources` | Raw Wasm exports                                                                       |
| `defining_ast_id.is_none()`          | Synthesised functions (CM bindings, dispatch wrappers, monomorph clones, auto-derives) |

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

`TirFunction` and `NirFunction` gain a `defining_ast_id: Option<AstId>`
field. Together with `module_source`, this forms a `SymbolKey` back to
the originating AST node — the canonical identity already used by
`Annotated`, the symbol table, and the LSP query API. `None` marks
synthesised functions, which automatically:

- become DCE roots (no source location to warn at), and
- are excluded from `DeadFunction` reporting.

This is the only structural change required outside the new
`analyze/unused.rs` module.

## Implementation

### Source files

| File                                  | Description                                                                                                                                 |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `wado-compiler/src/analyze/unused.rs` | New module hosting `check_unused` (Annotated layer) and `check_dead_functions` (DCE hook).                                                  |
| `wado-compiler/src/compiler_host.rs`  | Adds `Code::UnusedImport`, `UnusedVariable`, `UnusedParameter`, `DeadFunction` and their `Display` mappings.                                |
| `wado-compiler/src/logger.rs`         | Adds `Logger::warn_at(code, message, span, file)` for span-bearing warnings.                                                                |
| `wado-compiler/src/lib.rs`            | Adds `CompilerOptions::unused_diagnostics`. Calls `check_unused` after `annotate_loaded` and `check_dead_functions` after DCE reachability. |
| `wado-compiler/src/tir.rs`            | `TirFunction::defining_ast_id: Option<AstId>`.                                                                                              |
| `wado-compiler/src/nir.rs`            | `NirFunction::defining_ast_id: Option<AstId>`.                                                                                              |
| `wado-compiler/src/optimize/dce.rs`   | Splits `analyze_project` into reachability computation and removal so the diagnostic hook fits between them.                                |
| `wado-compiler/src/ast_index.rs`      | Adds `is_param(id)` predicate (1-bit table) so the locals pass can distinguish `UnusedVariable` from `UnusedParameter`.                     |
| `wado-cli`                            | Adds `--no-unused` flag.                                                                                                                    |
| `wado-lsp`                            | Calls `analyze::unused::check_unused` from `Engine::diagnostics`.                                                                           |

### Algorithms

#### Unused imports

Walk every `UseDecl` in user-authored modules. For each `UseItem`:

- `Simple { id, .. }` and `Namespace { name, .. }`: report
  `UnusedImport` when no entry in `Annotated::references` has the
  item's use-site `SymbolKey` as its `use_key` and no entry resolves to
  the item's imported `def_key`.
- `Wildcard`: skipped (side-effect imports).
- `InterfaceFunctions`: reported only at the whole-`UseDecl` level for
  MVP; per-function granularity is deferred.

If every item in a single `UseDecl` is unused, emit one diagnostic at
`UseDecl.span`; otherwise emit per-item diagnostics at
`AstIndex::name_span_of(item.id)`.

Prerequisite check: confirm `resolver` records `UseItem::Simple.id` as
a use-site in `references` during use-decl resolution. If not, add the
single `record_reference_to_decl` call there.

#### Unused variables and parameters

Build a single inverted index from `Annotated::iter_references()`:

```
use_count: IndexMap<SymbolKey, usize>
for (_use_key, def_key) in annotated.iter_references():
    *use_count.entry(def_key).or_insert(0) += 1
```

Walk `Annotated::locals`. For each `(key, sym)` whose kind is
`Variable`:

- Skip if `sym.name` starts with `_`.
- Skip if `use_count.get(&key) > 0`.
- Determine `is_param` via `AstIndex::is_param(key.ast_id)` on the
  module's index. Emit `UnusedParameter` or `UnusedVariable` at
  `sym.span` (which is the `name_span` recorded by
  `record_local_symbol`).

#### Dead functions

Group all NIR functions by `(module_source, defining_ast_id)`. For each
group, compute whether any monomorphisation is in the reachable set
from `compute_reachable_from_entries`. Emit `DeadFunction` for the
group when:

- `defining_ast_id` is `Some` (skip synthesised),
- the module is not a stdlib module,
- the function is not `is_export` / `is_cm_export`,
- no monomorphisation is reachable.

The diagnostic's span is `Annotated::name_span_of(SymbolKey)`,
falling back to `NirFunction::span`.

### Pipeline integration

```
parse → bind → desugar → load → analyze → annotate
                                            │
                                            ▼
                                   check_unused
                                  (imports, locals, params)
                                            │
                                            ▼
                              lower_tir → monomorphize
                                  → lower → optimize
                                            │
                                            ▼
                          compute_reachable_from_entries
                                            │
                                            ▼
                                check_dead_functions
                                            │
                                            ▼
                                  remove unreachable
                                            │
                                            ▼
                                       codegen
```

`compile_with_options` gates both passes on
`CompilerOptions::unused_diagnostics`. `Engine::diagnostics` (LSP)
calls `check_unused` after `annotate_loaded` without any extra cost.

### Migration plan

#### Phase 1 — diagnostic plumbing

- [ ] Add `Code::UnusedImport`, `UnusedVariable`, `UnusedParameter`, `DeadFunction` and their `Display` strings.
- [ ] Add `Logger::warn_at(code, message, span, file)`.
- [ ] Add `CompilerOptions::unused_diagnostics` (default `true`).
- [ ] Add `AstIndex::is_param(id)` and tests.

#### Phase 2 — `defining_ast_id` propagation

- [ ] Add `TirFunction::defining_ast_id: Option<AstId>`; default to `None`.
- [ ] Set `Some(function.id)` at every source-authored construction site (in `resolver`).
- [ ] Add `NirFunction::defining_ast_id`; propagate from TIR through `link`, `monomorphize`, `erase`, `lower`. Monomorphisation clones inherit the original `defining_ast_id`.
- [ ] Synthesis sites (`synthesis/cm_binding.rs`, `synthesis/effect_dispatch.rs`, auto-derives, etc.) leave the field as `None`.
- [ ] No behaviour change in this phase — codegen output is bit-identical.

#### Phase 3 — Annotated-layer lints

- [ ] Confirm `resolver` records `UseItem::Simple.id` as a use-site; patch if missing.
- [ ] Implement `analyze::unused::check_unused` (imports, locals, params).
- [ ] Wire into `lib.rs::compile_with_options` and `wado-lsp::Engine::diagnostics`.
- [ ] Add fixtures under `tests/fixtures/unused_*.wado`; touch `tests/e2e.rs`.

#### Phase 4 — DCE-layer dead-function lint

- [ ] Split `optimize/dce.rs::analyze_project` into a reachability function and a removal function.
- [ ] Implement `check_dead_functions`.
- [ ] Wire `defining_ast_id.is_none()` as an additional DCE root so synthesised functions stay alive without being reported.
- [ ] Add fixtures under `tests/fixtures/dead_*.wado`.

#### Phase 5 — CLI wiring

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
- `dead_function_private.wado`
- `dead_function_export_root.wado`
- `dead_function_generic.wado`
- `dead_function_test_world.wado`
- `dead_function_lib_pub_is_dead.wado`
- `unused_stdlib_no_report.wado`

Each carries the appropriate `stderr_contains` entries in its `__DATA__`
block. `wado-lsp` integration tests cover the Annotated-layer lints
through the diagnostics path.

## Consequences

### Benefits

- Users get immediate, location-precise feedback on dead imports,
  locals, parameters, and private functions, matching the experience
  every rustc user expects.
- LSP "greyed-out unused" works for free because the Annotated layer
  shares a code path between batch compilation and `Engine::diagnostics`.
- DCE keeps doing its silent removal job; the only addition is a
  diagnostic hook in front of removal.
- `defining_ast_id` becomes a reusable back-pointer for future
  diagnostics and tooling (e.g., span-aware optimisation traces).

### Costs

- One new optional field on `TirFunction` and `NirFunction`. Memory
  cost is negligible (`Option<AstId>` is a `u32` + niche).
- Every TIR construction site for a source-authored function gains one
  line to thread the AST id through. The change is mechanical but
  touches several files (`resolver/item.rs`, closure construction,
  global lowering).
- The DCE pass's public shape changes (one function becomes two). All
  current callers live in `optimize.rs`; the refactor is local.
- New warnings will fire on every existing Wado source file the first
  time CI runs the new compiler. The MVP defaults `unused_diagnostics`
  to `true`; a one-time cleanup pass on the in-tree examples and
  fixtures is part of Phase 3 / Phase 4 landing.

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
- Risk: users coming from Rust expect `pub fn` in a `lib` package to
  be a public API root and may be surprised that it is reported as
  dead. Mitigation: the lint message names the rule
  ("`pub` is package-internal; use `export` to expose at the package
  boundary") and points at [Package Manifest](./wep-2026-02-14-package-manifest.md).
