//! Function inlining optimization for Wado NIR.
//!
//! This module provides function inlining for small functions.
//! It uses labeled block expressions for cleaner value handling.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::nir::{FunctionRef, InlineHint, NirFunction, NirLocal, NirUnaryOp};
use crate::nir_arena::{
    ArenaCallArg, ArenaStructField, ArenaStructPatternField, ArmData, BlockId, BlockNode, Body,
    ExprId, ExprKind, ExprNode, NodeRef, Operand, PatId, PatKind, PatNode, StmtId, StmtKind,
    StmtNode,
};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use cranelift_entity::EntityRef;

use super::arena_query;
use super::dce::callee_descriptor;
use super::gate::{FunctionGate, GatedPass};
use crate::nir::FuncId;
use crate::token::Span;

// The inline threshold is based on expression count, which provides a more
// accurate measure of function complexity than statement count.
// - Simple statements like `let x = 1` have 1 expression
// - Complex statements like `let x = foo() + bar()` have 3+ expressions
// - Method calls, binary operations, field accesses all contribute

/// True when an expression is a `builtin::cold_path()` marker call.
fn is_cold_path_call(body: &Body, id: ExprId, descriptors: &[FunctionRef]) -> bool {
    matches!(
        &body.exprs[id].kind,
        ExprKind::Call { func_id, .. }
            if callee_descriptor(descriptors, *func_id).builtin_name().as_deref()
                == Some("builtin::cold_path")
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
fn block_cut(
    body: &Body,
    stmt: StmtId,
    type_table: &TypeTable,
    descriptors: &[FunctionRef],
) -> BlockCut {
    match &body.stmts[stmt].kind {
        StmtKind::Expr(e)
            if e.as_expr()
                .is_some_and(|e| is_cold_path_call(body, e, descriptors)) =>
        {
            BlockCut::Cold
        }
        StmtKind::Return { .. } | StmtKind::Break { .. } | StmtKind::Continue => BlockCut::Diverges,
        StmtKind::Expr(e)
            if e.as_expr()
                .is_some_and(|e| type_table.is_never(body.exprs[e].type_id)) =>
        {
            BlockCut::Diverges
        }
        _ => BlockCut::None,
    }
}

/// Inline cost of a single statement (its own expression count).
fn count_stmt(
    body: &Body,
    stmt: StmtId,
    type_table: &TypeTable,
    descriptors: &[FunctionRef],
) -> usize {
    match &body.stmts[stmt].kind {
        StmtKind::Expr(expr) => count_operand(body, *expr, type_table, descriptors),
        StmtKind::Let { value, .. } => count_operand(body, *value, type_table, descriptors),
        StmtKind::LetDestructure { value, .. } => {
            count_operand(body, *value, type_table, descriptors)
        }
        StmtKind::Return { value } => {
            value.map_or(0, |v| count_operand(body, v, type_table, descriptors))
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
            ..
        } => {
            count_operand(body, *condition, type_table, descriptors)
                + count_block_exprs(body, *then_block, type_table, descriptors)
                + else_block.map_or(0, |b| count_block_exprs(body, b, type_table, descriptors))
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            count_block_exprs(body, *b, type_table, descriptors)
        }
        StmtKind::Break { .. } | StmtKind::Continue => 0,
    }
}

/// Count expressions reachable through an operand (recursive).
fn count_operand(
    body: &Body,
    op: Operand,
    type_table: &TypeTable,
    descriptors: &[FunctionRef],
) -> usize {
    // A promoted constant counts as the one literal node it replaced.
    op.as_expr()
        .map_or(1, |e| count_expr(body, e, type_table, descriptors))
}

fn count_expr(
    body: &Body,
    id: ExprId,
    type_table: &TypeTable,
    descriptors: &[FunctionRef],
) -> usize {
    1 + match &body.exprs[id].kind {
        ExprKind::Binary { left, right, .. } => {
            count_operand(body, *left, type_table, descriptors)
                + count_operand(body, *right, type_table, descriptors)
        }
        ExprKind::Unary { expr, .. } => count_operand(body, *expr, type_table, descriptors),
        ExprKind::Call { args, .. } => args
            .iter()
            .map(|a| count_operand(body, a.expr, type_table, descriptors))
            .sum(),
        ExprKind::MethodCall { receiver, args, .. } => {
            count_operand(body, *receiver, type_table, descriptors)
                + args
                    .iter()
                    .map(|a| count_operand(body, a.expr, type_table, descriptors))
                    .sum::<usize>()
        }
        ExprKind::FieldAccess { expr, .. } => count_operand(body, *expr, type_table, descriptors),
        ExprKind::Index { expr, index, .. } => {
            count_operand(body, *expr, type_table, descriptors)
                + count_operand(body, *index, type_table, descriptors)
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => elements
            .iter()
            .map(|e| count_operand(body, *e, type_table, descriptors))
            .sum(),
        ExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .map(|f| count_operand(body, f.value, type_table, descriptors))
            .sum(),
        ExprKind::VariantConstruct { payload, .. } => {
            payload.map_or(0, |p| count_operand(body, p, type_table, descriptors))
        }
        ExprKind::Assign { target, value } => {
            count_expr(body, *target, type_table, descriptors)
                + count_operand(body, *value, type_table, descriptors)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            // Cold branches contribute nothing: `count_block_exprs` stops at a
            // `cold_path()` marker or a diverging statement within each arm.
            count_operand(body, *condition, type_table, descriptors)
                + count_block_exprs(body, *then_branch, type_table, descriptors)
                + else_branch.map_or(0, |b| count_block_exprs(body, b, type_table, descriptors))
        }
        ExprKind::Match { expr, arms } => {
            count_operand(body, *expr, type_table, descriptors)
                + arms
                    .iter()
                    .map(|arm| {
                        arm.guard
                            .map_or(0, |g| count_operand(body, g, type_table, descriptors))
                            + count_operand(body, arm.body, type_table, descriptors)
                    })
                    .sum::<usize>()
        }
        ExprKind::Block(block) => count_block_exprs(body, *block, type_table, descriptors),
        ExprKind::Cast { expr, .. } => count_operand(body, *expr, type_table, descriptors),
        ExprKind::GlobalVarSet { value, .. } => {
            count_operand(body, *value, type_table, descriptors)
        }
        // Leaf expressions (no children)
        ExprKind::PackedArray(_)
        | ExprKind::Dead
        | ExprKind::Local { .. }
        | ExprKind::GlobalVarGet { .. } => 0,
        // Closure and effect-related expressions
        ExprKind::EnumConstruct { .. } => 0,
        ExprKind::CmRawCall { args, .. } => args
            .iter()
            .map(|a| count_operand(body, *a, type_table, descriptors))
            .sum(),
        ExprKind::IndirectCall { callee, args } => {
            count_operand(body, *callee, type_table, descriptors)
                + args
                    .iter()
                    .map(|a| count_operand(body, *a, type_table, descriptors))
                    .sum::<usize>()
        }
        ExprKind::ClosureToCanonical { functor, .. } => {
            count_operand(body, *functor, type_table, descriptors)
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            count_operand(body, *scrutinee, type_table, descriptors)
                + arms
                    .iter()
                    .map(|a| count_block_exprs(body, *a, type_table, descriptors))
                    .sum::<usize>()
                + count_block_exprs(body, *default, type_table, descriptors)
        }
        // Lowered pattern matching nodes - count inner expressions
        ExprKind::VariantTag { expr }
        | ExprKind::VariantTest { expr, .. }
        | ExprKind::VariantPayload { expr, .. } => {
            count_operand(body, *expr, type_table, descriptors)
        }
        ExprKind::LabeledBlock { block, .. } => {
            count_block_exprs(body, *block, type_table, descriptors)
        }
    }
}

/// Count expressions in a NIR block (recursive), stopping once the rest of the
/// block becomes cold or unreachable. The walk ends at the first statement that
/// [`block_cut`] flags: a `cold_path()` marker drops the marker and everything
/// after it, while a diverging statement (`return` / `break` / `continue` or a
/// `-> !` call such as `panic`) is itself counted but cuts off its unreachable
/// tail.
fn count_block_exprs(
    body: &Body,
    block: BlockId,
    type_table: &TypeTable,
    descriptors: &[FunctionRef],
) -> usize {
    let mut total = 0;
    for i in 0..body.blocks[block].stmts.len() {
        let stmt = body.blocks[block].stmts[i];
        match block_cut(body, stmt, type_table, descriptors) {
            BlockCut::Cold => break,
            BlockCut::Diverges => {
                total += count_stmt(body, stmt, type_table, descriptors);
                break;
            }
            BlockCut::None => total += count_stmt(body, stmt, type_table, descriptors),
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
    let path = module_source.to_path();
    format!("{}/{}", path.join("/"), name)
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

fn collect_inner_labels(callee: &Body, node: NodeRef, labels: &mut IndexSet<String>) {
    match node {
        NodeRef::Stmt(s) => {
            if let StmtKind::LabeledBlock { label, .. } = &callee.stmts[s].kind {
                labels.insert(label.clone());
            }
        }
        NodeRef::Expr(e) => {
            if let ExprKind::LabeledBlock { label, .. } = &callee.exprs[e].kind {
                labels.insert(label.clone());
            }
        }
        NodeRef::Block(_) | NodeRef::Pat(_) => {}
    }
    let mut kids = Vec::new();
    callee.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_inner_labels(callee, c, labels);
    }
}

/// Check if a function is eligible for inlining.
fn is_inline_eligible(
    func: &NirFunction,
    recursive_functions: &IndexSet<String>,
    _module_source: &ModuleSource,
    type_table: &TypeTable,
    inline_threshold: usize,
    descriptors: &[FunctionRef],
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
    count_block_exprs(body, body.root, type_table, descriptors) <= effective_threshold
}

/// Detect recursive functions using call graph analysis
fn find_recursive_functions(
    functions: &[Rc<RefCell<NirFunction>>],
    descriptors: &[FunctionRef],
) -> IndexSet<String> {
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
                collect_callees_from_block(body, descriptors, body.root, &mut callee_names);
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

fn collect_callees_from_block(
    body: &Body,
    descriptors: &[FunctionRef],
    block: BlockId,
    callees: &mut IndexSet<String>,
) {
    for i in 0..body.blocks[block].stmts.len() {
        let sid = body.blocks[block].stmts[i];
        collect_callees_from_stmt(body, descriptors, sid, callees);
    }
}

fn collect_callees_from_stmt(
    body: &Body,
    descriptors: &[FunctionRef],
    stmt: StmtId,
    callees: &mut IndexSet<String>,
) {
    match &body.stmts[stmt].kind {
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            collect_callees_from_operand(body, descriptors, *value, callees);
        }
        StmtKind::Expr(value) => {
            if let Some(e) = value.as_expr() {
                collect_callees_from_expr(body, descriptors, e, callees);
            }
        }
        StmtKind::Return { value } => {
            if let Some(expr) = *value {
                collect_callees_from_operand(body, descriptors, expr, callees);
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            collect_callees_from_operand(body, descriptors, condition, callees);
            collect_callees_from_block(body, descriptors, then_block, callees);
            if let Some(else_blk) = else_block {
                collect_callees_from_block(body, descriptors, else_blk, callees);
            }
        }
        StmtKind::Loop { body: b } => {
            collect_callees_from_block(body, descriptors, *b, callees);
        }
        StmtKind::LabeledBlock { block, .. } => {
            collect_callees_from_block(body, descriptors, *block, callees);
        }
        StmtKind::Break { .. } | StmtKind::Continue => {}
    }
}

fn collect_callees_from_operand(
    body: &Body,
    descriptors: &[FunctionRef],
    op: Operand,
    callees: &mut IndexSet<String>,
) {
    if let Some(e) = op.as_expr() {
        collect_callees_from_expr(body, descriptors, e, callees);
    }
}

fn collect_callees_from_expr(
    body: &Body,
    descriptors: &[FunctionRef],
    id: ExprId,
    callees: &mut IndexSet<String>,
) {
    match &body.exprs[id].kind {
        ExprKind::Call { func_id, args, .. } => {
            callees.insert(func_ref_inline_key(callee_descriptor(
                descriptors,
                *func_id,
            )));
            for aid in args.iter().map(|a| a.expr).collect::<Vec<_>>() {
                collect_callees_from_operand(body, descriptors, aid, callees);
            }
        }
        ExprKind::MethodCall {
            receiver,
            func_id,
            args,
            ..
        } => {
            callees.insert(func_ref_inline_key(callee_descriptor(
                descriptors,
                *func_id,
            )));
            let receiver = *receiver;
            let arg_ids: Vec<Operand> = args.iter().map(|a| a.expr).collect();
            collect_callees_from_operand(body, descriptors, receiver, callees);
            for aid in arg_ids {
                collect_callees_from_operand(body, descriptors, aid, callees);
            }
        }
        ExprKind::Binary { left, right, .. } => {
            let (left, right) = (*left, *right);
            collect_callees_from_operand(body, descriptors, left, callees);
            collect_callees_from_operand(body, descriptors, right, callees);
        }
        ExprKind::Unary { expr, .. } => {
            collect_callees_from_operand(body, descriptors, *expr, callees);
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            collect_callees_from_expr(body, descriptors, target, callees);
            collect_callees_from_operand(body, descriptors, value, callees);
        }
        ExprKind::Cast { expr, .. } => {
            collect_callees_from_operand(body, descriptors, *expr, callees);
        }
        ExprKind::FieldAccess { expr, .. } => {
            collect_callees_from_operand(body, descriptors, *expr, callees);
        }
        ExprKind::Index { expr, index } => {
            let (expr, index) = (*expr, *index);
            collect_callees_from_operand(body, descriptors, expr, callees);
            collect_callees_from_operand(body, descriptors, index, callees);
        }
        ExprKind::Block(block) => {
            collect_callees_from_block(body, descriptors, *block, callees);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
            collect_callees_from_operand(body, descriptors, condition, callees);
            collect_callees_from_block(body, descriptors, then_branch, callees);
            if let Some(else_blk) = else_branch {
                collect_callees_from_block(body, descriptors, else_blk, callees);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            for fid in fields.iter().map(|f| f.value).collect::<Vec<_>>() {
                collect_callees_from_operand(body, descriptors, fid, callees);
            }
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            for eid in elements.clone() {
                collect_callees_from_operand(body, descriptors, eid, callees);
            }
        }
        ExprKind::IndirectCall { callee, args } => {
            let callee = *callee;
            let arg_ids = args.clone();
            collect_callees_from_operand(body, descriptors, callee, callees);
            for aid in arg_ids {
                collect_callees_from_operand(body, descriptors, aid, callees);
            }
        }
        ExprKind::ClosureToCanonical { functor, .. } => {
            collect_callees_from_operand(body, descriptors, *functor, callees);
        }
        ExprKind::CmRawCall { args, .. } => {
            for aid in args.clone() {
                collect_callees_from_operand(body, descriptors, aid, callees);
            }
        }
        ExprKind::Match { expr, arms } => {
            let expr = *expr;
            let arms = arms.clone();
            collect_callees_from_operand(body, descriptors, expr, callees);
            for arm in &arms {
                if let Some(guard) = arm.guard {
                    collect_callees_from_operand(body, descriptors, guard, callees);
                }
                collect_callees_from_operand(body, descriptors, arm.body, callees);
            }
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = *payload {
                collect_callees_from_operand(body, descriptors, payload_expr, callees);
            }
        }
        ExprKind::LabeledBlock { block, .. } => {
            collect_callees_from_block(body, descriptors, *block, callees);
        }
        ExprKind::GlobalVarSet { value, .. } => {
            collect_callees_from_operand(body, descriptors, *value, callees);
        }
        ExprKind::VariantTag { expr }
        | ExprKind::VariantTest { expr, .. }
        | ExprKind::VariantPayload { expr, .. } => {
            collect_callees_from_operand(body, descriptors, *expr, callees);
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
            collect_callees_from_operand(body, descriptors, scrutinee, callees);
            for arm in arms {
                collect_callees_from_block(body, descriptors, arm, callees);
            }
            collect_callees_from_block(body, descriptors, default, callees);
        }
        // Leaf nodes
        ExprKind::PackedArray(_)
        | ExprKind::Dead
        | ExprKind::Local { .. }
        | ExprKind::GlobalVarGet { .. }
        | ExprKind::EnumConstruct { .. } => {}
    }
}

/// Inline eligible functions at their call sites
///
/// The `inline_threshold` parameter controls the maximum number of statements
/// a function can have to be considered for inlining.
pub fn inline_functions(
    project: &mut NirPackage,
    inline_threshold: usize,
    gate: &mut FunctionGate,
) -> bool {
    // Callee identity by `func_id` (descriptor table built once from the records,
    // borrow-safe), so a call site is recognized by its stamped id rather than the
    // call node's `FunctionRef`. Indexed by `func_id.index()` (== store position).
    let descriptors = super::dce::build_callee_descriptors(project);
    let recursive_functions = find_recursive_functions(&project.functions, &descriptors);

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
            &descriptors,
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

    // Purity inputs for the graph-preserving inline gate (the splice site below):
    // an inlined call that mutates no caller-reachable state lets the caller's
    // `value_of` survive the splice. Computed once over the project; the
    // per-call `pure_calls` set is taken per body just before inlining it.
    let inline_first_param_types = super::alias::first_param_types(project);
    let inline_type_table = project.type_table.borrow();
    let inline_call_immutability = super::alias::CallImmutability::new(project, &inline_type_table);

    // Inline at call sites.
    for fid in gate.dirty_funcs(GatedPass::Inline, project.functions.len()) {
        let caller_idx = fid.index();
        let func_rc = project.functions[caller_idx].clone();
        let mut func = func_rc.borrow_mut();
        let caller_module_source = func.module_source.clone();
        let func_name = func.name.clone();
        if func.body.is_some() {
            // Track which functions were inlined into this function
            let mut inlined_funcs: Vec<(ModuleSource, String)> = Vec::new();
            // Splice-point re-valuation records (Method A): one per inlined block.
            let mut reval: Vec<InlineRevalInfo> = Vec::new();
            // Take ownership of local_count and locals to avoid borrow conflicts
            // with the `&mut func.body` walk below.
            let mut local_count = func.local_count();
            let mut locals = std::mem::take(&mut func.locals);
            // Counter for generating unique inline labels
            let mut inline_counter: u32 = 0;
            // Calls in this body that mutate no caller-reachable state, taken
            // *before* the splice (the call exprs survive as `reval.call_expr`
            // keys). Drives the graph-preserving gate below.
            let pure_set = {
                let body = func.body.as_ref().unwrap();
                super::alias::pure_calls(
                    body,
                    &inline_type_table,
                    &inline_first_param_types,
                    &inline_call_immutability,
                )
            };
            {
                let body = func.body.as_mut().unwrap();
                let root = body.root;
                inline_calls_in_block(
                    body,
                    root,
                    &inline_candidates,
                    &descriptors,
                    &caller_module_source,
                    &mut local_count,
                    &mut locals,
                    &project.type_table.borrow(),
                    &mut inlined_funcs,
                    &mut inline_counter,
                    &mut reval,
                    false,
                );
            }
            func.locals = locals;

            if !inlined_funcs.is_empty() {
                changed = true;
                // The splice restructures the body, staling the persisted graph's
                // `loop_entry_values` (licm's pre-header snapshots — the only
                // value-graph state any consumer still reads, `value_of` having
                // been retired). Keep them only for a graph-preserving splice —
                // every inlined call **pure** (mutates no caller-reachable state)
                // and **loop-free** (introduces no new back-edge) — otherwise clear
                // so licm re-derives conservatively (an absent entry is sound). The
                // value pool and promoted operands carry every value a consumer
                // reads across the splice.
                let preserving = func.body.as_ref().is_some_and(|b| {
                    reval.iter().all(|i| {
                        pure_set.contains(&i.call_expr) && !block_contains_loop(b, i.block)
                    })
                });
                if !preserving
                    && let Some(vg) = func.body.as_mut().and_then(|b| b.value_graph.as_mut())
                {
                    vg.loop_entry_values.clear();
                }
                // Only this caller's body changed (callee bodies are copied,
                // not modified), so report just the caller. The caller's
                // call-graph edges shift, but stale edges only cost 1-hop
                // propagation precision (quality), not correctness.
                gate.mark_changed(FuncId::new(caller_idx));
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

/// Inline function calls in a block (arena). Each statement is processed in
/// place (1:1); a `Let` / `Expr` / `Return` value gets a top-level inline
/// attempt (which then re-scans the inlined body), while other statements
/// recurse into their sub-expressions and sub-blocks.
///
/// `cold` marks a cold call-site context: once a `cold_path()` marker is seen,
/// the rest of the block (and everything nested in it) is cold, mirroring
/// [`block_cut`]. Calls at cold sites are not inlined (the callee body would
/// bloat the hot caller with code that rarely runs) unless the callee is
/// `#[inline(always)]`.
#[allow(clippy::too_many_arguments)]
fn inline_calls_in_block(
    body: &mut Body,
    block: BlockId,
    candidates: &IndexMap<(ModuleSource, String), NirFunction>,
    descriptors: &[FunctionRef],
    current_module: &ModuleSource,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    inlined_funcs: &mut Vec<(ModuleSource, String)>,
    inline_counter: &mut u32,
    reval: &mut Vec<InlineRevalInfo>,
    mut cold: bool,
) {
    enum Shape {
        TopLevel(ExprId),
        Nested(ExprId),
        If(Option<ExprId>, BlockId, Option<BlockId>),
        Block(BlockId),
        None,
    }
    for stmt_id in body.blocks[block].stmts.clone() {
        if let StmtKind::Expr(Operand::Expr(e)) = &body.stmts[stmt_id].kind
            && is_cold_path_call(body, *e, descriptors)
        {
            cold = true;
        }
        let shape = match &body.stmts[stmt_id].kind {
            StmtKind::Let { value, .. } => value.as_expr().map_or(Shape::None, Shape::TopLevel),
            StmtKind::Expr(expr) => expr.as_expr().map_or(Shape::None, Shape::TopLevel),
            StmtKind::Return { value: Some(v) } => v.as_expr().map_or(Shape::None, Shape::TopLevel),
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => Shape::If(condition.as_expr(), *then_block, *else_block),
            StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
                Shape::Block(*b)
            }
            StmtKind::Break { value: Some(v), .. } => {
                v.as_expr().map_or(Shape::None, Shape::Nested)
            }
            StmtKind::LetDestructure { value, .. } => {
                value.as_expr().map_or(Shape::None, Shape::Nested)
            }
            _ => Shape::None,
        };
        match shape {
            Shape::TopLevel(value) => {
                let new_value = inline_top_level(
                    body,
                    value,
                    candidates,
                    descriptors,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                    reval,
                    cold,
                );
                match &mut body.stmts[stmt_id].kind {
                    StmtKind::Let { value, .. } => *value = new_value.into(),
                    StmtKind::Expr(expr) => *expr = new_value.into(),
                    StmtKind::Return { value } => *value = Some(new_value.into()),
                    _ => {}
                }
            }
            Shape::Nested(value) => inline_calls_in_expr(
                body,
                value,
                candidates,
                descriptors,
                current_module,
                local_count,
                locals,
                type_table,
                inlined_funcs,
                inline_counter,
                reval,
                cold,
            ),
            Shape::If(cond, tb, eb) => {
                if let Some(cond) = cond {
                    inline_calls_in_expr(
                        body,
                        cond,
                        candidates,
                        descriptors,
                        current_module,
                        local_count,
                        locals,
                        type_table,
                        inlined_funcs,
                        inline_counter,
                        reval,
                        cold,
                    );
                }
                inline_calls_in_block(
                    body,
                    tb,
                    candidates,
                    descriptors,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                    reval,
                    cold,
                );
                if let Some(eb) = eb {
                    inline_calls_in_block(
                        body,
                        eb,
                        candidates,
                        descriptors,
                        current_module,
                        local_count,
                        locals,
                        type_table,
                        inlined_funcs,
                        inline_counter,
                        reval,
                        cold,
                    );
                }
            }
            Shape::Block(b) => inline_calls_in_block(
                body,
                b,
                candidates,
                descriptors,
                current_module,
                local_count,
                locals,
                type_table,
                inlined_funcs,
                inline_counter,
                reval,
                cold,
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
    descriptors: &[FunctionRef],
    current_module: &ModuleSource,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    inlined_funcs: &mut Vec<(ModuleSource, String)>,
    inline_counter: &mut u32,
    reval: &mut Vec<InlineRevalInfo>,
    cold: bool,
) -> ExprId {
    let result = try_inline_call_expr(
        body,
        value,
        candidates,
        descriptors,
        current_module,
        local_count,
        locals,
        type_table,
        inline_counter,
        reval,
        cold,
    )
    .or_else(|| {
        try_inline_method_call_expr(
            body,
            value,
            candidates,
            descriptors,
            current_module,
            local_count,
            locals,
            type_table,
            inline_counter,
            reval,
            cold,
        )
    });
    if let Some((new_id, inlined_key)) = result {
        if !inlined_funcs.contains(&inlined_key) {
            inlined_funcs.push(inlined_key);
        }
        inline_calls_in_expr(
            body,
            new_id,
            candidates,
            descriptors,
            current_module,
            local_count,
            locals,
            type_table,
            inlined_funcs,
            inline_counter,
            reval,
            cold,
        );
        new_id
    } else {
        inline_calls_in_expr(
            body,
            value,
            candidates,
            descriptors,
            current_module,
            local_count,
            locals,
            type_table,
            inlined_funcs,
            inline_counter,
            reval,
            cold,
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
    let push_op = |exprs: &mut Vec<ExprId>, o: Operand| {
        if let Some(x) = o.as_expr() {
            exprs.push(x);
        }
    };
    match &body.exprs[e].kind {
        ExprKind::Binary { left, right, .. }
        | ExprKind::Index {
            expr: left,
            index: right,
        } => {
            push_op(&mut exprs, *left);
            push_op(&mut exprs, *right);
        }
        ExprKind::Assign { target, value } => {
            exprs.push(*target);
            push_op(&mut exprs, *value);
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::ClosureToCanonical { functor: inner, .. }
        | ExprKind::GlobalVarSet { value: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. } => push_op(&mut exprs, *inner),
        ExprKind::CmRawCall { args, .. } => {
            for a in args {
                push_op(&mut exprs, *a);
            }
        }
        ExprKind::IndirectCall { callee, args } => {
            push_op(&mut exprs, *callee);
            for a in args {
                push_op(&mut exprs, *a);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                push_op(&mut exprs, f.value);
            }
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            for el in elements {
                push_op(&mut exprs, *el);
            }
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                push_op(&mut exprs, *p);
            }
        }
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => blocks.push(*block),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            push_op(&mut exprs, *condition);
            blocks.push(*then_branch);
            if let Some(eb) = else_branch {
                blocks.push(*eb);
            }
        }
        ExprKind::Match { expr, arms } => {
            push_op(&mut exprs, *expr);
            for arm in arms {
                if let Some(g) = arm.guard {
                    push_op(&mut exprs, g);
                }
                push_op(&mut exprs, arm.body);
            }
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            push_op(&mut exprs, *scrutinee);
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
    /// The callee-frame local index of the parameter.
    callee_local_index: u32,
    /// Parameter name (kept for the synthesized binding `Let`).
    name: String,
    is_mut: bool,
    /// The binding's declared type (the arg's type — handles monomorphization
    /// variance and `&mut self` ref wrapping).
    local_type: TypeId,
    /// The argument operand, already in the caller arena. The call node is
    /// discarded after inlining, so its argument subtrees / pool values are
    /// reused directly.
    value: Operand,
}

/// Threaded context for the callee->caller splice: how to remap the callee's
/// local indices and inner labels, and which label a `return` breaks to.
struct InlineCtx<'a> {
    param_to_local: &'a IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: &'a str,
    label_map: &'a IndexMap<String, String>,
}

impl InlineCtx<'_> {
    fn local(&self, idx: u32) -> u32 {
        remap_local_index(
            idx,
            self.param_to_local,
            self.local_offset,
            self.param_count,
        )
    }
    fn lbl(&self, l: &str) -> String {
        self.label_map
            .get(l)
            .cloned()
            .unwrap_or_else(|| l.to_string())
    }
}

/// A spliced inlined block to re-value at the splice point (Method A): walk
/// A spliced inlined block, recorded so the post-splice graph-preserving gate
/// can classify it (the call's purity + whether the block introduces a loop).
pub(super) struct InlineRevalInfo {
    pub block: BlockId,
    /// The original `Call` expr being inlined, keyed against `pure_calls`.
    pub call_expr: ExprId,
}

/// Whether any node under `block` (statements and nested expression blocks) is a
/// `Loop`. A loop-free splice introduces no new value-graph back-edge, so the
/// caller's `loop_entry_values` stay valid across the inline (see the
/// graph-preserving gate at the splice site).
fn block_contains_loop(body: &Body, block: BlockId) -> bool {
    let mut stack = vec![NodeRef::Block(block)];
    while let Some(n) = stack.pop() {
        if let NodeRef::Stmt(s) = n
            && matches!(body.stmts[s].kind, StmtKind::Loop { .. })
        {
            return true;
        }
        body.for_each_child(n, |c| stack.push(c));
    }
    false
}

/// Core inlining routine: builds a labeled block (in the caller arena) that
/// binds each prepared parameter value and executes the spliced callee body
/// with locals remapped into the caller's frame and `return`s converted to
/// `break label`.
#[allow(clippy::too_many_arguments)]
fn build_inlined_labeled_block(
    caller: &mut Body,
    candidate: &NirFunction,
    callee: &Body,
    func_name: &str,
    bindings: Vec<InlineBinding>,
    call_span: Span,
    call_expr: ExprId,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    inline_counter: &mut u32,
    reval: &mut Vec<InlineRevalInfo>,
) -> ExprId {
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

    let local_offset = *local_count;
    let callee_param_count = candidate.params.len() as u32;
    let callee_local_count = candidate.local_count();
    let new_locals_needed = callee_local_count.saturating_sub(callee_param_count);

    let mut block_stmts: Vec<StmtId> = Vec::with_capacity(bindings.len());
    let mut param_to_local: IndexMap<u32, u32> = IndexMap::default();

    for (i, binding) in bindings.into_iter().enumerate() {
        let new_local_index = local_offset + i as u32;
        param_to_local.insert(binding.callee_local_index, new_local_index);
        locals.push(NirLocal {
            name: binding.name.clone(),
            type_id: binding.local_type,
            is_mut: binding.is_mut,
        });
        *local_count += 1;
        let let_id = caller.stmts.push(StmtNode {
            kind: StmtKind::Let {
                name: binding.name,
                local_index: new_local_index,
                is_mut: binding.is_mut,
                is_reactive: false,
                type_id: binding.local_type,
                value: binding.value,
                skip_value_copy: false,
            },
            span: call_span,
        });
        block_stmts.push(let_id);
    }

    let param_offset = local_offset + callee_param_count;
    for i in callee_param_count..callee_local_count {
        if let Some(callee_local) = candidate.locals.get(i as usize) {
            locals.push(callee_local.clone());
        }
    }
    *local_count += new_locals_needed;

    let mut inner_labels: IndexSet<String> = IndexSet::default();
    collect_inner_labels(callee, NodeRef::Block(callee.root), &mut inner_labels);
    let mut label_map: IndexMap<String, String> = IndexMap::default();
    for inner_label in inner_labels {
        label_map.insert(inner_label.clone(), format!("{label}__{inner_label}"));
    }

    let ctx = InlineCtx {
        param_to_local: &param_to_local,
        local_offset: param_offset,
        param_count: callee_param_count,
        label: &label,
        label_map: &label_map,
    };
    splice_block_into(caller, callee, callee.root, &ctx, &mut block_stmts);

    let result_type = candidate.return_type;
    let bid = caller.blocks.push(BlockNode {
        stmts: block_stmts,
        span: call_span,
    });
    reval.push(InlineRevalInfo {
        block: bid,
        call_expr,
    });
    caller.exprs.push(ExprNode {
        kind: ExprKind::LabeledBlock {
            label,
            block: bid,
            result_type,
        },
        type_id: result_type,
        span: call_span,
    })
}

/// Try to inline a free function call `call_id` in `caller`, splicing the callee
/// body in place. Returns the new (labeled-block) expression id and the callee
/// key, or `None` if the call is not an inline candidate.
#[allow(clippy::too_many_arguments)]
fn try_inline_call_expr(
    caller: &mut Body,
    call_id: ExprId,
    candidates: &IndexMap<(ModuleSource, String), NirFunction>,
    descriptors: &[FunctionRef],
    current_module: &ModuleSource,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    _type_table: &TypeTable,
    inline_counter: &mut u32,
    reval: &mut Vec<InlineRevalInfo>,
    cold: bool,
) -> Option<(ExprId, (ModuleSource, String))> {
    let (module_source, func_name, arg_ops): (ModuleSource, String, Vec<Operand>) =
        match &caller.exprs[call_id].kind {
            ExprKind::Call { func_id, args, .. } => {
                let d = callee_descriptor(descriptors, *func_id);
                (
                    d.module_source.clone(),
                    d.name.clone(),
                    args.iter().map(|a| a.expr).collect(),
                )
            }
            _ => return None,
        };
    let (candidate, inlined_key) =
        find_inline_candidate(candidates, &module_source, current_module, &func_name)?;
    // A cold call site keeps the call: inlining there only bloats the hot
    // caller. An explicit `#[inline(always)]` wins over the suppression.
    if cold && candidate.inline_hint != InlineHint::Always {
        return None;
    }
    let callee = candidate.body.as_ref()?;
    let call_span = caller.exprs[call_id].span;

    // Args are already in the caller arena (operands of the discarded call); bind
    // each to its param `Let` directly (WEP: The Live ValueGraph).
    let bindings: Vec<InlineBinding> = candidate
        .params
        .iter()
        .zip(arg_ops.iter())
        .map(|(param, &arg)| InlineBinding {
            callee_local_index: param.local_index,
            name: param.name.clone(),
            is_mut: param.is_mut,
            local_type: caller.operand_type(arg),
            value: arg,
        })
        .collect();

    let inlined = build_inlined_labeled_block(
        caller,
        candidate,
        callee,
        &func_name,
        bindings,
        call_span,
        call_id,
        local_count,
        locals,
        inline_counter,
        reval,
    );
    Some((inlined, inlined_key))
}

/// Try to inline a method call `call_id` in `caller`.
#[allow(clippy::too_many_arguments)]
fn try_inline_method_call_expr(
    caller: &mut Body,
    call_id: ExprId,
    candidates: &IndexMap<(ModuleSource, String), NirFunction>,
    descriptors: &[FunctionRef],
    current_module: &ModuleSource,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    inline_counter: &mut u32,
    reval: &mut Vec<InlineRevalInfo>,
    cold: bool,
) -> Option<(ExprId, (ModuleSource, String))> {
    let (module_source, func_name, receiver_op, arg_ops): (
        ModuleSource,
        String,
        Operand,
        Vec<Operand>,
    ) = match &caller.exprs[call_id].kind {
        ExprKind::MethodCall {
            receiver,
            func_id,
            args,
            ..
        } => {
            let d = callee_descriptor(descriptors, *func_id);
            (
                d.module_source.clone(),
                d.name.clone(),
                *receiver,
                args.iter().map(|a| a.expr).collect(),
            )
        }
        _ => return None,
    };
    let call_span = caller.exprs[call_id].span;
    let (candidate, inlined_key) =
        find_inline_candidate(candidates, &module_source, current_module, &func_name)?;
    // A cold call site keeps the call: inlining there only bloats the hot
    // caller. An explicit `#[inline(always)]` wins over the suppression.
    if cold && candidate.inline_hint != InlineHint::Always {
        return None;
    }
    let callee = candidate.body.as_ref()?;

    let first_param = &candidate.params[0];
    let recv_type = caller.operand_type(receiver_op);
    // Bind receiver to the first parameter (self). For `&mut self`, wrap the
    // receiver in a `MutRef` so field mutations write back to the original (the
    // receiver is then an lvalue `Expr`, never a promoted constant); for
    // `&self` / by-value, pass the receiver operand directly.
    let (self_type_id, self_value): (TypeId, Operand) =
        if matches!(type_table.get(first_param.type_id), ResolvedType::MutRef(_)) {
            if matches!(type_table.get(recv_type), ResolvedType::MutRef(_)) {
                (recv_type, receiver_op)
            } else {
                let mr = caller.exprs.push(ExprNode {
                    kind: ExprKind::Unary {
                        op: NirUnaryOp::MutRef,
                        expr: receiver_op,
                    },
                    type_id: first_param.type_id,
                    span: call_span,
                });
                (first_param.type_id, mr.into())
            }
        } else {
            (recv_type, receiver_op)
        };

    let mut bindings: Vec<InlineBinding> = Vec::with_capacity(candidate.params.len());
    bindings.push(InlineBinding {
        callee_local_index: first_param.local_index,
        name: first_param.name.clone(),
        is_mut: first_param.is_mut,
        local_type: self_type_id,
        value: self_value,
    });
    for (param, &arg) in candidate.params.iter().skip(1).zip(arg_ops.iter()) {
        bindings.push(InlineBinding {
            callee_local_index: param.local_index,
            name: param.name.clone(),
            is_mut: param.is_mut,
            local_type: caller.operand_type(arg),
            value: arg,
        });
    }

    let inlined = build_inlined_labeled_block(
        caller,
        candidate,
        callee,
        &func_name,
        bindings,
        call_span,
        call_id,
        local_count,
        locals,
        inline_counter,
        reval,
    );
    Some((inlined, inlined_key))
}

/// Remap a local index from the callee frame into the caller frame.
fn remap_local_index(
    index: u32,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
) -> u32 {
    if let Some(&new_index) = param_to_local.get(&index) {
        return new_index;
    }
    if index >= param_count {
        local_offset + (index - param_count)
    } else {
        index
    }
}

/// Splice the statements of callee `block` into `out` (caller statement ids),
/// converting `return` to `break label` and flattening labeled blocks whose
/// label is never broken to (safe because all locals are uniquely remapped).
fn splice_block_into(
    caller: &mut Body,
    callee: &Body,
    block: BlockId,
    ctx: &InlineCtx,
    out: &mut Vec<StmtId>,
) {
    for sid in callee.blocks[block].stmts.clone() {
        match &callee.stmts[sid].kind {
            StmtKind::Return { value } => {
                let v = *value;
                let span = callee.stmts[sid].span;
                let value = v.map(|x| splice_operand(caller, callee, x, ctx));
                out.push(caller.stmts.push(StmtNode {
                    kind: StmtKind::Break {
                        label: Some(ctx.label.to_string()),
                        value,
                    },
                    span,
                }));
            }
            StmtKind::LabeledBlock {
                label: inner_label,
                block: inner,
            } => {
                let inner_label = inner_label.clone();
                let inner = *inner;
                if arena_query::has_break_to(callee, NodeRef::Block(inner), &inner_label) {
                    // The label is broken to, so the block must survive (with its
                    // label remapped); recurse converting returns inside it.
                    let span = callee.stmts[sid].span;
                    let nb = splice_block(caller, callee, inner, ctx);
                    out.push(caller.stmts.push(StmtNode {
                        kind: StmtKind::LabeledBlock {
                            label: ctx.lbl(&inner_label),
                            block: nb,
                        },
                        span,
                    }));
                } else {
                    // No break targets this label: flatten its statements into the
                    // parent (all locals are uniquely remapped, so scoping is moot).
                    splice_block_into(caller, callee, inner, ctx, out);
                }
            }
            _ => {
                let s = splice_stmt(caller, callee, sid, ctx);
                out.push(s);
            }
        }
    }
}

/// Splice a callee block into a fresh caller block id (return-converting).
fn splice_block(caller: &mut Body, callee: &Body, block: BlockId, ctx: &InlineCtx) -> BlockId {
    let span = callee.blocks[block].span;
    let mut out = Vec::new();
    splice_block_into(caller, callee, block, ctx, &mut out);
    caller.blocks.push(BlockNode { stmts: out, span })
}

fn splice_stmt(caller: &mut Body, callee: &Body, sid: StmtId, ctx: &InlineCtx) -> StmtId {
    let span = callee.stmts[sid].span;
    let kind = match &callee.stmts[sid].kind {
        StmtKind::Let {
            name,
            local_index,
            is_mut,
            is_reactive,
            type_id,
            value,
            skip_value_copy,
        } => {
            let (li, v) = (*local_index, *value);
            let (name, is_mut, is_reactive, type_id, scv) = (
                name.clone(),
                *is_mut,
                *is_reactive,
                *type_id,
                *skip_value_copy,
            );
            StmtKind::Let {
                name,
                local_index: ctx.local(li),
                is_mut,
                is_reactive,
                type_id,
                value: splice_operand(caller, callee, v, ctx),
                skip_value_copy: scv,
            }
        }
        StmtKind::Expr(e) => StmtKind::Expr(splice_operand(caller, callee, *e, ctx)),
        StmtKind::Return { value } => {
            let v = *value;
            StmtKind::Break {
                label: Some(ctx.label.to_string()),
                value: v.map(|x| splice_operand(caller, callee, x, ctx)),
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (c, t, e) = (*condition, *then_block, *else_block);
            StmtKind::If {
                condition: splice_operand(caller, callee, c, ctx),
                then_block: splice_block(caller, callee, t, ctx),
                else_block: e.map(|b| splice_block(caller, callee, b, ctx)),
            }
        }
        StmtKind::Loop { body } => {
            let b = *body;
            StmtKind::Loop {
                body: splice_block(caller, callee, b, ctx),
            }
        }
        StmtKind::LabeledBlock { label, block } => {
            let (l, b) = (label.clone(), *block);
            StmtKind::LabeledBlock {
                label: ctx.lbl(&l),
                block: splice_block(caller, callee, b, ctx),
            }
        }
        StmtKind::Break { label, value } => {
            let (l, v) = (label.clone(), *value);
            StmtKind::Break {
                label: l.map(|x| ctx.lbl(&x)),
                value: v.map(|x| splice_operand(caller, callee, x, ctx)),
            }
        }
        StmtKind::Continue => StmtKind::Continue,
        StmtKind::LetDestructure {
            pattern,
            is_mut,
            value,
        } => {
            let (p, m, v) = (*pattern, *is_mut, *value);
            StmtKind::LetDestructure {
                pattern: splice_pat(caller, callee, p, ctx),
                is_mut: m,
                value: splice_operand(caller, callee, v, ctx),
            }
        }
    };
    caller.stmts.push(StmtNode { kind, span })
}

fn splice_pat(caller: &mut Body, callee: &Body, pid: PatId, ctx: &InlineCtx) -> PatId {
    let span = callee.pats[pid].span;
    let kind = match &callee.pats[pid].kind {
        PatKind::Binding {
            name,
            local_index,
            type_id,
        } => PatKind::Binding {
            name: name.clone(),
            local_index: ctx.local(*local_index),
            type_id: *type_id,
        },
        PatKind::Tuple(ps, rest) => {
            let (ps, rest) = (ps.clone(), *rest);
            PatKind::Tuple(
                ps.into_iter()
                    .map(|p| splice_pat(caller, callee, p, ctx))
                    .collect(),
                rest,
            )
        }
        PatKind::Or(ps) => {
            let ps = ps.clone();
            PatKind::Or(
                ps.into_iter()
                    .map(|p| splice_pat(caller, callee, p, ctx))
                    .collect(),
            )
        }
        PatKind::Variant {
            enum_type,
            variant_name,
            bindings,
            payload_type,
        } => {
            let (et, vn, bs, pt) = (
                *enum_type,
                variant_name.clone(),
                bindings.clone(),
                *payload_type,
            );
            PatKind::Variant {
                enum_type: et,
                variant_name: vn,
                bindings: bs
                    .into_iter()
                    .map(|p| splice_pat(caller, callee, p, ctx))
                    .collect(),
                payload_type: pt,
            }
        }
        PatKind::Struct {
            struct_type,
            fields,
            has_rest,
        } => {
            let (st, fs, hr) = (*struct_type, fields.clone(), *has_rest);
            PatKind::Struct {
                struct_type: st,
                fields: fs
                    .into_iter()
                    .map(|f| ArenaStructPatternField {
                        field_name: f.field_name,
                        field_index: f.field_index,
                        pattern: splice_pat(caller, callee, f.pattern, ctx),
                    })
                    .collect(),
                has_rest: hr,
            }
        }
        PatKind::ConstantValue { expr } => {
            let e = *expr;
            PatKind::ConstantValue {
                expr: splice_operand(caller, callee, e, ctx),
            }
        }
        PatKind::Wildcard => PatKind::Wildcard,
        PatKind::Literal(l) => PatKind::Literal(l.clone()),
        PatKind::Enum {
            enum_type,
            case_name,
            case_index,
        } => PatKind::Enum {
            enum_type: *enum_type,
            case_name: case_name.clone(),
            case_index: *case_index,
        },
        PatKind::Range {
            start,
            end,
            inclusive,
            is_unsigned,
        } => PatKind::Range {
            start: *start,
            end: *end,
            inclusive: *inclusive,
            is_unsigned: *is_unsigned,
        },
    };
    caller.pats.push(PatNode { kind, span })
}

/// Splice an operand from the callee into the caller. An effectful subtree is
/// spliced as an expr; a promoted pure value is re-interned into the caller's
/// pool — `ValueId`s are pool-scoped, so the whole value tree must be
/// re-allocated against the caller's pool with its child `ValueId`s (and
/// `Opaque` source locals) remapped into the caller frame.
fn splice_operand(caller: &mut Body, callee: &Body, op: Operand, ctx: &InlineCtx) -> Operand {
    match op {
        Operand::Expr(e) => Operand::Expr(splice_expr(caller, callee, e, ctx)),
        Operand::Value(v) => Operand::Value(splice_value(caller, callee, v, ctx)),
    }
}

/// Re-allocate a callee pure value (and its whole tree) into the caller's pool.
/// `ValueId`s are pool-scoped, so every child id is recursively re-allocated and
/// an `Opaque`'s source local is remapped into the caller frame — otherwise a
/// composite value (`Binary` / `Cast` / `FieldAccess` / …) would carry child ids
/// that denote unrelated values (often a different width) in the caller's pool.
fn splice_value(
    caller: &mut Body,
    callee: &Body,
    v: crate::nir_value_graph::ValueId,
    ctx: &InlineCtx,
) -> crate::nir_value_graph::ValueId {
    use crate::nir_value_graph::{OpaqueSource, ValueKind};
    let recorded_ty = callee.values.type_of(v);
    let new_kind = match callee.values.kind(v).clone() {
        ValueKind::Binary { op, lhs, rhs, ty } => ValueKind::Binary {
            op,
            lhs: splice_value(caller, callee, lhs, ctx),
            rhs: splice_value(caller, callee, rhs, ctx),
            ty,
        },
        ValueKind::Unary { op, operand, ty } => ValueKind::Unary {
            op,
            operand: splice_value(caller, callee, operand, ctx),
            ty,
        },
        ValueKind::Cast { operand, target } => ValueKind::Cast {
            operand: splice_value(caller, callee, operand, ctx),
            target,
        },
        ValueKind::Select { cond, then, else_ } => ValueKind::Select {
            cond: splice_value(caller, callee, cond, ctx),
            then: splice_value(caller, callee, then, ctx),
            else_: splice_value(caller, callee, else_, ctx),
        },
        ValueKind::LoopPhi { entry, body_iter } => ValueKind::LoopPhi {
            entry: splice_value(caller, callee, entry, ctx),
            body_iter: splice_value(caller, callee, body_iter, ctx),
        },
        ValueKind::FieldAccess {
            receiver,
            field_index,
            heap_ver,
        } => ValueKind::FieldAccess {
            receiver: splice_value(caller, callee, receiver, ctx),
            field_index,
            heap_ver,
        },
        ValueKind::Opaque(oid) => {
            // Mint a fresh caller opaque, remapping its source local into the
            // caller frame (a skeleton-`Expr` source splices that expr).
            let new = match callee.values.opaque_source(oid) {
                Some(OpaqueSource::Local(idx)) => caller
                    .values
                    .fresh_opaque_with_source(OpaqueSource::Local(ctx.local(idx))),
                Some(OpaqueSource::Expr(e)) => {
                    let spliced = splice_expr(caller, callee, e, ctx);
                    caller
                        .values
                        .fresh_opaque_with_source(OpaqueSource::Expr(spliced))
                }
                None => caller.values.fresh_opaque(),
            };
            if let Some(t) = recorded_ty {
                caller.values.set_type(new, t);
            }
            return new;
        }
        leaf => leaf,
    };
    match recorded_ty {
        Some(t) => caller.values.alloc_unshared(new_kind, t),
        None => caller.values.intern(new_kind),
    }
}

fn splice_expr(caller: &mut Body, callee: &Body, id: ExprId, ctx: &InlineCtx) -> ExprId {
    let span = callee.exprs[id].span;
    let type_id = callee.exprs[id].type_id;
    let kind = match &callee.exprs[id].kind {
        ExprKind::Local { index, name } => ExprKind::Local {
            index: ctx.local(*index),
            name: name.clone(),
        },
        ExprKind::GlobalVarSet {
            module_source,
            name,
            value,
        } => {
            let (ms, n, v) = (module_source.clone(), name.clone(), *value);
            ExprKind::GlobalVarSet {
                module_source: ms,
                name: n,
                value: splice_operand(caller, callee, v, ctx),
            }
        }
        ExprKind::Binary { left, op, right } => {
            let (l, o, r) = (*left, *op, *right);
            ExprKind::Binary {
                left: splice_operand(caller, callee, l, ctx),
                op: o,
                right: splice_operand(caller, callee, r, ctx),
            }
        }
        ExprKind::Unary { op, expr } => {
            let (o, e) = (*op, *expr);
            ExprKind::Unary {
                op: o,
                expr: splice_operand(caller, callee, e, ctx),
            }
        }
        ExprKind::Assign { target, value } => {
            let (t, v) = (*target, *value);
            ExprKind::Assign {
                target: splice_expr(caller, callee, t, ctx),
                value: splice_operand(caller, callee, v, ctx),
            }
        }
        ExprKind::Cast { expr, target_type } => {
            let (e, tt) = (*expr, *target_type);
            ExprKind::Cast {
                expr: splice_operand(caller, callee, e, ctx),
                target_type: tt,
            }
        }
        ExprKind::Call {
            func_id,
            type_args,
            args,
        } => {
            let (func_id, type_args) = (*func_id, type_args.clone());
            let arg_data: Vec<(Operand, bool)> = args.iter().map(|a| (a.expr, a.is_mut)).collect();
            ExprKind::Call {
                func_id,
                type_args,
                args: arg_data
                    .into_iter()
                    .map(|(e, m)| ArenaCallArg {
                        expr: splice_operand(caller, callee, e, ctx),
                        is_mut: m,
                    })
                    .collect(),
            }
        }
        ExprKind::CmRawCall { local_name, args } => {
            let (ln, args) = (local_name.clone(), args.clone());
            ExprKind::CmRawCall {
                local_name: ln,
                args: args
                    .into_iter()
                    .map(|a| splice_operand(caller, callee, a, ctx))
                    .collect(),
            }
        }
        ExprKind::MethodCall {
            receiver,
            func_id,
            type_args,
            args,
        } => {
            let (rcv, func_id, type_args) = (*receiver, *func_id, type_args.clone());
            let arg_data: Vec<(Operand, bool)> = args.iter().map(|a| (a.expr, a.is_mut)).collect();
            ExprKind::MethodCall {
                receiver: splice_operand(caller, callee, rcv, ctx),
                func_id,
                type_args,
                args: arg_data
                    .into_iter()
                    .map(|(e, m)| ArenaCallArg {
                        expr: splice_operand(caller, callee, e, ctx),
                        is_mut: m,
                    })
                    .collect(),
            }
        }
        ExprKind::FieldAccess {
            expr,
            field_index,
            field_name,
        } => {
            let (e, fi, fname) = (*expr, *field_index, field_name.clone());
            ExprKind::FieldAccess {
                expr: splice_operand(caller, callee, e, ctx),
                field_index: fi,
                field_name: fname,
            }
        }
        ExprKind::Index { expr, index } => {
            let (e, i) = (*expr, *index);
            ExprKind::Index {
                expr: splice_operand(caller, callee, e, ctx),
                index: splice_operand(caller, callee, i, ctx),
            }
        }
        ExprKind::Block(b) => ExprKind::Block(splice_block(caller, callee, *b, ctx)),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (c, t, e) = (*condition, *then_branch, *else_branch);
            ExprKind::If {
                condition: splice_operand(caller, callee, c, ctx),
                then_branch: splice_block(caller, callee, t, ctx),
                else_branch: e.map(|b| splice_block(caller, callee, b, ctx)),
            }
        }
        ExprKind::Match { expr, arms } => {
            let e = *expr;
            let arms = arms.clone();
            ExprKind::Match {
                expr: splice_operand(caller, callee, e, ctx),
                arms: arms
                    .into_iter()
                    .map(|a| ArmData {
                        pattern: splice_pat(caller, callee, a.pattern, ctx),
                        guard: a.guard.map(|g| splice_operand(caller, callee, g, ctx)),
                        body: splice_operand(caller, callee, a.body, ctx),
                        span: a.span,
                    })
                    .collect(),
            }
        }
        ExprKind::StructLiteral {
            struct_type,
            struct_name,
            fields,
        } => {
            let (st, sn) = (*struct_type, struct_name.clone());
            let field_data: Vec<(String, Operand, u32)> = fields
                .iter()
                .map(|f| (f.name.clone(), f.value, f.field_index))
                .collect();
            ExprKind::StructLiteral {
                struct_type: st,
                struct_name: sn,
                fields: field_data
                    .into_iter()
                    .map(|(name, value, field_index)| ArenaStructField {
                        name,
                        value: splice_operand(caller, callee, value, ctx),
                        field_index,
                    })
                    .collect(),
            }
        }
        ExprKind::TupleLiteral { elements } => {
            let elements = elements.clone();
            ExprKind::TupleLiteral {
                elements: elements
                    .into_iter()
                    .map(|e| splice_operand(caller, callee, e, ctx))
                    .collect(),
            }
        }
        ExprKind::ArrayLiteral { elements } => {
            let elements = elements.clone();
            ExprKind::ArrayLiteral {
                elements: elements
                    .into_iter()
                    .map(|e| splice_operand(caller, callee, e, ctx))
                    .collect(),
            }
        }
        ExprKind::IndirectCall { callee: c, args } => {
            let (c, args) = (*c, args.clone());
            ExprKind::IndirectCall {
                callee: splice_operand(caller, callee, c, ctx),
                args: args
                    .into_iter()
                    .map(|a| splice_operand(caller, callee, a, ctx))
                    .collect(),
            }
        }
        ExprKind::ClosureToCanonical {
            functor,
            functor_id,
            target_fn_type,
            closure_module,
        } => {
            let (f, fid, tft, cm) = (
                *functor,
                *functor_id,
                *target_fn_type,
                closure_module.clone(),
            );
            ExprKind::ClosureToCanonical {
                functor: splice_operand(caller, callee, f, ctx),
                functor_id: fid,
                target_fn_type: tft,
                closure_module: cm,
            }
        }
        ExprKind::VariantConstruct {
            variant_type,
            case_index,
            case_name,
            payload,
        } => {
            let (vt, ci, cn, p) = (*variant_type, *case_index, case_name.clone(), *payload);
            ExprKind::VariantConstruct {
                variant_type: vt,
                case_index: ci,
                case_name: cn,
                payload: p.map(|x| splice_operand(caller, callee, x, ctx)),
            }
        }
        ExprKind::EnumConstruct {
            enum_type,
            case_index,
            case_name,
        } => ExprKind::EnumConstruct {
            enum_type: *enum_type,
            case_index: *case_index,
            case_name: case_name.clone(),
        },
        ExprKind::LabeledBlock {
            label,
            block,
            result_type,
        } => {
            let (l, b, rt) = (label.clone(), *block, *result_type);
            ExprKind::LabeledBlock {
                label: ctx.lbl(&l),
                block: splice_block(caller, callee, b, ctx),
                result_type: rt,
            }
        }
        ExprKind::VariantTag { expr } => ExprKind::VariantTag {
            expr: splice_operand(caller, callee, *expr, ctx),
        },
        ExprKind::VariantTest {
            expr,
            case_index,
            case_name,
        } => {
            let (e, ci, cn) = (*expr, *case_index, case_name.clone());
            ExprKind::VariantTest {
                expr: splice_operand(caller, callee, e, ctx),
                case_index: ci,
                case_name: cn,
            }
        }
        ExprKind::VariantPayload {
            expr,
            case_index,
            payload_type,
        } => {
            let (e, ci, pt) = (*expr, *case_index, *payload_type);
            ExprKind::VariantPayload {
                expr: splice_operand(caller, callee, e, ctx),
                case_index: ci,
                payload_type: pt,
            }
        }
        ExprKind::Switch {
            scrutinee,
            min_value,
            arms,
            default,
        } => {
            let (s, mv, arms, d) = (*scrutinee, *min_value, arms.clone(), *default);
            ExprKind::Switch {
                scrutinee: splice_operand(caller, callee, s, ctx),
                min_value: mv,
                arms: arms
                    .into_iter()
                    .map(|b| splice_block(caller, callee, b, ctx))
                    .collect(),
                default: splice_block(caller, callee, d, ctx),
            }
        }
        ExprKind::PackedArray(b) => ExprKind::PackedArray(b.clone()),
        ExprKind::Dead => ExprKind::Dead,
        ExprKind::GlobalVarGet {
            module_source,
            name,
        } => ExprKind::GlobalVarGet {
            module_source: module_source.clone(),
            name: name.clone(),
        },
    };
    caller.exprs.push(ExprNode {
        kind,
        type_id,
        span,
    })
}

/// Recursively inline calls within an expression
fn inline_calls_in_expr(
    body: &mut Body,
    e: ExprId,
    candidates: &IndexMap<(ModuleSource, String), NirFunction>,
    descriptors: &[FunctionRef],
    current_module: &ModuleSource,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    inlined_funcs: &mut Vec<(ModuleSource, String)>,
    inline_counter: &mut u32,
    reval: &mut Vec<InlineRevalInfo>,
    cold: bool,
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
            let args: Vec<Operand> = match &body.exprs[e].kind {
                ExprKind::Call { args, .. } => args.iter().map(|a| a.expr).collect(),
                _ => Vec::new(),
            };
            for a in args {
                let Some(a) = a.as_expr() else { continue };
                inline_calls_in_expr(
                    body,
                    a,
                    candidates,
                    descriptors,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                    reval,
                    cold,
                );
            }
            if let Some((new_id, inlined_key)) = try_inline_call_expr(
                body,
                e,
                candidates,
                descriptors,
                current_module,
                local_count,
                locals,
                type_table,
                inline_counter,
                reval,
                cold,
            ) {
                if !inlined_funcs.contains(&inlined_key) {
                    inlined_funcs.push(inlined_key);
                }
                // Move the inlined labeled-block node into the call slot and
                // null out the now-dead `new_id`, so the inner block is owned
                // by exactly one node (`e`). Cloning would leave `new_id` as an
                // orphan sharing the same `BlockId`, violating the arena's
                // one-parent-per-node invariant.
                let span = body.exprs[new_id].span;
                let moved = std::mem::replace(
                    &mut body.exprs[new_id],
                    ExprNode {
                        kind: ExprKind::Dead,
                        type_id: TypeTable::UNIT,
                        span,
                    },
                );
                body.exprs[e] = moved;
            }
        }
        Call::Method => {
            let (receiver, args): (Operand, Vec<Operand>) = match &body.exprs[e].kind {
                ExprKind::MethodCall { receiver, args, .. } => {
                    (*receiver, args.iter().map(|a| a.expr).collect())
                }
                _ => unreachable!(),
            };
            if let Some(receiver) = receiver.as_expr() {
                inline_calls_in_expr(
                    body,
                    receiver,
                    candidates,
                    descriptors,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                    reval,
                    cold,
                );
            }
            for a in args {
                let Some(a) = a.as_expr() else { continue };
                inline_calls_in_expr(
                    body,
                    a,
                    candidates,
                    descriptors,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                    reval,
                    cold,
                );
            }
            if let Some((new_id, inlined_key)) = try_inline_method_call_expr(
                body,
                e,
                candidates,
                descriptors,
                current_module,
                local_count,
                locals,
                type_table,
                inline_counter,
                reval,
                cold,
            ) {
                if !inlined_funcs.contains(&inlined_key) {
                    inlined_funcs.push(inlined_key);
                }
                // Move the inlined labeled-block node into the call slot and
                // null out the now-dead `new_id`, so the inner block is owned
                // by exactly one node (`e`). Cloning would leave `new_id` as an
                // orphan sharing the same `BlockId`, violating the arena's
                // one-parent-per-node invariant.
                let span = body.exprs[new_id].span;
                let moved = std::mem::replace(
                    &mut body.exprs[new_id],
                    ExprNode {
                        kind: ExprKind::Dead,
                        type_id: TypeTable::UNIT,
                        span,
                    },
                );
                body.exprs[e] = moved;
            }
        }
        Call::Other => {
            let (exprs, blocks) = inline_expr_children(body, e);
            for ex in exprs {
                inline_calls_in_expr(
                    body,
                    ex,
                    candidates,
                    descriptors,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                    reval,
                    cold,
                );
            }
            for b in blocks {
                inline_calls_in_block(
                    body,
                    b,
                    candidates,
                    descriptors,
                    current_module,
                    local_count,
                    locals,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                    reval,
                    cold,
                );
            }
        }
    }
}
