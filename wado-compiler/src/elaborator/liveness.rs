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
//! The pass reports `DeadFunction` / `DeadGlobal` for **free functions and
//! globals** only, and reify gates the emission of exactly those two item
//! kinds on `live_items`. Every impl/trait method is seeded as live, so it
//! serves as a live intermediary in the call graph without itself being a
//! dead-report or gating candidate. This keeps the analysis sound against
//! false positives (the failure the WEP optimises against) while deferring
//! method-level dead detection — which needs the operator / `?` / for-of
//! dispatch edges that leave no `references` entry — to a follow-up slice.
//!
//! The graph traces every site where reify can emit a call: function and
//! method bodies, global initializers, parameter defaults, and struct field
//! defaults. A callee reachable only through one of those still stays live.

use crate::ast::{self, AstId, AstVisitor, Block, Expr, Function, Item, Module};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::symbol::SymbolKey;
use crate::token::Span;

/// Result of the source-level liveness analysis.
///
/// `live_items` is the set reify will eventually gate emission on; in
/// slice 1 it is over-approximated (every method is seeded live) and used
/// only to derive `dead_items`. `dead_items` is the user-authored
/// free-function / global complement, in source order, consumed by the
/// diagnostic emitter.
#[derive(Default, Clone)]
pub(crate) struct Liveness {
    /// Reachable items. Reify gates free-function / global emission on this
    /// set: an item absent from it is dead source that reify drops. The
    /// diagnostic emitter reads its complement (`dead_items`).
    pub(crate) live_items: IndexSet<SymbolKey>,
    pub(crate) dead_items: Vec<SymbolKey>,
}

/// Compute liveness over every loaded module.
///
/// `world_export_names` holds every export name across all registered worlds; a
/// function whose name matches is a potential world entry point and is seeded
/// as a root, so a misdeclared entry (`fn run()` without `export`) survives
/// reify gating and still reaches the world-conformance check.
pub(crate) fn compute(
    modules: &IndexMap<ModuleSource, Module>,
    references: &IndexMap<SymbolKey, SymbolKey>,
    world_export_names: &IndexSet<String>,
) -> Liveness {
    let mut graph = Graph::default();

    for (source, module) in modules {
        let user = is_user_authored(source);
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    let key = SymbolKey::new(source.clone(), func.id);
                    graph.add_function_edges(source, func, references, &key);
                    if func.is_export
                        || has_export_attr(func)
                        || world_export_names.contains(&func.name)
                    {
                        graph.seed(key.clone());
                    }
                    // Bodyless functions are compiler builtins / imports, not
                    // user-authored code that could be "dead".
                    if user && func.body.is_some() {
                        graph.report_candidates.push(key);
                    }
                }
                Item::Global(global) => {
                    let key = SymbolKey::new(source.clone(), global.id);
                    graph.add_expr_edges(source, &global.initializer, references, &key);
                    if user {
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
                    let key = SymbolKey::new(source.clone(), struct_decl.id);
                    for field in &struct_decl.fields {
                        if let Some(default) = &field.default {
                            graph.add_expr_edges(source, default, references, &key);
                            has_default = true;
                        }
                    }
                    if has_default {
                        graph.seed(key);
                    }
                }
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        let key = SymbolKey::new(source.clone(), method.id);
                        graph.add_function_edges(source, method, references, &key);
                        // Slice 1: methods are live intermediaries.
                        graph.seed(key);
                    }
                }
                Item::Trait(trait_decl) => {
                    for method in &trait_decl.methods {
                        if method.body.is_none() {
                            continue;
                        }
                        let key = SymbolKey::new(source.clone(), method.id);
                        graph.add_function_edges(source, method, references, &key);
                        graph.seed(key);
                    }
                }
                Item::Test(test) => {
                    // Test blocks are export-boundary roots in the test world.
                    // Treated as roots in every world so functions used only by
                    // tests are never falsely reported dead.
                    let key = SymbolKey::new(source.clone(), test.id);
                    graph.add_block_edges(source, &test.body, references, &key);
                    graph.seed(key);
                }
                _ => {}
            }
        }
    }

    graph.finish()
}

/// Call graph plus seeds and report candidates, assembled in one walk.
#[derive(Default)]
struct Graph {
    /// `owner -> called items`.
    edges: IndexMap<SymbolKey, Vec<SymbolKey>>,
    /// Always-live roots (export functions, methods, tests).
    seeds: Vec<SymbolKey>,
    /// User-authored free functions / globals eligible for dead reporting,
    /// in source order.
    report_candidates: Vec<SymbolKey>,
}

impl Graph {
    fn seed(&mut self, key: SymbolKey) {
        self.seeds.push(key);
    }

    fn add_function_edges(
        &mut self,
        source: &ModuleSource,
        func: &Function,
        references: &IndexMap<SymbolKey, SymbolKey>,
        owner: &SymbolKey,
    ) {
        if let Some(body) = &func.body {
            self.add_block_edges(source, body, references, owner);
        }
        // A parameter default is materialized by reify at every call site that
        // omits the argument, so anything it references is reachable whenever
        // the function itself is. Edge from the function (not a seed) keeps that
        // precise: a dead function's defaults stay dead too.
        for param in &func.params {
            if let Some(default) = &param.default {
                self.add_expr_edges(source, default, references, owner);
            }
        }
    }

    fn add_block_edges(
        &mut self,
        source: &ModuleSource,
        block: &Block,
        references: &IndexMap<SymbolKey, SymbolKey>,
        owner: &SymbolKey,
    ) {
        let mut collector = IdCollector::default();
        ast::walk_block(&mut collector, block);
        self.link(source, &collector.ids, references, owner);
    }

    fn add_expr_edges(
        &mut self,
        source: &ModuleSource,
        expr: &Expr,
        references: &IndexMap<SymbolKey, SymbolKey>,
        owner: &SymbolKey,
    ) {
        let mut collector = IdCollector::default();
        ast::walk_expr(&mut collector, expr);
        self.link(source, &collector.ids, references, owner);
    }

    /// For each id in the owner's body that resolves to a definition, add an
    /// `owner -> def` edge.
    fn link(
        &mut self,
        source: &ModuleSource,
        ids: &[AstId],
        references: &IndexMap<SymbolKey, SymbolKey>,
        owner: &SymbolKey,
    ) {
        for &id in ids {
            let use_key = SymbolKey::new(source.clone(), id);
            if let Some(def) = references.get(&use_key) {
                self.edges
                    .entry(owner.clone())
                    .or_default()
                    .push(def.clone());
            }
        }
    }

    /// Run the reachability closure and collect the dead set.
    fn finish(self) -> Liveness {
        let mut live: IndexSet<SymbolKey> = IndexSet::default();
        let mut work = self.seeds;
        while let Some(key) = work.pop() {
            if !live.insert(key.clone()) {
                continue;
            }
            if let Some(targets) = self.edges.get(&key) {
                for target in targets {
                    if !live.contains(target) {
                        work.push(target.clone());
                    }
                }
            }
        }

        let dead_items = self
            .report_candidates
            .into_iter()
            .filter(|key| !live.contains(key))
            .collect();

        Liveness {
            live_items: live,
            dead_items,
        }
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
