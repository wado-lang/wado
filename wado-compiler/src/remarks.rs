//! Optimizer remarks (WEP 2026-06-03): the costs that survive optimization and
//! would otherwise be invisible while coding — a value-semantic copy, a branch a
//! build-time parameter still decides, a block computing a constant at run time.
//!
//! NIR is the last IR carrying per-expression spans and `wir_build` lowers these
//! one-to-one, so walking optimized NIR yields exact source locations. Detection
//! covers the entry package alone: a dependency's internals would drown out the
//! program.

use cranelift_entity::EntityRef;

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::nir::{FunctionRef, NirUnaryOp};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, StmtKind};
use crate::nir_package::NirPackage;
use crate::niri::{
    CtfeBuiltin, CtfeBuiltinMap, build_callee_map, build_ctfe_builtin_map, is_ctfe_runnable,
    materializing_globals, region_queries,
};
use crate::tir::{TypeId, TypeTable};
use crate::token::Span;

/// A single optimizer remark: a residual cost with its source span.
pub struct Remark {
    /// The remark text, without the `remark:` prefix the logger adds.
    pub message: String,
    /// The module the span belongs to. A span carries no file, and the entry
    /// package is more than one.
    pub module: ModuleSource,
    /// Where the cost survives, in the original source.
    pub span: Span,
}

/// Collect remarks for value-semantic copies that survive optimization,
/// restricted to functions the entry package defines.
pub fn collect_value_copy_remarks(package: &NirPackage) -> Vec<Remark> {
    let value_copy_set = package.value_copy_helper_types();
    // Callee descriptor by `func_id` (the call node carries no `FunctionRef`).
    let callees: Vec<FunctionRef> = package
        .functions
        .iter()
        .map(|f| {
            let f = f.borrow();
            FunctionRef::from_resolved(&f, f.module_source.clone())
        })
        .collect();

    let type_table_ref = package.type_table.borrow();
    let type_table: &TypeTable = &type_table_ref;
    let mut remarks = Vec::new();
    for func_rc in &package.functions {
        let func = func_rc.borrow();
        // Skipping helper *bodies* keeps the copies inside a value-copy helper
        // from being reported against the helper itself.
        if !func.module_source.is_entry_package() || func.is_value_copy() {
            continue;
        }
        let Some(body) = &func.body else {
            continue;
        };
        let mut collector = Collector {
            value_copy_set: &value_copy_set,
            callees: &callees,
            type_table,
            module: func.module_source.clone(),
            remarks: &mut remarks,
            current_span: body.blocks[body.root].span,
            in_value_block: false,
        };
        collector.scan_node(body, NodeRef::Block(body.root));
    }
    remarks
}

struct Collector<'a> {
    value_copy_set: &'a IndexMap<(ModuleSource, String), TypeId>,
    /// Callee descriptor indexed by `func_id.index()`. See [`collect_value_copy_remarks`].
    callees: &'a [FunctionRef],
    type_table: &'a TypeTable,
    module: ModuleSource,
    remarks: &'a mut Vec<Remark>,
    /// Span of the enclosing real statement. Synthesized copy nodes
    /// (`array_clone`, demoted spine copies) carry placeholder spans, so the
    /// remark points at the statement that performs the copy (`let mut b = a;`).
    current_span: Span,
    /// True while walking a block used as an expression value, whose inner
    /// statements are synthesized and must not override `current_span`.
    in_value_block: bool,
}

impl Collector<'_> {
    /// If the expression `id` is a surviving value-copy operation, return the
    /// type whose value is copied.
    fn copied_type(&self, body: &Body, id: ExprId) -> Option<TypeId> {
        let node = &body.exprs[id];
        let ExprKind::Call { func_id, args, .. } = &node.kind else {
            return None;
        };
        let func = self.callees.get(func_id.index())?;
        // Deep `$value_copy$T` helper call.
        if args.len() == 1
            && let Some(&type_id) = self
                .value_copy_set
                .get(&(func.module_source.clone(), func.name.clone()))
        {
            return Some(type_id);
        }
        // Lowered / demoted copies. `array_copy` is excluded on purpose: it is
        // bulk buffer movement inside stdlib helpers, not a value-semantic copy.
        // `array_clone_shallow` is interned as a plain builtin (no
        // `monomorph_info`, since a spine copy needs no element type), so classify
        // by either the monomorphized- or the plain-builtin name.
        let builtin = func
            .monomorphized_builtin_name()
            .or_else(|| func.builtin_name());
        match builtin.as_deref() {
            // `array_clone(&agg.repr)` copies a `List<T>` / `String` backing
            // array; recover the owning aggregate type from the argument.
            Some(
                "builtin::array_clone"
                | "builtin::array_clone_prefix"
                | "builtin::array_clone_shallow",
            ) => args
                .first()
                .and_then(|a| a.expr.as_expr())
                .map(|e| clone_source_type(body, e)),
            // `copy_value(v)` deep-copies a value directly, so the call's result
            // type is the copied type.
            Some("builtin::copy_value") => Some(node.type_id),
            _ => None,
        }
    }

    /// Walk the arena body parent-before-children, mirroring the previous tree
    /// visitor: a real statement sets the current span, a block-as-value freezes
    /// it (its synthesized inner statements carry placeholder spans), and a
    /// surviving copy expression is reported against the current span.
    fn scan_node(&mut self, body: &Body, node: NodeRef) {
        match node {
            NodeRef::Stmt(s) => {
                if self.in_value_block {
                    // Inside a block-as-value: keep the enclosing real
                    // statement's span rather than the synthesized inner
                    // statement's placeholder span.
                    self.scan_children(body, node);
                } else {
                    let prev = self.current_span;
                    self.current_span = body.stmts[s].span;
                    self.scan_children(body, node);
                    self.current_span = prev;
                }
            }
            NodeRef::Expr(e) => {
                if let Some(type_id) = self.copied_type(body, e) {
                    let type_name = self.type_table.type_name(type_id);
                    self.remarks.push(Remark {
                        message: format!("a copy of `{type_name}` survives optimization"),
                        module: self.module.clone(),
                        span: self.current_span,
                    });
                }
                if is_value_block(body, e) {
                    let prev = self.in_value_block;
                    self.in_value_block = true;
                    self.scan_children(body, node);
                    self.in_value_block = prev;
                } else {
                    self.scan_children(body, node);
                }
            }
            NodeRef::Block(_) | NodeRef::Pat(_) => self.scan_children(body, node),
        }
    }

    fn scan_children(&mut self, body: &Body, node: NodeRef) {
        let mut kids = Vec::new();
        body.for_each_child(node, |c| kids.push(c));
        for c in kids {
            self.scan_node(body, c);
        }
    }
}

/// Block-shaped expressions used as a value. Their inner statement lists come
/// from lowering / SROA reconstruction and carry placeholder spans, so the
/// remark anchors to the enclosing real statement instead of descending into
/// them.
fn is_value_block(body: &Body, id: ExprId) -> bool {
    matches!(
        body.exprs[id].kind,
        ExprKind::LabeledBlock { .. }
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Switch { .. }
    )
}

/// For an `array_clone(&agg.repr)` call, recover the aggregate type (`List<T>`
/// or `String`) that owns the cloned `repr`, peeling the `&`/`&mut` reference
/// and the `.repr` field access. Falls back to the argument's own type.
fn clone_source_type(body: &Body, arg: ExprId) -> TypeId {
    let inner = match &body.exprs[arg].kind {
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr,
        } => *expr,
        _ => arg.into(),
    };
    // A promoted `Operand::Value` has no skeleton place; fall back to the
    // argument's own type (this only feeds a diagnostic remark).
    let Some(inner_e) = inner.as_expr() else {
        return body.exprs[arg].type_id;
    };
    match &body.exprs[inner_e].kind {
        ExprKind::FieldAccess { expr, .. } => expr
            .as_expr()
            .map_or(body.exprs[inner_e].type_id, |e| body.exprs[e].type_id),
        _ => body.exprs[inner_e].type_id,
    }
}

/// Collect remarks for compile-time parameters that still decide a branch in
/// the optimized NIR — a build asked to strip `trace` and `debug` shipping them
/// and consulting the parameter at run time.
///
/// A read outside a scrutinee is an ordinary use of the value, not a gate that
/// failed, and is not reported. Scoped to the entry package, like the
/// value-copy remarks.
pub fn collect_param_gate_remarks(package: &NirPackage) -> Vec<Remark> {
    let params = parameter_decided_globals(package);
    if params.is_empty() {
        return Vec::new();
    }

    let mut scan = ParamGateScan {
        params: &params,
        module: package.entry_module_source.clone(),
        reported: crate::hashmap::IndexSet::default(),
        remarks: Vec::new(),
    };
    for func_rc in &package.functions {
        let func = func_rc.borrow();
        if !func.module_source.is_entry_package() {
            continue;
        }
        let Some(body) = &func.body else {
            continue;
        };
        scan.module = func.module_source.clone();
        scan.walk(body);
    }
    scan.remarks
}

/// Every global a build-time parameter decides the value of, and how: the
/// `#[param]` globals, plus any global assigned from one.
///
/// The second kind is what `core:log` reads at its gate
/// (`global LOG_STATIC_LEVEL: Level = level_from_str(LOG_LEVEL)`).
/// `globals::extract` has moved such an initializer into `__initialize_module`
/// by now, so the assignment is an ordinary `GlobalVarSet` in a body.
fn parameter_decided_globals(package: &NirPackage) -> IndexMap<GlobalKey, ParamOrigin> {
    let mut decided: IndexMap<GlobalKey, ParamOrigin> = package
        .globals
        .iter()
        .filter_map(|g| {
            let param = g.param_name.clone()?;
            let key = (g.module_source.clone(), g.name.clone());
            Some((key, ParamOrigin { param, via: None }))
        })
        .collect();
    if decided.is_empty() {
        return decided;
    }

    // A chain (`A = f(PARAM); B = g(A)`) settles in as many rounds as it is
    // long, and each round only grows the map, so this terminates.
    loop {
        let mut grew = false;
        for func_rc in &package.functions {
            let func = func_rc.borrow();
            let Some(body) = &func.body else {
                continue;
            };
            for (assigned, value) in global_assignments(body) {
                if decided.contains_key(&assigned) {
                    continue;
                }
                let Some(param) = reads_decided_global(body, value, &decided) else {
                    continue;
                };
                let via = Some(assigned.1.clone());
                decided.insert(assigned, ParamOrigin { param, via });
                grew = true;
            }
        }
        if !grew {
            return decided;
        }
    }
}

/// Which parameter decides a global, and the global's own name when the gate
/// will not name the parameter itself.
struct ParamOrigin {
    param: String,
    via: Option<String>,
}

/// Every `GlobalVarSet` reachable from `body`'s root, as the global it writes
/// and the operand it writes there.
fn global_assignments(body: &Body) -> Vec<(GlobalKey, crate::nir_arena::Operand)> {
    let mut out = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(e) = node
            && let ExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } = &body.exprs[e].kind
        {
            out.push(((module_source.clone(), name.clone()), *value));
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    out
}

/// The parameter deciding some global that the expression rooted at `op` reads.
fn reads_decided_global(
    body: &Body,
    op: crate::nir_arena::Operand,
    decided: &IndexMap<GlobalKey, ParamOrigin>,
) -> Option<String> {
    let mut stack = vec![NodeRef::Expr(op.as_expr()?)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(e) = node
            && let ExprKind::GlobalVarGet {
                module_source,
                name,
            } = &body.exprs[e].kind
            && let Some(origin) = decided.get(&(module_source.clone(), name.clone()))
        {
            return Some(origin.param.clone());
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    None
}

type GlobalKey = (ModuleSource, String);

struct ParamGateScan<'a> {
    params: &'a IndexMap<GlobalKey, ParamOrigin>,
    /// The module currently being walked; a `Remark` records where it was
    /// found, since one span means nothing without its file.
    module: ModuleSource,
    /// `(parameter, file, gate span)` triples already remarked on, so one
    /// source gate yields one remark however many times a scrutinee reads the
    /// parameter and however many callers inlining copied it into. The module
    /// is in the key because a span carries no file.
    reported: crate::hashmap::IndexSet<(String, ModuleSource, Span)>,
    remarks: Vec<Remark>,
}

impl ParamGateScan<'_> {
    /// Walk from the root, not over the arena: `StmtKind` has no tombstone, so
    /// a detached `If` still occupies its slot.
    ///
    /// Each node carries the span of the innermost scrutinee it sits inside, or
    /// `None` outside every scrutinee. It passes to children unchanged and is
    /// replaced, never cleared, on entering a nested scrutinee.
    fn walk(&mut self, body: &Body) {
        let mut stack = vec![(NodeRef::Block(body.root), None)];
        while let Some((node, gate)) = stack.pop() {
            if let (NodeRef::Expr(e), Some(span)) = (node, gate) {
                self.report(body, e, span);
            }
            let scrutinee = branch_gate(body, node)
                .and_then(|(op, span)| Some((NodeRef::Expr(op.as_expr()?), span)));
            body.for_each_child(node, |c| {
                let child_gate = match scrutinee {
                    Some((s, span)) if s == c => Some(span),
                    _ => gate,
                };
                stack.push((c, child_gate));
            });
        }
    }

    fn report(&mut self, body: &Body, expr: ExprId, span: Span) {
        let ExprKind::GlobalVarGet {
            module_source,
            name,
        } = &body.exprs[expr].kind
        else {
            return;
        };
        let Some(origin) = self.params.get(&(module_source.clone(), name.clone())) else {
            return;
        };
        let param = &origin.param;
        if !self
            .reported
            .insert((param.clone(), self.module.clone(), span))
        {
            return;
        }
        let read = match &origin.via {
            Some(global) => format!("is still read here through global `{global}`"),
            None => "is still read here".to_string(),
        };
        self.remarks.push(Remark {
            message: format!(
                "compile-time parameter `{param}` {read}, so this branch is \
                 decided at run time; the code it guards was not stripped"
            ),
            module: self.module.clone(),
            span,
        });
    }
}

/// The operand whose value picks a branch, with the span a remark about it
/// should point at. `None` for every non-branching node.
fn branch_gate(body: &Body, node: NodeRef) -> Option<(crate::nir_arena::Operand, Span)> {
    match node {
        NodeRef::Stmt(s) => match &body.stmts[s].kind {
            crate::nir_arena::StmtKind::If { condition, .. } => {
                Some((*condition, body.stmts[s].span))
            }
            _ => None,
        },
        NodeRef::Expr(e) => match &body.exprs[e].kind {
            ExprKind::If { condition, .. } => Some((*condition, body.exprs[e].span)),
            ExprKind::Match { expr, .. } => Some((*expr, body.exprs[e].span)),
            ExprKind::Switch { scrutinee, .. } => Some((*scrutinee, body.exprs[e].span)),
            _ => None,
        },
        NodeRef::Block(_) | NodeRef::Pat(_) => None,
    }
}

/// Report each block that computes a constant at run time: a compile-time region
/// the fold did not reach. Read off the final IR, never off a refusal a pass
/// recorded, so it retires itself as the fold reaches each shape.
pub fn collect_const_region_remarks(package: &NirPackage) -> Vec<Remark> {
    let callees = build_callee_map(package);
    let ctfe_builtins = build_ctfe_builtin_map(package);
    let materializing = materializing_globals(package);
    let names = ctfe_runnable_names(package);
    let type_table_ref = package.type_table.borrow();
    let type_table: &TypeTable = &type_table_ref;

    let mut remarks = Vec::new();
    for func_rc in &package.functions {
        let func = func_rc.borrow();
        if !func.module_source.is_entry_package() {
            continue;
        }
        let Some(body) = &func.body else {
            continue;
        };
        for region in region_queries(body, &callees, &ctfe_builtins, &materializing, type_table) {
            // Reading an outer local means no constant to miss; writing one
            // means an inner block of a template, reported through its parent.
            if region.reads_outer || region.writes_outer {
                continue;
            }
            let surviving = surviving_calls(body, region.expr, &names, &ctfe_builtins);
            let cause = match (region.refusal, surviving.is_empty()) {
                (Some(refusal), _) => refusal.describe().to_string(),
                (None, false) => format!(
                    "{} still runs here",
                    surviving
                        .iter()
                        .map(|n| format!("`{n}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                (None, true) => "no call on its path explains it".to_string(),
            };
            remarks.push(Remark {
                message: format!("this block computes a constant at run time: {cause}"),
                module: func.module_source.clone(),
                span: body.exprs[region.expr].span,
            });
        }
    }
    remarks
}

/// The functions on `region`'s own path the engine was free to run and did not,
/// under the names their authors wrote. A call under a `cold_path` marker is in
/// the block but not on the path, and naming it buries the callee that is.
///
/// Empty is an answer: the fold waits on a value the engine cannot represent
/// rather than a body it cannot run, which `WADO_TRACE=ctfe_stmt` locates.
fn surviving_calls(
    body: &Body,
    region: ExprId,
    names: &[Option<String>],
    ctfe_builtins: &CtfeBuiltinMap,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack = vec![(NodeRef::Expr(region), false)];
    while let Some((node, in_cold)) = stack.pop() {
        let in_cold = in_cold || block_is_cold(body, node, ctfe_builtins);
        if !in_cold
            && let NodeRef::Expr(e) = node
            && let ExprKind::Call { func_id, .. } = &body.exprs[e].kind
            && let Some(Some(name)) = names.get(func_id.index())
            && !out.contains(name)
        {
            out.push(name.clone());
        }
        body.for_each_child(node, |c| stack.push((c, in_cold)));
    }
    out
}

/// Whether `node` is a block the program itself marks unlikely, which the
/// optimizer spells as a `cold_path` call among its statements.
fn block_is_cold(body: &Body, node: NodeRef, ctfe_builtins: &CtfeBuiltinMap) -> bool {
    let NodeRef::Block(b) = node else {
        return false;
    };
    body.blocks[b].stmts.iter().any(|s| {
        let StmtKind::Expr(op) = &body.stmts[*s].kind else {
            return false;
        };
        op.as_expr().is_some_and(|e| {
            matches!(&body.exprs[e].kind, ExprKind::Call { func_id, .. }
                if ctfe_builtins.get(func_id) == Some(&CtfeBuiltin::ColdPath))
        })
    })
}

/// Display name per `func_id`, for the functions a compile-time frame can run.
/// `None` for every other id — a builtin, an impure callee — so a call to one is
/// not named as work the engine declined.
fn ctfe_runnable_names(package: &NirPackage) -> Vec<Option<String>> {
    package
        .functions
        .iter()
        .map(|f| {
            let f = f.borrow();
            is_ctfe_runnable(&f).then(|| crate::name::diagnostic_function_name(&f.name).to_string())
        })
        .collect()
}
