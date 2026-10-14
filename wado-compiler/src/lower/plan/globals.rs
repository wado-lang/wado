use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::call_args::CallArgs;
use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::Visibility;
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::flat_package::FlatPackage;
use crate::logger::{Bail, ErrorSink};
use crate::module_source::ModuleSource;
use crate::name::{
    MODULE_INIT_FUNCTION, MODULES_INIT_FLAG, MODULES_INIT_FUNCTION, global_init_target,
};
use crate::synthesis::common::builtin_call;
use crate::tir::{
    FunctionRef, GlobalInit, LocalFrame, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction,
    TirGlobal, TirStmt, TirStmtKind, TypeTable, is_constant_initializer,
};
use crate::tir_visitor::{TirRefVisitor, shift_locals};
use crate::token::Span;

/// A global, by the module declaring it and its name: two modules may each
/// declare one of the same name.
type GlobalKey = (ModuleSource, String);

/// One global initializer, taken out of the `$init$` function reify put it in.
/// It lives only in `functions` until here, so no pass between reify and this
/// one can walk the functions and miss an initializer.
struct LazyInit {
    global: String,
    module_source: ModuleSource,
    /// What the global is assigned, in the frame `frame` describes.
    value: TirExpr,
    frame: LocalFrame,
    /// What `value` reads, assigns and calls.
    scan: BodyReads,
}

/// The value a `$init$` function returns. Reify writes the one return, and
/// every pass since rewrote expressions within it.
fn returned_value(body: TirBlock) -> TirExpr {
    let mut stmts = body.stmts;
    assert_eq!(stmts.len(), 1, "a global initializer is a single statement");
    let Some(TirStmt {
        kind: TirStmtKind::Return { value: Some(value) },
        ..
    }) = stmts.pop()
    else {
        panic!("a global initializer returns its value");
    };
    value
}

/// Take each global initializer out of its own function and into the module's
/// `$initialize_module`, in dependency order. Must run before `boxing`, the
/// extracted code being able to contain the `&primitive` and closure
/// expressions boxing rewrites; the top-level aggregator calling each
/// `$initialize_module` is built later by [`build_initialize_modules`].
pub fn extract(flat: &mut FlatPackage, errors: &dyn ErrorSink) -> Result<(), Bail> {
    // Reify classified these, and nothing since could have changed the answer:
    // the typed IR folds no constant, and turns no literal into code.
    {
        let type_table = flat.type_table.borrow();
        for global in &flat.globals {
            assert!(
                global.init.is_deferred()
                    || is_constant_initializer(global.init.slot_expr(), &type_table),
                "a Direct global stays a Wasm constant: {}",
                global.name
            );
        }
    }

    // Insertion order is preserved so cross-module sibling ordering matches the
    // original global declaration order, which the aggregator then walks in
    // entry-last order (see `build_initialize_modules`).
    let mut by_module: IndexMap<ModuleSource, Vec<LazyInit>> = IndexMap::default();
    flat.functions.retain(|func_rc| {
        let mut func = func_rc.borrow_mut();
        let Some(global) = global_init_target(&func.name).map(str::to_string) else {
            return true;
        };
        let value = returned_value(func.body.take().expect("$init$ carries a body"));
        let mut scan = BodyReads::default();
        scan.visit_expr(&value);
        let init = LazyInit {
            global,
            module_source: func.module_source.clone(),
            value,
            frame: func.take_frame(),
            scan,
        };
        by_module
            .entry(init.module_source.clone())
            .or_default()
            .push(init);
        false
    });
    if by_module.is_empty() {
        return Ok(());
    }

    // An initializer depends on every global the functions it calls read.
    let call_graph = CallGraph::new(&flat.functions);
    let must_reads = call_graph.must_reads(by_module.values().flatten());
    let orders = by_module
        .values()
        .map(|module_inits| {
            topological_sort_global_inits(module_inits, &call_graph, &must_reads, errors)
        })
        .collect::<Result<Vec<_>, Bail>>()?;
    let span = Span::new(0, 0, 1, 1);
    for ((module_source, module_inits), order) in by_module.into_iter().zip(orders) {
        let mut taken: Vec<Option<LazyInit>> = module_inits.into_iter().map(Some).collect();
        let sorted_inits = order
            .into_iter()
            .map(|i| {
                taken[i]
                    .take()
                    .expect("the order names each initializer once")
            })
            .collect();
        let init_func = build_module_init_function(module_source, sorted_inits, span);
        flat.functions.push(Rc::new(RefCell::new(init_func)));
    }
    Ok(())
}

/// Assign every initializer to its global, in the order given, under one frame.
fn build_module_init_function(
    module_source: ModuleSource,
    sorted_inits: Vec<LazyInit>,
    span: Span,
) -> TirFunction {
    let mut init_stmts: Vec<TirStmt> = Vec::new();
    let mut merged = LocalFrame::default();

    for init in sorted_inits {
        let LazyInit {
            global,
            module_source,
            mut value,
            frame,
            ..
        } = init;
        merged.absorb(frame, |offset| shift_locals(&mut value, offset));
        init_stmts.push(set_global(module_source, global, value, span));
    }

    TirFunction::synthesized(
        module_source,
        MODULE_INIT_FUNCTION.to_string(),
        TypeTable::UNIT,
        TirBlock {
            stmts: init_stmts,
            span,
        },
        merged,
        span,
    )
}

fn set_global(module_source: ModuleSource, name: String, value: TirExpr, span: Span) -> TirStmt {
    let global_set = TirExpr::new(
        TirExprKind::GlobalVarSet {
            module_source,
            name,
            value: Box::new(value),
        },
        TypeTable::UNIT,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(global_set), span)
}

/// What a function body reads, assigns and calls, in one walk.
///
/// Callees are keyed by [`FunctionRef::full_name`], the same key
/// [`CallGraph`] maps.
#[derive(Default)]
struct BodyReads {
    globals: IndexSet<GlobalKey>,
    assigned: IndexSet<GlobalKey>,
    callees: IndexSet<String>,
}

impl TirRefVisitor for BodyReads {
    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::GlobalVarGet {
                name,
                module_source,
            } => {
                self.globals.insert((module_source.clone(), name.clone()));
            }
            TirExprKind::GlobalVarSet {
                name,
                module_source,
                ..
            } => {
                self.assigned.insert((module_source.clone(), name.clone()));
            }
            TirExprKind::Call { func, .. } => {
                self.callees.insert(func.full_name());
            }
            // A function named as a value reaches a call this walk cannot see —
            // `apply(reader)` calls `reader` through a parameter, and a closure
            // body is a function of its own. Counting the mention keeps what it
            // reads on the mentioning side of the graph.
            TirExprKind::FuncRef {
                module_source,
                name,
                ..
            } => {
                self.callees.insert(function_key(module_source, name));
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

/// The key [`CallGraph`] maps a function under: the same string
/// [`FunctionRef::full_name`] produces for a free function, so a call and a
/// bare mention of the same function land on one entry.
fn function_key(module_source: &ModuleSource, name: &str) -> String {
    FunctionRef {
        module_source: module_source.clone(),
        name: name.to_string(),
        template: None,
        monomorph_info: None,
        method_info: None,
    }
    .full_name()
}

/// The functions with a body, what each calls, and the globals each may read
/// or assign.
struct CallGraph<'a> {
    bodies: IndexMap<String, &'a Rc<RefCell<TirFunction>>>,
    callees: IndexMap<String, IndexSet<String>>,
    /// Every global each function reads, closed over `callees`.
    ///
    /// An initializer's dependency is not always written in it: a call reads
    /// globals the caller never names, and ordering on the directly-written
    /// references alone leaves such a global still holding its placeholder
    /// when the caller runs.
    reads: IndexMap<String, IndexSet<GlobalKey>>,
    /// Every global each function assigns, closed over `callees`.
    assigned: IndexMap<String, IndexSet<GlobalKey>>,
}

impl<'a> CallGraph<'a> {
    fn new(functions: &'a [Rc<RefCell<TirFunction>>]) -> Self {
        let mut graph = Self {
            bodies: IndexMap::default(),
            callees: IndexMap::default(),
            reads: IndexMap::default(),
            assigned: IndexMap::default(),
        };
        for func_rc in functions {
            let func = func_rc.borrow();
            let Some(body) = func.body.as_ref() else {
                continue;
            };
            let mut scan = BodyReads::default();
            scan.visit_block(body);
            let key = FunctionRef::from_resolved(&func, func.module_source.clone()).full_name();
            graph.bodies.insert(key.clone(), func_rc);
            graph.reads.insert(key.clone(), scan.globals);
            graph.assigned.insert(key.clone(), scan.assigned);
            graph.callees.insert(key, scan.callees);
        }
        close_over_calls(&mut graph.reads, &graph.callees);
        close_over_calls(&mut graph.assigned, &graph.callees);
        graph
    }

    /// What `scan` and the functions it reaches may assign.
    fn assigned_from(&self, scan: &BodyReads) -> IndexSet<GlobalKey> {
        let mut assigned = scan.assigned.clone();
        for callee in &scan.callees {
            assigned.extend(self.assigned.get(callee).into_iter().flatten().cloned());
        }
        assigned
    }

    /// The globals each function the initializers reach reads on every path
    /// through it, leaving out any it or what it reaches may assign.
    ///
    /// A least fixpoint, rising from nothing known: a recursive function's
    /// answer under-approximates, and the same whatever order it is asked in.
    /// Only the callers of a function whose answer grew are walked again.
    fn must_reads<'i>(
        &self,
        inits: impl IntoIterator<Item = &'i LazyInit>,
    ) -> IndexMap<String, IndexSet<GlobalKey>> {
        let mut reached: IndexSet<&str> = IndexSet::default();
        let mut pending: Vec<&str> = inits
            .into_iter()
            .flat_map(|init| &init.scan.callees)
            .map(String::as_str)
            .collect();
        while let Some(name) = pending.pop() {
            let Some((name, _)) = self.bodies.get_key_value(name) else {
                continue;
            };
            if reached.insert(name) {
                pending.extend(self.callees[name].iter().map(String::as_str));
            }
        }
        let mut callers: IndexMap<&str, Vec<&str>> = IndexMap::default();
        for &caller in &reached {
            for callee in &self.callees[caller] {
                callers.entry(callee.as_str()).or_default().push(caller);
            }
        }

        let mut known: IndexMap<String, IndexSet<GlobalKey>> = IndexMap::default();
        let mut queued: IndexSet<&str> = reached.clone();
        while let Some(name) = queued.pop() {
            let func = self.bodies[name].borrow();
            let body = func.body.as_ref().expect("the graph holds bodies only");
            let mut walk = MustReads::new(&known);
            walk.visit_block(body);
            let reads: IndexSet<GlobalKey> = walk
                .reads
                .difference(&self.assigned[name])
                .cloned()
                .collect();
            if known.get(name).is_none_or(|old| old.len() < reads.len()) {
                known.insert(name.to_string(), reads);
                queued.extend(callers.get(name).into_iter().flatten());
            }
        }
        known
    }
}

/// Extend each function's set by its callees', until nothing grows: a least
/// fixpoint, so a recursive cycle contributes each member's own and
/// terminates.
fn close_over_calls(
    sets: &mut IndexMap<String, IndexSet<GlobalKey>>,
    callees: &IndexMap<String, IndexSet<String>>,
) {
    loop {
        let mut grown: Vec<(String, IndexSet<GlobalKey>)> = Vec::new();
        for (name, called) in callees {
            let known = &sets[name];
            let fresh: IndexSet<GlobalKey> = called
                .iter()
                .filter_map(|callee| sets.get(callee))
                .flatten()
                .filter(|global| !known.contains(*global))
                .cloned()
                .collect();
            if !fresh.is_empty() {
                grown.push((name.clone(), fresh));
            }
        }
        if grown.is_empty() {
            return;
        }
        for (name, fresh) in grown {
            sets[&name].extend(fresh);
        }
    }
}

/// The globals read on every path through what it walks, up to the first point
/// a path can leave it.
///
/// Once a `return`, `break` or `continue` may have been taken, what follows is
/// not on every path, so it counts only after the construct that exit leaves.
/// Branches count what they all read. A closure body runs only when called, and
/// a callee read through a function value is unknown, so neither counts.
struct MustReads<'k> {
    /// What each callee is known to read on every path through it.
    by_function: &'k IndexMap<String, IndexSet<GlobalKey>>,
    reads: IndexSet<GlobalKey>,
    /// The loops and labeled blocks being walked, innermost last.
    targets: Vec<Option<String>>,
    /// The outermost construct a path may have left so far, as its depth in
    /// `targets` (`0` is the function). While set, nothing counts.
    exited_to: Option<usize>,
}

impl<'k> MustReads<'k> {
    fn new(by_function: &'k IndexMap<String, IndexSet<GlobalKey>>) -> Self {
        Self {
            by_function,
            reads: IndexSet::default(),
            targets: Vec::new(),
            exited_to: None,
        }
    }

    fn exit_to(&mut self, depth: usize) {
        self.exited_to = Some(self.exited_to.map_or(depth, |d| d.min(depth)));
    }

    /// The depth of the construct a `break` or `continue` leaves.
    fn target_depth(&self, label: Option<&str>) -> usize {
        let index = self
            .targets
            .iter()
            .rposition(|target| match label {
                Some(label) => target.as_deref() == Some(label),
                None => target.is_none(),
            })
            .expect("a break or continue sits inside its target");
        index + 1
    }

    /// Walk a loop or labeled block: an exit to it lands right after it.
    fn within(&mut self, label: Option<String>, block: &TirBlock) {
        self.targets.push(label);
        let depth = self.targets.len();
        self.visit_block(block);
        self.targets.pop();
        if self.exited_to == Some(depth) {
            self.exited_to = None;
        }
    }

    /// What each alternative reads on every path, and the outermost exit any
    /// of them may take.
    fn alternatives<'e>(&mut self, branches: impl IntoIterator<Item = Branch<'e>>) {
        let mut common: Option<IndexSet<GlobalKey>> = None;
        for branch in branches {
            let mut walk = MustReads {
                targets: self.targets.clone(),
                ..MustReads::new(self.by_function)
            };
            match branch {
                Branch::Block(block) => walk.visit_block(block),
                Branch::Expr(expr) => walk.visit_expr(expr),
                Branch::Empty => {}
            }
            let MustReads {
                reads, exited_to, ..
            } = walk;
            common = Some(match common {
                None => reads,
                Some(common) => common.intersection(&reads).cloned().collect(),
            });
            if let Some(depth) = exited_to {
                self.exit_to(depth);
            }
        }
        self.reads.extend(common.into_iter().flatten());
    }

    fn branch_on(&mut self, condition: &TirExpr, then: &TirBlock, otherwise: Option<&TirBlock>) {
        self.visit_expr(condition);
        if self.exited_to.is_none() {
            let otherwise = otherwise.map_or(Branch::Empty, Branch::Block);
            self.alternatives([Branch::Block(then), otherwise]);
        }
    }
}

enum Branch<'e> {
    Block(&'e TirBlock),
    Expr(&'e TirExpr),
    Empty,
}

/// Every kind is named, so a new one is classified as strict or as branching
/// rather than counted as always evaluated.
impl TirRefVisitor for MustReads<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if self.exited_to.is_some() {
            return;
        }
        match &stmt.kind {
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => self.branch_on(condition, then_block, else_block.as_ref()),
            TirStmtKind::Loop { body } => self.within(None, body),
            TirStmtKind::LabeledBlock { label, block } => self.within(Some(label.clone()), block),
            TirStmtKind::Return { value } => {
                if let Some(value) = value {
                    self.visit_expr(value);
                }
                self.exit_to(0);
            }
            TirStmtKind::Break { label, value } => {
                if let Some(value) = value {
                    self.visit_expr(value);
                }
                let depth = self.target_depth(label.as_deref());
                self.exit_to(depth);
            }
            TirStmtKind::Continue => {
                let depth = self.target_depth(None);
                self.exit_to(depth);
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("monomorphization expands a variadic for-of")
            }
            TirStmtKind::Let { .. }
            | TirStmtKind::LetDestructure { .. }
            | TirStmtKind::Expr(_)
            | TirStmtKind::TaskReturn { .. } => self.walk_stmt(stmt),
        }
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        if self.exited_to.is_some() {
            return;
        }
        match &expr.kind {
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            } => {
                self.reads.insert((module_source.clone(), name.clone()));
            }
            TirExprKind::Call { func, args, .. } => {
                for arg in args {
                    self.visit_expr(&arg.expr);
                }
                if self.exited_to.is_none()
                    && let Some(callee_reads) = self.by_function.get(&func.full_name())
                {
                    self.reads.extend(callee_reads.iter().cloned());
                }
            }
            TirExprKind::Binary {
                left,
                op: TirBinaryOp::And | TirBinaryOp::Or,
                right,
            } => {
                self.visit_expr(left);
                if self.exited_to.is_none() {
                    self.alternatives([Branch::Expr(right), Branch::Empty]);
                }
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => self.branch_on(condition, then_branch, else_branch.as_ref()),
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.visit_expr(scrutinee);
                // A guard runs only once its pattern matches.
                for guard in arms.iter().filter_map(|arm| arm.guard.as_ref()) {
                    if self.exited_to.is_none() {
                        self.alternatives([Branch::Expr(guard), Branch::Empty]);
                    }
                }
                if self.exited_to.is_none() {
                    self.alternatives(arms.iter().map(|arm| Branch::Expr(&arm.body)));
                }
            }
            TirExprKind::LabeledBlock { label, block, .. } => {
                self.within(Some(label.clone()), block);
            }
            TirExprKind::Closure { .. } => {}
            TirExprKind::TypePackExpansion { .. }
            | TirExprKind::VariadicTupleComprehension { .. } => {
                unreachable!("monomorphization expands a pack")
            }
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. }
            | TirExprKind::GlobalVarSet { .. }
            | TirExprKind::Binary { .. }
            | TirExprKind::Unary { .. }
            | TirExprKind::Assign { .. }
            | TirExprKind::Cast { .. }
            | TirExprKind::CmRawCall { .. }
            | TirExprKind::FieldAccess { .. }
            | TirExprKind::Index { .. }
            | TirExprKind::Block(_)
            | TirExprKind::StructLiteral { .. }
            | TirExprKind::TupleLiteral { .. }
            | TirExprKind::ArrayLiteral { .. }
            | TirExprKind::TupleSpread { .. }
            | TirExprKind::TupleZip { .. }
            | TirExprKind::TupleLen { .. }
            | TirExprKind::IndirectCall { .. }
            | TirExprKind::VariantConstruct { .. }
            | TirExprKind::VariantTag { .. }
            | TirExprKind::VariantTest { .. }
            | TirExprKind::VariantPayload { .. }
            | TirExprKind::TemplateString { .. }
            | TirExprKind::WithHandler { .. } => self.walk_expr(expr),
        }
    }
}

/// The globals an initializer depends on, kept apart by how certainly.
struct InitRefs {
    /// Named anywhere in it, a closure body included, which orders it after
    /// them. Its own name there is no dependency: a closure may call it later.
    named: IndexSet<GlobalKey>,
    /// Read on every path through it, its own global included.
    must: IndexSet<GlobalKey>,
    /// Read by the functions it reaches, on some path.
    may: IndexSet<GlobalKey>,
}

fn collect_global_refs(
    init: &LazyInit,
    call_graph: &CallGraph,
    must_reads: &IndexMap<String, IndexSet<GlobalKey>>,
) -> InitRefs {
    let scan = &init.scan;
    let mut walk = MustReads::new(must_reads);
    walk.visit_expr(&init.value);
    let must: IndexSet<GlobalKey> = walk
        .reads
        .difference(&call_graph.assigned_from(scan))
        .cloned()
        .collect();
    let may = scan
        .callees
        .iter()
        .filter_map(|callee| call_graph.reads.get(callee))
        .flatten()
        .filter(|global| !scan.globals.contains(*global) && !must.contains(*global))
        .cloned()
        .collect();
    InitRefs {
        named: scan.globals.clone(),
        must,
        may,
    }
}

/// Whether `from` already depends on `to`, directly or transitively — so an
/// edge `to → from` would close a cycle.
fn depends_on(deps: &[IndexSet<usize>], from: usize, to: usize) -> bool {
    let mut seen = vec![false; deps.len()];
    let mut stack = vec![from];
    while let Some(node) = stack.pop() {
        if node == to {
            return true;
        }
        if std::mem::replace(&mut seen[node], true) {
            continue;
        }
        stack.extend(deps[node].iter().copied());
    }
    false
}

/// The order to run `lazy_inits` in, as indices into it, each initializer after
/// every one it depends on.
fn topological_sort_global_inits(
    lazy_inits: &[LazyInit],
    call_graph: &CallGraph,
    must_reads: &IndexMap<String, IndexSet<GlobalKey>>,
    errors: &dyn ErrorSink,
) -> Result<Vec<usize>, Bail> {
    // Build a map from `(module_source, name)` to its index in
    // lazy_inits. The compound key keeps cross-module same-named
    // globals separated when both happen to share a topo-sort input
    // (today the planner partitions by module, but the keying is
    // defensive against any future re-merge).
    let key_to_idx: IndexMap<GlobalKey, usize> = lazy_inits
        .iter()
        .enumerate()
        .map(|(i, init)| ((init.module_source.clone(), init.global.clone()), i))
        .collect();

    // Build dependency graph: deps[i] = set of indices that i depends on.
    let mut deps: Vec<IndexSet<usize>> = vec![IndexSet::default(); lazy_inits.len()];
    let indices = |refs: &IndexSet<GlobalKey>| -> Vec<usize> {
        refs.iter()
            .filter_map(|key| key_to_idx.get(key).copied())
            .collect()
    };

    let scanned: Vec<InitRefs> = lazy_inits
        .iter()
        .map(|init| collect_global_refs(init, call_graph, must_reads))
        .collect();

    // A global named in the initializer, or read on every path through it, is
    // a definite dependency, and a cycle among those is an error.
    for (i, refs) in scanned.iter().enumerate() {
        deps[i].extend(indices(&refs.named).into_iter().filter(|&dep| dep != i));
        deps[i].extend(indices(&refs.must));
    }
    // What a callee may read is path-insensitive: a helper that reads two
    // globals makes each initializer calling it look dependent on the other,
    // even when no execution reads both. Such an edge yields rather than
    // manufacturing a cycle out of a program that has none.
    for (i, refs) in scanned.iter().enumerate() {
        for dep in indices(&refs.may) {
            if dep != i && !depends_on(&deps, dep, i) {
                deps[i].insert(dep);
            }
        }
    }

    // Kahn's algorithm for topological sort
    let mut in_degree: Vec<usize> = deps.iter().map(IndexSet::len).collect();
    let mut queue: VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter(|(_, d)| **d == 0)
        .map(|(i, _)| i)
        .collect();

    let mut sorted: Vec<usize> = Vec::with_capacity(lazy_inits.len());

    while let Some(idx) = queue.pop_front() {
        sorted.push(idx);

        // Update dependents
        for (i, dep_set) in deps.iter().enumerate() {
            if dep_set.contains(&idx) {
                in_degree[i] -= 1;
                if in_degree[i] == 0 {
                    queue.push_back(i);
                }
            }
        }
    }

    if sorted.len() < lazy_inits.len() {
        let cycle = blocking_cycle(&deps, &in_degree);
        let names: Vec<&str> = cycle
            .iter()
            .map(|&i| lazy_inits[i].global.as_str())
            .collect();
        let LazyInit {
            module_source,
            value,
            ..
        } = &lazy_inits[cycle[0]];
        return Err(errors.fatal_in(
            module_source,
            Diagnostic {
                severity: Severity::Error,
                code: Code::CircularDependency,
                message: if let [name] = names[..] {
                    format!(
                        "the initializer of `{name}` reads `{name}` on every path, \
                         before it has been given a value."
                    )
                } else {
                    format!(
                        "global initializers form a cycle: {}. Each waits for a value \
                         another has not been given yet, so none can go first.",
                        names.join(", ")
                    )
                },
                span: Some(DiagnosticSpan::from_span(&value.span, None)),
            },
        ));
    }

    Ok(sorted)
}

/// One cycle among the initializers the sort left unplaced, in dependency
/// order. Those include initializers that merely wait on a cycle, so it
/// follows unplaced dependencies until one repeats.
fn blocking_cycle(deps: &[IndexSet<usize>], in_degree: &[usize]) -> Vec<usize> {
    let mut path: Vec<usize> = Vec::new();
    let mut node = in_degree
        .iter()
        .position(|&d| d > 0)
        .expect("an initializer is unplaced");
    while !path.contains(&node) {
        path.push(node);
        node = *deps[node]
            .iter()
            .find(|&&dep| in_degree[dep] > 0)
            .expect("an unplaced initializer waits on another unplaced one");
    }
    let start = path.iter().position(|&n| n == node).expect("node repeats");
    let mut cycle = path.split_off(start);
    // Lead with the first declared, wherever the walk entered the cycle.
    let first = (0..cycle.len())
        .min_by_key(|&i| cycle[i])
        .expect("a cycle has a member");
    cycle.rotate_left(first);
    cycle
}

/// Order the per-module initializers so each runs after the modules whose
/// globals it reads, entry last.
///
/// [`topological_sort_global_inits`] settles the order within a module; a
/// global read across a module boundary needs the same treatment one level up,
/// or the reader finds a placeholder. Discovery order is no substitute: it
/// tracks how the loader happened to reach the modules.
fn sort_modules_by_dependency(
    modules: &mut Vec<ModuleSource>,
    entry_source: &ModuleSource,
    functions: &[Rc<RefCell<TirFunction>>],
) {
    let reads = CallGraph::new(functions).reads;
    let module_deps = |module: &ModuleSource| -> IndexSet<ModuleSource> {
        let key = function_key(module, MODULE_INIT_FUNCTION);
        reads
            .get(&key)
            .into_iter()
            .flatten()
            .map(|(source, _)| source.clone())
            .filter(|source| source != module)
            .collect()
    };

    let mut ordered: Vec<ModuleSource> = Vec::with_capacity(modules.len());
    let mut placed: IndexSet<ModuleSource> = IndexSet::default();
    // Entry last: it is the one module every other is linked into, and a cycle
    // among the rest — which imports cannot form — would otherwise strand it.
    let mut pending: Vec<ModuleSource> = modules
        .iter()
        .filter(|ms| *ms != entry_source)
        .cloned()
        .collect();

    while !pending.is_empty() {
        let ready = pending
            .iter()
            .position(|ms| module_deps(ms).iter().all(|dep| placed.contains(dep)));
        // No module is ready only if the remainder depends on each other, which
        // the import graph cannot express. Take one so the loop terminates.
        let next = pending.remove(ready.unwrap_or(0));
        placed.insert(next.clone());
        ordered.push(next);
    }
    if modules.iter().any(|ms| ms == entry_source) {
        ordered.push(entry_source.clone());
    }
    *modules = ordered;
}

/// Build the top-level aggregator calling every module's
/// `$initialize_module`. Must run after [`extract`] has created them.
pub fn build_initialize_modules(flat: &mut FlatPackage) {
    let entry_source = flat.entry_module_source.clone();

    let mut modules_with_init: Vec<ModuleSource> = Vec::new();
    let mut seen = IndexSet::default();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        if func.name == MODULE_INIT_FUNCTION && seen.insert(func.module_source.clone()) {
            modules_with_init.push(func.module_source.clone());
        }
    }

    if modules_with_init.is_empty() {
        return;
    }

    sort_modules_by_dependency(&mut modules_with_init, &entry_source, &flat.functions);

    let span = Span::new(0, 0, 1, 1);

    flat.globals.push(TirGlobal {
        name: MODULES_INIT_FLAG.to_string(),
        ty: TypeTable::BOOL,
        init: GlobalInit::Direct(TirExpr::new(
            TirExprKind::BoolLiteral(false),
            TypeTable::BOOL,
            span,
        )),
        param: None,
        wado_mutable: true,
        visibility: Visibility::Private,
        module_source: entry_source.clone(),
        span,
    });

    let mut init_stmts: Vec<TirStmt> = Vec::new();

    let flag_check = TirExpr::new(
        TirExprKind::GlobalVarGet {
            module_source: entry_source.clone(),
            name: MODULES_INIT_FLAG.to_string(),
        },
        TypeTable::BOOL,
        span,
    );
    let early_return_stmt = TirStmt::new(TirStmtKind::Return { value: None }, span);
    let early_return_block = TirBlock {
        stmts: vec![early_return_stmt],
        span,
    };
    let if_already_init = TirStmt::new(
        TirStmtKind::If {
            condition: flag_check,
            then_block: early_return_block,
            else_block: None,
        },
        span,
    );
    init_stmts.push(if_already_init);

    // The fall-through below the guard runs once per program: mark it cold so
    // the guard is hinted likely-taken (`hint_guard_fall_through`), including
    // the copies inlined into each export entry, and the inliner excludes the
    // one-shot init calls from its cost estimate.
    init_stmts.push(TirStmt::new(
        TirStmtKind::Expr(builtin_call("cold_path", Vec::new(), TypeTable::UNIT)),
        span,
    ));

    for module_source in &modules_with_init {
        let call = TirExpr::new(
            TirExprKind::Call {
                func: Box::new(FunctionRef {
                    module_source: module_source.clone(),
                    name: MODULE_INIT_FUNCTION.to_string(),
                    template: None,
                    monomorph_info: None,
                    method_info: None,
                }),
                type_args: Vec::new(),
                args: CallArgs::free(Vec::new()),
            },
            TypeTable::UNIT,
            span,
        );
        init_stmts.push(TirStmt::new(TirStmtKind::Expr(call), span));
    }

    let initialized = TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, span);
    init_stmts.push(set_global(
        entry_source.clone(),
        MODULES_INIT_FLAG.to_string(),
        initialized,
        span,
    ));

    let init_body = TirBlock {
        stmts: init_stmts,
        span,
    };

    let init_modules_func = TirFunction::synthesized(
        entry_source.clone(),
        MODULES_INIT_FUNCTION.to_string(),
        TypeTable::UNIT,
        init_body,
        LocalFrame::default(),
        span,
    );
    flat.functions
        .push(Rc::new(RefCell::new(init_modules_func)));

    let init_call = TirExpr::new(
        TirExprKind::Call {
            func: Box::new(FunctionRef {
                module_source: entry_source.clone(),
                name: MODULES_INIT_FUNCTION.to_string(),
                template: None,
                monomorph_info: None,
                method_info: None,
            }),
            type_args: Vec::new(),
            args: CallArgs::free(Vec::new()),
        },
        TypeTable::UNIT,
        span,
    );
    let init_call_stmt = TirStmt::new(TirStmtKind::Expr(init_call), span);

    for func_rc in &flat.functions {
        let mut func = func_rc.borrow_mut();
        if func.module_source != entry_source {
            continue;
        }
        if func.is_export
            && let Some(ref mut body) = func.body
        {
            body.stmts.insert(0, init_call_stmt.clone());
        }
    }
}
