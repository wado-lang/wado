//! Source-level liveness / dead-code analysis.
//!
//! Implements the `liveness` pass described in
//! [`wep-2026-05-16-unused-diagnostics.md`] (policy) and
//! [`wep-2026-05-26-elaborator-rearchitecture.md`] (mechanism). The pass
//! runs after `annotate_bodies` and before `reify`, computing source-level
//! reachability from the export boundary over the call graph the elaborator
//! recorded in `references`.
//!
//! # Slice 1 (this module's current scope)
//!
//! This first slice reports `DeadFunction` / `DeadGlobal` for **free
//! functions and globals** only. Every impl/trait method is seeded as live,
//! so it serves as a live intermediary in the call graph without itself
//! being a dead-report candidate. This makes the analysis sound against
//! false positives (the failure the WEP optimises against) while deferring
//! method-level dead detection — which needs the operator / `?` / for-of
//! dispatch edges that leave no `references` entry — and reify gating to a
//! follow-up slice.

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
    /// Read by reify gating in the next slice; populated now so the
    /// reachability result is observable and testable.
    #[expect(dead_code, reason = "reify gating consumes this in the next slice")]
    pub(crate) live_items: IndexSet<SymbolKey>,
    pub(crate) dead_items: Vec<SymbolKey>,
}

/// Compute liveness over every loaded module.
pub(crate) fn compute(
    modules: &IndexMap<ModuleSource, Module>,
    references: &IndexMap<SymbolKey, SymbolKey>,
) -> Liveness {
    let mut graph = Graph::default();

    for (source, module) in modules {
        let user = is_user_authored(source);
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    let key = SymbolKey::new(source.clone(), func.id);
                    graph.add_function_edges(source, func, references, &key);
                    if func.is_export || has_export_attr(func) {
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
/// transitively imports. Stdlib (`Core` / `Wasi` / `Wasm` / `Builtin`) is
/// never reported.
fn is_user_authored(source: &ModuleSource) -> bool {
    matches!(
        source,
        ModuleSource::EntryPoint { .. }
            | ModuleSource::Local { .. }
            | ModuleSource::Remote { .. }
            | ModuleSource::Redirected { .. }
    )
}

/// `#[export]` marks a raw Wasm export — an export-boundary root.
fn has_export_attr(func: &Function) -> bool {
    func.attrs.iter().any(|attr| attr.name == "export")
}
