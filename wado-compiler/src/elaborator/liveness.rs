//! Source-level liveness / dead-code analysis (WEP 2026-05-16, 2026-05-26):
//! between `annotate_bodies` and `reify`, reachability from the
//! package-external boundary over the two edges annotate records — the use→def
//! edge of every name the source spells, and the dispatch decision at every
//! site that spells none.
//!
//! Only free functions and globals are classified for diagnostics; a method
//! counts as used so a function only it calls is not reported dead, but is
//! emitted only when a call reaches it.

use crate::ast::{
    self, AstId, Block, Expr, Function, IdCollector, Item, Module, UseItem, Visibility,
    type_head_name,
};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;

/// Result of the source-level liveness analysis. Two root sets run over one call
/// graph: `E` reachable from production roots, `T` from `test` blocks. A
/// user-authored free function or global is live in `E`, test-only in `T \ E`,
/// dead in neither. The `E`/`T` split only picks which diagnostic is raised.
#[derive(Default, Clone)]
pub(crate) struct Liveness {
    /// Reachable from the roots that survive into the emitted program ∪ tests,
    /// which reify gates emission on. Narrower than `E ∪ T`: a method is a root
    /// of `E` so that a free function only it calls is not reported dead, but
    /// enters this set only when a call reaches it.
    pub(crate) emit_live: IndexSet<AstId>,
    /// Candidates reachable from neither production nor tests (`∉ E ∧ ∉ T`),
    /// in source order. `DeadFunction` / `DeadGlobal`.
    pub(crate) dead_items: Vec<AstId>,
    /// Candidates reachable from tests but not production (`∈ T \ E`), in
    /// source order. `TestOnlyFunction` / `TestOnlyGlobal`.
    pub(crate) test_only_items: Vec<AstId>,
}

/// Which modules a liveness run sees, and so which roots it can trust.
pub(crate) enum LivenessScope {
    /// One compilation: the modules its entry reaches. Another module of the
    /// package may use an `internal` item unseen, so one counts as used.
    Compilation,
    /// A whole package, loaded together.
    Package(Box<PackageScope>),
}

/// One package's modules, sorted by what they are for.
pub(crate) struct PackageScope {
    /// The modules its manifest declares. Their `pub` items, and what they
    /// re-export, are what the package offers.
    pub(crate) entries: IndexSet<ModuleSource>,
    /// What the entries reach by `use`: production code. A member outside it
    /// is test code.
    pub(crate) production: IndexSet<ModuleSource>,
    /// The modules that reach one a lint reports in by `use`, itself included.
    /// A module outside it names no reported item, and impl methods are roots, so
    /// its bodies decide nothing and go unwalked: Gale's generated parsers,
    /// most of what its tests load, import only the standard library.
    pub(crate) walked: IndexSet<ModuleSource>,
}

/// What one module's visibility modifiers mean for liveness, under a scope.
struct ModuleRoots {
    /// Its `pub` items are offered beyond the package, and so are roots.
    offers_pub: bool,
    /// Whether another module uses its `internal` items is unknown here.
    internal_undecided: bool,
    /// It is test code: reaching an item from a test is its purpose.
    test_code: bool,
}

impl ModuleRoots {
    fn offers(&self, visibility: Visibility) -> bool {
        self.offers_pub && visibility.is_public()
    }

    fn undecided(&self, visibility: Visibility) -> bool {
        self.internal_undecided && visibility.is_internal()
    }
}

impl LivenessScope {
    /// Whether the body walk of `source` can change what liveness reports.
    pub(crate) fn walks_bodies_of(&self, source: &ModuleSource) -> bool {
        match self {
            Self::Compilation => true,
            Self::Package(package) => package.walked.contains(source),
        }
    }

    fn module_roots(&self, source: &ModuleSource) -> ModuleRoots {
        let compilation = ModuleRoots {
            offers_pub: true,
            internal_undecided: true,
            test_code: false,
        };
        match self {
            // Another package's modules keep the compilation rules: only their
            // own package decides their roots.
            Self::Compilation => compilation,
            Self::Package(_) if !source.is_entry_package() => compilation,
            Self::Package(package) => ModuleRoots {
                offers_pub: package.entries.contains(source),
                internal_undecided: false,
                test_code: !package.production.contains(source),
            },
        }
    }
}

/// Compute liveness over every loaded module.
///
/// `world_export_names` holds every export name across all registered worlds; a
/// function whose name matches is a potential world entry point and is seeded
/// as a root, so a misdeclared entry (`fn run()` without `export`) survives
/// reify gating and still reaches the world-conformance check.
pub(crate) fn compute(
    modules: &IndexMap<ModuleSource, Module>,
    references: &References<'_>,
    world_export_names: &IndexSet<String>,
    compiler_named: &CompilerNamed,
    trait_method_impls: &IndexMap<AstId, IndexSet<AstId>>,
    scope: &LivenessScope,
) -> Liveness {
    let mut graph = Graph::default();

    // Which impl a call through a generic bound reaches is settled by
    // monomorphization, which this graph does not run, so a trait method
    // reaches every impl's. See the WEP's Liveness section for the two rules.
    for (trait_method, impl_methods) in trait_method_impls {
        graph
            .edges
            .entry(*trait_method)
            .or_default()
            .extend(impl_methods);
    }
    // Every impl of a method its trait binds by a `contract` opens with a call
    // to the clauses' holder, which reify mints after this pass.
    for module in modules.values() {
        for item in &module.items {
            let Item::Trait(trait_decl) = item else {
                continue;
            };
            for holder in &trait_decl.methods {
                let Some(held) = holder.holds_contracts_of else {
                    continue;
                };
                let impls = trait_method_impls.get(&held).into_iter().flatten();
                for caller in std::iter::once(&held).chain(impls) {
                    graph.edges.entry(*caller).or_default().push(holder.id);
                }
            }
        }
    }

    for (source, module) in modules {
        let roots = scope.module_roots(source);
        // Which candidates a lint reports is the emitter's to decide.
        let candidates = source.is_entry_package();
        // A file-level `#![allow(dead_code)]` waives the lint for every item in
        // the module — the idiom for test-helper files whose functions exist
        // only to back `test` blocks.
        let module_allows_dead =
            ast::inner_attrs_allow(&module.inner_attributes, ast::lint::DEAD_CODE);
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    let key = func.id;
                    graph.add_function_edges(func, references, &key);
                    // Any module may be compiled as an entry of its own (a
                    // script `wado run` runs), so an `export` is a root
                    // wherever it is written. The last two catch roots with no
                    // visibility modifier (a misdeclared entry point, or a
                    // compiler item).
                    if roots.offers(func.visibility)
                        || func.is_export
                        || (roots.offers_pub && world_export_names.contains(&func.name))
                        || compiler_named.names_function(source, &func.name)
                    {
                        graph.seed_world(key);
                    } else if roots.undecided(func.visibility) {
                        graph.seed_export(key);
                    }
                    // Bodyless functions are compiler builtins / imports, not
                    // user-authored code that could be "dead". `#[allow(dead_code)]`
                    // (item- or module-level) opts an item out of the lint while
                    // leaving its call-graph edges intact.
                    if candidates
                        && func.body.is_some()
                        && !module_allows_dead
                        && !ast::attrs_allow(&func.attrs, ast::lint::DEAD_CODE)
                    {
                        graph.add_candidate(key, roots.test_code);
                    }
                }
                Item::Global(global) => {
                    // Reify emits every global, read or not, so its
                    // initializer is a call site whatever the visibility. Only
                    // one visible beyond its file is a *use*, so an unread
                    // private global still reports dead without masking its
                    // callees.
                    let key = global.id;
                    graph.add_expr_edges(&global.initializer, references, &key);
                    if roots.offers(global.visibility) {
                        graph.seed_world(key);
                    } else {
                        graph.seed_emit(key);
                        if roots.undecided(global.visibility) {
                            graph.seed_export(key);
                        }
                    }
                    if candidates
                        && !module_allows_dead
                        && !ast::attrs_allow(&global.attributes, ast::lint::DEAD_CODE)
                    {
                        graph.add_candidate(key, roots.test_code);
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
                        graph.seed_world(key);
                    }
                }
                Item::Impl(impl_block) => {
                    // A method rides the graph on the call that reaches it. It
                    // is a root only where that call is minted after this pass:
                    // by synthesis reading a format specifier, or by the
                    // compiler naming an inherent method itself.
                    let synthesis_dispatched = compiler_named
                        .synthesis_dispatched_impls
                        .contains(&impl_block.id);
                    let self_name = type_head_name(&impl_block.ty);
                    for method in &impl_block.methods {
                        let key = method.id;
                        graph.add_function_edges(method, references, &key);
                        if synthesis_dispatched
                            || compiler_named.names_method(self_name, &method.name)
                        {
                            graph.seed_world(key);
                        } else {
                            graph.seed_export(key);
                        }
                    }
                    // A constant's body is materialized at every read, like a
                    // struct field default above, and seeded a root for the same
                    // reason: no module here knows whether it is read.
                    for constant in &impl_block.constants {
                        let key = constant.id;
                        graph.add_expr_edges(&constant.value, references, &key);
                        graph.seed_world(key);
                    }
                }
                Item::Trait(trait_decl) => {
                    seed_operations(&mut graph, &trait_decl.methods, references, Dispatch::Fact);
                    // An impl this graph does not see may inherit a default
                    // body, so one a module offers counts as used, as a `pub`
                    // function does.
                    let visibility = trait_decl.visibility;
                    if roots.offers(visibility) || roots.undecided(visibility) {
                        for method in trait_decl.methods.iter().filter(|m| m.body.is_some()) {
                            graph.seed_export(method.id);
                        }
                    }
                }
                Item::Interface(interface_decl) => {
                    seed_operations(
                        &mut graph,
                        &interface_decl.methods,
                        references,
                        Dispatch::MintedLater,
                    );
                }
                Item::Resource(resource_decl) => {
                    seed_operations(
                        &mut graph,
                        &resource_decl.methods,
                        references,
                        Dispatch::MintedLater,
                    );
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
                // What an offering module re-exports is offered too. Elsewhere
                // a `pub` item is a root of its own, so only this case needs it.
                Item::Use(use_decl) if roots.offers_pub && use_decl.visibility.is_public() => {
                    let sites = use_decl.items.iter().flat_map(|item| match item {
                        UseItem::Simple { id, .. } => vec![*id],
                        UseItem::InterfaceFunctions { id, functions, .. } => std::iter::once(*id)
                            .chain(functions.iter().map(|f| f.id))
                            .collect(),
                        UseItem::Wildcard | UseItem::Namespace { .. } => Vec::new(),
                    });
                    for site in sites {
                        if let Some(def) = references.direct.get(&site) {
                            graph.seed_export(*def);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    graph.finish()
}

/// Where a declared method's only call comes from.
#[derive(PartialEq)]
enum Dispatch {
    /// A trait method: the call site records the fact that reaches the body.
    Fact,
    /// An `interface` or `resource` operation, whose wrapper
    /// `synthesis::effect_dispatch` mints after this pass.
    MintedLater,
}

/// Add the edges of every declared method carrying a body or a parameter
/// default, and root the two kinds no dispatch fact names: an operation, and
/// one with a parameter default that reify materializes at the call.
fn seed_operations(
    graph: &mut Graph,
    methods: &[Function],
    references: &References<'_>,
    dispatch: Dispatch,
) {
    for method in methods {
        let has_param_default = method.params.iter().any(|p| p.default.is_some());
        if method.body.is_none() && !has_param_default {
            continue;
        }
        let key = method.id;
        graph.add_function_edges(method, references, &key);
        if dispatch == Dispatch::MintedLater || has_param_default {
            graph.seed_world(key);
        }
    }
}

/// The use→def edges liveness walks. A trait's default body is walked once per
/// inheriting `impl`, so one use there has as many definitions as there are
/// impls — `inherited` holds those sets, `direct` the single-definition edges
/// every other body records.
pub(crate) struct References<'a> {
    pub(crate) direct: &'a IndexMap<AstId, AstId>,
    pub(crate) inherited: &'a IndexMap<AstId, IndexSet<AstId>>,
    /// Callees a dispatch fact names: an operator, a subscript, a `From`
    /// conversion, a `for-of` iterator, a handler binding — and every method
    /// call, since `direct` keeps one definition per node while a tuple
    /// `for-of` resolves the same node once per element.
    pub(crate) dispatch: &'a IndexMap<AstId, IndexSet<AstId>>,
}

/// Call graph plus the two root sets and report candidates, assembled in one
/// walk.
#[derive(Default)]
struct Graph {
    /// `owner -> called items`.
    edges: IndexMap<AstId, Vec<AstId>>,
    /// Production roots (world exports, `pub` / `export` / `internal` items,
    /// methods, struct-field defaults) — seeds of the `E` closure.
    export_seeds: Vec<AstId>,
    /// `test` block roots — seeds of the `T` closure.
    test_seeds: Vec<AstId>,
    /// Roots that survive into the emitted program — seeds of the set reify
    /// gates on. Neither a subset nor a superset of [`Self::export_seeds`]: a
    /// method enters only when called, an unread private global enters anyway.
    emit_seeds: Vec<AstId>,
    /// User-authored free functions / globals eligible for dead reporting,
    /// in source order.
    report_candidates: Vec<AstId>,
    /// The candidates declared in test code, which a test may be alone in
    /// reaching.
    test_code: IndexSet<AstId>,
}

impl Graph {
    fn add_candidate(&mut self, key: AstId, test_code: bool) {
        self.report_candidates.push(key);
        if test_code {
            self.test_code.insert(key);
        }
    }

    fn seed_export(&mut self, key: AstId) {
        self.export_seeds.push(key);
    }

    fn seed_test(&mut self, key: AstId) {
        self.test_seeds.push(key);
    }

    /// Seed a root of the emitted program: reify emits it whether or not
    /// anything reaches it. Not itself a use, so the `E` closure is unmoved.
    fn seed_emit(&mut self, key: AstId) {
        self.emit_seeds.push(key);
    }

    /// Seed a root of both closures: it is emitted, and so counts as used.
    fn seed_world(&mut self, key: AstId) {
        self.seed_export(key);
        self.seed_emit(key);
    }

    fn add_function_edges(&mut self, func: &Function, references: &References<'_>, owner: &AstId) {
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

    fn add_block_edges(&mut self, block: &Block, references: &References<'_>, owner: &AstId) {
        let mut collector = IdCollector::default();
        ast::walk_block(&mut collector, block);
        self.link(&collector.ids, references, owner);
    }

    fn add_expr_edges(&mut self, expr: &Expr, references: &References<'_>, owner: &AstId) {
        let mut collector = IdCollector::default();
        ast::walk_expr(&mut collector, expr);
        self.link(&collector.ids, references, owner);
    }

    /// For each id in the owner's body that resolves to a definition, add an
    /// `owner -> def` edge.
    fn link(&mut self, ids: &[AstId], references: &References<'_>, owner: &AstId) {
        let edges = self.edges.entry(*owner).or_default();
        for &id in ids {
            edges.extend(references.direct.get(&id));
            edges.extend(references.inherited.get(&id).into_iter().flatten());
            edges.extend(references.dispatch.get(&id).into_iter().flatten());
        }
    }

    /// Run both reachability closures and classify each report candidate.
    fn finish(self) -> Liveness {
        let production = self.closure(&self.export_seeds);
        let tests = self.closure(&self.test_seeds);

        let mut emit_live = self.closure(&self.emit_seeds);
        for key in &tests {
            emit_live.insert(*key);
        }

        let mut dead_items = Vec::new();
        let mut test_only_items = Vec::new();
        for key in &self.report_candidates {
            if production.contains(key) {
                continue;
            }
            if tests.contains(key) {
                if !self.test_code.contains(key) {
                    test_only_items.push(*key);
                }
            } else {
                dead_items.push(*key);
            }
        }

        Liveness {
            emit_live,
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

/// What the compiler names by construction, so no edge in this graph reaches
/// it. Each entry is one entity, not a whole type: registering a type would
/// root every method on it.
#[derive(Default)]
pub(crate) struct CompilerNamed {
    /// Inherent methods, by the type that declares them.
    pub(crate) methods: IndexMap<String, IndexSet<String>>,
    /// Free functions, by the module that declares them — the CM ABI helpers.
    pub(crate) functions: IndexMap<ModuleSource, IndexSet<String>>,
    /// `impl` blocks whose trait a synthesis pass dispatches — see
    /// [`crate::compiler_item::CompilerItem::dispatched_by_synthesis`]. Their
    /// methods are roots: no edge here can name a call minted after this pass.
    pub(crate) synthesis_dispatched_impls: IndexSet<AstId>,
}

impl CompilerNamed {
    fn names_method(&self, owner: Option<&str>, name: &str) -> bool {
        owner.is_some_and(|owner| {
            self.methods
                .get(owner)
                .is_some_and(|names| names.contains(name))
        })
    }

    fn names_function(&self, source: &ModuleSource, name: &str) -> bool {
        self.functions
            .get(source)
            .is_some_and(|names| names.contains(name))
    }
}
