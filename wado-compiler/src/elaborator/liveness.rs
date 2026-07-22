//! Source-level liveness / dead-code analysis.
//!
//! Implements the `liveness` pass described in
//! [`wep-2026-05-16-unused-diagnostics.md`] (policy) and
//! [`wep-2026-05-26-elaborator-rearchitecture.md`] (mechanism). The pass
//! runs after `annotate_bodies` and before `reify`, computing source-level
//! reachability from the package-external boundary (`pub` and `export`
//! items) over the call graph the elaborator recorded in `references`.
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
/// Reachability is computed from two independent root sets over the same
/// call graph: `E` = reachable from production roots (world exports, `pub`
/// / `export` items, `#[export]`, methods, struct-field defaults), `T` =
/// reachable from `test` blocks. Each user-authored free function / global
/// is then classified:
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
    /// Local last-use liveness (WEP 2026-05-21, value-copy client). Each entry
    /// is the `AstId` of an identifier *use* that is the final use, on every
    /// path, of a move-eligible local binding. The canonical `AstId`-keyed
    /// output — reusable by the LSP and the future affine-resource client.
    /// Sound by construction: a use is recorded only when the analysis proves
    /// the local dead afterward, so an unrecorded use always falls back to a
    /// copy.
    pub(crate) last_uses: IndexSet<AstId>,
    /// The same last-use facts projected to `(module, source span)`, the form
    /// the value-copy planner consumes in the `lower` phase — TIR carries a
    /// `Span` but no `AstId`, and a `Span` is only unique within its own
    /// source, so the outer key disambiguates across modules. Threaded through
    /// `Package` → `FlatPackage` to the planner.
    pub(crate) moved_spans: IndexMap<ModuleSource, IndexSet<Span>>,
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
                    // `export` implies `Visibility::Public`, so this subsumes
                    // `is_export`; the other two catch roots with no visibility
                    // modifier (a raw Wasm export, or a misdeclared entry point).
                    if func.visibility.is_public()
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
                    if global.visibility.is_public() {
                        graph.seed_export(key);
                    }
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

    let mut liveness = graph.finish();
    compute_last_uses(
        modules,
        references,
        &mut liveness.last_uses,
        &mut liveness.moved_spans,
    );
    liveness
}

/// Compute local last-use liveness over every function / method body in the
/// program (WEP 2026-05-21). Fills `last_uses` (the `AstId`-keyed set) and
/// `moved_spans` (the `(module, span)` projection). Analyzed over all modules —
/// stdlib bodies benefit from copy elision too.
fn compute_last_uses(
    modules: &IndexMap<ModuleSource, Module>,
    references: &IndexMap<AstId, AstId>,
    last_uses: &mut IndexSet<AstId>,
    moved_spans: &mut IndexMap<ModuleSource, IndexSet<Span>>,
) {
    for (source, module) in modules {
        let spans = moved_spans.entry(source.clone()).or_default();
        for item in &module.items {
            match item {
                Item::Function(func) => analyze_body(func, references, last_uses, spans),
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        analyze_body(method, references, last_uses, spans);
                    }
                }
                Item::Trait(trait_decl) => {
                    for method in &trait_decl.methods {
                        analyze_body(method, references, last_uses, spans);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Run the eligibility pre-pass and the backward last-use pass over one
/// function body, merging discovered last uses into `last_uses` (by use-site
/// `AstId`) and `spans` (by source span, within the enclosing module).
fn analyze_body(
    func: &Function,
    references: &IndexMap<AstId, AstId>,
    last_uses: &mut IndexSet<AstId>,
    spans: &mut IndexSet<Span>,
) {
    let Some(body) = &func.body else {
        return;
    };

    let mut elig = EligibilityPass {
        references,
        bindings: IndexSet::default(),
        excluded: IndexSet::default(),
        closure_depth: 0,
    };
    // By-value parameters are move-eligible; borrow (`&T` / `&mut T`) and `self`
    // receivers are not — they name the caller's storage and are never moved.
    for param in &func.params {
        if param.self_kind == ast::SelfKind::None
            && !matches!(
                &param.ty,
                ast::Type::Reference(_) | ast::Type::MutReference(_)
            )
        {
            elig.bindings.insert(param.id);
        }
    }
    ast::walk_block(&mut elig, body);

    let eligible: IndexSet<AstId> = elig
        .bindings
        .iter()
        .copied()
        .filter(|id| !elig.excluded.contains(id))
        .collect();
    if eligible.is_empty() {
        return;
    }

    let mut analyzer = LastUseAnalyzer {
        references,
        eligible,
        last_uses,
        spans,
        loop_exit: Vec::new(),
    };
    let mut live = IndexSet::default();
    analyzer.walk_block(body, &mut live, true);
}

/// Forward pass over a body collecting move-eligibility facts: every local
/// binding site, and the locals that must be excluded from move eligibility
/// because their address is taken (`&x` / `&mut x`, at any projection root) or
/// they are captured by a closure. Over-exclusion only costs an extra copy, so
/// the pass errs toward marking a local ineligible whenever unsure.
struct EligibilityPass<'a> {
    references: &'a IndexMap<AstId, AstId>,
    bindings: IndexSet<AstId>,
    excluded: IndexSet<AstId>,
    closure_depth: u32,
}

impl AstVisitor for EligibilityPass<'_> {
    fn visit_pattern(&mut self, pat: &ast::Pattern) {
        match pat {
            ast::Pattern::Ident { id, .. } | ast::Pattern::MutIdent { id, .. } => {
                self.bindings.insert(*id);
            }
            _ => {}
        }
        ast::walk_pattern(self, pat);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Unary(u) if matches!(u.op, ast::UnaryOp::Ref | ast::UnaryOp::MutRef) => {
                if let Some(root) = root_local_def(&u.expr, self.references) {
                    self.excluded.insert(root);
                }
            }
            Expr::Closure(_) => {
                self.closure_depth += 1;
                ast::walk_expr(self, expr);
                self.closure_depth -= 1;
                return;
            }
            Expr::Ident(ident) if self.closure_depth > 0 => {
                if let Some(def) = self.references.get(&ident.id) {
                    self.excluded.insert(*def);
                }
            }
            _ => {}
        }
        ast::walk_expr(self, expr);
    }
}

/// Follow projections (`.field`, `[i]`, `as`, `*`) down to the root identifier
/// and return the local it resolves to, if any. Used to attribute `&place` to
/// the local whose storage the reference aliases.
fn root_local_def(expr: &Expr, references: &IndexMap<AstId, AstId>) -> Option<AstId> {
    match expr {
        Expr::Ident(ident) => references.get(&ident.id).copied(),
        Expr::FieldAccess(e) => root_local_def(&e.expr, references),
        Expr::Index(e) => root_local_def(&e.expr, references),
        Expr::Cast(e) => root_local_def(&e.expr, references),
        Expr::Unary(u) if matches!(u.op, ast::UnaryOp::Deref) => {
            root_local_def(&u.expr, references)
        }
        _ => None,
    }
}

/// Backward last-use liveness pass (WEP 2026-05-21). A structural walk over the
/// typed AST — Wado has no `goto`, so `if` / `match` / loops / early `return`
/// suffice and no explicit CFG is needed. The live set holds the eligible local
/// def-ids that MAY be read on some path below the current point; a use is a
/// final use iff its local is not live immediately afterward. Over-approximating
/// liveness (treating more locals as live) only suppresses moves, so every
/// imprecise arm stays sound.
struct LastUseAnalyzer<'a> {
    references: &'a IndexMap<AstId, AstId>,
    eligible: IndexSet<AstId>,
    last_uses: &'a mut IndexSet<AstId>,
    /// Span projection for the enclosing module (see [`Liveness::moved_spans`]).
    spans: &'a mut IndexSet<Span>,
    /// Live-after-loop set per enclosing loop, for `break` targets.
    loop_exit: Vec<IndexSet<AstId>>,
}

impl LastUseAnalyzer<'_> {
    /// Resolve an identifier use to its eligible local def, if any.
    fn use_def(&self, use_id: AstId) -> Option<AstId> {
        let def = *self.references.get(&use_id)?;
        self.eligible.contains(&def).then_some(def)
    }

    /// Record a read of local `def` at identifier `ident`: if `def` is not live
    /// afterward, this is its final use. Then mark `def` live above this point.
    fn read(&mut self, ident: &ast::IdentExpr, live: &mut IndexSet<AstId>, record: bool) {
        if let Some(def) = self.use_def(ident.id) {
            if record && !live.contains(&def) {
                self.last_uses.insert(ident.id);
                self.spans.insert(ident.span);
            }
            live.insert(def);
        }
    }

    fn kill_pattern(&mut self, pat: &ast::Pattern, live: &mut IndexSet<AstId>) {
        match pat {
            ast::Pattern::Ident { id, .. } | ast::Pattern::MutIdent { id, .. } => {
                live.swap_remove(id);
            }
            ast::Pattern::Tuple(subs, _) => {
                for sub in subs {
                    self.kill_pattern(sub, live);
                }
            }
            ast::Pattern::Variant { bindings, .. } => {
                for sub in bindings {
                    self.kill_pattern(sub, live);
                }
            }
            ast::Pattern::Struct { fields, .. } => {
                for field in fields {
                    self.kill_pattern(&field.pattern, live);
                }
            }
            ast::Pattern::Or(alts) => {
                for alt in alts {
                    self.kill_pattern(alt, live);
                }
            }
            _ => {}
        }
    }

    fn walk_block(&mut self, block: &Block, live: &mut IndexSet<AstId>, record: bool) {
        for stmt in block.stmts.iter().rev() {
            self.walk_stmt(stmt, live, record);
        }
    }

    fn walk_stmt(&mut self, stmt: &ast::Stmt, live: &mut IndexSet<AstId>, record: bool) {
        match stmt {
            ast::Stmt::Let(l) => {
                // `live` reflects the statements after this one — for a
                // `let ... else` that is the match-success continuation. Kill
                // the pattern bindings (defined here) from it.
                self.kill_pattern(&l.pattern, live);
                // The else block diverges: its live-in starts empty (nothing
                // after it is live) and unions in the variables it uses.
                if let Some(eb) = &l.else_block {
                    let mut else_live = IndexSet::default();
                    self.walk_block(eb, &mut else_live, record);
                    *live = union(live, &else_live);
                }
                if let Some(value) = &l.value {
                    self.walk_expr(value, live, record);
                }
            }
            ast::Stmt::Expr(e) => self.walk_expr(&e.expr, live, record),
            ast::Stmt::Return(r) => {
                live.clear();
                if let Some(value) = &r.value {
                    self.walk_expr(value, live, record);
                }
            }
            ast::Stmt::TaskReturn(t) => self.walk_expr(&t.value, live, record),
            ast::Stmt::If(s) => {
                self.walk_if(
                    &s.condition,
                    &s.then_block,
                    s.else_block.as_ref(),
                    live,
                    record,
                );
            }
            ast::Stmt::While(s) => self.walk_while(&s.condition, &s.body, live, record),
            ast::Stmt::For(s) => self.walk_for(s, live, record),
            ast::Stmt::ForOf(s) => self.walk_for_of(s, live, record),
            ast::Stmt::Loop(s) => self.walk_loop(&s.body, live, record),
            ast::Stmt::Match(m) => self.walk_match(m, live, record),
            ast::Stmt::Break(b) => {
                // Reset to the innermost loop-exit set (the fall-through below a
                // break is unreachable). Unknown target (labeled / no context) →
                // keep every eligible local live, the safe over-approximation.
                *live = self
                    .loop_exit
                    .last()
                    .cloned()
                    .unwrap_or_else(|| self.eligible.clone());
                if let Some(value) = &b.value {
                    self.walk_expr(value, live, record);
                }
            }
            ast::Stmt::Continue(_) => {
                // The value flowing to the loop head is whatever survives to the
                // next iteration; without threading the head set, keep every
                // eligible local live (sound over-approximation).
                *live = self.eligible.clone();
            }
            ast::Stmt::Assert(a) => {
                if let Some(msg) = &a.message {
                    self.walk_expr(msg, live, record);
                }
                self.walk_expr(&a.condition, live, record);
            }
            ast::Stmt::LabeledBlock(lb) => self.walk_block(&lb.block, live, record),
            // A local type/impl declaration's methods aren't closures, so
            // they can't read/write the enclosing function's locals; nothing
            // here affects variable liveness.
            ast::Stmt::Item(_) => {}
            ast::Stmt::Error(_) => {}
        }
    }

    fn walk_if(
        &mut self,
        cond: &ast::Condition,
        then_block: &Block,
        else_block: Option<&Block>,
        live: &mut IndexSet<AstId>,
        record: bool,
    ) {
        let mut then_live = live.clone();
        self.walk_block(then_block, &mut then_live, record);
        let mut else_live = live.clone();
        if let Some(eb) = else_block {
            self.walk_block(eb, &mut else_live, record);
        }
        *live = union(&then_live, &else_live);
        self.walk_condition(cond, live, record);
    }

    fn walk_while(
        &mut self,
        cond: &ast::Condition,
        body: &Block,
        live: &mut IndexSet<AstId>,
        record: bool,
    ) {
        let exit_live = live.clone();
        self.loop_exit.push(exit_live.clone());
        let mut head = exit_live.clone();
        loop {
            let mut work = head.clone();
            self.walk_block(body, &mut work, false);
            let mut candidate = union(&work, &exit_live);
            self.walk_condition(cond, &mut candidate, false);
            if candidate == head {
                break;
            }
            head = candidate;
        }
        if record {
            let mut work = head.clone();
            self.walk_block(body, &mut work, true);
            let mut candidate = union(&work, &exit_live);
            self.walk_condition(cond, &mut candidate, true);
            head = candidate;
        }
        self.loop_exit.pop();
        *live = head;
    }

    fn walk_loop(&mut self, body: &Block, live: &mut IndexSet<AstId>, record: bool) {
        let exit_live = live.clone();
        self.loop_exit.push(exit_live.clone());
        let mut head = exit_live;
        loop {
            let mut work = head.clone();
            self.walk_block(body, &mut work, false);
            if work == head {
                break;
            }
            head = work;
        }
        if record {
            let mut work = head.clone();
            self.walk_block(body, &mut work, true);
            head = work;
        }
        self.loop_exit.pop();
        *live = head;
    }

    fn walk_for(&mut self, s: &ast::ForStmt, live: &mut IndexSet<AstId>, record: bool) {
        // `for (init; cond; update) body`: init runs once before the loop;
        // cond/update/body iterate. Model update as part of the loop body tail.
        let exit_live = live.clone();
        self.loop_exit.push(exit_live.clone());
        let mut head = exit_live.clone();
        loop {
            let mut work = head.clone();
            if let Some(update) = &s.update {
                self.walk_expr(update, &mut work, false);
            }
            self.walk_block(&s.body, &mut work, false);
            let mut candidate = union(&work, &exit_live);
            if let Some(cond) = &s.condition {
                self.walk_condition(cond, &mut candidate, false);
            }
            if candidate == head {
                break;
            }
            head = candidate;
        }
        let mut live_after_init = if record {
            let mut work = head;
            if let Some(update) = &s.update {
                self.walk_expr(update, &mut work, true);
            }
            self.walk_block(&s.body, &mut work, true);
            let mut candidate = union(&work, &exit_live);
            if let Some(cond) = &s.condition {
                self.walk_condition(cond, &mut candidate, true);
            }
            candidate
        } else {
            head
        };
        self.loop_exit.pop();
        if let Some(init) = &s.init {
            self.walk_stmt(init, &mut live_after_init, record);
        }
        *live = live_after_init;
    }

    fn walk_for_of(&mut self, s: &ast::ForOfStmt, live: &mut IndexSet<AstId>, record: bool) {
        let exit_live = live.clone();
        self.loop_exit.push(exit_live.clone());
        let mut head = exit_live.clone();
        loop {
            let mut work = head.clone();
            self.kill_pattern(&s.binding, &mut work);
            self.walk_block(&s.body, &mut work, false);
            let candidate = union(&work, &exit_live);
            if candidate == head {
                break;
            }
            head = candidate;
        }
        if record {
            let mut work = head.clone();
            self.kill_pattern(&s.binding, &mut work);
            self.walk_block(&s.body, &mut work, true);
        }
        self.loop_exit.pop();
        // The iterable is evaluated once, before the loop.
        *live = union(&head, &exit_live);
        self.walk_expr(&s.iterable, live, record);
    }

    fn walk_match(&mut self, m: &ast::MatchExpr, live: &mut IndexSet<AstId>, record: bool) {
        let after = live.clone();
        let mut merged = IndexSet::default();
        for arm in &m.arms {
            let mut arm_live = after.clone();
            self.walk_expr(&arm.body, &mut arm_live, record);
            if let Some(guard) = &arm.guard {
                self.walk_expr(guard, &mut arm_live, record);
            }
            self.kill_pattern(&arm.pattern, &mut arm_live);
            merged = union(&merged, &arm_live);
        }
        *live = merged;
        self.walk_expr(&m.expr, live, record);
    }

    fn walk_condition(&mut self, cond: &ast::Condition, live: &mut IndexSet<AstId>, record: bool) {
        match cond {
            ast::Condition::Expr(e) => self.walk_expr(e, live, record),
            ast::Condition::LetChain { elements, .. } => {
                for element in elements.iter().rev() {
                    match element {
                        ast::ConditionElement::Let { pattern, expr, .. } => {
                            self.kill_pattern(pattern, live);
                            self.walk_expr(expr, live, record);
                        }
                        ast::ConditionElement::Expr(e) => self.walk_expr(e, live, record),
                    }
                }
            }
        }
    }

    /// Collect uses in an expression, processing sub-expressions in reverse
    /// evaluation order so that among repeated uses of one local only the
    /// textually-last is eligible to be the final use.
    fn walk_expr(&mut self, expr: &Expr, live: &mut IndexSet<AstId>, record: bool) {
        match expr {
            Expr::Ident(ident) => self.read(ident, live, record),
            Expr::Literal(_) | Expr::Error(_) => {}
            Expr::Binary(e) => {
                self.walk_expr(&e.right, live, record);
                self.walk_expr(&e.left, live, record);
            }
            Expr::Unary(e) => self.walk_expr(&e.expr, live, record),
            Expr::Assign(e) => {
                // A whole-local rebind redefines the target: below the assign the
                // new value is live, above it the old binding is dead. Kill the
                // target local, then evaluate the RHS.
                self.walk_assign_target(&e.target, live);
                self.walk_expr(&e.value, live, record);
            }
            Expr::CompoundAssign(e) => {
                // `x += v` reads and writes x, so x stays a use (not killed).
                self.walk_expr(&e.value, live, record);
                self.walk_expr(&e.target, live, record);
            }
            Expr::ComparisonChain(e) => {
                for cmp in e.comparisons.iter().rev() {
                    self.walk_expr(&cmp.right, live, record);
                }
                self.walk_expr(&e.first, live, record);
            }
            Expr::Call(e) => {
                for arg in e.args.iter().rev() {
                    self.walk_expr(arg, live, record);
                }
                self.walk_expr(&e.callee, live, record);
            }
            Expr::MethodCall(e) => {
                for arg in e.args.iter().rev() {
                    self.walk_expr(arg, live, record);
                }
                self.walk_expr(&e.receiver, live, record);
            }
            Expr::StaticMethodCall(e) => {
                for arg in e.args.iter().rev() {
                    self.walk_expr(arg, live, record);
                }
            }
            Expr::FieldAccess(e) => self.walk_expr(&e.expr, live, record),
            Expr::Index(e) => {
                self.walk_expr(&e.index, live, record);
                self.walk_expr(&e.expr, live, record);
            }
            Expr::Cast(e) => self.walk_expr(&e.expr, live, record),
            Expr::TryOp(e) => self.walk_expr(&e.expr, live, record),
            Expr::Spread(inner, _) => self.walk_expr(inner, live, record),
            Expr::Range(e) => {
                self.walk_expr(&e.end, live, record);
                self.walk_expr(&e.start, live, record);
            }
            Expr::StructLiteral(e) => {
                for spread in e.spreads.iter().rev() {
                    self.walk_expr(&spread.expr, live, record);
                }
                for field in e.fields.iter().rev() {
                    self.walk_expr(&field.value, live, record);
                }
            }
            Expr::TupleLiteral(e) => {
                for element in e.elements.iter().rev() {
                    self.walk_expr(element, live, record);
                }
            }
            Expr::TemplateString(e) => {
                for part in e.parts.iter().rev() {
                    if let ast::TemplatePart::Interpolation { expr, .. } = part {
                        self.walk_expr(expr, live, record);
                    }
                }
            }
            Expr::Block(b) => self.walk_block(b, live, record),
            Expr::LabeledBlock(b) => self.walk_block(&b.block, live, record),
            Expr::If(e) => self.walk_if(
                &e.condition,
                &e.then_block,
                e.else_block.as_ref(),
                live,
                record,
            ),
            Expr::Match(m) => self.walk_match(m, live, record),
            Expr::Matches(e) => {
                if let Some(guard) = &e.guard {
                    // A guard reads the pattern's bindings; kill them, then the
                    // scrutinee is evaluated.
                    self.walk_expr(guard, live, record);
                    self.kill_pattern(&e.pattern, live);
                }
                self.walk_expr(&e.expr, live, record);
            }
            Expr::Closure(_) => {
                // Captured locals were excluded from eligibility by the pre-pass;
                // a closure body needs no last-use marking here.
            }
            Expr::WithHandler(e) => {
                self.walk_block(&e.body, live, record);
                for handler in e.handlers.iter().rev() {
                    self.walk_expr(&handler.handler, live, record);
                }
            }
            Expr::Resume(e) => self.walk_expr(&e.value, live, record),
        }
    }

    /// An assignment target: a bare local is redefined (killed here); a
    /// projection (`x.f = …`, `a[i] = …`) reads its root, so treat it as a use.
    fn walk_assign_target(&mut self, target: &Expr, live: &mut IndexSet<AstId>) {
        match target {
            Expr::Ident(ident) => {
                if let Some(def) = self.use_def(ident.id) {
                    live.swap_remove(&def);
                }
            }
            _ => self.walk_expr(target, live, false),
        }
    }
}

/// Set union without mutating either operand.
fn union(a: &IndexSet<AstId>, b: &IndexSet<AstId>) -> IndexSet<AstId> {
    let mut out = a.clone();
    for id in b {
        out.insert(*id);
    }
    out
}

/// Call graph plus the two root sets and report candidates, assembled in one
/// walk.
#[derive(Default)]
struct Graph {
    /// `owner -> called items`.
    edges: IndexMap<AstId, Vec<AstId>>,
    /// Production roots (world exports, `pub` / `export` items, `#[export]`,
    /// methods, struct-field defaults) — seeds of the `E` closure.
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
            last_uses: IndexSet::default(),
            moved_spans: IndexMap::default(),
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
        | ModuleSource::Dependency { .. }
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

#[cfg(test)]
mod last_use_tests {
    use crate::compiler_host::InMemoryCompilerHost;
    use crate::semantics::semantics;

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn raw() -> RawWaker {
            fn no_op(_: *const ()) {}
            fn clone(_: *const ()) -> RawWaker {
                raw()
            }
            RawWaker::new(
                std::ptr::null(),
                &RawWakerVTable::new(clone, no_op, no_op, no_op),
            )
        }
        let waker = unsafe { Waker::from_raw(raw()) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(future);
        loop {
            if let Poll::Ready(v) = Pin::new(&mut fut).as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    /// Return a sorted list of the identifier texts that the analysis marked as
    /// final uses, one entry per marked use.
    fn last_use_names(source: &str) -> Vec<String> {
        let host = InMemoryCompilerHost::new();
        let sem = block_on(semantics(source, &host, Some("entry.wado")));
        let entry = sem.interner.borrow_mut().entry_point("entry.wado");
        let mut names: Vec<String> = sem
            .liveness
            .last_uses
            .iter()
            .filter(|id| sem.module_of_id(**id) == Some(&entry))
            .filter_map(|id| sem.span_of_id(*id))
            .filter(|span| span.end <= source.len() && source.is_char_boundary(span.start))
            .map(|span| source[span.start..span.end].to_string())
            .collect();
        names.sort();
        names
    }

    fn count(source: &str, name: &str) -> usize {
        last_use_names(source)
            .iter()
            .filter(|n| n.as_str() == name)
            .count()
    }

    #[test]
    fn single_use_param_is_a_last_use() {
        let src = "export fn f(a: List<i32>) -> List<i32> { return a; }";
        assert_eq!(count(src, "a"), 1);
    }

    #[test]
    fn earlier_of_two_uses_is_not_a_last_use() {
        let src = "export fn f(a: List<i32>) -> i32 { \
                   let x = a.len(); let y = a.len(); return x + y; }";
        // Only the second `a` is a final use; the first is live afterward.
        assert_eq!(count(src, "a"), 1);
    }

    #[test]
    fn address_taken_local_is_not_move_eligible() {
        let src = "export fn f() -> i32 { \
                   let a = 1; let r = &a; return *r; }";
        // `a` has its address taken, so its use is never a last-use candidate.
        assert_eq!(count(src, "a"), 0);
    }

    #[test]
    fn accumulator_element_is_moved_across_the_loop() {
        let src = "export fn f(n: i32) -> List<i32> { \
                   let mut items: List<i32> = []; \
                   let mut i = 0; \
                   while i < n { let v = i * 2; items.push(v); i = i + 1; } \
                   return items; }";
        // `v` is bound and consumed within one iteration → its push is a last use.
        assert_eq!(count(src, "v"), 1);
    }

    #[test]
    fn serde_list_deserialize_item_is_moved() {
        // Mirrors `impl Deserialize for List<T>` in lib/core/serde.wado: the
        // accumulator loop pushes each `item` bound from `if let Some(item) =
        // next_opt`, with the only loop exit a `return` in the else branch.
        let src = "\
            fn src(x: i32) -> Option<List<i32>> { if x > 0 { return Option::Some([x]); } return Option::None; } \
            export fn f() -> List<List<i32>> { \
              let mut items: List<List<i32>> = List::with_capacity(2); \
              let mut i = 0; \
              loop { \
                let next_opt = src(i); \
                if let Some(item) = next_opt { \
                  items.push(item); \
                } else { \
                  return items; \
                } \
                i = i + 1; \
              } \
            }";
        assert_eq!(count(src, "item"), 1);
    }

    #[test]
    fn iflet_binding_in_loop_is_moved() {
        let src = "export fn f(n: i32) -> List<List<i32>> { \
                   let mut items: List<List<i32>> = []; \
                   let mut i = 0; \
                   loop { \
                     let next_opt = if i < n { Option::Some([i, i]) } else { Option::None }; \
                     if let Some(item) = next_opt { items.push(item); } else { break; } \
                     i = i + 1; \
                   } \
                   return items; }";
        assert_eq!(count(src, "item"), 1);
    }

    #[test]
    fn value_live_after_branch_is_not_moved() {
        let src = "export fn f(a: List<i32>, c: bool) -> i32 { \
                   let n = if c { a.len() } else { a.len() }; return n; }";
        // `a` is read on both branches but never after; each branch use is a
        // per-path last use.
        assert_eq!(count(src, "a"), 2);
    }
}
