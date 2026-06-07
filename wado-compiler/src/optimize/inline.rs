//! Function inlining optimization for Wado NIR.
//!
//! This module provides function inlining for small functions.
//! It uses labeled block expressions for cleaner value handling.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::nir::{
    CallArg, InlineHint, NirBlock, NirExpr, NirExprKind, NirFunction, NirLocal, NirPattern,
    NirStmt, NirStmtKind, NirUnaryOp,
};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::nir_visitor::block_has_break_to;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

// The inline threshold is based on expression count, which provides a more
// accurate measure of function complexity than statement count.
// - Simple statements like `let x = 1` have 1 expression
// - Complex statements like `let x = foo() + bar()` have 3+ expressions
// - Method calls, binary operations, field accesses all contribute

/// True when an expression is a `builtin::cold_path()` marker call.
fn is_cold_path_call(body: &Body, id: ExprId) -> bool {
    matches!(
        &body.exprs[id].kind,
        ExprKind::Call { func, .. }
            if func.builtin_name().as_deref() == Some("builtin::cold_path")
    )
}

/// How a statement ends the reachable, hot portion of its block, for the inline
/// cost walk in [`count_block_exprs`].
enum BlockCut {
    /// Not a cut — keep accumulating cost.
    None,
    /// A `cold_path()` marker: this statement and everything after it is cold,
    /// so neither contributes (counted as zero).
    Cold,
    /// An unconditional divergence (a `return` / `break` / `continue`, or a call
    /// to a `-> !` function such as `panic`): the statement itself is counted,
    /// but the unreachable tail after it is not.
    Diverges,
}

/// Classify whether a statement cuts off the rest of its block from the inline
/// cost estimate.
fn block_cut(body: &Body, stmt: StmtId, type_table: &TypeTable) -> BlockCut {
    match &body.stmts[stmt].kind {
        StmtKind::Expr(e) if is_cold_path_call(body, *e) => BlockCut::Cold,
        StmtKind::Return { .. } | StmtKind::Break { .. } | StmtKind::Continue => BlockCut::Diverges,
        StmtKind::Expr(e) if type_table.is_never(body.exprs[*e].type_id) => BlockCut::Diverges,
        _ => BlockCut::None,
    }
}

/// Inline cost of a single statement (its own expression count).
fn count_stmt(body: &Body, stmt: StmtId, type_table: &TypeTable) -> usize {
    match &body.stmts[stmt].kind {
        StmtKind::Expr(expr) => count_expr(body, *expr, type_table),
        StmtKind::Let { value, .. } => count_expr(body, *value, type_table),
        StmtKind::LetDestructure { value, .. } => count_expr(body, *value, type_table),
        StmtKind::Return { value } => value.map_or(0, |v| count_expr(body, v, type_table)),
        StmtKind::If {
            condition,
            then_block,
            else_block,
            ..
        } => {
            count_expr(body, *condition, type_table)
                + count_block_exprs(body, *then_block, type_table)
                + else_block.map_or(0, |b| count_block_exprs(body, b, type_table))
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            count_block_exprs(body, *b, type_table)
        }
        StmtKind::Break { .. } | StmtKind::Continue => 0,
    }
}

/// Count expressions in a NIR expression (recursive)
fn count_expr(body: &Body, id: ExprId, type_table: &TypeTable) -> usize {
    1 + match &body.exprs[id].kind {
        ExprKind::Binary { left, right, .. } => {
            count_expr(body, *left, type_table) + count_expr(body, *right, type_table)
        }
        ExprKind::Unary { expr, .. } => count_expr(body, *expr, type_table),
        ExprKind::Call { args, .. } => args
            .iter()
            .map(|a| count_expr(body, a.expr, type_table))
            .sum(),
        ExprKind::MethodCall { receiver, args, .. } => {
            count_expr(body, *receiver, type_table)
                + args
                    .iter()
                    .map(|a| count_expr(body, a.expr, type_table))
                    .sum::<usize>()
        }
        ExprKind::FieldAccess { expr, .. } => count_expr(body, *expr, type_table),
        ExprKind::Index { expr, index, .. } => {
            count_expr(body, *expr, type_table) + count_expr(body, *index, type_table)
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            elements.iter().map(|e| count_expr(body, *e, type_table)).sum()
        }
        ExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .map(|f| count_expr(body, f.value, type_table))
            .sum(),
        ExprKind::VariantConstruct { payload, .. } => {
            payload.map_or(0, |p| count_expr(body, p, type_table))
        }
        ExprKind::Assign { target, value } => {
            count_expr(body, *target, type_table) + count_expr(body, *value, type_table)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            // Cold branches contribute nothing: `count_block_exprs` stops at a
            // `cold_path()` marker or a diverging statement within each arm.
            count_expr(body, *condition, type_table)
                + count_block_exprs(body, *then_branch, type_table)
                + else_branch.map_or(0, |b| count_block_exprs(body, b, type_table))
        }
        ExprKind::Match { expr, arms } => {
            count_expr(body, *expr, type_table)
                + arms
                    .iter()
                    .map(|arm| {
                        arm.guard.map_or(0, |g| count_expr(body, g, type_table))
                            + count_expr(body, arm.body, type_table)
                    })
                    .sum::<usize>()
        }
        ExprKind::Block(block) => count_block_exprs(body, *block, type_table),
        ExprKind::Cast { expr, .. } => count_expr(body, *expr, type_table),
        ExprKind::GlobalVarSet { value, .. } => count_expr(body, *value, type_table),
        // Leaf expressions (no children)
        ExprKind::IntLiteral { .. }
        | ExprKind::FloatLiteral { .. }
        | ExprKind::BoolLiteral(_)
        | ExprKind::CharLiteral(_)
        | ExprKind::StringLiteral(_)
        | ExprKind::BytesLiteral(_)
        | ExprKind::Unit
        | ExprKind::Local { .. }
        | ExprKind::GlobalVarGet { .. }
        | ExprKind::Null => 0,
        // Closure and effect-related expressions
        ExprKind::EnumConstruct { .. } => 0,
        ExprKind::CmRawCall { args, .. } => {
            args.iter().map(|a| count_expr(body, *a, type_table)).sum()
        }
        ExprKind::IndirectCall { callee, args } => {
            count_expr(body, *callee, type_table)
                + args
                    .iter()
                    .map(|a| count_expr(body, *a, type_table))
                    .sum::<usize>()
        }
        ExprKind::ClosureToCanonical { functor, .. } => count_expr(body, *functor, type_table),
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            count_expr(body, *scrutinee, type_table)
                + arms
                    .iter()
                    .map(|a| count_block_exprs(body, *a, type_table))
                    .sum::<usize>()
                + count_block_exprs(body, *default, type_table)
        }
        // Lowered pattern matching nodes - count inner expressions
        ExprKind::VariantTag { expr }
        | ExprKind::VariantTest { expr, .. }
        | ExprKind::VariantPayload { expr, .. } => count_expr(body, *expr, type_table),
        ExprKind::LabeledBlock { block, .. } => count_block_exprs(body, *block, type_table),
    }
}

/// Count expressions in a NIR block (recursive), stopping once the rest of the
/// block becomes cold or unreachable. The walk ends at the first statement that
/// [`block_cut`] flags: a `cold_path()` marker drops the marker and everything
/// after it, while a diverging statement (`return` / `break` / `continue` or a
/// `-> !` call such as `panic`) is itself counted but cuts off its unreachable
/// tail.
fn count_block_exprs(body: &Body, block: BlockId, type_table: &TypeTable) -> usize {
    let mut total = 0;
    for i in 0..body.blocks[block].stmts.len() {
        let stmt = body.blocks[block].stmts[i];
        match block_cut(body, stmt, type_table) {
            BlockCut::Cold => break,
            BlockCut::Diverges => {
                total += count_stmt(body, stmt, type_table);
                break;
            }
            BlockCut::None => total += count_stmt(body, stmt, type_table),
        }
    }
    total
}

/// Build the canonical inliner key for a function identity.
///
/// We key the call graph on `(module_source, post-mono mangled name)` so that
/// the function-definition side and the call-site side agree on a single
/// identity. `FuncRef::full_name()` / `MethodInfo::to_mangled_name()` cannot
/// be used here because they derive the struct portion from
/// `MethodInfo.struct_name`, which is populated by different code paths with
/// different mangling rules:
///
///  - The monomorphizer's `func_inst.rs` rebuilds `MethodInfo.struct_name`
///    with qualified type args (`mangle_type_arg_for_generic`) on the
///    function-definition side.
///  - `synthesis/traits.rs::decompose_type_for_method_name` builds call-site
///    `MethodInfo.struct_name` with unqualified `mangle_type_name`. The
///    monomorphizer's `call_rewrite` later rewrites `FuncRef.name` to the
///    post-mono canonical mangled name but **preserves** the original
///    `method_info` unchanged.
///
/// So the call site has `name = "List<{mod}/Node>^Eq::eq"` (qualified) but
/// `method_info.to_mangled_name() = "List<Node>^Eq::eq"` (unqualified). Two
/// representations of the same logical call. The function-definition side
/// after monomorphization has both fields qualified, so a key based on
/// `to_mangled_name()` misses the recursive cycle. Keying on
/// `(module_source, func.name)` sidesteps the divergence because both sides
/// pull `func.name` from the same `Monomorphizer::functions.instantiated`
/// map.
///
/// Mirrors the same fix `wir_build/functions.rs::build_mangled_name` adopted
/// in commit 2b005695b for exactly this divergence.
fn function_inline_key(module_source: &ModuleSource, name: &str) -> String {
    if module_source.is_entry_point() {
        name.to_string()
    } else {
        let path = module_source.to_path();
        format!("{}/{}", path.join("/"), name)
    }
}

/// Compute the call-graph key for a `NirFunction` definition.  Must agree
/// with `func_ref_inline_key` so call sites resolve to the same node.
fn tir_function_full_name(func: &NirFunction) -> String {
    function_inline_key(&func.module_source, &func.name)
}

/// Compute the call-graph key for a `FunctionRef` call site.  Must agree
/// with `tir_function_full_name`.
fn func_ref_inline_key(func: &crate::nir::FunctionRef) -> String {
    function_inline_key(&func.module_source, &func.name)
}

fn collect_inner_labels_from_block(block: &NirBlock, labels: &mut IndexSet<String>) {
    for stmt in &block.stmts {
        match &stmt.kind {
            NirStmtKind::LabeledBlock { label, block } => {
                labels.insert(label.clone());
                collect_inner_labels_from_block(block, labels);
            }
            NirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                collect_inner_labels_from_expr(condition, labels);
                collect_inner_labels_from_block(then_block, labels);
                if let Some(else_block) = else_block {
                    collect_inner_labels_from_block(else_block, labels);
                }
            }
            NirStmtKind::Loop { body } => collect_inner_labels_from_block(body, labels),
            NirStmtKind::Expr(expr)
            | NirStmtKind::Let { value: expr, .. }
            | NirStmtKind::LetDestructure { value: expr, .. } => {
                collect_inner_labels_from_expr(expr, labels);
            }
            NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
                if let Some(value) = value {
                    collect_inner_labels_from_expr(value, labels);
                }
            }
            NirStmtKind::Continue => {}
        }
    }
}

fn collect_inner_labels_from_expr(expr: &NirExpr, labels: &mut IndexSet<String>) {
    match &expr.kind {
        NirExprKind::Block(block) => collect_inner_labels_from_block(block, labels),
        NirExprKind::LabeledBlock { label, block, .. } => {
            labels.insert(label.clone());
            collect_inner_labels_from_block(block, labels);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_inner_labels_from_expr(condition, labels);
            collect_inner_labels_from_block(then_branch, labels);
            if let Some(else_branch) = else_branch {
                collect_inner_labels_from_block(else_branch, labels);
            }
        }
        NirExprKind::Match { expr, arms } => {
            collect_inner_labels_from_expr(expr, labels);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_inner_labels_from_expr(guard, labels);
                }
                collect_inner_labels_from_expr(&arm.body, labels);
            }
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_inner_labels_from_expr(scrutinee, labels);
            for arm in arms {
                collect_inner_labels_from_block(arm, labels);
            }
            collect_inner_labels_from_block(default, labels);
        }
        NirExprKind::Binary { left, right, .. } => {
            collect_inner_labels_from_expr(left, labels);
            collect_inner_labels_from_expr(right, labels);
        }
        NirExprKind::Unary { expr, .. }
        | NirExprKind::FieldAccess { expr, .. }
        | NirExprKind::Cast { expr, .. }
        | NirExprKind::VariantTag { expr }
        | NirExprKind::VariantTest { expr, .. }
        | NirExprKind::VariantPayload { expr, .. } => collect_inner_labels_from_expr(expr, labels),
        NirExprKind::Call { args, .. } => {
            for arg in args {
                collect_inner_labels_from_expr(&arg.expr, labels);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            collect_inner_labels_from_expr(receiver, labels);
            for arg in args {
                collect_inner_labels_from_expr(&arg.expr, labels);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_inner_labels_from_expr(arg, labels);
            }
        }
        NirExprKind::Index { expr, index } => {
            collect_inner_labels_from_expr(expr, labels);
            collect_inner_labels_from_expr(index, labels);
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_inner_labels_from_expr(&field.value, labels);
            }
        }
        NirExprKind::TupleLiteral { elements } | NirExprKind::ArrayLiteral { elements } => {
            for elem in elements {
                collect_inner_labels_from_expr(elem, labels);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload) = payload {
                collect_inner_labels_from_expr(payload, labels);
            }
        }
        NirExprKind::Assign { target, value } => {
            collect_inner_labels_from_expr(target, labels);
            collect_inner_labels_from_expr(value, labels);
        }
        NirExprKind::IndirectCall { callee, args } => {
            collect_inner_labels_from_expr(callee, labels);
            for arg in args {
                collect_inner_labels_from_expr(arg, labels);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            collect_inner_labels_from_expr(functor, labels);
        }
        NirExprKind::GlobalVarSet { value, .. } => collect_inner_labels_from_expr(value, labels),
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => {}
    }
}

/// Check if a function is eligible for inlining.
fn is_inline_eligible(
    func: &NirFunction,
    recursive_functions: &IndexSet<String>,
    _module_source: &ModuleSource,
    type_table: &TypeTable,
    inline_threshold: usize,
) -> bool {
    // #[inline(never)] unconditionally prevents inlining
    if func.inline_hint == InlineHint::Never {
        return false;
    }

    // Must have a body
    let Some(body) = &func.body else {
        return false;
    };

    // Don't inline CM binding functions - they are ABI bridges between
    // Wado GC types and CM linear memory that must remain as separate functions
    if func.is_cm_binding {
        return false;
    }

    // #[inline(always)] skips all heuristic checks (but still requires a body and non-adapter)
    if func.inline_hint == InlineHint::Always {
        return true;
    }

    // Don't inline functions that return Never (!)
    // These are error/abort paths that are never hot, so no performance benefit to inlining
    if type_table.is_never(func.return_type) {
        return false;
    }

    // Not recursive — compare using the same fully qualified name used to build
    // the recursive set, so that cross-module recursive functions are not missed.
    if recursive_functions.contains(&tir_function_full_name(func)) {
        return false;
    }

    // The size threshold applies even to functions with a single call site.
    // "One call site ⇒ inlining is always a size win" is *false* here: if the
    // sole call site sits inside a function that is itself duplicated by
    // threshold inlining (or nested inlining) at N sites, the large callee is
    // copied N times instead of being shared. Bypassing the threshold for
    // single-call-site functions measured +87% (pi_approx) / +186% (zlib) at
    // -Os, and regressed already at -O1, so it is not a stale-DCE artifact.
    //
    // `#[inline]` hint raises the threshold by 5x, allowing functions up to 50
    // expressions (at the default threshold of 10) to be inlined.
    let effective_threshold = if func.inline_hint == InlineHint::Hint {
        inline_threshold * 5
    } else {
        inline_threshold
    };

    // Small enough (based on expression count)
    count_block_exprs(body, body.root, type_table) <= effective_threshold
}

/// Detect recursive functions using call graph analysis
fn find_recursive_functions(functions: &[Rc<RefCell<NirFunction>>]) -> IndexSet<String> {
    // Phase 1: Build fully-qualified-name→index mapping.  Keys come from
    // `tir_function_full_name` / `func_ref_inline_key`, both of which hash
    // `(module_source, func.name)`.  See `function_inline_key`'s docstring
    // for why we deliberately ignore `MethodInfo::to_mangled_name()` here.
    let mut name_to_idx: IndexMap<String, usize> =
        IndexMap::with_capacity_and_hasher(functions.len(), rustc_hash::FxBuildHasher);
    let mut idx_to_name: Vec<String> = Vec::with_capacity(functions.len());

    for func_rc in functions {
        let func = func_rc.borrow();
        let name = tir_function_full_name(&func);
        if !name_to_idx.contains_key(&name) {
            let idx = idx_to_name.len();
            idx_to_name.push(name.clone());
            name_to_idx.insert(name, idx);
        }
    }

    let n = idx_to_name.len();
    // Phase 2: Build call graph using indices (no String allocations in inner loop)
    let mut call_graph: Vec<Vec<usize>> = vec![Vec::new(); n];

    for func_rc in functions {
        let func = func_rc.borrow();
        let full_name = tir_function_full_name(&func);
        if let Some(caller_idx) = name_to_idx.get(&full_name) {
            let mut callee_names: IndexSet<String> = IndexSet::default();
            if let Some(body) = &func.body {
                collect_callees_from_block(body, body.root, &mut callee_names);
            }
            let callees: Vec<usize> = callee_names
                .iter()
                .filter_map(|name| name_to_idx.get(name).copied())
                .collect();
            call_graph[*caller_idx] = callees;
        }
    }

    // Phase 3: Find functions that can reach themselves using index-based DFS
    let mut recursive = IndexSet::default();
    let mut visited = vec![false; n];

    for func_idx in 0..n {
        visited.fill(false);
        if can_reach_idx(&call_graph, func_idx, func_idx, &mut visited) {
            recursive.insert(idx_to_name[func_idx].clone());
        }
    }

    recursive
}

fn can_reach_idx(
    call_graph: &[Vec<usize>],
    start: usize,
    target: usize,
    visited: &mut [bool],
) -> bool {
    if visited[start] {
        return false;
    }
    visited[start] = true;

    for &callee in &call_graph[start] {
        if callee == target {
            return true;
        }
        if can_reach_idx(call_graph, callee, target, visited) {
            return true;
        }
    }

    false
}

fn collect_callees_from_block(body: &Body, block: BlockId, callees: &mut IndexSet<String>) {
    for i in 0..body.blocks[block].stmts.len() {
        let sid = body.blocks[block].stmts[i];
        collect_callees_from_stmt(body, sid, callees);
    }
}

fn collect_callees_from_stmt(body: &Body, stmt: StmtId, callees: &mut IndexSet<String>) {
    match &body.stmts[stmt].kind {
        StmtKind::Let { value, .. }
        | StmtKind::LetDestructure { value, .. }
        | StmtKind::Expr(value) => {
            collect_callees_from_expr(body, *value, callees);
        }
        StmtKind::Return { value } => {
            if let Some(expr) = *value {
                collect_callees_from_expr(body, expr, callees);
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            collect_callees_from_expr(body, condition, callees);
            collect_callees_from_block(body, then_block, callees);
            if let Some(else_blk) = else_block {
                collect_callees_from_block(body, else_blk, callees);
            }
        }
        StmtKind::Loop { body: b } => {
            collect_callees_from_block(body, *b, callees);
        }
        StmtKind::LabeledBlock { block, .. } => {
            collect_callees_from_block(body, *block, callees);
        }
        StmtKind::Break { .. } | StmtKind::Continue => {}
    }
}

fn collect_callees_from_expr(body: &Body, id: ExprId, callees: &mut IndexSet<String>) {
    match &body.exprs[id].kind {
        ExprKind::Call { func, args, .. } => {
            callees.insert(func_ref_inline_key(func));
            for aid in args.iter().map(|a| a.expr).collect::<Vec<_>>() {
                collect_callees_from_expr(body, aid, callees);
            }
        }
        ExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            callees.insert(func_ref_inline_key(func));
            let receiver = *receiver;
            let arg_ids: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            collect_callees_from_expr(body, receiver, callees);
            for aid in arg_ids {
                collect_callees_from_expr(body, aid, callees);
            }
        }
        ExprKind::Binary { left, right, .. } => {
            let (left, right) = (*left, *right);
            collect_callees_from_expr(body, left, callees);
            collect_callees_from_expr(body, right, callees);
        }
        ExprKind::Unary { expr, .. } => {
            collect_callees_from_expr(body, *expr, callees);
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            collect_callees_from_expr(body, target, callees);
            collect_callees_from_expr(body, value, callees);
        }
        ExprKind::Cast { expr, .. } => {
            collect_callees_from_expr(body, *expr, callees);
        }
        ExprKind::FieldAccess { expr, .. } => {
            collect_callees_from_expr(body, *expr, callees);
        }
        ExprKind::Index { expr, index } => {
            let (expr, index) = (*expr, *index);
            collect_callees_from_expr(body, expr, callees);
            collect_callees_from_expr(body, index, callees);
        }
        ExprKind::Block(block) => {
            collect_callees_from_block(body, *block, callees);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
            collect_callees_from_expr(body, condition, callees);
            collect_callees_from_block(body, then_branch, callees);
            if let Some(else_blk) = else_branch {
                collect_callees_from_block(body, else_blk, callees);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            for fid in fields.iter().map(|f| f.value).collect::<Vec<_>>() {
                collect_callees_from_expr(body, fid, callees);
            }
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            for eid in elements.clone() {
                collect_callees_from_expr(body, eid, callees);
            }
        }
        ExprKind::IndirectCall { callee, args } => {
            let callee = *callee;
            let arg_ids = args.clone();
            collect_callees_from_expr(body, callee, callees);
            for aid in arg_ids {
                collect_callees_from_expr(body, aid, callees);
            }
        }
        ExprKind::ClosureToCanonical { functor, .. } => {
            collect_callees_from_expr(body, *functor, callees);
        }
        ExprKind::CmRawCall { args, .. } => {
            for aid in args.clone() {
                collect_callees_from_expr(body, aid, callees);
            }
        }
        ExprKind::Match { expr, arms } => {
            let expr = *expr;
            let arms = arms.clone();
            collect_callees_from_expr(body, expr, callees);
            for arm in &arms {
                if let Some(guard) = arm.guard {
                    collect_callees_from_expr(body, guard, callees);
                }
                collect_callees_from_expr(body, arm.body, callees);
            }
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = *payload {
                collect_callees_from_expr(body, payload_expr, callees);
            }
        }
        ExprKind::LabeledBlock { block, .. } => {
            collect_callees_from_block(body, *block, callees);
        }
        ExprKind::GlobalVarSet { value, .. } => {
            collect_callees_from_expr(body, *value, callees);
        }
        ExprKind::VariantTag { expr }
        | ExprKind::VariantTest { expr, .. }
        | ExprKind::VariantPayload { expr, .. } => {
            collect_callees_from_expr(body, *expr, callees);
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let scrutinee = *scrutinee;
            let default = *default;
            let arms = arms.clone();
            collect_callees_from_expr(body, scrutinee, callees);
            for arm in arms {
                collect_callees_from_block(body, arm, callees);
            }
            collect_callees_from_block(body, default, callees);
        }
        // Leaf nodes
        ExprKind::IntLiteral { .. }
        | ExprKind::FloatLiteral { .. }
        | ExprKind::BoolLiteral(_)
        | ExprKind::CharLiteral(_)
        | ExprKind::StringLiteral(_)
        | ExprKind::BytesLiteral(_)
        | ExprKind::Null
        | ExprKind::Unit
        | ExprKind::Local { .. }
        | ExprKind::GlobalVarGet { .. }
        | ExprKind::EnumConstruct { .. } => {}
    }
}

/// Inline eligible functions at their call sites
///
/// The `inline_threshold` parameter controls the maximum number of statements
/// a function can have to be considered for inlining.
pub fn inline_functions(project: &mut NirPackage, inline_threshold: usize) -> bool {
    let recursive_functions = find_recursive_functions(&project.functions);

    // Collect inline candidates from all modules
    // Key: (module_source, func_name), Value: cloned function
    let mut inline_candidates: IndexMap<(ModuleSource, String), NirFunction> = IndexMap::default();

    // Also collect function_strings for each candidate (to update caller's strings after inlining)
    let mut candidate_strings: IndexMap<(ModuleSource, String), Vec<String>> = IndexMap::default();

    let type_table = project.type_table.borrow();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let module_source = &func.module_source;
        let key = (module_source.clone(), func.name.clone());
        if is_inline_eligible(
            &func,
            &recursive_functions,
            module_source,
            &type_table,
            inline_threshold,
        ) {
            inline_candidates.insert(key.clone(), func.clone());
            // Get the strings used by this function
            if let Some(strings) = project.function_strings.get(&key) {
                candidate_strings.insert(key, strings.clone());
            }
        }
    }
    drop(type_table);

    crate::compiler_trace!(
        "opt_loop",
        "inline: threshold={} candidates={}",
        inline_threshold,
        inline_candidates.len()
    );

    if inline_candidates.is_empty() {
        return false;
    }

    let mut changed = false;

    // Inline at call sites
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let caller_module_source = func.module_source.clone();
        let func_name = func.name.clone();
        if func.body.is_some() {
            // Track which functions were inlined into this function
            let mut inlined_funcs: Vec<(ModuleSource, String)> = Vec::new();
            // Take ownership of local_count and locals to avoid borrow conflicts
            // with the `&mut func.body` walk below.
            let mut local_count = func.local_count;
            let mut locals = std::mem::take(&mut func.locals);
            // Counter for generating unique inline labels
            let mut inline_counter: u32 = 0;
            {
                let body = func.body.as_mut().unwrap();
                let root = body.root;
                inline_calls_in_block(
                    body,
                    root,
                    &inline_candidates,
                    &caller_module_source,
                    &mut local_count,
                    &mut locals,
                    &project.type_table.borrow(),
                    &mut inlined_funcs,
                    &mut inline_counter,
                );
            }
            func.local_count = local_count;
            func.locals = locals;

            if !inlined_funcs.is_empty() {
                changed = true;
            }

            // Update function_strings: add strings from inlined functions to the caller
            let mut all_inlined_strings: IndexSet<String> = IndexSet::default();
            for inlined_key in inlined_funcs {
                if let Some(inlined_strings) = candidate_strings.get(&inlined_key) {
                    all_inlined_strings.extend(inlined_strings.iter().cloned());
                }
            }
            if !all_inlined_strings.is_empty() {
                // Need to drop func borrow before borrowing project.function_strings mutably
                drop(func);
                {
                    let caller_strings = project
                        .function_strings
                        .entry((caller_module_source.clone(), func_name.clone()))
                        .or_default();
                    let existing: IndexSet<&str> =
                        caller_strings.iter().map(String::as_str).collect();
                    let to_add: Vec<String> = all_inlined_strings
                        .iter()
                        .filter(|s| !existing.contains(s.as_str()))
                        .cloned()
                        .collect();
                    caller_strings.extend(to_add);
                }
                let to_add: Vec<String> = {
                    let existing_literals: IndexSet<&str> =
                        project.string_literals.iter().map(String::as_str).collect();
                    all_inlined_strings
                        .into_iter()
                        .filter(|s| !existing_literals.contains(s.as_str()))
                        .collect()
                };
                project.string_literals.extend(to_add);
            }
        }
    }
    changed
}

/// Inline function calls in a block
/// Inline function calls in a block (arena). Each statement is processed in
/// place (1:1); a `Let` / `Expr` / `Return` value gets a top-level inline
/// attempt (which then re-scans the inlined body), while other statements
/// recurse into their sub-expressions and sub-blocks.
fn inline_calls_in_block(
    body: &mut Body,
    block: BlockId,
    candidates: &IndexMap<(ModuleSource, String), NirFunction>,
    current_module: &ModuleSource,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    inlined_funcs: &mut Vec<(ModuleSource, String)>,
    inline_counter: &mut u32,
) {
    enum Shape {
        TopLevel(ExprId),
        Nested(ExprId),
        If(ExprId, BlockId, Option<BlockId>),
        Block(BlockId),
        None,
    }
    for stmt_id in body.blocks[block].stmts.clone() {
        let shape = match &body.stmts[stmt_id].kind {
            StmtKind::Let { value, .. } => Shape::TopLevel(*value),
            StmtKind::Expr(expr) => Shape::TopLevel(*expr),
            StmtKind::Return { value: Some(v) } => Shape::TopLevel(*v),
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => Shape::If(*condition, *then_block, *else_block),
            StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
                Shape::Block(*b)
            }
            StmtKind::Break { value: Some(v), .. } => Shape::Nested(*v),
            StmtKind::LetDestructure { value, .. } => Shape::Nested(*value),
            _ => Shape::None,
        };
        match shape {
            Shape::TopLevel(value) => {
                let new_value = inline_top_level(
                    body,
                    value,
                    candidates,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
                match &mut body.stmts[stmt_id].kind {
                    StmtKind::Let { value, .. } => *value = new_value,
                    StmtKind::Expr(expr) => *expr = new_value,
                    StmtKind::Return { value } => *value = Some(new_value),
                    _ => {}
                }
            }
            Shape::Nested(value) => inline_calls_in_expr(
                body,
                value,
                candidates,
                current_module,
                local_count,
                locals,
                type_table,
                inlined_funcs,
                inline_counter,
            ),
            Shape::If(cond, tb, eb) => {
                inline_calls_in_expr(
                    body,
                    cond,
                    candidates,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
                inline_calls_in_block(
                    body,
                    tb,
                    candidates,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
                if let Some(eb) = eb {
                    inline_calls_in_block(
                        body,
                        eb,
                        candidates,
                        current_module,
                        local_count,
                        locals,
                        type_table,
                        inlined_funcs,
                        inline_counter,
                    );
                }
            }
            Shape::Block(b) => inline_calls_in_block(
                body,
                b,
                candidates,
                current_module,
                local_count,
                locals,
                type_table,
                inlined_funcs,
                inline_counter,
            ),
            Shape::None => {}
        }
    }
}

/// Top-level inline of a statement value: try to inline the call, and if it
/// fires, re-scan the inlined body for nested opportunities. Returns the
/// (possibly new) value expression id.
#[allow(clippy::too_many_arguments)]
fn inline_top_level(
    body: &mut Body,
    value: ExprId,
    candidates: &IndexMap<(ModuleSource, String), NirFunction>,
    current_module: &ModuleSource,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    inlined_funcs: &mut Vec<(ModuleSource, String)>,
    inline_counter: &mut u32,
) -> ExprId {
    let tree = body.to_tree_expr(value);
    let result = try_inline_call_expr(
        &tree,
        candidates,
        current_module,
        local_count,
        locals,
        type_table,
        inline_counter,
    )
    .or_else(|| {
        try_inline_method_call_expr(
            &tree,
            candidates,
            current_module,
            local_count,
            locals,
            type_table,
            inline_counter,
        )
    });
    if let Some((inlined_tree, inlined_key)) = result {
        if !inlined_funcs.contains(&inlined_key) {
            inlined_funcs.push(inlined_key);
        }
        let new_id = body.lower_expr(&inlined_tree);
        inline_calls_in_expr(
            body,
            new_id,
            candidates,
            current_module,
            local_count,
            locals,
            type_table,
            inlined_funcs,
            inline_counter,
        );
        new_id
    } else {
        inline_calls_in_expr(
            body,
            value,
            candidates,
            current_module,
            local_count,
            locals,
            type_table,
            inlined_funcs,
            inline_counter,
        );
        value
    }
}

/// The expression and block children of `e`, excluding patterns, in the order
/// the tree `inline_calls_in_expr` recursed (expression children first, then
/// block children — `If`/`Switch` put condition/scrutinee before their blocks,
/// so the split preserves visitation order, which drives label / local
/// numbering).
fn inline_expr_children(body: &Body, e: ExprId) -> (Vec<ExprId>, Vec<BlockId>) {
    let mut exprs = Vec::new();
    let mut blocks = Vec::new();
    match &body.exprs[e].kind {
        ExprKind::Binary { left, right, .. }
        | ExprKind::Assign {
            target: left,
            value: right,
        }
        | ExprKind::Index {
            expr: left,
            index: right,
        } => {
            exprs.push(*left);
            exprs.push(*right);
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::ClosureToCanonical { functor: inner, .. }
        | ExprKind::GlobalVarSet { value: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. } => exprs.push(*inner),
        ExprKind::CmRawCall { args, .. } => exprs.extend(args.iter().copied()),
        ExprKind::IndirectCall { callee, args } => {
            exprs.push(*callee);
            exprs.extend(args.iter().copied());
        }
        ExprKind::StructLiteral { fields, .. } => exprs.extend(fields.iter().map(|f| f.value)),
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            exprs.extend(elements.iter().copied());
        }
        ExprKind::VariantConstruct { payload, .. } => exprs.extend(payload.iter().copied()),
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => blocks.push(*block),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            exprs.push(*condition);
            blocks.push(*then_branch);
            if let Some(eb) = else_branch {
                blocks.push(*eb);
            }
        }
        ExprKind::Match { expr, arms } => {
            exprs.push(*expr);
            for arm in arms {
                if let Some(g) = arm.guard {
                    exprs.push(g);
                }
                exprs.push(arm.body);
            }
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            exprs.push(*scrutinee);
            blocks.extend(arms.iter().copied());
            blocks.push(*default);
        }
        _ => {}
    }
    (exprs, blocks)
}

/// Look up an inline candidate by module path and function name.
fn find_inline_candidate<'a>(
    candidates: &'a IndexMap<(ModuleSource, String), NirFunction>,
    call_module_source: &ModuleSource,
    current_module: &ModuleSource,
    func_name: &str,
) -> Option<(&'a NirFunction, (ModuleSource, String))> {
    // Use the call's module_source directly; fall back to caller's module for local calls
    let target_module = if call_module_source.is_entry_point() {
        current_module.clone()
    } else {
        call_module_source.clone()
    };

    let key = (target_module, func_name.to_string());
    candidates.get(&key).map(|c| (c, key))
}

/// Binding for a single parameter during inlining.
///
/// Each binding becomes a `Let` statement at the head of the synthesized
/// labeled block. Fields carry the information needed without requiring the
/// shared helper to know whether the call site is a free function or a method.
struct InlineBinding {
    /// Original callee-side local index of the parameter being bound.
    /// Used to build the `param_to_local` remapping for the inlined body.
    callee_local_index: u32,
    /// Parameter name (used verbatim as the new `let` name for debuggability).
    name: String,
    /// Whether the synthesized `let` should be mutable. Free function calls
    /// preserve the original parameter's `is_mut`; method calls always pass
    /// `false` (the method body cannot rebind `self` or its arguments).
    is_mut: bool,
    /// Type of the value bound to the new local. This may differ from
    /// `param.type_id` due to monomorphization (arg type differs from param type)
    /// or `&mut self` wrapping (receiver gets wrapped in a `MutRef` unary).
    local_type: TypeId,
    /// The value expression bound to the new local.
    value: NirExpr,
}

/// Core inlining routine: builds a labeled block that binds each prepared
/// parameter value and executes the callee body with locals remapped into the
/// caller's frame.
///
/// Shared by `try_inline_call_expr` and `try_inline_method_call_expr`. The
/// difference between the two lies entirely in how they prepare `bindings`.
fn build_inlined_labeled_block(
    candidate: &NirFunction,
    body: &NirBlock,
    func_name: &str,
    bindings: Vec<InlineBinding>,
    call_span: Span,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    inline_counter: &mut u32,
) -> NirExpr {
    // Generate unique label for this inline site.
    // Sanitize function name for use as label (replace non-alphanumeric with _)
    let sanitized_name: String = func_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let label = format!("__inline_{}_{}", sanitized_name, *inline_counter);
    *inline_counter += 1;

    // Calculate local index offset for remapping.
    let local_offset = *local_count;

    let callee_param_count = candidate.params.len() as u32;
    let callee_local_count = candidate.local_count;
    let new_locals_needed = callee_local_count.saturating_sub(callee_param_count);

    // IMPORTANT: Push param types first to match index assignment order
    // (params get indices local_offset+0, local_offset+1, ..., then non-params follow)
    let mut block_stmts = Vec::with_capacity(bindings.len() + body.stmts.len());
    let mut param_to_local: IndexMap<u32, u32> = IndexMap::default();

    for (i, binding) in bindings.into_iter().enumerate() {
        let new_local_index = local_offset + i as u32;
        param_to_local.insert(binding.callee_local_index, new_local_index);

        // Extend locals for parameter using the binding's actual local_type
        // (handles monomorphization type variance and &mut self ref wrapping).
        locals.push(NirLocal {
            name: binding.name.clone(),
            type_id: binding.local_type,
            is_mut: binding.is_mut,
        });
        *local_count += 1;

        // Use original parameter name (not _inline_ prefix).
        block_stmts.push(NirStmt::new(
            NirStmtKind::Let {
                name: binding.name,
                local_index: new_local_index,
                is_mut: binding.is_mut,
                is_reactive: false,
                type_id: binding.local_type,
                value: binding.value,
                skip_value_copy: false,
            },
            call_span,
        ));
    }

    // param_offset marks where non-param locals start (after all params).
    let param_offset = local_offset + callee_param_count;

    // Now extend locals for the non-parameter locals — keep their original
    // names from the callee so the inlined body's debug-friendly identity
    // survives, just renumbered into the caller's index space.
    for i in callee_param_count..callee_local_count {
        if let Some(callee_local) = candidate.locals.get(i as usize) {
            locals.push(callee_local.clone());
        }
    }
    *local_count += new_locals_needed;

    let mut inner_labels: IndexSet<String> = IndexSet::default();
    collect_inner_labels_from_block(body, &mut inner_labels);
    let mut label_map: IndexMap<String, String> = IndexMap::default();
    for inner_label in inner_labels {
        label_map.insert(inner_label.clone(), format!("{label}__{inner_label}"));
    }

    // Convert the body, transforming `return` into `break label: expr`.
    let remapped_stmts = remap_and_convert_returns(
        body,
        &param_to_local,
        param_offset,
        callee_param_count,
        &label,
        &label_map,
    );

    block_stmts.extend(remapped_stmts);

    // Create a labeled block expression that produces the return value.
    NirExpr::new(
        NirExprKind::LabeledBlock {
            label,
            block: NirBlock::new(block_stmts, call_span),
            result_type: candidate.return_type,
        },
        candidate.return_type,
        call_span,
    )
}

/// Try to inline a free function call expression, returning the inlined
/// expression and the callee's lookup key.
fn try_inline_call_expr(
    expr: &NirExpr,
    candidates: &IndexMap<(ModuleSource, String), NirFunction>,
    current_module: &ModuleSource,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    _type_table: &TypeTable,
    inline_counter: &mut u32,
) -> Option<(NirExpr, (ModuleSource, String))> {
    let NirExprKind::Call { func, args, .. } = &expr.kind else {
        return None;
    };

    let func_name = func.name.clone();
    let (candidate, inlined_key) =
        find_inline_candidate(candidates, &func.module_source, current_module, &func_name)?;
    let bb = candidate
        .body
        .as_ref()
        .map(crate::nir_arena::Body::to_block);
    let body = bb.as_ref()?;

    // Use argument's type_id to match the actual value being assigned
    // (handles monomorphization type variance).
    let bindings: Vec<InlineBinding> = candidate
        .params
        .iter()
        .zip(args.iter())
        .map(|(param, arg)| InlineBinding {
            callee_local_index: param.local_index,
            name: param.name.clone(),
            is_mut: param.is_mut,
            local_type: arg.expr.type_id,
            value: arg.expr.clone(),
        })
        .collect();

    let inlined_expr = build_inlined_labeled_block(
        candidate,
        body,
        &func_name,
        bindings,
        expr.span,
        local_count,
        locals,
        inline_counter,
    );

    Some((inlined_expr, inlined_key))
}

/// Try to inline a method call expression, returning the inlined expression
/// and the callee's lookup key.
fn try_inline_method_call_expr(
    expr: &NirExpr,
    candidates: &IndexMap<(ModuleSource, String), NirFunction>,
    current_module: &ModuleSource,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    inline_counter: &mut u32,
) -> Option<(NirExpr, (ModuleSource, String))> {
    let NirExprKind::MethodCall {
        receiver,
        func,
        args,
        ..
    } = &expr.kind
    else {
        return None;
    };

    let func_name = func.name.clone();
    let (candidate, inlined_key) =
        find_inline_candidate(candidates, &func.module_source, current_module, &func_name)?;
    let bb = candidate
        .body
        .as_ref()
        .map(crate::nir_arena::Body::to_block);
    let body = bb.as_ref()?;

    let mut bindings: Vec<InlineBinding> = Vec::with_capacity(candidate.params.len());

    // Bind receiver to first parameter (self).
    // For &mut self receivers, wrap in a MutRef expression so that field
    // mutations (`self.field = x`) translate to WIR StructSet on the original
    // receiver rather than on a value copy. A value copy would lose writes.
    // For &self receivers, a value copy is safe (no mutations) and lets copy
    // propagation simplify `self.field` → `receiver.field` without a ref level.
    let first_param = &candidate.params[0];
    let (self_type_id, self_value) =
        if matches!(type_table.get(first_param.type_id), ResolvedType::MutRef(_)) {
            if matches!(type_table.get(receiver.type_id), ResolvedType::MutRef(_)) {
                // Receiver is already &mut T — pass through without double-wrapping.
                // This happens when an &mut self method is called on a local whose
                // type is already &mut T (e.g. after inlining a sequence literal builder).
                (receiver.type_id, (**receiver).clone())
            } else {
                let ref_expr = NirExpr {
                    kind: NirExprKind::Unary {
                        op: NirUnaryOp::MutRef,
                        expr: receiver.clone(),
                    },
                    type_id: first_param.type_id,
                    span: expr.span,
                };
                (first_param.type_id, ref_expr)
            }
        } else {
            (receiver.type_id, (**receiver).clone())
        };
    bindings.push(InlineBinding {
        callee_local_index: first_param.local_index,
        name: first_param.name.clone(),
        is_mut: first_param.is_mut,
        local_type: self_type_id,
        value: self_value,
    });

    // Bind remaining args to remaining parameters.
    // Use argument's type_id to handle monomorphization type variance.
    for (param, arg) in candidate.params.iter().skip(1).zip(args.iter()) {
        bindings.push(InlineBinding {
            callee_local_index: param.local_index,
            name: param.name.clone(),
            is_mut: param.is_mut,
            local_type: arg.expr.type_id,
            value: arg.expr.clone(),
        });
    }

    let inlined_expr = build_inlined_labeled_block(
        candidate,
        body,
        &func_name,
        bindings,
        expr.span,
        local_count,
        locals,
        inline_counter,
    );

    Some((inlined_expr, inlined_key))
}

/// Remap locals and convert returns to break statements with the given label.
///
/// Scope blocks (`LabeledBlock` stmts) whose labels are not targeted by any
/// `break` in their body are flattened into the parent. This is safe because
/// inlining remaps all locals to unique indices, making variable scoping
/// irrelevant. Without flattening, intermediate void scope blocks between the
/// inline label and a `break` targeting it produce invalid Wasm: the void
/// block's fallthrough leaves nothing on the stack for the outer typed block.
fn remap_and_convert_returns(
    block: &NirBlock,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: &str,
    label_map: &IndexMap<String, String>,
) -> Vec<NirStmt> {
    let mut stmts = Vec::new();

    for stmt in &block.stmts {
        match &stmt.kind {
            NirStmtKind::Return { value } => {
                // Convert return to break with the inline label.
                // Use label-aware remapping because the return value expression
                // may itself contain nested blocks with return statements
                // (e.g., try-op expansions inside tuple/struct literals).
                let break_value = value.as_ref().map(|v| {
                    remap_expr_inner(
                        v,
                        param_to_local,
                        local_offset,
                        param_count,
                        Some(label),
                        label_map,
                    )
                });
                stmts.push(NirStmt::new(
                    NirStmtKind::Break {
                        label: Some(label.to_string()),
                        value: break_value,
                    },
                    stmt.span,
                ));
            }
            NirStmtKind::LabeledBlock {
                label: inner_label,
                block: inner_block,
            } if !block_has_break_to(inner_label, inner_block) => {
                // Flatten: scope block has no breaks targeting its own label,
                // so just inline its stmts into the parent
                let inner = remap_and_convert_returns(
                    inner_block,
                    param_to_local,
                    local_offset,
                    param_count,
                    label,
                    label_map,
                );
                stmts.extend(inner);
            }
            _ => {
                stmts.push(remap_stmt_with_label(
                    stmt,
                    param_to_local,
                    local_offset,
                    param_count,
                    label,
                    label_map,
                ));
            }
        }
    }

    stmts
}

fn remap_stmt_with_label(
    stmt: &NirStmt,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: &str,
    label_map: &IndexMap<String, String>,
) -> NirStmt {
    remap_stmt_inner(
        stmt,
        param_to_local,
        local_offset,
        param_count,
        Some(label),
        label_map,
    )
}

/// Remap local indices in a pattern
fn remap_pattern(
    pattern: &NirPattern,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
) -> NirPattern {
    match pattern {
        NirPattern::Wildcard => NirPattern::Wildcard,
        NirPattern::Binding {
            name,
            local_index,
            type_id,
        } => NirPattern::Binding {
            name: name.clone(),
            local_index: remap_local_index(*local_index, param_to_local, local_offset, param_count),
            type_id: *type_id,
        },
        NirPattern::Literal(lit) => NirPattern::Literal(lit.clone()),
        NirPattern::Tuple(patterns, has_rest) => NirPattern::Tuple(
            patterns
                .iter()
                .map(|p| remap_pattern(p, param_to_local, local_offset, param_count))
                .collect(),
            *has_rest,
        ),
        NirPattern::Variant {
            enum_type,
            variant_name,
            bindings,
            payload_type,
        } => NirPattern::Variant {
            enum_type: *enum_type,
            variant_name: variant_name.clone(),
            bindings: bindings
                .iter()
                .map(|p| remap_pattern(p, param_to_local, local_offset, param_count))
                .collect(),
            payload_type: *payload_type,
        },
        NirPattern::Enum {
            enum_type,
            case_name,
            case_index,
        } => NirPattern::Enum {
            enum_type: *enum_type,
            case_name: case_name.clone(),
            case_index: *case_index,
        },
        NirPattern::Struct {
            struct_type,
            fields,
            has_rest,
        } => NirPattern::Struct {
            struct_type: *struct_type,
            fields: fields
                .iter()
                .map(|f| crate::nir::NirStructPatternField {
                    field_name: f.field_name.clone(),
                    field_index: f.field_index,
                    pattern: remap_pattern(&f.pattern, param_to_local, local_offset, param_count),
                })
                .collect(),
            has_rest: *has_rest,
        },
        NirPattern::Or(alternatives) => NirPattern::Or(
            alternatives
                .iter()
                .map(|p| remap_pattern(p, param_to_local, local_offset, param_count))
                .collect(),
        ),
        NirPattern::ConstantValue { expr } => NirPattern::ConstantValue { expr: expr.clone() },
        NirPattern::Range {
            start,
            end,
            inclusive,
            is_unsigned,
        } => NirPattern::Range {
            start: *start,
            end: *end,
            inclusive: *inclusive,
            is_unsigned: *is_unsigned,
        },
    }
}

/// Remap a local index
fn remap_local_index(
    index: u32,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
) -> u32 {
    // If it's a parameter, use the param_to_local mapping
    if let Some(&new_index) = param_to_local.get(&index) {
        return new_index;
    }
    // Otherwise, offset the non-parameter locals
    if index >= param_count {
        local_offset + (index - param_count)
    } else {
        // This shouldn't happen if param_to_local is complete
        index
    }
}

/// Remap local indices in an expression, optionally converting `return` to `break`.
///
/// When `label` is `Some`, any `return` statement reachable from this expression
/// (e.g. inside blocks nested in struct literals, tuple literals, variant payloads)
/// is converted to `break label: value`. This is critical for correct inlining of
/// functions whose bodies contain early returns inside nested expressions.
fn remap_expr_inner(
    expr: &NirExpr,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: Option<&str>,
    label_map: &IndexMap<String, String>,
) -> NirExpr {
    let re = |e: &NirExpr| {
        remap_expr_inner(
            e,
            param_to_local,
            local_offset,
            param_count,
            label,
            label_map,
        )
    };
    let re_box = |e: &NirExpr| Box::new(re(e));
    let rb = |b: &NirBlock| {
        remap_block_inner(
            b,
            param_to_local,
            local_offset,
            param_count,
            label,
            label_map,
        )
    };

    let kind = match &expr.kind {
        NirExprKind::Local { index, name } => {
            let new_index = remap_local_index(*index, param_to_local, local_offset, param_count);
            NirExprKind::Local {
                index: new_index,
                name: name.clone(),
            }
        }
        NirExprKind::Binary { left, op, right } => NirExprKind::Binary {
            left: re_box(left),
            op: *op,
            right: re_box(right),
        },
        NirExprKind::Unary { op, expr: inner } => NirExprKind::Unary {
            op: *op,
            expr: re_box(inner),
        },
        NirExprKind::Assign { target, value } => NirExprKind::Assign {
            target: re_box(target),
            value: re_box(value),
        },
        NirExprKind::Cast {
            expr: inner,
            target_type,
        } => NirExprKind::Cast {
            expr: re_box(inner),
            target_type: *target_type,
        },
        NirExprKind::Call {
            func,
            type_args,
            args,
        } => NirExprKind::Call {
            func: func.clone(),
            type_args: type_args.clone(),
            args: args
                .iter()
                .map(|a| CallArg::new(re(&a.expr), a.is_mut))
                .collect(),
        },
        NirExprKind::MethodCall {
            receiver,
            func,
            type_args,
            args,
            ..
        } => NirExprKind::method_call(
            re_box(receiver),
            func.clone(),
            type_args.clone(),
            args.iter()
                .map(|a| CallArg::new(re(&a.expr), a.is_mut))
                .collect(),
        ),
        NirExprKind::CmRawCall { local_name, args } => NirExprKind::CmRawCall {
            local_name: local_name.clone(),
            args: args.iter().map(&re).collect(),
        },
        NirExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => NirExprKind::FieldAccess {
            expr: re_box(inner),
            field_index: *field_index,
            field_name: field_name.clone(),
        },
        NirExprKind::Index { expr: inner, index } => NirExprKind::Index {
            expr: re_box(inner),
            index: re_box(index),
        },
        NirExprKind::Block(block) => NirExprKind::Block(rb(block)),
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => NirExprKind::If {
            condition: re_box(condition),
            then_branch: rb(then_branch),
            else_branch: else_branch.as_ref().map(&rb),
        },
        NirExprKind::Match { expr: inner, arms } => NirExprKind::Match {
            expr: re_box(inner),
            arms: arms
                .iter()
                .map(|arm| crate::nir::NirMatchArm {
                    pattern: remap_pattern(&arm.pattern, param_to_local, local_offset, param_count),
                    guard: arm.guard.as_ref().map(&re),
                    body: re(&arm.body),
                    span: arm.span,
                })
                .collect(),
        },
        NirExprKind::StructLiteral {
            struct_type,
            struct_name,
            fields,
        } => NirExprKind::StructLiteral {
            struct_type: *struct_type,
            struct_name: struct_name.clone(),
            fields: fields
                .iter()
                .map(|f| crate::nir::NirStructField {
                    name: f.name.clone(),
                    value: re(&f.value),
                    field_index: f.field_index,
                })
                .collect(),
        },
        NirExprKind::TupleLiteral { elements } => NirExprKind::TupleLiteral {
            elements: elements.iter().map(&re).collect(),
        },
        NirExprKind::ArrayLiteral { elements } => NirExprKind::ArrayLiteral {
            elements: elements.iter().map(&re).collect(),
        },
        NirExprKind::IndirectCall { callee, args } => NirExprKind::IndirectCall {
            callee: re_box(callee),
            args: args.iter().map(&re).collect(),
        },
        NirExprKind::ClosureToCanonical {
            functor,
            functor_id,
            target_fn_type,
            closure_module,
        } => NirExprKind::ClosureToCanonical {
            functor: re_box(functor),
            functor_id: *functor_id,
            target_fn_type: *target_fn_type,
            closure_module: closure_module.clone(),
        },
        NirExprKind::VariantConstruct {
            variant_type,
            case_index,
            case_name,
            payload,
        } => NirExprKind::VariantConstruct {
            variant_type: *variant_type,
            case_index: *case_index,
            case_name: case_name.clone(),
            payload: payload.as_ref().map(|p| Box::new(re(p))),
        },
        NirExprKind::LabeledBlock {
            label: inner_label,
            block,
            result_type,
        } => NirExprKind::LabeledBlock {
            label: label_map
                .get(inner_label)
                .cloned()
                .unwrap_or_else(|| inner_label.clone()),
            block: rb(block),
            result_type: *result_type,
        },
        NirExprKind::GlobalVarSet {
            module_source,
            name,
            value,
        } => NirExprKind::GlobalVarSet {
            module_source: module_source.clone(),
            name: name.clone(),
            value: re_box(value),
        },
        NirExprKind::VariantTag { expr } => NirExprKind::VariantTag { expr: re_box(expr) },
        NirExprKind::VariantTest {
            expr,
            case_index,
            case_name,
        } => NirExprKind::VariantTest {
            expr: re_box(expr),
            case_index: *case_index,
            case_name: case_name.clone(),
        },
        NirExprKind::VariantPayload {
            expr,
            case_index,
            payload_type,
        } => NirExprKind::VariantPayload {
            expr: re_box(expr),
            case_index: *case_index,
            payload_type: *payload_type,
        },
        NirExprKind::Switch {
            scrutinee,
            min_value,
            arms,
            default,
        } => NirExprKind::Switch {
            scrutinee: re_box(scrutinee),
            min_value: *min_value,
            arms: arms.iter().map(&rb).collect(),
            default: rb(default),
        },
        // Leaf nodes - no remapping needed
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => expr.kind.clone(),
    };

    NirExpr::new(kind, expr.type_id, expr.span)
}

fn remap_block_inner(
    block: &NirBlock,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: Option<&str>,
    label_map: &IndexMap<String, String>,
) -> NirBlock {
    let mut stmts = Vec::new();
    for stmt in &block.stmts {
        if let Some(label) = label {
            match &stmt.kind {
                NirStmtKind::LabeledBlock {
                    label: inner_label,
                    block: inner_block,
                } if !block_has_break_to(inner_label, inner_block) => {
                    // Flatten: scope block has no breaks targeting its own label
                    let inner = remap_and_convert_returns(
                        inner_block,
                        param_to_local,
                        local_offset,
                        param_count,
                        label,
                        label_map,
                    );
                    stmts.extend(inner);
                    continue;
                }
                _ => {}
            }
        }
        stmts.push(remap_stmt_inner(
            stmt,
            param_to_local,
            local_offset,
            param_count,
            label,
            label_map,
        ));
    }
    NirBlock::new(stmts, block.span)
}

fn remap_stmt_inner(
    stmt: &NirStmt,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: Option<&str>,
    label_map: &IndexMap<String, String>,
) -> NirStmt {
    let re = |e: &NirExpr| {
        remap_expr_inner(
            e,
            param_to_local,
            local_offset,
            param_count,
            label,
            label_map,
        )
    };
    let rb = |b: &NirBlock| {
        remap_block_inner(
            b,
            param_to_local,
            local_offset,
            param_count,
            label,
            label_map,
        )
    };

    let kind = match &stmt.kind {
        NirStmtKind::Let {
            name,
            local_index,
            is_mut,
            is_reactive,
            type_id,
            value,
            skip_value_copy,
        } => {
            let new_index =
                remap_local_index(*local_index, param_to_local, local_offset, param_count);
            NirStmtKind::Let {
                name: name.clone(),
                local_index: new_index,
                is_mut: *is_mut,
                is_reactive: *is_reactive,
                type_id: *type_id,
                value: re(value),
                skip_value_copy: *skip_value_copy,
            }
        }
        NirStmtKind::Expr(expr) => NirStmtKind::Expr(re(expr)),
        NirStmtKind::Return { value } => {
            if let Some(label) = label {
                // Convert return to break with the inline label
                NirStmtKind::Break {
                    label: Some(label.to_string()),
                    value: value.as_ref().map(&re),
                }
            } else {
                NirStmtKind::Return {
                    value: value.as_ref().map(&re),
                }
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => NirStmtKind::If {
            condition: re(condition),
            then_block: rb(then_block),
            else_block: else_block.as_ref().map(&rb),
        },
        NirStmtKind::Loop { body } => NirStmtKind::Loop { body: rb(body) },
        NirStmtKind::LabeledBlock {
            label: inner_label,
            block,
        } => NirStmtKind::LabeledBlock {
            label: label_map
                .get(inner_label)
                .cloned()
                .unwrap_or_else(|| inner_label.clone()),
            block: rb(block),
        },
        NirStmtKind::Break {
            label: break_label,
            value,
        } => NirStmtKind::Break {
            label: break_label.as_ref().map(|break_label| {
                label_map
                    .get(break_label)
                    .cloned()
                    .unwrap_or_else(|| break_label.clone())
            }),
            value: value.as_ref().map(&re),
        },
        NirStmtKind::Continue => NirStmtKind::Continue,
        NirStmtKind::LetDestructure {
            pattern,
            is_mut,
            value,
        } => NirStmtKind::LetDestructure {
            pattern: remap_pattern(pattern, param_to_local, local_offset, param_count),
            is_mut: *is_mut,
            value: re(value),
        },
    };

    NirStmt::new(kind, stmt.span)
}

/// Recursively inline calls within an expression
fn inline_calls_in_expr(
    body: &mut Body,
    e: ExprId,
    candidates: &IndexMap<(ModuleSource, String), NirFunction>,
    current_module: &ModuleSource,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    inlined_funcs: &mut Vec<(ModuleSource, String)>,
    inline_counter: &mut u32,
) {
    enum Call {
        Free,
        Method,
        Other,
    }
    let kind = match &body.exprs[e].kind {
        ExprKind::Call { .. } => Call::Free,
        ExprKind::MethodCall { .. } => Call::Method,
        _ => Call::Other,
    };
    match kind {
        Call::Free => {
            // Recurse into arguments first, then attempt to inline this call.
            let args: Vec<ExprId> = match &body.exprs[e].kind {
                ExprKind::Call { args, .. } => args.iter().map(|a| a.expr).collect(),
                _ => Vec::new(),
            };
            for a in args {
                inline_calls_in_expr(
                    body,
                    a,
                    candidates,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
            }
            let tree = body.to_tree_expr(e);
            if let Some((inlined_tree, inlined_key)) = try_inline_call_expr(
                &tree,
                candidates,
                current_module,
                local_count,
                locals,
                type_table,
                inline_counter,
            ) {
                if !inlined_funcs.contains(&inlined_key) {
                    inlined_funcs.push(inlined_key);
                }
                let new_id = body.lower_expr(&inlined_tree);
                let node = body.exprs[new_id].clone();
                body.exprs[e] = node;
            }
        }
        Call::Method => {
            let (receiver, args): (ExprId, Vec<ExprId>) = match &body.exprs[e].kind {
                ExprKind::MethodCall { receiver, args, .. } => {
                    (*receiver, args.iter().map(|a| a.expr).collect())
                }
                _ => unreachable!(),
            };
            inline_calls_in_expr(
                body,
                receiver,
                candidates,
                current_module,
                local_count,
                locals,
                type_table,
                inlined_funcs,
                inline_counter,
            );
            for a in args {
                inline_calls_in_expr(
                    body,
                    a,
                    candidates,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
            }
            let tree = body.to_tree_expr(e);
            if let Some((inlined_tree, inlined_key)) = try_inline_method_call_expr(
                &tree,
                candidates,
                current_module,
                local_count,
                locals,
                type_table,
                inline_counter,
            ) {
                if !inlined_funcs.contains(&inlined_key) {
                    inlined_funcs.push(inlined_key);
                }
                let new_id = body.lower_expr(&inlined_tree);
                let node = body.exprs[new_id].clone();
                body.exprs[e] = node;
            }
        }
        Call::Other => {
            let (exprs, blocks) = inline_expr_children(body, e);
            for ex in exprs {
                inline_calls_in_expr(
                    body,
                    ex,
                    candidates,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
            }
            for b in blocks {
                inline_calls_in_block(
                    body,
                    b,
                    candidates,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
            }
        }
    }
}
