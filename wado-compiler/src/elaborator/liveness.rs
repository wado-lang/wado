//! Source-level liveness / dead-code analysis.
//!
//! Implements the `liveness` pass described in
//! [`wep-2026-05-16-unused-diagnostics.md`] (policy) and
//! [`wep-2026-05-26-elaborator-rearchitecture.md`] (mechanism). The pass
//! runs after `annotate_bodies` and before `reify`, computing source-level
//! reachability from the export boundary over the call graph the elaborator
//! recorded in `references`.
//!
//! # Current scope
//!
//! The pass classifies **free functions and globals** as live / test-only /
//! dead (see [`Liveness`]) and reports `DeadFunction` / `DeadGlobal` /
//! `TestOnlyFunction` / `TestOnlyGlobal` accordingly. Reify gates emission of
//! those two item kinds on `live_items` (`E ∪ T`). Every impl/trait method is
//! seeded as a production root, so it serves as a live intermediary in the call
//! graph without itself being a report or gating candidate. This keeps the
//! analysis sound against false positives (the failure the WEP optimises
//! against) while deferring method-level dead detection — which needs the
//! operator / `?` / for-of dispatch edges that leave no `references` entry — to
//! a follow-up slice.
//!
//! The graph traces every site where reify can emit a call: function and
//! method bodies, global initializers, parameter defaults, and struct field
//! defaults. A callee reachable only through one of those still stays live.
//!
//! # Suppression
//!
//! `#[allow(dead_code)]` on a function or global (or `#![allow(dead_code)]` at
//! the module level, covering every item in the file) drops the item from the
//! lint's report candidates while leaving its call-graph edges intact, so a
//! callee it reaches still stays live. The attribute name matches rustc's
//! `dead_code` lint.

use crate::ast::{self, AstId, AstVisitor, Block, Expr, Function, Item, Module};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::token::Span;

/// Result of the source-level liveness analysis.
///
/// Reachability is computed from two independent root sets over the same call
/// graph: `E` = reachable from production roots (world exports, `#[export]`,
/// methods, struct-field defaults), `T` = reachable from `test` blocks. Each
/// user-authored free function / global is then classified:
///
/// - **live** (`∈ E`): used by production. Not reported.
/// - **test-only** (`∈ T \ E`): used by tests but not production → `test_only_items`.
/// - **dead** (`∉ E ∧ ∉ T`): used by neither → `dead_items`.
///
/// `live_items = E ∪ T` is the set reify gates emission on (a test-reachable
/// item is still kept so test code compiles); the split only affects which
/// diagnostic the emitter raises.
#[derive(Default, Clone)]
pub(crate) struct Liveness {
    /// Reachable from production roots ∪ tests (`E ∪ T`). Reify gates
    /// free-function / global emission on this set.
    pub(crate) live_items: IndexSet<AstId>,
    /// Candidates reachable from neither production nor tests (`∉ E ∧ ∉ T`),
    /// in source order. `DeadFunction` / `DeadGlobal`.
    pub(crate) dead_items: Vec<AstId>,
    /// Candidates reachable from tests but not production (`∈ T \ E`), in
    /// source order. `TestOnlyFunction` / `TestOnlyGlobal`.
    pub(crate) test_only_items: Vec<AstId>,
}

/// Compute liveness over every loaded module.
///
/// `world_export_names` holds every export name across all registered worlds; a
/// function whose name matches is a potential world entry point and is seeded
/// as a root, so a misdeclared entry (`fn run()` without `export`) survives
/// reify gating and still reaches the world-conformance check.
pub(crate) fn compute(
    modules: &IndexMap<ModuleSource, Module>,
    references: &IndexMap<AstId, AstId>,
    world_export_names: &IndexSet<String>,
) -> Liveness {
    let mut graph = Graph::default();

    for (source, module) in modules {
        // `#![generated]` modules are machine-emitted (e.g. Gale's parser
        // output), not hand-edited source — linting them is pure noise, so they
        // are never report candidates. They still seed exports / edges below, so
        // they keep the items they call live.
        let user = is_user_authored(source) && !module.has_generated();
        // A file-level `#![allow(dead_code)]` waives the lint for every item in
        // the module — the idiom for test-helper files whose functions exist
        // only to back `test` blocks.
        let module_allows_dead = inner_attrs_allow_dead_code(&module.inner_attributes);
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    let key = func.id;
                    graph.add_function_edges(func, references, &key);
                    if func.is_export
                        || has_export_attr(func)
                        || world_export_names.contains(&func.name)
                    {
                        graph.seed_export(key);
                    }
                    // Bodyless functions are compiler builtins / imports, not
                    // user-authored code that could be "dead". `#[allow(dead_code)]`
                    // (item- or module-level) opts an item out of the lint while
                    // leaving its call-graph edges intact.
                    if user
                        && func.body.is_some()
                        && !module_allows_dead
                        && !attrs_allow_dead_code(&func.attrs)
                    {
                        graph.report_candidates.push(key);
                    }
                }
                Item::Global(global) => {
                    let key = global.id;
                    graph.add_expr_edges(&global.initializer, references, &key);
                    if user && !module_allows_dead && !attrs_allow_dead_code(&global.attributes) {
                        graph.report_candidates.push(key);
                    }
                }
                Item::Struct(struct_decl) => {
                    // A field default is materialized by reify wherever the
                    // struct is built with the field omitted (and by the
                    // auto-derived `Default::default`), so any function it
                    // references must stay live. We cannot cheaply tell whether
                    // the struct is ever constructed, so seed the struct as a
                    // root and edge it to its field defaults — sound against
                    // dropping a reachable callee. Structs are not report
                    // candidates, so the extra live entry is harmless.
                    let mut has_default = false;
                    let key = struct_decl.id;
                    for field in &struct_decl.fields {
                        if let Some(default) = &field.default {
                            graph.add_expr_edges(default, references, &key);
                            has_default = true;
                        }
                    }
                    if has_default {
                        graph.seed_export(key);
                    }
                }
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        let key = method.id;
                        graph.add_function_edges(method, references, &key);
                        // Slice 1: methods are live production intermediaries.
                        graph.seed_export(key);
                    }
                }
                Item::Trait(trait_decl) => {
                    for method in &trait_decl.methods {
                        if method.body.is_none() {
                            continue;
                        }
                        let key = method.id;
                        graph.add_function_edges(method, references, &key);
                        graph.seed_export(key);
                    }
                }
                Item::Test(test) => {
                    // Test blocks are roots of the `T` (test-reachable) closure
                    // only — never the production `E` closure. A function reached
                    // solely from a test is therefore classified `test-only`
                    // rather than live, so genuinely dead production code is not
                    // masked by a lingering test reference.
                    let key = test.id;
                    graph.add_block_edges(&test.body, references, &key);
                    graph.seed_test(key);
                }
                _ => {}
            }
        }
    }

    graph.finish()
}

/// Call graph plus the two root sets and report candidates, assembled in one
/// walk.
#[derive(Default)]
struct Graph {
    /// `owner -> called items`.
    edges: IndexMap<AstId, Vec<AstId>>,
    /// Production roots (world exports, `#[export]`, methods, struct-field
    /// defaults) — seeds of the `E` closure.
    export_seeds: Vec<AstId>,
    /// `test` block roots — seeds of the `T` closure.
    test_seeds: Vec<AstId>,
    /// User-authored free functions / globals eligible for dead reporting,
    /// in source order.
    report_candidates: Vec<AstId>,
}

impl Graph {
    fn seed_export(&mut self, key: AstId) {
        self.export_seeds.push(key);
    }

    fn seed_test(&mut self, key: AstId) {
        self.test_seeds.push(key);
    }

    fn add_function_edges(
        &mut self,
        func: &Function,
        references: &IndexMap<AstId, AstId>,
        owner: &AstId,
    ) {
        if let Some(body) = &func.body {
            self.add_block_edges(body, references, owner);
        }
        // A parameter default is materialized by reify at every call site that
        // omits the argument, so anything it references is reachable whenever
        // the function itself is. Edge from the function (not a seed) keeps that
        // precise: a dead function's defaults stay dead too.
        for param in &func.params {
            if let Some(default) = &param.default {
                self.add_expr_edges(default, references, owner);
            }
        }
    }

    fn add_block_edges(
        &mut self,
        block: &Block,
        references: &IndexMap<AstId, AstId>,
        owner: &AstId,
    ) {
        let mut collector = IdCollector::default();
        ast::walk_block(&mut collector, block);
        self.link(&collector.ids, references, owner);
    }

    fn add_expr_edges(&mut self, expr: &Expr, references: &IndexMap<AstId, AstId>, owner: &AstId) {
        let mut collector = IdCollector::default();
        ast::walk_expr(&mut collector, expr);
        self.link(&collector.ids, references, owner);
    }

    /// For each id in the owner's body that resolves to a definition, add an
    /// `owner -> def` edge.
    fn link(&mut self, ids: &[AstId], references: &IndexMap<AstId, AstId>, owner: &AstId) {
        for &id in ids {
            if let Some(def) = references.get(&id) {
                self.edges.entry(*owner).or_default().push(*def);
            }
        }
    }

    /// Run both reachability closures and classify each report candidate.
    fn finish(self) -> Liveness {
        let production = self.closure(&self.export_seeds);
        let tests = self.closure(&self.test_seeds);

        let mut live_items = production.clone();
        for key in &tests {
            live_items.insert(*key);
        }

        let mut dead_items = Vec::new();
        let mut test_only_items = Vec::new();
        for key in &self.report_candidates {
            if production.contains(key) {
                continue;
            }
            if tests.contains(key) {
                test_only_items.push(*key);
            } else {
                dead_items.push(*key);
            }
        }

        Liveness {
            live_items,
            dead_items,
            test_only_items,
        }
    }

    /// BFS reachability from `seeds` over the call-graph edges.
    fn closure(&self, seeds: &[AstId]) -> IndexSet<AstId> {
        let mut reached: IndexSet<AstId> = IndexSet::default();
        let mut work: Vec<AstId> = seeds.to_vec();
        while let Some(key) = work.pop() {
            if !reached.insert(key) {
                continue;
            }
            if let Some(targets) = self.edges.get(&key) {
                for target in targets {
                    if !reached.contains(target) {
                        work.push(*target);
                    }
                }
            }
        }
        reached
    }
}

/// Visitor that records every [`AstId`] it traverses.
#[derive(Default)]
struct IdCollector {
    ids: Vec<AstId>,
}

impl AstVisitor for IdCollector {
    fn visit_id(&mut self, id: AstId, _span: Span) {
        self.ids.push(id);
    }
}

/// User-authored modules are the entry point and the files / URLs it
/// transitively imports. Stdlib is never reported — and never reify-gated:
/// stdlib functions reached only through compiler synthesis (CM bindings,
/// effect dispatch) have no source-level caller, so the optimize-time DCE
/// removes their dead ones.
///
/// Stdlib lives in the `Core` / `Wasi` / `Wasm` variants *and* in bundled
/// `.wado` files that the loader registers as `Local` with a scheme-prefixed
/// path (`wasi:cli/terminal_stdout.wado`, `core:…`). Those must be excluded
/// too; a user's `Local` import is a relative path with no such scheme.
pub(crate) fn is_user_authored(source: &ModuleSource) -> bool {
    match source {
        ModuleSource::EntryPoint { .. }
        | ModuleSource::Remote { .. }
        | ModuleSource::Redirected { .. } => true,
        ModuleSource::Local { path } => {
            let path = path.as_str();
            !(path.starts_with("core:") || path.starts_with("wasi:") || path.starts_with("wasm:"))
        }
        _ => false,
    }
}

/// `#[export]` marks a raw Wasm export — an export-boundary root.
fn has_export_attr(func: &Function) -> bool {
    func.attrs.iter().any(|attr| attr.name == "export")
}

/// True if `args` name the `dead_code` lint, as in `allow(dead_code)`.
fn args_name_dead_code(args: &[ast::AttrArg]) -> bool {
    args.iter()
        .any(|arg| matches!(arg, ast::AttrArg::Ident(name) if name == "dead_code"))
}

/// `#[allow(dead_code)]` on an item waives its unused / test-only lint.
fn attrs_allow_dead_code(attrs: &[ast::Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.name == "allow" && args_name_dead_code(&attr.args))
}

/// `#![allow(dead_code)]` at the top of a module waives the lint for every item.
fn inner_attrs_allow_dead_code(attrs: &[ast::InnerAttribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.name == "allow" && args_name_dead_code(&attr.args))
}
