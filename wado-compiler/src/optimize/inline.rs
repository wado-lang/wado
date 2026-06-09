//! Function inlining optimization for Wado NIR.
//!
//! This module provides function inlining for small functions.
//! It uses labeled block expressions for cleaner value handling.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::nir::{InlineHint, NirFunction, NirLocal, NirUnaryOp};
use crate::nir_arena::{
    ArenaCallArg, ArenaStructField, ArenaStructPatternField, ArmData, BlockId, BlockNode, Body,
    ExprId, ExprKind, ExprNode, NodeRef, PatId, PatKind, PatNode, StmtId, StmtKind, StmtNode,
};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use cranelift_entity::EntityRef;

use super::arena_query;
use super::gate::{FunctionGate, FunctionId};
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
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => elements
            .iter()
            .map(|e| count_expr(body, *e, type_table))
            .sum(),
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
pub fn inline_functions(
    project: &mut NirPackage,
    inline_threshold: usize,
    gate: &mut FunctionGate,
) -> bool {
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
    for (caller_idx, func_rc) in project.functions.iter().enumerate() {
        let mut func = func_rc.borrow_mut();
        let caller_module_source = func.module_source.clone();
        let func_name = func.name.clone();
        if func.body.is_some() {
            // Track which functions were inlined into this function
            let mut inlined_funcs: Vec<(ModuleSource, String)> = Vec::new();
            // Take ownership of local_count and locals to avoid borrow conflicts
            // with the `&mut func.body` walk below.
            let mut local_count = func.local_count();
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
            func.locals = locals;

            if !inlined_funcs.is_empty() {
                changed = true;
                // Only this caller's body changed (callee bodies are copied,
                // not modified), so report just the caller. The caller's
                // call-graph edges shift, but stale edges only cost 1-hop
                // propagation precision (quality), not correctness.
                gate.mark_changed(FunctionId::new(caller_idx));
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
    let result = try_inline_call_expr(
        body,
        value,
        candidates,
        current_module,
        local_count,
        locals,
        type_table,
        inline_counter,
    )
    .or_else(|| {
        try_inline_method_call_expr(
            body,
            value,
            candidates,
            current_module,
            local_count,
            locals,
            type_table,
            inline_counter,
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
    /// The callee-frame local index of the parameter.
    callee_local_index: u32,
    /// Parameter name (kept for the synthesized binding `Let`).
    name: String,
    is_mut: bool,
    /// The binding's declared type (the arg's type — handles monomorphization
    /// variance and `&mut self` ref wrapping).
    local_type: TypeId,
    /// The argument expression id, already in the caller arena. The call node is
    /// discarded after inlining, so its argument subtrees are reused directly.
    value: ExprId,
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
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    inline_counter: &mut u32,
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
    current_module: &ModuleSource,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    _type_table: &TypeTable,
    inline_counter: &mut u32,
) -> Option<(ExprId, (ModuleSource, String))> {
    let (module_source, func_name, arg_ids): (ModuleSource, String, Vec<ExprId>) =
        match &caller.exprs[call_id].kind {
            ExprKind::Call { func, args, .. } => (
                func.module_source.clone(),
                func.name.clone(),
                args.iter().map(|a| a.expr).collect(),
            ),
            _ => return None,
        };
    let (candidate, inlined_key) =
        find_inline_candidate(candidates, &module_source, current_module, &func_name)?;
    let callee = candidate.body.as_ref()?;
    let call_span = caller.exprs[call_id].span;

    let bindings: Vec<InlineBinding> = candidate
        .params
        .iter()
        .zip(arg_ids.iter())
        .map(|(param, &arg)| InlineBinding {
            callee_local_index: param.local_index,
            name: param.name.clone(),
            is_mut: param.is_mut,
            local_type: caller.exprs[arg].type_id,
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
        local_count,
        locals,
        inline_counter,
    );
    Some((inlined, inlined_key))
}

/// Try to inline a method call `call_id` in `caller`.
#[allow(clippy::too_many_arguments)]
fn try_inline_method_call_expr(
    caller: &mut Body,
    call_id: ExprId,
    candidates: &IndexMap<(ModuleSource, String), NirFunction>,
    current_module: &ModuleSource,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    inline_counter: &mut u32,
) -> Option<(ExprId, (ModuleSource, String))> {
    let (module_source, func_name, receiver_id, arg_ids): (
        ModuleSource,
        String,
        ExprId,
        Vec<ExprId>,
    ) = match &caller.exprs[call_id].kind {
        ExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => (
            func.module_source.clone(),
            func.name.clone(),
            *receiver,
            args.iter().map(|a| a.expr).collect(),
        ),
        _ => return None,
    };
    let (candidate, inlined_key) =
        find_inline_candidate(candidates, &module_source, current_module, &func_name)?;
    let callee = candidate.body.as_ref()?;
    let call_span = caller.exprs[call_id].span;

    let first_param = &candidate.params[0];
    let recv_type = caller.exprs[receiver_id].type_id;
    // Bind receiver to the first parameter (self). For `&mut self`, wrap the
    // receiver in a `MutRef` so field mutations write back to the original;
    // for `&self` / by-value, pass the receiver expression directly.
    let (self_type_id, self_value) =
        if matches!(type_table.get(first_param.type_id), ResolvedType::MutRef(_)) {
            if matches!(type_table.get(recv_type), ResolvedType::MutRef(_)) {
                (recv_type, receiver_id)
            } else {
                let mr = caller.exprs.push(ExprNode {
                    kind: ExprKind::Unary {
                        op: NirUnaryOp::MutRef,
                        expr: receiver_id,
                    },
                    type_id: first_param.type_id,
                    span: call_span,
                });
                (first_param.type_id, mr)
            }
        } else {
            (recv_type, receiver_id)
        };

    let mut bindings: Vec<InlineBinding> = Vec::with_capacity(candidate.params.len());
    bindings.push(InlineBinding {
        callee_local_index: first_param.local_index,
        name: first_param.name.clone(),
        is_mut: first_param.is_mut,
        local_type: self_type_id,
        value: self_value,
    });
    for (param, &arg) in candidate.params.iter().skip(1).zip(arg_ids.iter()) {
        bindings.push(InlineBinding {
            callee_local_index: param.local_index,
            name: param.name.clone(),
            is_mut: param.is_mut,
            local_type: caller.exprs[arg].type_id,
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
        local_count,
        locals,
        inline_counter,
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
                let value = v.map(|x| splice_expr(caller, callee, x, ctx));
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
                value: splice_expr(caller, callee, v, ctx),
                skip_value_copy: scv,
            }
        }
        StmtKind::Expr(e) => StmtKind::Expr(splice_expr(caller, callee, *e, ctx)),
        StmtKind::Return { value } => {
            let v = *value;
            StmtKind::Break {
                label: Some(ctx.label.to_string()),
                value: v.map(|x| splice_expr(caller, callee, x, ctx)),
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (c, t, e) = (*condition, *then_block, *else_block);
            StmtKind::If {
                condition: splice_expr(caller, callee, c, ctx),
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
                value: v.map(|x| splice_expr(caller, callee, x, ctx)),
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
                value: splice_expr(caller, callee, v, ctx),
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
                expr: splice_expr(caller, callee, e, ctx),
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
                value: splice_expr(caller, callee, v, ctx),
            }
        }
        ExprKind::Binary { left, op, right } => {
            let (l, o, r) = (*left, *op, *right);
            ExprKind::Binary {
                left: splice_expr(caller, callee, l, ctx),
                op: o,
                right: splice_expr(caller, callee, r, ctx),
            }
        }
        ExprKind::Unary { op, expr } => {
            let (o, e) = (*op, *expr);
            ExprKind::Unary {
                op: o,
                expr: splice_expr(caller, callee, e, ctx),
            }
        }
        ExprKind::Assign { target, value } => {
            let (t, v) = (*target, *value);
            ExprKind::Assign {
                target: splice_expr(caller, callee, t, ctx),
                value: splice_expr(caller, callee, v, ctx),
            }
        }
        ExprKind::Cast { expr, target_type } => {
            let (e, tt) = (*expr, *target_type);
            ExprKind::Cast {
                expr: splice_expr(caller, callee, e, ctx),
                target_type: tt,
            }
        }
        ExprKind::Call {
            func,
            type_args,
            args,
        } => {
            let (func, type_args) = (func.clone(), type_args.clone());
            let arg_data: Vec<(ExprId, bool)> = args.iter().map(|a| (a.expr, a.is_mut)).collect();
            ExprKind::Call {
                func,
                type_args,
                args: arg_data
                    .into_iter()
                    .map(|(e, m)| ArenaCallArg {
                        expr: splice_expr(caller, callee, e, ctx),
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
                    .map(|a| splice_expr(caller, callee, a, ctx))
                    .collect(),
            }
        }
        ExprKind::MethodCall {
            receiver,
            func,
            type_args,
            args,
        } => {
            let (rcv, func, type_args) = (*receiver, func.clone(), type_args.clone());
            let arg_data: Vec<(ExprId, bool)> = args.iter().map(|a| (a.expr, a.is_mut)).collect();
            ExprKind::MethodCall {
                receiver: splice_expr(caller, callee, rcv, ctx),
                func,
                type_args,
                args: arg_data
                    .into_iter()
                    .map(|(e, m)| ArenaCallArg {
                        expr: splice_expr(caller, callee, e, ctx),
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
                expr: splice_expr(caller, callee, e, ctx),
                field_index: fi,
                field_name: fname,
            }
        }
        ExprKind::Index { expr, index } => {
            let (e, i) = (*expr, *index);
            ExprKind::Index {
                expr: splice_expr(caller, callee, e, ctx),
                index: splice_expr(caller, callee, i, ctx),
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
                condition: splice_expr(caller, callee, c, ctx),
                then_branch: splice_block(caller, callee, t, ctx),
                else_branch: e.map(|b| splice_block(caller, callee, b, ctx)),
            }
        }
        ExprKind::Match { expr, arms } => {
            let e = *expr;
            let arms = arms.clone();
            ExprKind::Match {
                expr: splice_expr(caller, callee, e, ctx),
                arms: arms
                    .into_iter()
                    .map(|a| ArmData {
                        pattern: splice_pat(caller, callee, a.pattern, ctx),
                        guard: a.guard.map(|g| splice_expr(caller, callee, g, ctx)),
                        body: splice_expr(caller, callee, a.body, ctx),
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
            let field_data: Vec<(String, ExprId, u32)> = fields
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
                        value: splice_expr(caller, callee, value, ctx),
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
                    .map(|e| splice_expr(caller, callee, e, ctx))
                    .collect(),
            }
        }
        ExprKind::ArrayLiteral { elements } => {
            let elements = elements.clone();
            ExprKind::ArrayLiteral {
                elements: elements
                    .into_iter()
                    .map(|e| splice_expr(caller, callee, e, ctx))
                    .collect(),
            }
        }
        ExprKind::IndirectCall { callee: c, args } => {
            let (c, args) = (*c, args.clone());
            ExprKind::IndirectCall {
                callee: splice_expr(caller, callee, c, ctx),
                args: args
                    .into_iter()
                    .map(|a| splice_expr(caller, callee, a, ctx))
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
                functor: splice_expr(caller, callee, f, ctx),
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
                payload: p.map(|x| splice_expr(caller, callee, x, ctx)),
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
            expr: splice_expr(caller, callee, *expr, ctx),
        },
        ExprKind::VariantTest {
            expr,
            case_index,
            case_name,
        } => {
            let (e, ci, cn) = (*expr, *case_index, case_name.clone());
            ExprKind::VariantTest {
                expr: splice_expr(caller, callee, e, ctx),
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
                expr: splice_expr(caller, callee, e, ctx),
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
                scrutinee: splice_expr(caller, callee, s, ctx),
                min_value: mv,
                arms: arms
                    .into_iter()
                    .map(|b| splice_block(caller, callee, b, ctx))
                    .collect(),
                default: splice_block(caller, callee, d, ctx),
            }
        }
        ExprKind::IntLiteral { value, repr } => ExprKind::IntLiteral {
            value: *value,
            repr: repr.clone(),
        },
        ExprKind::FloatLiteral { value, repr } => ExprKind::FloatLiteral {
            value: *value,
            repr: repr.clone(),
        },
        ExprKind::BoolLiteral(b) => ExprKind::BoolLiteral(*b),
        ExprKind::CharLiteral(c) => ExprKind::CharLiteral(*c),
        ExprKind::StringLiteral(s) => ExprKind::StringLiteral(s.clone()),
        ExprKind::BytesLiteral(b) => ExprKind::BytesLiteral(b.clone()),
        ExprKind::Null => ExprKind::Null,
        ExprKind::Unit => ExprKind::Unit,
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
            if let Some((new_id, inlined_key)) = try_inline_call_expr(
                body,
                e,
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
                // Move the inlined labeled-block node into the call slot and
                // null out the now-dead `new_id`, so the inner block is owned
                // by exactly one node (`e`). Cloning would leave `new_id` as an
                // orphan sharing the same `BlockId`, violating the arena's
                // one-parent-per-node invariant.
                let span = body.exprs[new_id].span;
                let moved = std::mem::replace(
                    &mut body.exprs[new_id],
                    ExprNode {
                        kind: ExprKind::Unit,
                        type_id: TypeTable::UNIT,
                        span,
                    },
                );
                body.exprs[e] = moved;
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
            if let Some((new_id, inlined_key)) = try_inline_method_call_expr(
                body,
                e,
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
                // Move the inlined labeled-block node into the call slot and
                // null out the now-dead `new_id`, so the inner block is owned
                // by exactly one node (`e`). Cloning would leave `new_id` as an
                // orphan sharing the same `BlockId`, violating the arena's
                // one-parent-per-node invariant.
                let span = body.exprs[new_id].span;
                let moved = std::mem::replace(
                    &mut body.exprs[new_id],
                    ExprNode {
                        kind: ExprKind::Unit,
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
