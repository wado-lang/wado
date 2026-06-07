//! Hot Field Scalarization for Wado NIR
//!
//! Promotes a hot struct field `obj.field` (read+written inside a loop)
//! to a mutable local `__hfs_field_N`, hoisted out of the loop. Reads
//! and writes inside the loop become `local.get`/`local.set`; sync with
//! the underlying GC field happens only when control leaves the
//! scalar's domain (calls, escape paths, loop back-edges).
//!
//! For a field `obj.field` accessed at least `MIN_ACCESS_COUNT` times:
//! 1. Allocate a mutable local `__hfs_field_N`.
//! 2. Pre-load `let __hfs_field_N = obj.field;` before the loop.
//! 3. Rewrite every `obj.field` read in the body to `__hfs_field_N`.
//! 4. Rewrite every `obj.field = v` write in the body to
//!    `__hfs_field_N = v`.
//! 5. Walk the body with a dataflow lattice that tracks which side
//!    holds the truth and emits sync only at transitions (see below).
//!
//! ## Sync placement (dataflow-driven)
//!
//! For each scalarized candidate `(L, F)` the walker tracks one of:
//!
//! - `Both`        — `__hfs_F == L.F` (no sync needed for either side).
//! - `ScalarOnly`  — `__hfs_F` holds the latest value; `L.F` is stale.
//! - `FieldOnly`   — `L.F` holds the latest value; `__hfs_F` is stale.
//!
//! Transitions:
//!
//! - Scalar write `__hfs_F = v`     → state becomes `ScalarOnly`.
//! - `&mut T` call touching `L.F`   → pre-call: `write_back` if not
//!   field-canonical; post-call: state becomes `FieldOnly`.
//! - `&T` call touching `L.F`       → pre-call: `write_back` if not
//!   field-canonical; state unchanged otherwise.
//! - Field read `obj.field`         → re-read if not scalar-canonical;
//!   state becomes `Both`.
//!
//! Sync is emitted only at canonical-side transitions:
//! `ScalarOnly → Both/FieldOnly` writes back, `FieldOnly → Both/ScalarOnly`
//! re-reads, `Both → *` is a relabel with no sync. Consecutive `&mut`
//! calls therefore emit zero inter-call sync — once `FieldOnly`, every
//! subsequent `&mut` call's pre-state is already satisfied.
//!
//! Branch joins (`If`/`Switch`/`Match`) walk each arm with a cloned
//! entry state and pick a per-candidate join target; convergence sync
//! is inserted at each arm's exit. A call in one match arm cannot
//! trigger sync that clobbers a sibling scalar-update arm (issue #1008).
//!
//! Loop boundaries:
//! - The body-end of the HFS loop appends sync to drive every
//!   candidate back to `Both`, restoring the loop back-edge invariant.
//! - `return` / `break` to a non-enclosing target / `continue` emit
//!   sync inline before the control-flow stmt, since none of those
//!   reach the body-end fall-through.
//! - Nested loops commit any `ScalarOnly` candidate before recursing
//!   so inner reads see an up-to-date field, then set the outer state
//!   to `JOIN(entry_state, body_exit_state)` per candidate.
//!
//! ## Field-selective sync
//!
//! For each call site, the walker queries a pre-computed
//! `FieldUsageCache` to determine which scalarized fields the callee
//! actually touches. An immutable-ref parameter elides the post-call
//! `re_read` since the callee cannot mutate through it. Unresolved
//! callees fall back to "all fields" conservatively.
//!
//! TODO(optimizer): the "unresolved callee → all fields" fallback (used
//! by indirect / cm-raw / closure-functor invocations and by callees
//! outside the local function set) writes back every scalarized field
//! on every such call. Propagating an "opaque-callee transparent on
//! fields this function never writes" summary up the call graph would
//! eliminate the sync cliff for thin wrapper functions that only forward
//! the receiver.
//!
//! ## Generated locals
//!
//! - `__hfs_<field>_<idx>` — the scalar holding `obj.field`.
//! - `__hfs_call_<idx>`    — pooled per-type temps used to capture the
//!   trailing value of a non-unit `Match` arm body / `If`/`Switch`
//!   block when convergence sync must run after the value-producing
//!   expression.

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{
    FunctionRef, NirBlock, NirExpr, NirExprKind, NirFunction, NirLocal, NirStmt, NirStmtKind,
    NirUnaryOp,
};
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, NodeRef, PatId, PatKind, StmtId, StmtKind,
};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

const MIN_ACCESS_COUNT: usize = 4;

/// Per-parameter field usage: `Some(set)` = only these fields accessed,
/// `None` = all fields potentially accessed (conservative).
type ParamFieldUsage = Option<IndexSet<u32>>;

/// Per-function cache entry: field usage plus the set of parameter positions
/// that are immutable references (`&T`). A callee cannot modify the struct
/// through a `&T` parameter, so re-read after the call is unnecessary even
/// when the caller's argument has type `&mut T`.
struct FuncUsageEntry {
    params: IndexMap<u32, ParamFieldUsage>,
    immut_ref_params: IndexSet<u32>,
}

/// Maps each function (by module + name) to its usage info.
type FieldUsageCache = IndexMap<(ModuleSource, String), FuncUsageEntry>;

pub fn scalarize_hot_fields(project: &mut NirPackage) -> bool {
    // Phase 1: Build field usage cache (immutable access to all functions)
    let cache = build_field_usage_cache(project);

    // Phase 2: Run scalarization (mutable access)
    let type_table = project.type_table.borrow();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= scalarize_function(&mut func, &type_table, &cache);
    }
    changed
}

fn build_field_usage_cache(project: &NirPackage) -> FieldUsageCache {
    let mut cache = FieldUsageCache::default();
    let type_table = project.type_table.borrow();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let params = analyze_function_field_usage(&func, &type_table);
        let mut immut_ref_params = IndexSet::default();
        for (position, param) in func.params.iter().enumerate() {
            if matches!(type_table.get(param.type_id), ResolvedType::Ref(_)) {
                immut_ref_params.insert(position as u32);
            }
        }
        if !params.is_empty() || !immut_ref_params.is_empty() {
            cache.insert(
                (func.module_source.clone(), func.name.clone()),
                FuncUsageEntry {
                    params,
                    immut_ref_params,
                },
            );
        }
    }
    cache
}

/// Analyze which fields of struct-typed parameters a function accesses.
/// Returns a map from parameter position (0-based index in params list) to the set
/// of field indices accessed. `None` means "all fields" (conservative).
fn analyze_function_field_usage(
    func: &NirFunction,
    type_table: &TypeTable,
) -> IndexMap<u32, ParamFieldUsage> {
    // Read-only field-usage scan; materialize the callee body to a tree.
    let Some(body) = &func.body.as_ref().map(crate::nir_arena::Body::to_block) else {
        return IndexMap::default();
    };

    // Build mapping: local_index → param_position for struct-typed params
    let mut local_to_position: IndexMap<u32, u32> = IndexMap::default();
    let mut struct_param_locals: IndexSet<u32> = IndexSet::default();

    for (position, param) in func.params.iter().enumerate() {
        if is_gc_heap_type(param.type_id, type_table) {
            local_to_position.insert(param.local_index, position as u32);
            struct_param_locals.insert(param.local_index);
        }
    }

    if struct_param_locals.is_empty() {
        return IndexMap::default();
    }

    let mut field_sets: IndexMap<u32, IndexSet<u32>> = IndexMap::default();
    let mut conservative_locals: IndexSet<u32> = IndexSet::default();

    collect_param_field_usage_in_block(
        body,
        &struct_param_locals,
        &mut field_sets,
        &mut conservative_locals,
        type_table,
    );

    // Convert from local_index-keyed to position-keyed
    let mut result: IndexMap<u32, ParamFieldUsage> = IndexMap::default();
    for (&local_idx, &position) in &local_to_position {
        if conservative_locals.contains(&local_idx) {
            result.insert(position, None);
        } else if let Some(fields) = field_sets.get(&local_idx) {
            result.insert(position, Some(fields.clone()));
        }
        // If not in field_sets and not conservative, param is unused → empty set
        // (no fields need syncing)
    }
    result
}

fn collect_param_field_usage_in_block(
    block: &NirBlock,
    struct_params: &IndexSet<u32>,
    field_sets: &mut IndexMap<u32, IndexSet<u32>>,
    conservative_params: &mut IndexSet<u32>,
    type_table: &TypeTable,
) {
    for stmt in &block.stmts {
        collect_param_field_usage_in_stmt(
            stmt,
            struct_params,
            field_sets,
            conservative_params,
            type_table,
        );
    }
}

fn collect_param_field_usage_in_stmt(
    stmt: &NirStmt,
    struct_params: &IndexSet<u32>,
    field_sets: &mut IndexMap<u32, IndexSet<u32>>,
    conservative_params: &mut IndexSet<u32>,
    type_table: &TypeTable,
) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } => {
            collect_param_field_usage_in_expr(
                value,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirStmtKind::Expr(expr) => {
            collect_param_field_usage_in_expr(
                expr,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_param_field_usage_in_expr(
                    v,
                    struct_params,
                    field_sets,
                    conservative_params,
                    type_table,
                );
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_param_field_usage_in_expr(
                condition,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
            collect_param_field_usage_in_block(
                then_block,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
            if let Some(eb) = else_block {
                collect_param_field_usage_in_block(
                    eb,
                    struct_params,
                    field_sets,
                    conservative_params,
                    type_table,
                );
            }
        }
        NirStmtKind::Loop { body } => {
            collect_param_field_usage_in_block(
                body,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            collect_param_field_usage_in_block(
                block,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_param_field_usage_in_expr(
                    v,
                    struct_params,
                    field_sets,
                    conservative_params,
                    type_table,
                );
            }
        }
        NirStmtKind::Continue => {}
        NirStmtKind::LetDestructure { value, .. } => {
            collect_param_field_usage_in_expr(
                value,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
    }
}

fn collect_param_field_usage_in_expr(
    expr: &NirExpr,
    struct_params: &IndexSet<u32>,
    field_sets: &mut IndexMap<u32, IndexSet<u32>>,
    conservative_params: &mut IndexSet<u32>,
    type_table: &TypeTable,
) {
    match &expr.kind {
        NirExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            // Track field access on struct param: `self.field` or `(&mut self).field`
            let local_idx = extract_local_index(inner);
            if let Some(idx) = local_idx
                && struct_params.contains(&idx)
            {
                field_sets.entry(idx).or_default().insert(*field_index);
                return;
            }
            collect_param_field_usage_in_expr(
                inner,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirExprKind::Assign { target, value } => {
            // Check for `self.field = val` (field assignment)
            if let NirExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } = &target.kind
            {
                let local_idx = extract_local_index(inner);
                if let Some(idx) = local_idx
                    && struct_params.contains(&idx)
                {
                    field_sets.entry(idx).or_default().insert(*field_index);
                    collect_param_field_usage_in_expr(
                        value,
                        struct_params,
                        field_sets,
                        conservative_params,
                        type_table,
                    );
                    return;
                }
            }
            // Check for full local assignment `param = val` → conservative
            if let NirExprKind::Local { index, .. } = &target.kind
                && struct_params.contains(index)
            {
                conservative_params.insert(*index);
            }
            collect_param_field_usage_in_expr(
                target,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
            collect_param_field_usage_in_expr(
                value,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirExprKind::Call { args, .. } => {
            // If a struct param is passed as argument → conservative
            for arg in args {
                mark_if_param_passed(&arg.expr, struct_params, conservative_params, type_table);
                collect_param_field_usage_in_expr(
                    &arg.expr,
                    struct_params,
                    field_sets,
                    conservative_params,
                    type_table,
                );
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            // If a struct param is the receiver → conservative (self passed to another method)
            mark_if_param_passed(receiver, struct_params, conservative_params, type_table);
            collect_param_field_usage_in_expr(
                receiver,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
            for arg in args {
                mark_if_param_passed(&arg.expr, struct_params, conservative_params, type_table);
                collect_param_field_usage_in_expr(
                    &arg.expr,
                    struct_params,
                    field_sets,
                    conservative_params,
                    type_table,
                );
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            collect_param_field_usage_in_expr(
                callee,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
            for arg in args {
                mark_if_param_passed(arg, struct_params, conservative_params, type_table);
                collect_param_field_usage_in_expr(
                    arg,
                    struct_params,
                    field_sets,
                    conservative_params,
                    type_table,
                );
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_param_field_usage_in_expr(
                    arg,
                    struct_params,
                    field_sets,
                    conservative_params,
                    type_table,
                );
            }
        }
        NirExprKind::Binary { left, right, .. } => {
            collect_param_field_usage_in_expr(
                left,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
            collect_param_field_usage_in_expr(
                right,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirExprKind::Unary { expr, .. } => {
            collect_param_field_usage_in_expr(
                expr,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirExprKind::Cast { expr, .. } => {
            collect_param_field_usage_in_expr(
                expr,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirExprKind::Index { expr, index } => {
            collect_param_field_usage_in_expr(
                expr,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
            collect_param_field_usage_in_expr(
                index,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirExprKind::Block(block) => {
            collect_param_field_usage_in_block(
                block,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_param_field_usage_in_expr(
                condition,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
            collect_param_field_usage_in_block(
                then_branch,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
            if let Some(eb) = else_branch {
                collect_param_field_usage_in_block(
                    eb,
                    struct_params,
                    field_sets,
                    conservative_params,
                    type_table,
                );
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_param_field_usage_in_expr(
                    &field.value,
                    struct_params,
                    field_sets,
                    conservative_params,
                    type_table,
                );
            }
        }
        NirExprKind::TupleLiteral { elements } | NirExprKind::ArrayLiteral { elements } => {
            for elem in elements {
                collect_param_field_usage_in_expr(
                    elem,
                    struct_params,
                    field_sets,
                    conservative_params,
                    type_table,
                );
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            collect_param_field_usage_in_expr(
                functor,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                collect_param_field_usage_in_expr(
                    p,
                    struct_params,
                    field_sets,
                    conservative_params,
                    type_table,
                );
            }
        }
        NirExprKind::LabeledBlock { block, .. } => {
            collect_param_field_usage_in_block(
                block,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            collect_param_field_usage_in_expr(
                value,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirExprKind::VariantTag { expr } | NirExprKind::VariantTest { expr, .. } => {
            collect_param_field_usage_in_expr(
                expr,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirExprKind::VariantPayload { expr, .. } => {
            collect_param_field_usage_in_expr(
                expr,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_param_field_usage_in_expr(
                scrutinee,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
            for arm in arms {
                collect_param_field_usage_in_block(
                    arm,
                    struct_params,
                    field_sets,
                    conservative_params,
                    type_table,
                );
            }
            collect_param_field_usage_in_block(
                default,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
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
        NirExprKind::Match { expr, arms } => {
            collect_param_field_usage_in_expr(
                expr,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_param_field_usage_in_expr(
                        guard,
                        struct_params,
                        field_sets,
                        conservative_params,
                        type_table,
                    );
                }
                collect_param_field_usage_in_expr(
                    &arm.body,
                    struct_params,
                    field_sets,
                    conservative_params,
                    type_table,
                );
            }
        }
    }
}

/// Extract local index from a local expression or `&mut local`.
fn extract_local_index(expr: &NirExpr) -> Option<u32> {
    match &expr.kind {
        NirExprKind::Local { index, .. } => Some(*index),
        NirExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => {
            if let NirExprKind::Local { index, .. } = &inner.kind {
                Some(*index)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// If `expr` is a struct param (or &mut of one), mark it as conservative.
fn mark_if_param_passed(
    expr: &NirExpr,
    struct_params: &IndexSet<u32>,
    conservative_params: &mut IndexSet<u32>,
    type_table: &TypeTable,
) {
    match &expr.kind {
        NirExprKind::Local { index, .. } => {
            if struct_params.contains(index) && is_gc_heap_type(expr.type_id, type_table) {
                conservative_params.insert(*index);
            }
        }
        NirExprKind::Unary {
            op: NirUnaryOp::MutRef | NirUnaryOp::Ref,
            expr: inner,
        } => {
            if let NirExprKind::Local { index, .. } = &inner.kind
                && struct_params.contains(index)
                && is_gc_heap_type(inner.type_id, type_table)
            {
                conservative_params.insert(*index);
            }
        }
        _ => {}
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scalarization core (candidate selection and rewriting)
// ─────────────────────────────────────────────────────────────────────────────

fn scalarize_function(
    func: &mut NirFunction,
    type_table: &TypeTable,
    cache: &FieldUsageCache,
) -> bool {
    if func.body.is_none() {
        return false;
    }
    // Function-wide alias scan. Loop-local alias detection in
    // `count_field_accesses_in_expr` only sees the loop body, but
    // `tmpl_hoist`-style passes hoist a local's `&mut` capture out of
    // the loop, where the alias is *live* across the loop and writes
    // through it bypass any HFS scalar. Collect every GC-heap local
    // whose address is taken anywhere in the function (read-only, over
    // the arena body) so loop-level scalarization can refuse those
    // candidates.
    let aliased_in_function =
        collect_function_aliased_locals(func.body.as_ref().unwrap(), type_table);
    let analysis = HfsAnalysis {
        aliased_in_function: &aliased_in_function,
    };
    let mut local_count = func.local_count;
    let mut locals = func.locals.clone();
    let changed = {
        let body = func.body.as_mut().unwrap();
        let root = body.root;
        scalarize_block(
            body,
            root,
            &mut local_count,
            &mut locals,
            type_table,
            cache,
            &analysis,
        )
    };
    func.local_count = local_count;
    func.locals = locals;
    changed
}

/// Read-only function-wide pre-analysis shared by every `scalarize_loop`
/// invocation in the same function. Computed once in `scalarize_function`
/// to keep the per-loop work linear in the loop body's size.
struct HfsAnalysis<'a> {
    aliased_in_function: &'a IndexSet<u32>,
}

/// Arena driver: walk the function body finding loops, recursing into nested
/// blocks first, then scalarizing each loop. The per-loop transform runs on a
/// materialized tree of the loop body (the battle-tested tree state machine),
/// lowered back into the arena afterward.
fn scalarize_block(
    body: &mut Body,
    block: BlockId,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    cache: &FieldUsageCache,
    analysis: &HfsAnalysis<'_>,
) -> bool {
    enum Shape {
        Loop(BlockId),
        If(BlockId, Option<BlockId>),
        Labeled(BlockId),
        Other,
    }
    let mut changed = false;
    let mut new_stmts: Vec<StmtId> = Vec::new();

    for s in std::mem::take(&mut body.blocks[block].stmts) {
        let shape = match &body.stmts[s].kind {
            StmtKind::Loop { body: lb } => Shape::Loop(*lb),
            StmtKind::If {
                then_block,
                else_block,
                ..
            } => Shape::If(*then_block, *else_block),
            StmtKind::LabeledBlock { block: inner, .. } => Shape::Labeled(*inner),
            _ => Shape::Other,
        };
        match shape {
            Shape::Loop(lb) => {
                // Recurse into inner blocks/loops first.
                changed |=
                    scalarize_block(body, lb, local_count, locals, type_table, cache, analysis);
                // Scalarize hot fields at this loop level.
                let (pre, post) =
                    scalarize_loop_at(body, lb, local_count, locals, type_table, cache, analysis);
                if pre.is_empty() {
                    new_stmts.push(s);
                } else {
                    changed = true;
                    new_stmts.extend(pre);
                    new_stmts.push(s);
                    new_stmts.extend(post);
                }
            }
            Shape::If(then_b, else_b) => {
                changed |= scalarize_block(
                    body,
                    then_b,
                    local_count,
                    locals,
                    type_table,
                    cache,
                    analysis,
                );
                if let Some(eb) = else_b {
                    changed |=
                        scalarize_block(body, eb, local_count, locals, type_table, cache, analysis);
                }
                new_stmts.push(s);
            }
            Shape::Labeled(inner) => {
                changed |= scalarize_block(
                    body,
                    inner,
                    local_count,
                    locals,
                    type_table,
                    cache,
                    analysis,
                );
                new_stmts.push(s);
            }
            Shape::Other => new_stmts.push(s),
        }
    }

    body.blocks[block].stmts = new_stmts;
    changed
}

/// Run the (tree-shaped) loop scalarizer on a materialized copy of `lb`, lower
/// the transformed loop body back into the arena, and lower the pre / post
/// statements that wrap the loop, returning them as arena statement ids.
#[allow(clippy::too_many_arguments)]
fn scalarize_loop_at(
    body: &mut Body,
    lb: BlockId,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    cache: &FieldUsageCache,
    analysis: &HfsAnalysis<'_>,
) -> (Vec<StmtId>, Vec<StmtId>) {
    // Collect the locals introduced inside the loop on the arena before
    // materializing, so the (otherwise tree-shaped) scalarize machinery does
    // not need a tree walker for it.
    let inside_loop_locals = collect_locals_introduced_in_block(body, lb);
    let mut tree_lb = body.to_tree_block(lb);
    let result = scalarize_loop(
        &mut tree_lb,
        &inside_loop_locals,
        local_count,
        locals,
        type_table,
        cache,
        analysis,
    );
    // Splice the transformed loop body back into the arena.
    let lowered = body.lower_block(&tree_lb);
    let new_stmts = std::mem::take(&mut body.blocks[lowered].stmts);
    body.blocks[lb].stmts = new_stmts;
    (
        lower_tree_stmts(body, result.pre_stmts),
        lower_tree_stmts(body, result.post_stmts),
    )
}

/// Lower a list of tree statements into the arena, returning their ids.
fn lower_tree_stmts(body: &mut Body, stmts: Vec<NirStmt>) -> Vec<StmtId> {
    if stmts.is_empty() {
        return Vec::new();
    }
    let span = stmts[0].span;
    let wrapper = NirBlock { stmts, span };
    let b = body.lower_block(&wrapper);
    std::mem::take(&mut body.blocks[b].stmts)
}

struct ScalarizeResult {
    pre_stmts: Vec<NirStmt>,
    post_stmts: Vec<NirStmt>,
}

#[derive(Debug, Clone)]
struct ScalarizeCandidate {
    local_index: u32,
    local_name: String,
    local_type_id: TypeId,
    field_index: u32,
    field_name: String,
    type_id: TypeId,
    new_local_index: u32,
}

fn scalarize_loop(
    loop_body: &mut NirBlock,
    inside_loop_locals: &IndexSet<u32>,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    cache: &FieldUsageCache,
    analysis: &HfsAnalysis<'_>,
) -> ScalarizeResult {
    // Step 1: Count field accesses (reads + writes) in the loop body
    let mut access_counts: IndexMap<(u32, u32), FieldAccessInfo> = IndexMap::default();
    count_field_accesses_in_block(loop_body, &mut access_counts, type_table);

    // Step 1b: `inside_loop_locals` lists locals introduced inside the loop
    // body (computed on the arena by the caller). These cannot be safely
    // scalarized at this loop level — their owning storage (the GC struct ref)
    // is unbound at the loop's pre-header where the hoisted
    // `let __hfs_field = local.field;` would run, producing a null-reference
    // trap. Locals declared in the parent scope (i.e., not listed here) are
    // fine to scalarize.

    // Step 2: Select candidates - fields accessed frequently enough,
    // where the field is modified only by direct assignment (not by the whole local being reassigned)
    let mut candidates: Vec<ScalarizeCandidate> = Vec::new();
    let mut next_local = *local_count;

    for (&(local_idx, field_idx), info) in &access_counts {
        let total = info.read_count + info.write_count;
        if total < MIN_ACCESS_COUNT {
            continue;
        }
        // The field must have writes (otherwise LICM handles it)
        if info.write_count == 0 {
            continue;
        }
        // The local must not be fully reassigned in the loop (e.g., `state = new_state`)
        // Only direct local assignment counts — `Let` for different locals and
        // field assignments do NOT make the original local "fully assigned"
        if info.local_fully_assigned {
            continue;
        }
        // The local must not be aliased (e.g., `other = pos` creates an alias,
        // and field modifications through the alias would bypass the scalar local)
        if info.aliased {
            continue;
        }
        // The local must not have its address captured anywhere in the
        // function (e.g. `Formatter { buf: &mut local }` stored in another
        // local, then read inside this loop). Writes through a captured
        // alias bypass the HFS scalar — and since the capture can be
        // hoisted *out* of the loop by `tmpl_hoist`, the loop-local
        // alias scan in `count_field_accesses_in_expr` does not see it.
        if analysis.aliased_in_function.contains(&local_idx) {
            continue;
        }
        // The local must be bound in the parent scope (visible at the loop's
        // pre-header). A local introduced inside the loop body is unbound
        // before its `Let`, and our hoisted `let _hfs = local.field;`
        // would null-deref at runtime.
        if inside_loop_locals.contains(&local_idx) {
            continue;
        }
        // The type must be a GC struct (not a primitive)
        if !is_gc_heap_type(info.local_type_id, type_table) {
            continue;
        }

        candidates.push(ScalarizeCandidate {
            local_index: local_idx,
            local_name: info.local_name.clone(),
            local_type_id: info.local_type_id,
            field_index: field_idx,
            field_name: info.field_name.clone(),
            type_id: info.field_type_id,
            new_local_index: next_local,
        });
        locals.push(NirLocal {
            name: format!("__hfs_{}_{}", info.field_name, next_local),
            type_id: info.field_type_id,
            is_mut: true,
        });
        next_local += 1;
    }

    if candidates.is_empty() {
        return ScalarizeResult {
            pre_stmts: Vec::new(),
            post_stmts: Vec::new(),
        };
    }

    *local_count = next_local;

    // Step 3: Create pre-loop load statements
    let span = crate::token::Span::new(0, 0, 0, 0);
    let mut pre_stmts = Vec::new();
    for c in &candidates {
        let local_type_id = if (c.local_index as usize) < locals.len() {
            locals[c.local_index as usize].type_id
        } else {
            c.type_id
        };

        let load_stmt = NirStmt::new(
            NirStmtKind::Let {
                name: format!("__hfs_{}_{}", c.field_name, c.new_local_index),
                local_index: c.new_local_index,
                is_mut: true,
                is_reactive: false,
                type_id: c.type_id,
                value: NirExpr::new(
                    NirExprKind::FieldAccess {
                        expr: Box::new(NirExpr::new(
                            NirExprKind::Local {
                                index: c.local_index,
                                name: c.local_name.clone(),
                            },
                            local_type_id,
                            span,
                        )),
                        field_index: c.field_index,
                        field_name: c.field_name.clone(),
                    },
                    c.type_id,
                    span,
                ),
                skip_value_copy: true,
            },
            span,
        );
        pre_stmts.push(load_stmt);
    }

    // Step 4: Walk the loop body with the dataflow-driven sync-placement
    // pass. The walker tracks per-candidate canonical-side state
    // (Scalar/Field/Both) and inserts write-back / re-read stmts only at
    // state transitions. At escape paths (return / non-enclosing break) it
    // commits the scalar to the field if `ScalarOnly`. At body end it
    // forces all candidates back to `Both` to satisfy the loop's
    // back-edge invariant (entry == exit). With this discipline the
    // post-loop write-back is always redundant — the body and escape
    // paths leave every candidate's field canonical — so no `post_stmts`
    // are generated.
    process_loop_body(
        loop_body,
        &candidates,
        locals,
        local_count,
        type_table,
        cache,
    );
    let post_stmts: Vec<NirStmt> = Vec::new();

    ScalarizeResult {
        pre_stmts,
        post_stmts,
    }
}

fn make_write_back_stmt(c: &ScalarizeCandidate, span: crate::token::Span) -> NirStmt {
    NirStmt::new(
        NirStmtKind::Expr(NirExpr::new(
            NirExprKind::Assign {
                target: Box::new(NirExpr::new(
                    NirExprKind::FieldAccess {
                        expr: Box::new(NirExpr::new(
                            NirExprKind::Local {
                                index: c.local_index,
                                name: c.local_name.clone(),
                            },
                            c.local_type_id,
                            span,
                        )),
                        field_index: c.field_index,
                        field_name: c.field_name.clone(),
                    },
                    c.type_id,
                    span,
                )),
                value: Box::new(NirExpr::new(
                    NirExprKind::Local {
                        index: c.new_local_index,
                        name: format!("__hfs_{}_{}", c.field_name, c.new_local_index),
                    },
                    c.type_id,
                    span,
                )),
            },
            c.type_id,
            span,
        )),
        span,
    )
}

fn make_re_read_stmt(c: &ScalarizeCandidate, span: crate::token::Span) -> NirStmt {
    NirStmt::new(
        NirStmtKind::Expr(NirExpr::new(
            NirExprKind::Assign {
                target: Box::new(NirExpr::new(
                    NirExprKind::Local {
                        index: c.new_local_index,
                        name: format!("__hfs_{}_{}", c.field_name, c.new_local_index),
                    },
                    c.type_id,
                    span,
                )),
                value: Box::new(NirExpr::new(
                    NirExprKind::FieldAccess {
                        expr: Box::new(NirExpr::new(
                            NirExprKind::Local {
                                index: c.local_index,
                                name: c.local_name.clone(),
                            },
                            c.local_type_id,
                            span,
                        )),
                        field_index: c.field_index,
                        field_name: c.field_name.clone(),
                    },
                    c.type_id,
                    span,
                )),
            },
            c.type_id,
            span,
        )),
        span,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Field access counting (unchanged from original)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct FieldAccessInfo {
    local_name: String,
    field_name: String,
    local_type_id: TypeId,
    field_type_id: TypeId,
    read_count: usize,
    write_count: usize,
    local_fully_assigned: bool,
    /// True if the struct reference is copied to another local (alias created),
    /// meaning field modifications through the alias won't update the scalar local.
    aliased: bool,
}

fn count_field_accesses_in_block(
    block: &NirBlock,
    counts: &mut IndexMap<(u32, u32), FieldAccessInfo>,
    type_table: &TypeTable,
) {
    for stmt in &block.stmts {
        count_field_accesses_in_stmt(stmt, counts, type_table);
    }
}

/// Walk the function body once and collect every GC-heap-typed local
/// whose address (`&local` / `&mut local`) is taken outside a direct
/// call-argument position. Such locals cannot be HFS-scalarized at any
/// loop level: writes through the captured alias bypass the scalar.
///
/// Direct call arguments are excluded from the set because the call's
/// write-back/re-read mechanism synchronises HFS scalars around the
/// call, bounding the alias's lifetime to that single call.
fn collect_function_aliased_locals(body: &Body, type_table: &TypeTable) -> IndexSet<u32> {
    let mut out: IndexSet<u32> = IndexSet::default();
    visit_block_for_alias(body, body.root, type_table, &mut out);
    out
}

fn visit_block_for_alias(
    body: &Body,
    block: BlockId,
    type_table: &TypeTable,
    out: &mut IndexSet<u32>,
) {
    for i in 0..body.blocks[block].stmts.len() {
        let sid = body.blocks[block].stmts[i];
        visit_stmt_for_alias(body, sid, type_table, out);
    }
}

fn visit_stmt_for_alias(
    body: &Body,
    stmt: StmtId,
    type_table: &TypeTable,
    out: &mut IndexSet<u32>,
) {
    match &body.stmts[stmt].kind {
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            visit_expr_for_alias(body, *value, false, type_table, out);
        }
        StmtKind::Expr(expr) => {
            visit_expr_for_alias(body, *expr, false, type_table, out);
        }
        StmtKind::Return { value } | StmtKind::Break { value, .. } => {
            if let Some(v) = *value {
                visit_expr_for_alias(body, v, false, type_table, out);
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            visit_expr_for_alias(body, condition, false, type_table, out);
            visit_block_for_alias(body, then_block, type_table, out);
            if let Some(eb) = else_block {
                visit_block_for_alias(body, eb, type_table, out);
            }
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            let b = *b;
            visit_block_for_alias(body, b, type_table, out);
        }
        StmtKind::Continue => {}
    }
}

fn visit_expr_for_alias(
    body: &Body,
    id: ExprId,
    in_call_arg: bool,
    type_table: &TypeTable,
    out: &mut IndexSet<u32>,
) {
    match &body.exprs[id].kind {
        ExprKind::Unary { op, expr: inner } => {
            let (op, inner) = (*op, *inner);
            // `&local` / `&mut local`: record the alias if we are not in a
            // call-argument position (where write-back/re-read synchronises
            // around the call), then stop — the inner `Local` is the place
            // we take the address of, not a value-position read.
            if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
                && matches!(&body.exprs[inner].kind, ExprKind::Local { .. })
            {
                if !in_call_arg
                    && is_gc_heap_type(body.exprs[inner].type_id, type_table)
                    && let ExprKind::Local { index, .. } = &body.exprs[inner].kind
                {
                    out.insert(*index);
                }
                return;
            }
            visit_expr_for_alias(body, inner, false, type_table, out);
        }
        ExprKind::Call { args, .. } => {
            for aid in args.iter().map(|a| a.expr).collect::<Vec<_>>() {
                visit_expr_for_alias(body, aid, true, type_table, out);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            let receiver = *receiver;
            let arg_ids: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            visit_expr_for_alias(body, receiver, true, type_table, out);
            for aid in arg_ids {
                visit_expr_for_alias(body, aid, true, type_table, out);
            }
        }
        ExprKind::CmRawCall { args, .. } => {
            for aid in args.clone() {
                visit_expr_for_alias(body, aid, true, type_table, out);
            }
        }
        ExprKind::IndirectCall { callee, args, .. } => {
            let callee = *callee;
            let arg_ids = args.clone();
            visit_expr_for_alias(body, callee, false, type_table, out);
            for aid in arg_ids {
                visit_expr_for_alias(body, aid, true, type_table, out);
            }
        }
        ExprKind::Binary { left, right, .. } => {
            let (left, right) = (*left, *right);
            visit_expr_for_alias(body, left, false, type_table, out);
            visit_expr_for_alias(body, right, false, type_table, out);
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            visit_expr_for_alias(body, target, false, type_table, out);
            visit_expr_for_alias(body, value, false, type_table, out);
        }
        ExprKind::Index { expr, index } => {
            let (expr, index) = (*expr, *index);
            visit_expr_for_alias(body, expr, false, type_table, out);
            visit_expr_for_alias(body, index, false, type_table, out);
        }
        ExprKind::Cast { expr, .. }
        | ExprKind::FieldAccess { expr, .. }
        | ExprKind::VariantTag { expr }
        | ExprKind::VariantTest { expr, .. }
        | ExprKind::VariantPayload { expr, .. }
        | ExprKind::GlobalVarSet { value: expr, .. }
        | ExprKind::ClosureToCanonical { functor: expr, .. } => {
            let expr = *expr;
            visit_expr_for_alias(body, expr, false, type_table, out);
        }
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            let block = *block;
            visit_block_for_alias(body, block, type_table, out);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
            visit_expr_for_alias(body, condition, false, type_table, out);
            visit_block_for_alias(body, then_branch, type_table, out);
            if let Some(eb) = else_branch {
                visit_block_for_alias(body, eb, type_table, out);
            }
        }
        ExprKind::Match { expr, arms } => {
            let expr = *expr;
            let arms = arms.clone();
            visit_expr_for_alias(body, expr, false, type_table, out);
            for arm in &arms {
                if let Some(g) = arm.guard {
                    visit_expr_for_alias(body, g, false, type_table, out);
                }
                visit_expr_for_alias(body, arm.body, false, type_table, out);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            for fid in fields.iter().map(|f| f.value).collect::<Vec<_>>() {
                visit_expr_for_alias(body, fid, false, type_table, out);
            }
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            for eid in elements.clone() {
                visit_expr_for_alias(body, eid, false, type_table, out);
            }
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = *payload {
                visit_expr_for_alias(body, p, false, type_table, out);
            }
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
            visit_expr_for_alias(body, scrutinee, false, type_table, out);
            for arm in arms {
                visit_block_for_alias(body, arm, type_table, out);
            }
            visit_block_for_alias(body, default, type_table, out);
        }
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

/// Collects every local index introduced (by `Let`, `LetDestructure`, match-
/// arm patterns, etc.) anywhere inside `block`, walking through nested
/// arms / blocks but NOT through nested `Loop` bodies (those are processed
/// independently by their own `scalarize_loop` call). Used to filter out
/// locals whose owning storage is unbound at the loop's pre-header — those
/// locals must not be scalarized at this loop level, otherwise the hoisted
/// `let __hfs_field = local.field;` null-derefs at runtime.
fn collect_locals_introduced_in_block(body: &Body, block: BlockId) -> IndexSet<u32> {
    let mut out = IndexSet::default();
    collect_locals_introduced_node(body, NodeRef::Block(block), &mut out);
    out
}

fn collect_locals_introduced_node(body: &Body, node: NodeRef, out: &mut IndexSet<u32>) {
    match node {
        NodeRef::Stmt(s) => match &body.stmts[s].kind {
            StmtKind::Let { local_index, .. } => {
                out.insert(*local_index);
            }
            // Skip nested loops: their locals are processed by their own
            // scalarize_loop pass and are not visible at *this* loop's
            // pre-header anyway.
            StmtKind::Loop { .. } => return,
            _ => {}
        },
        NodeRef::Expr(e) => match &body.exprs[e].kind {
            // Closure bodies live in a separate local-index namespace from the
            // enclosing function; recursing in would mistake closure-local
            // indices for outer-function locals.
            ExprKind::ClosureToCanonical { .. } => return,
            // Match-arm pattern bindings introduce locals. Other patterns
            // (e.g. `LetDestructure`) are intentionally not collected, matching
            // the previous visitor.
            ExprKind::Match { arms, .. } => {
                for pid in arms.iter().map(|a| a.pattern).collect::<Vec<_>>() {
                    collect_pattern_bindings(body, pid, out);
                }
            }
            _ => {}
        },
        NodeRef::Block(_) | NodeRef::Pat(_) => {}
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_locals_introduced_node(body, c, out);
    }
}

fn collect_pattern_bindings(body: &Body, pid: PatId, out: &mut IndexSet<u32>) {
    match &body.pats[pid].kind {
        PatKind::Binding { local_index, .. } => {
            out.insert(*local_index);
        }
        PatKind::Tuple(ps, _) | PatKind::Or(ps) => {
            for p in ps.clone() {
                collect_pattern_bindings(body, p, out);
            }
        }
        PatKind::Variant { bindings, .. } => {
            for p in bindings.clone() {
                collect_pattern_bindings(body, p, out);
            }
        }
        PatKind::Struct { fields, .. } => {
            for p in fields.iter().map(|f| f.pattern).collect::<Vec<_>>() {
                collect_pattern_bindings(body, p, out);
            }
        }
        PatKind::Wildcard
        | PatKind::Literal(_)
        | PatKind::Enum { .. }
        | PatKind::ConstantValue { .. }
        | PatKind::Range { .. } => {}
    }
}

fn count_field_accesses_in_stmt(
    stmt: &NirStmt,
    counts: &mut IndexMap<(u32, u32), FieldAccessInfo>,
    type_table: &TypeTable,
) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } => {
            // A Let inside a loop defines a new local variable. Unlike Assign,
            // it doesn't reassign an existing local. We don't need to mark
            // anything as fully assigned here — only process the value expression.
            //
            // However, if the value is a Local reference (e.g. `__local_47 = pos`),
            // this creates an alias. Any field modifications through the alias
            // won't be tracked by the scalarization, so mark the original as aliased.
            if let NirExprKind::Local { index, .. } = &value.kind
                && is_gc_heap_type(value.type_id, type_table)
            {
                mark_local_aliased(*index, counts);
            }
            count_field_accesses_in_expr(value, counts, false, false, type_table);
        }
        NirStmtKind::Expr(expr) => {
            count_field_accesses_in_expr(expr, counts, false, false, type_table);
        }
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                count_field_accesses_in_expr(v, counts, false, false, type_table);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            count_field_accesses_in_expr(condition, counts, false, false, type_table);
            count_field_accesses_in_block(then_block, counts, type_table);
            if let Some(eb) = else_block {
                count_field_accesses_in_block(eb, counts, type_table);
            }
        }
        NirStmtKind::Loop { body: _ } => {
            // Do NOT recurse into nested loops. Each loop level is processed
            // independently by its own scalarize_loop call in scalarize_block.
            // Recursing here would cause outer-level HFS to hoist fields that
            // are only accessed inside an inner loop, potentially before the
            // struct containing them is even initialized.
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            count_field_accesses_in_block(block, counts, type_table);
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                count_field_accesses_in_expr(v, counts, false, false, type_table);
            }
        }
        NirStmtKind::Continue => {}
        NirStmtKind::LetDestructure { value, .. } => {
            count_field_accesses_in_expr(value, counts, false, false, type_table);
        }
    }
}

/// `in_call_arg` is `true` only when this expression is the direct value
/// of a `Call` / `MethodCall` / `CmRawCall` / `IndirectCall` argument (or
/// the receiver of a method call). The call's write-back/re-read
/// mechanism synchronises HFS scalars around the call, so `&[mut] local`
/// in this position does NOT escape and need not mark the local aliased.
/// Anywhere else (`Let`, struct/array literal field, `Assign` rhs,
/// `Return` / `Break` value, …) the reference escapes and the local
/// must be marked aliased.
fn count_field_accesses_in_expr(
    expr: &NirExpr,
    counts: &mut IndexMap<(u32, u32), FieldAccessInfo>,
    is_assign_target: bool,
    in_call_arg: bool,
    type_table: &TypeTable,
) {
    match &expr.kind {
        NirExprKind::Assign { target, value } => {
            count_field_accesses_in_expr(target, counts, true, false, type_table);
            count_field_accesses_in_expr(value, counts, false, false, type_table);
            // If target is a direct local assignment, mark it fully assigned
            if let NirExprKind::Local { index, .. } = &target.kind {
                mark_local_fully_assigned(*index, counts);
            }
            // If value is a local reference (e.g., `other = pos`), the source is aliased
            if let NirExprKind::Local { index, .. } = &value.kind
                && is_gc_heap_type(value.type_id, type_table)
            {
                mark_local_aliased(*index, counts);
            }
        }
        NirExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => {
            // Match both `local.field` and `(&mut local).field` patterns.
            // The latter occurs for `&mut local.field` which NIR represents as
            // FieldAccess { expr: Unary { MutRef, Local { ... } }, field }.
            let local_info = match &inner.kind {
                NirExprKind::Local { index, name } => Some((*index, name.clone(), inner.type_id)),
                NirExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr: ref_inner,
                } => {
                    if let NirExprKind::Local { index, name } = &ref_inner.kind {
                        Some((*index, name.clone(), ref_inner.type_id))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some((index, name, local_type_id)) = local_info {
                let key = (index, *field_index);
                let info = counts.entry(key).or_insert_with(|| FieldAccessInfo {
                    local_name: name,
                    field_name: field_name.clone(),
                    local_type_id,
                    field_type_id: expr.type_id,
                    read_count: 0,
                    write_count: 0,
                    local_fully_assigned: false,
                    aliased: false,
                });
                if is_assign_target {
                    info.write_count += 1;
                } else {
                    info.read_count += 1;
                }
            } else {
                count_field_accesses_in_expr(inner, counts, false, false, type_table);
            }
        }
        NirExprKind::Binary { left, right, .. } => {
            count_field_accesses_in_expr(left, counts, false, false, type_table);
            count_field_accesses_in_expr(right, counts, false, false, type_table);
        }
        NirExprKind::Unary { op, expr } => {
            // `&local` / `&mut local` taken outside a direct call-argument
            // position escapes: the reference can be stored in a struct
            // field (e.g. `Formatter { buf: &mut __tmpl_buf }`), a local,
            // or returned, and writes through that alias do not go via
            // any HFS scalar. Mark the local aliased so HFS skips it.
            //
            // Direct call arguments are exempt because the call's write-
            // back/re-read mechanism synchronises the scalar around the
            // call, bounding the alias's lifetime to that single call.
            //
            // For `&[mut] local` we also stop the recursion: the inner
            // `Local` is the place we take the address of, not a
            // value-position read, so it must not trigger the Local arm
            // below (which would over-mark the local as aliased even
            // when the address is consumed by a call's write-back/
            // re-read mechanism).
            if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
                && let NirExprKind::Local { index, .. } = &expr.kind
            {
                if !in_call_arg && is_gc_heap_type(expr.type_id, type_table) {
                    mark_local_aliased(*index, counts);
                }
                return;
            }
            count_field_accesses_in_expr(expr, counts, false, false, type_table);
        }
        NirExprKind::Cast { expr, .. } => {
            count_field_accesses_in_expr(expr, counts, false, false, type_table);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                count_field_accesses_in_expr(&arg.expr, counts, false, true, type_table);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            count_field_accesses_in_expr(receiver, counts, false, true, type_table);
            for arg in args {
                count_field_accesses_in_expr(&arg.expr, counts, false, true, type_table);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                count_field_accesses_in_expr(arg, counts, false, true, type_table);
            }
        }
        NirExprKind::Index { expr, index } => {
            count_field_accesses_in_expr(expr, counts, false, false, type_table);
            count_field_accesses_in_expr(index, counts, false, false, type_table);
        }
        NirExprKind::Block(block) => {
            count_field_accesses_in_block(block, counts, type_table);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            count_field_accesses_in_expr(condition, counts, false, false, type_table);
            count_field_accesses_in_block(then_branch, counts, type_table);
            if let Some(eb) = else_branch {
                count_field_accesses_in_block(eb, counts, type_table);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                count_field_accesses_in_expr(&field.value, counts, false, false, type_table);
            }
        }
        NirExprKind::TupleLiteral { elements } | NirExprKind::ArrayLiteral { elements } => {
            for elem in elements {
                count_field_accesses_in_expr(elem, counts, false, false, type_table);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            count_field_accesses_in_expr(callee, counts, false, false, type_table);
            for arg in args {
                count_field_accesses_in_expr(arg, counts, false, true, type_table);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            count_field_accesses_in_expr(functor, counts, false, false, type_table);
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                count_field_accesses_in_expr(p, counts, false, false, type_table);
            }
        }
        NirExprKind::LabeledBlock { block, .. } => {
            count_field_accesses_in_block(block, counts, type_table);
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            count_field_accesses_in_expr(value, counts, false, false, type_table);
        }
        NirExprKind::VariantTag { expr } | NirExprKind::VariantTest { expr, .. } => {
            count_field_accesses_in_expr(expr, counts, false, false, type_table);
        }
        NirExprKind::VariantPayload { expr, .. } => {
            count_field_accesses_in_expr(expr, counts, false, false, type_table);
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            count_field_accesses_in_expr(scrutinee, counts, false, false, type_table);
            for arm in arms {
                count_field_accesses_in_block(arm, counts, type_table);
            }
            count_field_accesses_in_block(default, counts, type_table);
        }
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => {}
        NirExprKind::Local { index, .. } => {
            // Whole-local read of a GC-heap-typed local outside a direct
            // call-argument position escapes — the value flows into a
            // struct/tuple literal field, a break/return, a binary op, …
            // from where the surrounding code can read or write the
            // struct's fields without going through any HFS scalar.
            //
            // Concrete failure mode: `break __tmpl: __tmpl_buf` reads
            // `__tmpl_buf.used`/`.repr` directly, but if the inlined
            // `push_str` body that just ran wrote the new length only
            // to the scalar, the break sees a stale `.used` and the
            // produced `String` is truncated.
            //
            // Direct call arguments are exempt: the call's write-back/
            // re-read mechanism synchronises HFS scalars around the call.
            // Direct assignment targets are also exempt — writing TO
            // the local is handled separately by
            // `mark_local_fully_assigned`.
            if !in_call_arg && !is_assign_target && is_gc_heap_type(expr.type_id, type_table) {
                mark_local_aliased(*index, counts);
            }
        }
        NirExprKind::Match { expr, arms } => {
            count_field_accesses_in_expr(expr, counts, false, false, type_table);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    count_field_accesses_in_expr(guard, counts, false, false, type_table);
                }
                count_field_accesses_in_expr(&arm.body, counts, false, false, type_table);
            }
        }
    }
}

fn mark_local_fully_assigned(local_idx: u32, counts: &mut IndexMap<(u32, u32), FieldAccessInfo>) {
    for (&(li, _fi), info) in counts.iter_mut() {
        if li == local_idx {
            info.local_fully_assigned = true;
        }
    }
}

fn mark_local_aliased(local_idx: u32, counts: &mut IndexMap<(u32, u32), FieldAccessInfo>) {
    for (&(li, _fi), info) in counts.iter_mut() {
        if li == local_idx {
            info.aliased = true;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Replacement pass — dataflow-driven sync placement
//
// For each scalarized field `(L, F)` (with associated scalar local
// `__hfs_F`), the walker tracks one of three canonical-side states at
// each program point:
//
//   - `Both`        : `__hfs_F == L.F` (both sides agree).
//   - `ScalarOnly`  : `__hfs_F` is the truth, `L.F` is stale.
//   - `FieldOnly`   : `L.F` is the truth, `__hfs_F` is stale.
//
// Each operation has a state requirement and a state effect:
//
// | operation              | requires (state ∈)         | post state           |
// | scalar read            | {Both, ScalarOnly}         | unchanged            |
// | scalar write           | (any)                      | ScalarOnly           |
// | call w/ `&T` arg       | {Both, FieldOnly}          | unchanged            |
// | call w/ `&mut T` arg   | {Both, FieldOnly}          | FieldOnly            |
//
// When the requirement is not met, the walker inserts the cheapest sync
// to transition the state — `re_read` (FieldOnly→Both) for scalar reads
// and `write_back` (ScalarOnly→Both) for calls. After the operation the
// new state is recorded.
//
// At branch joins (Match/If/Switch arms), each arm is walked with a
// fresh copy of the entry state. The walker then picks a target state
// for the join and inserts at-end-of-arm convergence sync where exit ≠
// target.
//
// At escape paths (return / non-enclosing break), every `ScalarOnly`
// candidate gets a write-back — outside the loop only the field is
// observable.
//
// At loop body end, every candidate is forced back to `Both` (entry
// state), maintaining the back-edge invariant. This makes the
// post-loop write-back redundant in every case, so `scalarize_loop`
// returns no `post_stmts`.
//
// The temp call locals introduced by non-unit call wrappers are pooled
// per `TypeId` and reused across separate wrappers. Since each temp's
// def and use are confined to its containing wrapper Block, reuse is
// always sound.
// ─────────────────────────────────────────────────────────────────────────────

/// Per-candidate canonical-side state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CanonState {
    Both,
    ScalarOnly,
    FieldOnly,
}

impl CanonState {
    /// Whether `__hfs_F` holds the latest value (scalar reads are safe).
    fn scalar_canonical(self) -> bool {
        matches!(self, CanonState::Both | CanonState::ScalarOnly)
    }

    /// Whether `L.F` holds the latest value (calls are safe).
    fn field_canonical(self) -> bool {
        matches!(self, CanonState::Both | CanonState::FieldOnly)
    }
}

/// Map from candidate index (into `WalkCtx::candidates`) to its state.
type ScalarStates = Vec<CanonState>;

fn init_states(candidates: &[ScalarizeCandidate]) -> ScalarStates {
    vec![CanonState::Both; candidates.len()]
}

/// Mutable context threaded through the walker.
struct WalkCtx<'a> {
    candidates: &'a [ScalarizeCandidate],
    type_table: &'a TypeTable,
    cache: &'a FieldUsageCache,
    locals: &'a mut Vec<NirLocal>,
    local_count: &'a mut u32,
    /// Per-type free pool of `__hfs_call_*` temp local indices. Each call
    /// wrap that captures a non-unit return value pulls an index from the
    /// pool of the matching type and returns it when the wrap is fully
    /// constructed. Each temp's def/use are confined to one Block, so
    /// reuse across separate Blocks is sound.
    temp_pool: IndexMap<TypeId, Vec<u32>>,
    /// Per-active-label break-state observations. `walk_labeled_block`
    /// pushes an empty entry on enter and pops it on exit; every
    /// `walk_stmt` `Break { label: Some(l), .. }` arm appends the
    /// walker's current `ScalarStates` to the entry for `l`. The
    /// labeled-block exit then JOINs the fall-through state with every
    /// observed break-state to derive the post-block walker state —
    /// over-approximating by entry alone (the prior fix) wrongly
    /// dropped `FieldOnly` walker states when entry was `ScalarOnly`,
    /// causing missed re-reads in post-block code (#1190 regression).
    label_break_states: IndexMap<String, Vec<ScalarStates>>,
}

impl WalkCtx<'_> {
    fn alloc_temp(&mut self, type_id: TypeId) -> u32 {
        if let Some(idx) = self
            .temp_pool
            .get_mut(&type_id)
            .and_then(std::vec::Vec::pop)
        {
            return idx;
        }
        let idx = *self.local_count;
        *self.local_count += 1;
        self.locals.push(NirLocal {
            name: format!("__hfs_call_{idx}"),
            type_id,
            is_mut: false,
        });
        idx
    }

    fn free_temp(&mut self, idx: u32, type_id: TypeId) {
        self.temp_pool.entry(type_id).or_default().push(idx);
    }

    fn temp_name(&self, idx: u32) -> String {
        format!("__hfs_call_{idx}")
    }
}

/// Top-level entry: walk the loop body with state initialized to `Both`,
/// then force every candidate's state back to `Both` at body end so the
/// loop's back-edge invariant holds.
fn process_loop_body(
    body: &mut NirBlock,
    candidates: &[ScalarizeCandidate],
    locals: &mut Vec<NirLocal>,
    local_count: &mut u32,
    type_table: &TypeTable,
    cache: &FieldUsageCache,
) {
    let mut states = init_states(candidates);
    let mut ctx = WalkCtx {
        candidates,
        type_table,
        cache,
        locals,
        local_count,
        temp_pool: IndexMap::default(),
        label_break_states: IndexMap::default(),
    };
    walk_block(body, &mut states, &mut ctx);
    let span = crate::token::Span::new(0, 0, 0, 0);
    // Body-end: converge every candidate back to `Both` so the loop's
    // back-edge state matches the entry state established by the
    // pre-load. Insert one sync per candidate that diverged.
    let body_end = sync_to_target(&mut states, CanonState::Both, &ctx, span);
    body.stmts.extend(body_end);
}

// ─────────────────────────────────────────────────────────────────────────────
// State transition helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Emit sync stmts to bring every candidate from its current state to
/// `target`, mutating `states` accordingly. Returns the stmts in the
/// order they should be inserted (stable wrt candidate index).
fn sync_to_target(
    states: &mut ScalarStates,
    target: CanonState,
    ctx: &WalkCtx,
    span: crate::token::Span,
) -> Vec<NirStmt> {
    let mut out = Vec::new();
    for (i, c) in ctx.candidates.iter().enumerate() {
        if let Some(stmt) = state_transition_stmt(states[i], target, c, span) {
            out.push(stmt);
        }
        states[i] = target;
    }
    out
}

/// Emit a single sync stmt for one candidate transitioning from `from` to
/// `to`. Returns None if no sync is needed.
fn state_transition_stmt(
    from: CanonState,
    to: CanonState,
    c: &ScalarizeCandidate,
    span: crate::token::Span,
) -> Option<NirStmt> {
    use CanonState::{Both, FieldOnly, ScalarOnly};
    match (from, to) {
        (Both, Both) | (ScalarOnly, ScalarOnly) | (FieldOnly, FieldOnly) => None,
        // Tightening the label without crossing canonical sides — no sync.
        (Both, ScalarOnly | FieldOnly) => None,
        // ScalarOnly → Both / FieldOnly: scalar is canonical, field is
        // stale; commit scalar to field via write-back.
        (ScalarOnly, Both | FieldOnly) => Some(make_write_back_stmt(c, span)),
        // FieldOnly → Both / ScalarOnly: field is canonical, scalar is
        // stale; refresh scalar from field via re-read.
        (FieldOnly, Both | ScalarOnly) => Some(make_re_read_stmt(c, span)),
    }
}

/// Pick the join target state for a branch with the given arm-exit
/// states. Prefers the strongest state compatible with all arms; falls
/// back to `ScalarOnly` for `{Scalar, Field}` mixes since HFS-eligible
/// code overwhelmingly reads the scalar after a branch.
fn pick_join_target_for_candidate(arm_exits: &[CanonState]) -> CanonState {
    let mut has_scalar = false;
    let mut has_field = false;
    let mut has_both = false;
    for s in arm_exits {
        match s {
            CanonState::Both => has_both = true,
            CanonState::ScalarOnly => has_scalar = true,
            CanonState::FieldOnly => has_field = true,
        }
    }
    match (has_scalar, has_field, has_both) {
        // All arms agreed.
        (true, false, false) => CanonState::ScalarOnly,
        (false, true, false) => CanonState::FieldOnly,
        (false, false, true) => CanonState::Both,
        // Subset of {Both, ScalarOnly}: weaken to ScalarOnly (no sync needed
        // for either kind of arm — Both ⊆ ScalarOnly).
        (true, false, true) => CanonState::ScalarOnly,
        (false, true, true) => CanonState::FieldOnly,
        // {ScalarOnly, FieldOnly} or all three: must converge — heuristic
        // ScalarOnly preserves typical post-branch scalar reads.
        (true, true, _) => CanonState::ScalarOnly,
        (false, false, false) => CanonState::Both,
    }
}

/// Pick join targets for every candidate independently.
fn pick_join_targets(arm_exits: &[&ScalarStates]) -> ScalarStates {
    if arm_exits.is_empty() {
        return Vec::new();
    }
    let n = arm_exits[0].len();
    (0..n)
        .map(|i| {
            let exits: Vec<CanonState> = arm_exits.iter().map(|s| s[i]).collect();
            pick_join_target_for_candidate(&exits)
        })
        .collect()
}

/// Insert convergence sync at the end of `block` to bring `from` to
/// `to`. Only affects diverging candidates.
///
/// If the block's trailing stmt is a value-producing `Expr(e)` (non-unit
/// type), the value must survive the appended sync stmts. In that case
/// the trailing expr is captured into a pooled temp, the sync stmts are
/// inserted, and a final `Local(temp)` stmt is appended so the block
/// still evaluates to the original trailing value.
fn insert_convergence_at_block_end(
    block: &mut NirBlock,
    from: &ScalarStates,
    to: &ScalarStates,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    debug_assert_eq!(from.len(), to.len());
    let mut sync_stmts: Vec<NirStmt> = Vec::new();
    for (i, c) in ctx.candidates.iter().enumerate() {
        if let Some(stmt) = state_transition_stmt(from[i], to[i], c, span) {
            sync_stmts.push(stmt);
        }
    }
    if sync_stmts.is_empty() {
        return;
    }
    append_sync_preserving_block_value(block, sync_stmts, ctx);
}

/// Append `sync_stmts` to `block`, preserving the block's trailing value
/// if it has one. If the block's last stmt is a non-unit value-producing
/// `Expr(e)`, the trailing expr is captured into a pooled temp before
/// the sync stmts run, and a final `Local(temp)` stmt restores the
/// block's value contract. Otherwise (empty block, non-Expr trailing
/// stmt, or unit-typed trailing Expr) the sync stmts are simply
/// appended.
fn append_sync_preserving_block_value(
    block: &mut NirBlock,
    sync_stmts: Vec<NirStmt>,
    ctx: &mut WalkCtx,
) {
    let trailing_is_value = matches!(
        block.stmts.last(),
        Some(s) if matches!(&s.kind, NirStmtKind::Expr(e) if e.type_id != TypeTable::UNIT),
    );
    if !trailing_is_value {
        block.stmts.extend(sync_stmts);
        return;
    }
    let last_stmt = block.stmts.pop().expect("checked non-empty above");
    let last_span = last_stmt.span;
    let NirStmtKind::Expr(value_expr) = last_stmt.kind else {
        unreachable!("checked Expr above")
    };
    let body_type = value_expr.type_id;
    let tmp_idx = ctx.alloc_temp(body_type);
    let tmp_name = ctx.temp_name(tmp_idx);
    block.stmts.push(NirStmt::new(
        NirStmtKind::Let {
            name: tmp_name.clone(),
            local_index: tmp_idx,
            is_mut: false,
            is_reactive: false,
            type_id: body_type,
            value: value_expr,
            skip_value_copy: true,
        },
        last_span,
    ));
    block.stmts.extend(sync_stmts);
    block.stmts.push(NirStmt::new(
        NirStmtKind::Expr(NirExpr::new(
            NirExprKind::Local {
                index: tmp_idx,
                name: tmp_name,
            },
            body_type,
            last_span,
        )),
        last_span,
    ));
    ctx.free_temp(tmp_idx, body_type);
}

/// Build a fresh block holding only the convergence stmts from `from`
/// to `to`. Used to synthesize an else-branch when the original was
/// absent and the implicit no-op path needs sync.
fn build_convergence_block(
    from: &ScalarStates,
    to: &ScalarStates,
    ctx: &WalkCtx,
    span: crate::token::Span,
) -> NirBlock {
    let mut block = NirBlock::empty(span);
    for (i, c) in ctx.candidates.iter().enumerate() {
        if let Some(stmt) = state_transition_stmt(from[i], to[i], c, span) {
            block.stmts.push(stmt);
        }
    }
    block
}

/// True if any of the candidates' state in `from` differs from `to`.
fn states_differ(from: &ScalarStates, to: &ScalarStates) -> bool {
    debug_assert_eq!(from.len(), to.len());
    from.iter().zip(to.iter()).any(|(a, b)| a != b)
}

// ─────────────────────────────────────────────────────────────────────────────
// Statement walker
// ─────────────────────────────────────────────────────────────────────────────

fn walk_block(block: &mut NirBlock, states: &mut ScalarStates, ctx: &mut WalkCtx) {
    let span = crate::token::Span::new(0, 0, 0, 0);
    let stmts = std::mem::take(&mut block.stmts);
    let mut new_stmts = Vec::new();
    for stmt in stmts {
        walk_stmt(stmt, states, &mut new_stmts, ctx, span);
    }
    block.stmts = new_stmts;
}

fn walk_stmt(
    mut stmt: NirStmt,
    states: &mut ScalarStates,
    out: &mut Vec<NirStmt>,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    match &mut stmt.kind {
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            walk_expr(condition, states, true, out, ctx);
            walk_branching_block_if(then_block, else_block, states, ctx, span);
            out.push(stmt);
        }
        NirStmtKind::Loop { body } => {
            walk_nested_loop(body, states, out, ctx, span);
            out.push(stmt);
        }
        NirStmtKind::LabeledBlock {
            label,
            block: inner,
        } => {
            walk_labeled_block(label, inner, states, ctx);
            out.push(stmt);
        }
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                walk_expr(v, states, true, out, ctx);
            }
            // Escape: commit every ScalarOnly candidate to the field
            // before the return. After commit, only the field is
            // observable outside the loop.
            //
            // If the return expression itself transitioned a candidate to
            // `ScalarOnly` (e.g. an inlined `self.pos = self.pos + 1`
            // nested under `__inline_advance: { ... break tok; }` inside
            // the return value), a writeback emitted before the Return
            // stmt would run with the **pre-expression** scalar value at
            // runtime — the expression mutates `__hfs_pos` after the
            // writeback has already syncd the old value, and the post-
            // mutation value is then discarded by the return. Hoist the
            // value into a temp local so the writeback can run between
            // the expression's evaluation and the return jump.
            let needs_post_eval_writeback =
                value.is_some() && states.contains(&CanonState::ScalarOnly);
            if needs_post_eval_writeback {
                let return_value = value.take().expect("checked Some above");
                let return_type = return_value.type_id;
                let tmp_idx = ctx.alloc_temp(return_type);
                let tmp_name = ctx.temp_name(tmp_idx);
                out.push(NirStmt::new(
                    NirStmtKind::Let {
                        name: tmp_name.clone(),
                        local_index: tmp_idx,
                        is_mut: false,
                        is_reactive: false,
                        type_id: return_type,
                        value: return_value,
                        skip_value_copy: true,
                    },
                    span,
                ));
                commit_scalar_for_escape(states, out, ctx, span);
                out.push(NirStmt::new(
                    NirStmtKind::Return {
                        value: Some(NirExpr::new(
                            NirExprKind::Local {
                                index: tmp_idx,
                                name: tmp_name,
                            },
                            return_type,
                            span,
                        )),
                    },
                    span,
                ));
                ctx.free_temp(tmp_idx, return_type);
            } else {
                commit_scalar_for_escape(states, out, ctx, span);
                out.push(stmt);
            }
        }
        NirStmtKind::Break { value, label } => {
            if let Some(v) = value {
                walk_expr(v, states, true, out, ctx);
            }
            // Unlabeled `break` exits the innermost loop; at the HFS-loop
            // top level (the common case) this leaves the HFS scope, so
            // any `ScalarOnly` candidate must be committed inline before
            // the break — the body-end force-Both is not reached.
            //
            // Labeled `break <name>` exits a labeled block. Emitting an
            // unconditional commit here ("commit-on-every-labeled-break")
            // proved a runtime-perf disaster on gale's hot loops. Record
            // the walker's current state instead so `walk_labeled_block`
            // can JOIN every per-path exit into its post-block walker
            // state — the precise alternative to over-syncing.
            if let Some(l) = label {
                if let Some(bucket) = ctx.label_break_states.get_mut(l) {
                    bucket.push(states.clone());
                }
            } else {
                commit_scalar_for_escape(states, out, ctx, span);
            }
            out.push(stmt);
        }
        NirStmtKind::Continue => {
            // `continue` jumps back to the loop header, skipping the
            // body-end force-Both. Drive every candidate to `Both` so
            // the next iteration's invariant (every candidate is `Both`
            // at body entry) holds at runtime as well as at
            // compile-time.
            let sync_stmts = sync_to_target(states, CanonState::Both, ctx, span);
            out.extend(sync_stmts);
            out.push(stmt);
        }
        NirStmtKind::Let { value, .. } => {
            walk_expr(value, states, true, out, ctx);
            out.push(stmt);
        }
        NirStmtKind::LetDestructure { value, .. } => {
            walk_expr(value, states, true, out, ctx);
            out.push(stmt);
        }
        NirStmtKind::Expr(expr) => {
            walk_expr(expr, states, false, out, ctx);
            out.push(stmt);
        }
    }
}

/// Commit every scalar-canonical candidate to the field for an escape
/// path (return / non-enclosing break). After this, every candidate's
/// state has its field canonical (write-back was inserted only where
/// needed).
fn commit_scalar_for_escape(
    states: &mut ScalarStates,
    out: &mut Vec<NirStmt>,
    ctx: &WalkCtx,
    span: crate::token::Span,
) {
    for (i, c) in ctx.candidates.iter().enumerate() {
        if states[i] == CanonState::ScalarOnly {
            out.push(make_write_back_stmt(c, span));
            // The escape leaves the loop scope; subsequent code does not
            // observe `__hfs_F`, but recording the post-commit state keeps
            // the invariant for any downstream walker logic.
            states[i] = CanonState::Both;
        }
    }
}

/// Walk an If/IfLet's branches with cloned entry state, compute the
/// per-candidate join target, and insert convergence sync at the end of
/// each branch.
fn walk_branching_block_if(
    then_block: &mut NirBlock,
    else_block: &mut Option<NirBlock>,
    states: &mut ScalarStates,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    let entry = states.clone();
    let mut then_states = entry.clone();
    walk_block(then_block, &mut then_states, ctx);
    let (else_states, has_else) = if let Some(eb) = else_block.as_mut() {
        let mut s = entry.clone();
        walk_block(eb, &mut s, ctx);
        (s, true)
    } else {
        // The implicit no-op path leaves state unchanged from entry.
        (entry.clone(), false)
    };
    let target = pick_join_targets(&[&then_states, &else_states]);
    insert_convergence_at_block_end(then_block, &then_states, &target, ctx, span);
    if has_else {
        let eb = else_block.as_mut().unwrap();
        insert_convergence_at_block_end(eb, &else_states, &target, ctx, span);
    } else if states_differ(&entry, &target) {
        // The implicit no-op path needs sync to converge — synthesize
        // an else-block holding the convergence stmts.
        *else_block = Some(build_convergence_block(&entry, &target, ctx, span));
    }
    *states = target;
}

/// Walk a nested loop's body and update the outer state for the
/// post-loop point.
///
/// Two things must hold for the single-walk analysis to match runtime:
///
/// 1. **Back-edge invariant.** Iter 2+ at runtime starts with whatever
///    state the previous iteration's body left. The walker analyzed
///    iter 2+ assuming `entry_states`. Bridge that gap by appending
///    sync stmts at body-end that drive `body_exit_states` back to
///    `entry_states`. Same shape as `process_loop_body`'s body-end
///    force-Both, but targets `entry_states`. Without this, a
///    `read → &mut call` pattern miscompiles: walker emits no
///    re-read at the read (state=Both), but iter 2's runtime state
///    after iter 1's call is `FieldOnly` — scalar is stale.
///
/// 2. **Post-loop conservatism.** The NIR `Loop` only exits via
///    `break`/`return`, so the post-loop state at runtime is the
///    state at the break-point. The walker doesn't track break-paths
///    precisely; the OLD `pick_join_target_for_candidate(entry,
///    body_exit_pre_sync)` is a reasonable over-approximation since
///    `body_exit_pre_sync` captures the linear-walk's belief about
///    the body's running state, which in typical shapes coincides
///    with break-state. Use `body_exit_pre_sync` here, NOT the
///    post-back-edge-sync state, so the JOIN keeps that information.
fn walk_nested_loop(
    body: &mut NirBlock,
    states: &mut ScalarStates,
    out: &mut Vec<NirStmt>,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    // Pre-recurse: commit any `ScalarOnly` outer candidate so inner
    // reads (and a nested HFS's pre-load) observe an up-to-date field.
    for (i, c) in ctx.candidates.iter().enumerate() {
        if states[i] == CanonState::ScalarOnly {
            out.push(make_write_back_stmt(c, span));
            states[i] = CanonState::Both;
        }
    }
    let entry_states = states.clone();
    let mut body_exit_states = states.clone();
    walk_block(body, &mut body_exit_states, ctx);
    // Snapshot for post-loop JOIN before we overwrite body_exit with
    // the body-end sync.
    let body_exit_pre_sync = body_exit_states.clone();
    // Restore back-edge invariant: drive body_exit back to entry so
    // iter 2+ starts at the same state the walker analyzed.
    for (i, c) in ctx.candidates.iter().enumerate() {
        if let Some(stmt) = state_transition_stmt(body_exit_states[i], entry_states[i], c, span) {
            body.stmts.push(stmt);
        }
    }
    // Post-loop state: JOIN(entry_states, body_exit_pre_sync). The
    // outer scope sees the conservative over-approximation; subsequent
    // scalar reads emit re-reads when break could leave `FieldOnly`.
    //
    // A nested loop's post-state can never soundly be `ScalarOnly`: the
    // zero-iteration path exits at `entry_states`, which the pre-recurse
    // commit above forced to `{Both, FieldOnly}` (field canonical, never
    // scalar-only). Every other exit also leaves the field canonical —
    // the body-end back-edge sync drives `body_exit` back to `entry`, and
    // `commit_scalar_for_escape` writes the scalar back before any
    // `break`/`return`. So if the linear `body_exit_pre_sync` pushes the
    // JOIN to `ScalarOnly` (the `{ScalarOnly, FieldOnly}` branch-join
    // heuristic, sound only when each arm gets convergence sync — which
    // the un-syncable zero-iteration path does not), the scalar may in
    // fact be stale at runtime. Demote to `FieldOnly` so the next scalar
    // read re-reads from the canonical field instead of trusting a stale
    // `__hfs`. (Regression: a `&mut self` push run followed by an empty
    // inner loop, e.g. `gale dump`'s `render_follow_variants`, otherwise
    // wrote the stale length back and dropped the pushed bytes.)
    for i in 0..states.len() {
        let joined = pick_join_target_for_candidate(&[entry_states[i], body_exit_pre_sync[i]]);
        states[i] = match joined {
            CanonState::ScalarOnly => CanonState::FieldOnly,
            other => other,
        };
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Expression walker
// ─────────────────────────────────────────────────────────────────────────────

/// Walk an expression, mutating states and emitting sync stmts at the
/// surrounding stmt level (`out`). Field access rewrites and call
/// wrappings happen in place.
///
/// `result_used` indicates whether the expression's value is consumed
/// by its parent. When false, a Call's non-unit return can be discarded
/// without allocating a temp.
fn walk_expr(
    expr: &mut NirExpr,
    states: &mut ScalarStates,
    result_used: bool,
    out: &mut Vec<NirStmt>,
    ctx: &mut WalkCtx,
) {
    let span = expr.span;

    // Field assignment: `local.field = value` becomes `__hfs_F = value`.
    if let NirExprKind::Assign { target, value } = &mut expr.kind {
        if let Some((cand_idx, c)) = field_assign_to_candidate(target, ctx) {
            // Walk RHS first (state may transition through it).
            walk_expr(value, states, true, out, ctx);
            // Commit the assignment: rewrite target and update state.
            let new_target = NirExpr::new(
                NirExprKind::Local {
                    index: c.new_local_index,
                    name: format!("__hfs_{}_{}", c.field_name, c.new_local_index),
                },
                c.type_id,
                target.span,
            );
            **target = new_target;
            states[cand_idx] = CanonState::ScalarOnly;
            return;
        }
        // Not a scalarized field assign. Fall through to general recursion.
        walk_expr(target, states, true, out, ctx);
        walk_expr(value, states, true, out, ctx);
        return;
    }

    // Field read: `local.field` becomes `__hfs_F`. Requires scalar canonical;
    // insert re-read at stmt level if state is FieldOnly.
    if let Some((cand_idx, c)) = field_read_to_candidate(expr, ctx) {
        if !states[cand_idx].scalar_canonical() {
            out.push(make_re_read_stmt(&c, span));
            states[cand_idx] = CanonState::Both;
        }
        expr.kind = NirExprKind::Local {
            index: c.new_local_index,
            name: format!("__hfs_{}_{}", c.field_name, c.new_local_index),
        };
        return;
    }

    // Call sites: handle separately.
    if matches!(
        &expr.kind,
        NirExprKind::Call { .. }
            | NirExprKind::MethodCall { .. }
            | NirExprKind::IndirectCall { .. }
    ) {
        walk_call_expr(expr, states, out, ctx);
        return;
    }

    // Other expressions: recurse into sub-expressions in evaluation order,
    // letting state transitions propagate naturally.
    walk_other_expr_kinds(expr, states, result_used, out, ctx);
}

/// Match a target expression that's a scalarized field's `local.field`
/// in an assignment. Returns the candidate index + a clone of the
/// candidate. (Cloning avoids holding a borrow on `ctx` across the
/// caller's mutations.)
fn field_assign_to_candidate(
    target: &NirExpr,
    ctx: &WalkCtx,
) -> Option<(usize, ScalarizeCandidate)> {
    if let NirExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &target.kind
        && let NirExprKind::Local { index, .. } = &inner.kind
    {
        for (i, c) in ctx.candidates.iter().enumerate() {
            if c.local_index == *index && c.field_index == *field_index {
                return Some((i, c.clone()));
            }
        }
    }
    None
}

fn field_read_to_candidate(expr: &NirExpr, ctx: &WalkCtx) -> Option<(usize, ScalarizeCandidate)> {
    if let NirExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &expr.kind
        && let NirExprKind::Local { index, .. } = &inner.kind
    {
        for (i, c) in ctx.candidates.iter().enumerate() {
            if c.local_index == *index && c.field_index == *field_index {
                return Some((i, c.clone()));
            }
        }
    }
    None
}

/// Process a call expression (`Call` / `MethodCall` / `IndirectCall`).
/// Recurses into args first, then determines which candidates the
/// callee touches via `&T` (read-only) or `&mut T` (read-write),
/// emits pre-call write-backs at stmt level for any not yet
/// field-canonical, and updates state to `FieldOnly` for `&mut`-touched
/// candidates. The call expression itself is left in place; the wrap
/// (if any) is just the pre-call write-back stmts.
fn walk_call_expr(
    expr: &mut NirExpr,
    states: &mut ScalarStates,
    out: &mut Vec<NirStmt>,
    ctx: &mut WalkCtx,
) {
    let span = expr.span;
    // Compute the call's field effects BEFORE recursing into args (the
    // computation looks at direct args; recursion may wrap nested calls
    // and obscure the args' shape from extract_gc_local_index).
    let effects = compute_call_field_effects(expr, ctx);
    // Recurse into args. Their walk may emit its own sync at stmt level,
    // and may transition states for fields touched by nested calls.
    recurse_into_call_args(expr, states, out, ctx);
    // After args have been walked, commit pre-call state for THIS call.
    // (Only the call itself contributes; nested calls already handled.)
    for &i in &effects.read_required {
        let c = &ctx.candidates[i];
        if !states[i].field_canonical() {
            out.push(make_write_back_stmt(c, span));
            states[i] = CanonState::Both;
        }
    }
    // Post-call: candidates the callee may mutate become FieldOnly.
    for &i in &effects.mutated {
        states[i] = CanonState::FieldOnly;
    }
    // The call expression itself stays unchanged (no wrap at expression
    // level). All sync sits at stmt level via `out`.
}

fn recurse_into_call_args(
    expr: &mut NirExpr,
    states: &mut ScalarStates,
    out: &mut Vec<NirStmt>,
    ctx: &mut WalkCtx,
) {
    match &mut expr.kind {
        NirExprKind::Call { args, .. } => {
            for arg in args {
                walk_expr(&mut arg.expr, states, true, out, ctx);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            walk_expr(receiver, states, true, out, ctx);
            for arg in args {
                walk_expr(&mut arg.expr, states, true, out, ctx);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            walk_expr(callee, states, true, out, ctx);
            for arg in args {
                walk_expr(arg, states, true, out, ctx);
            }
        }
        _ => unreachable!("recurse_into_call_args called on non-call expr"),
    }
}

/// Effects of one call on the scalarized candidates.
struct CallFieldEffects {
    /// Candidate indices the callee reads (via `&T` or `&mut T` arg) —
    /// pre-call requires field canonical.
    read_required: Vec<usize>,
    /// Candidate indices the callee may write through (`&mut T` arg) —
    /// post-call state becomes `FieldOnly`.
    mutated: Vec<usize>,
}

fn compute_call_field_effects(call: &NirExpr, ctx: &WalkCtx) -> CallFieldEffects {
    let mut sync = SyncFields {
        write_back: IndexSet::default(),
        re_read: IndexSet::default(),
    };
    accumulate_call_sync(call, ctx.candidates, ctx.type_table, ctx.cache, &mut sync);
    let mut read_required = Vec::new();
    let mut mutated = Vec::new();
    for (i, c) in ctx.candidates.iter().enumerate() {
        if sync.write_back.contains(&(c.local_index, c.field_index)) {
            read_required.push(i);
        }
        if sync.re_read.contains(&(c.local_index, c.field_index)) {
            mutated.push(i);
        }
    }
    CallFieldEffects {
        read_required,
        mutated,
    }
}

/// Sync field accumulator (used both by the dataflow walker for one call
/// and historically for the legacy whole-stmt sync).
struct SyncFields {
    /// Fields the callee reads — pre-call write-back is needed if the
    /// scalar is currently canonical.
    write_back: IndexSet<(u32, u32)>,
    /// Fields the callee may write (`&mut T`) — post-call the field
    /// becomes canonical.
    re_read: IndexSet<(u32, u32)>,
}

fn accumulate_call_sync(
    call: &NirExpr,
    candidates: &[ScalarizeCandidate],
    type_table: &TypeTable,
    cache: &FieldUsageCache,
    result: &mut SyncFields,
) {
    match &call.kind {
        NirExprKind::Call { func, args, .. } => {
            for (arg_position, arg) in args.iter().enumerate() {
                let immut_ref = is_immut_ref_arg(&arg.expr, type_table);
                add_sync_fields_for_arg(
                    &arg.expr,
                    func,
                    arg_position as u32,
                    candidates,
                    type_table,
                    cache,
                    immut_ref,
                    result,
                );
            }
        }
        NirExprKind::MethodCall {
            func,
            receiver,
            args,
            ..
        } => {
            let immut_ref = is_immut_ref_arg(receiver, type_table);
            add_sync_fields_for_arg(
                receiver, func, 0, candidates, type_table, cache, immut_ref, result,
            );
            for (arg_position, arg) in args.iter().enumerate() {
                let immut_ref = is_immut_ref_arg(&arg.expr, type_table);
                add_sync_fields_for_arg(
                    &arg.expr,
                    func,
                    (arg_position + 1) as u32,
                    candidates,
                    type_table,
                    cache,
                    immut_ref,
                    result,
                );
            }
        }
        NirExprKind::IndirectCall { args, .. } => {
            for arg in args {
                if let Some(local_idx) = extract_gc_local_index(arg, type_table) {
                    add_all_fields_for_local(local_idx, candidates, &mut result.write_back);
                    if !is_immut_ref_arg(arg, type_table) {
                        add_all_fields_for_local(local_idx, candidates, &mut result.re_read);
                    }
                }
            }
        }
        NirExprKind::CmRawCall { .. } => {
            // CmRawCall is a lowered Wasm import — its args are primitive
            // Wasm types, never struct refs.
        }
        // The remaining NirExprKind variants are not call sites. The
        // single caller (`compute_call_field_effects`) is reached only
        // from `walk_call_expr`, which dispatches exclusively for
        // `Call` / `MethodCall` / `IndirectCall`. Fail loud if a future
        // refactor pushes a different shape into here, rather than
        // silently producing no sync.
        _ => unreachable!("accumulate_call_sync called on non-call expression"),
    }
}

/// Recurse into non-call, non-field-access expression kinds. Field
/// rewrites and call wrappings are handled by the higher-level
/// `walk_expr`; this helper just propagates the walk through structural
/// kinds.
fn walk_other_expr_kinds(
    expr: &mut NirExpr,
    states: &mut ScalarStates,
    result_used: bool,
    out: &mut Vec<NirStmt>,
    ctx: &mut WalkCtx,
) {
    let span = expr.span;
    match &mut expr.kind {
        NirExprKind::FieldAccess { expr: inner, .. } => {
            walk_expr(inner, states, true, out, ctx);
        }
        NirExprKind::Binary { left, right, .. } => {
            walk_expr(left, states, true, out, ctx);
            walk_expr(right, states, true, out, ctx);
        }
        NirExprKind::Unary { expr: inner, .. } => {
            walk_expr(inner, states, true, out, ctx);
        }
        NirExprKind::Cast { expr: inner, .. } => {
            walk_expr(inner, states, true, out, ctx);
        }
        NirExprKind::Index { expr: inner, index } => {
            walk_expr(inner, states, true, out, ctx);
            walk_expr(index, states, true, out, ctx);
        }
        NirExprKind::Block(block) => {
            walk_inline_block(block, states, result_used, ctx);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            walk_expr(condition, states, true, out, ctx);
            walk_expr_branches_if(then_branch, else_branch, states, result_used, ctx, span);
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                walk_expr(&mut field.value, states, true, out, ctx);
            }
        }
        NirExprKind::TupleLiteral { elements } | NirExprKind::ArrayLiteral { elements } => {
            for elem in elements {
                walk_expr(elem, states, true, out, ctx);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            walk_expr(functor, states, true, out, ctx);
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                walk_expr(p, states, true, out, ctx);
            }
        }
        NirExprKind::LabeledBlock { label, block, .. } => {
            walk_labeled_block(label, block, states, ctx);
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            walk_expr(value, states, true, out, ctx);
        }
        NirExprKind::VariantTag { expr: inner } | NirExprKind::VariantTest { expr: inner, .. } => {
            walk_expr(inner, states, true, out, ctx);
        }
        NirExprKind::VariantPayload { expr: inner, .. } => {
            walk_expr(inner, states, true, out, ctx);
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            walk_expr(scrutinee, states, true, out, ctx);
            walk_expr_branches_switch(arms, default, states, ctx, span);
        }
        NirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            walk_expr(scrutinee, states, true, out, ctx);
            walk_expr_branches_match(arms, states, result_used, ctx, span);
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                walk_expr(arg, states, true, out, ctx);
            }
        }
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
        NirExprKind::Assign { .. } => {
            unreachable!("Assign handled at the top of walk_expr")
        }
        NirExprKind::Call { .. }
        | NirExprKind::MethodCall { .. }
        | NirExprKind::IndirectCall { .. } => {
            unreachable!("call exprs handled by walk_call_expr in walk_expr")
        }
    }
}

/// Walk a Block expression at the position where it's ENCLOSED inside
/// another expression. Sync stmts emitted inside the inner block stay
/// inside the block (its own stmt sequence), so `walk_block` does the
/// right thing.
fn walk_inline_block(
    block: &mut NirBlock,
    states: &mut ScalarStates,
    _result_used: bool,
    ctx: &mut WalkCtx,
) {
    walk_block(block, states, ctx);
}

/// Walk a labeled block (stmt- or expr-position), accounting for
/// labeled-break early-exits that bypass any sync the walker emits
/// inside the block.
///
/// `walk_stmt`'s `Break { label: Some(l), .. }` arm does not emit any
/// sync (an experiment with "commit-on-every-labeled-break" caused
/// unacceptable over-syncing in gale's hot loops). So a sync the
/// walker emits inside the block — e.g. `walk_nested_loop`'s
/// pre-recurse `write_back`, or a pre-call `write_back` / re-read — may
/// be skipped at runtime when a labeled break exits before reaching
/// it.
///
/// `walk_stmt`'s `Break` arm instead pushes the walker's current
/// `ScalarStates` into `ctx.label_break_states[label]`. Here we JOIN
/// the fall-through state with every observed break-state to derive
/// the post-block walker state, so subsequent code's sync decisions
/// see every per-candidate state that any runtime path can leave the
/// block with. Issue #1187 (the early-return + nested-loop bug) and
/// the #1190 regression (`FieldOnly` walker exit silently weakened to
/// `ScalarOnly` by a JOIN with `ScalarOnly` entry) both fall out of
/// this precise per-path join.
fn walk_labeled_block(
    label: &str,
    block: &mut NirBlock,
    states: &mut ScalarStates,
    ctx: &mut WalkCtx,
) {
    let prior = ctx.label_break_states.insert(label.to_string(), Vec::new());
    walk_block(block, states, ctx);
    let break_states = ctx
        .label_break_states
        .swap_remove(label)
        .unwrap_or_default();
    if let Some(p) = prior {
        ctx.label_break_states.insert(label.to_string(), p);
    }
    for i in 0..states.len() {
        let mut exits = Vec::with_capacity(1 + break_states.len());
        exits.push(states[i]);
        for bs in &break_states {
            exits.push(bs[i]);
        }
        states[i] = pick_join_target_for_candidate(&exits);
    }
}

/// Walk an If used in expression position (then/else are `NirBlocks`).
fn walk_expr_branches_if(
    then_branch: &mut NirBlock,
    else_branch: &mut Option<NirBlock>,
    states: &mut ScalarStates,
    _result_used: bool,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    let entry = states.clone();
    let mut then_states = entry.clone();
    walk_block(then_branch, &mut then_states, ctx);
    let (else_states, has_else) = if let Some(eb) = else_branch.as_mut() {
        let mut s = entry.clone();
        walk_block(eb, &mut s, ctx);
        (s, true)
    } else {
        (entry.clone(), false)
    };
    let target = pick_join_targets(&[&then_states, &else_states]);
    insert_convergence_at_block_end(then_branch, &then_states, &target, ctx, span);
    if has_else {
        let eb = else_branch.as_mut().unwrap();
        insert_convergence_at_block_end(eb, &else_states, &target, ctx, span);
    } else if states_differ(&entry, &target) {
        *else_branch = Some(build_convergence_block(&entry, &target, ctx, span));
    }
    *states = target;
}

/// Walk a Switch expression: arms are `NirBlocks`.
fn walk_expr_branches_switch(
    arms: &mut [NirBlock],
    default: &mut NirBlock,
    states: &mut ScalarStates,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    let entry = states.clone();
    let mut arm_states: Vec<ScalarStates> = Vec::new();
    for arm in arms.iter_mut() {
        let mut s = entry.clone();
        walk_block(arm, &mut s, ctx);
        arm_states.push(s);
    }
    let mut default_states = entry;
    walk_block(default, &mut default_states, ctx);
    let mut all_refs: Vec<&ScalarStates> = arm_states.iter().collect();
    all_refs.push(&default_states);
    let target = pick_join_targets(&all_refs);
    for (arm, exit) in arms.iter_mut().zip(arm_states.iter()) {
        insert_convergence_at_block_end(arm, exit, &target, ctx, span);
    }
    insert_convergence_at_block_end(default, &default_states, &target, ctx, span);
    *states = target;
}

/// Walk a Match expression: each arm.body is a `NirExpr` (NOT a `NirBlock`).
/// The guard (if any) and the body each get their own per-expression
/// sync wrapper so that pre-stmts emitted while walking the guard run
/// BEFORE the guard's evaluation (not after — which would let the
/// guard's mutations be observed before the wrap committed scalar
/// values), and pre-stmts emitted while walking the body run BEFORE
/// the body's value-producing expression.
///
/// Cross-arm guard side effects: at runtime, arm K's guard runs only
/// after arms 1..K-1's patterns matched-and-their-guards-failed. A
/// `&mut` call inside arm 1's guard mutates the field state before
/// arm 2's pattern is even tested. The walker reflects this by
/// carrying an `accumulated_pre`: the join over {entry, `arm_i_after_guard`}
/// for each prior side-effecting guard. arm K's guard and body are
/// then walked starting from `accumulated_pre`, matching the runtime
/// state at the dispatch point.
fn walk_expr_branches_match(
    arms: &mut [crate::nir::NirMatchArm],
    states: &mut ScalarStates,
    result_used: bool,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    let entry = states.clone();
    let mut accumulated_pre = entry;
    let mut arm_states: Vec<ScalarStates> = Vec::with_capacity(arms.len());
    for arm in arms.iter_mut() {
        let mut s = accumulated_pre.clone();
        // Guard (if any). Pre-stmts emitted by walking the guard must
        // run BEFORE the guard's expression evaluates; wrap the guard
        // in a Block to hold them.
        let has_guard = arm.guard.is_some();
        if let Some(guard) = &mut arm.guard {
            let mut guard_pre: Vec<NirStmt> = Vec::new();
            walk_expr(guard, &mut s, true, &mut guard_pre, ctx);
            if !guard_pre.is_empty() {
                wrap_expr_with_prefix(guard, guard_pre, span);
            }
        }
        // After the guard ran (whether matched or not), state for
        // subsequent arms includes JOIN(accumulated_pre, s).
        // Patterns themselves are side-effect-free in Wado, so the
        // pattern-mismatch path leaves state at the prior accumulated_pre.
        if has_guard {
            for i in 0..accumulated_pre.len() {
                accumulated_pre[i] = pick_join_target_for_candidate(&[accumulated_pre[i], s[i]]);
            }
        }
        // Body. Pre-stmts emitted by walking the body must run BEFORE
        // the body's value-producing expression; wrap the body in a
        // Block to hold them.
        let mut body_pre: Vec<NirStmt> = Vec::new();
        walk_expr(&mut arm.body, &mut s, result_used, &mut body_pre, ctx);
        if !body_pre.is_empty() {
            wrap_expr_with_prefix(&mut arm.body, body_pre, span);
        }
        arm_states.push(s);
    }
    let arm_refs: Vec<&ScalarStates> = arm_states.iter().collect();
    let target = pick_join_targets(&arm_refs);
    for (arm, exit) in arms.iter_mut().zip(arm_states.iter()) {
        // Emit convergence sync at the end of the arm body.
        emit_convergence_at_arm_body_end(&mut arm.body, exit, &target, ctx, span);
    }
    *states = target;
}

/// Prepend prefix stmts into an expression's evaluation by wrapping it
/// in a Block expression that holds the prefix stmts followed by the
/// original expression as its value-producing stmt. The Block carries
/// the original's type id so the surrounding context still sees the
/// same result type.
fn wrap_expr_with_prefix(expr: &mut NirExpr, prefix: Vec<NirStmt>, _span: crate::token::Span) {
    let expr_type = expr.type_id;
    let expr_span = expr.span;
    let placeholder = NirExpr::new(
        NirExprKind::Block(NirBlock::empty(expr_span)),
        expr_type,
        expr_span,
    );
    let original = std::mem::replace(expr, placeholder);
    let mut stmts = Vec::with_capacity(prefix.len() + 1);
    stmts.extend(prefix);
    stmts.push(NirStmt::new(NirStmtKind::Expr(original), expr_span));
    *expr = NirExpr::new(
        NirExprKind::Block(NirBlock::new(stmts, expr_span)),
        expr_type,
        expr_span,
    );
}

/// Insert convergence sync at the end of an arm body (which is a
/// `NirExpr`). If the body is already a Block, append the sync stmts
/// directly. Otherwise wrap the body in a Block { `body_as_stmt`; sync }
/// (when body is unit-typed) or Block { let __tmp = body; sync; __tmp }
/// (when non-unit, so the temp preserves the value across the trailing
/// sync). The temp uses the per-type pool.
fn emit_convergence_at_arm_body_end(
    body: &mut NirExpr,
    from: &ScalarStates,
    to: &ScalarStates,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    let mut sync_stmts: Vec<NirStmt> = Vec::new();
    for (i, c) in ctx.candidates.iter().enumerate() {
        if let Some(stmt) = state_transition_stmt(from[i], to[i], c, span) {
            sync_stmts.push(stmt);
        }
    }
    if sync_stmts.is_empty() {
        return;
    }
    // Existing Block bodies: append sync to the block, preserving the
    // block's trailing value if it has one (a non-unit Expr stmt).
    if let NirExprKind::Block(block) = &mut body.kind {
        append_sync_preserving_block_value(block, sync_stmts, ctx);
        return;
    }
    let body_type = body.type_id;
    let body_span = body.span;
    if body_type == TypeTable::UNIT {
        // Unit body: Block { Expr(body); sync... }
        let placeholder = NirExpr::new(
            NirExprKind::Block(NirBlock::empty(body_span)),
            body_type,
            body_span,
        );
        let original = std::mem::replace(body, placeholder);
        let mut stmts = Vec::with_capacity(1 + sync_stmts.len());
        stmts.push(NirStmt::new(NirStmtKind::Expr(original), body_span));
        stmts.extend(sync_stmts);
        *body = NirExpr::new(
            NirExprKind::Block(NirBlock::new(stmts, body_span)),
            body_type,
            body_span,
        );
        return;
    }
    // Non-unit body: capture into a pooled temp so the Block evaluates
    // to the original body's value after sync.
    let tmp_idx = ctx.alloc_temp(body_type);
    let tmp_name = ctx.temp_name(tmp_idx);
    let placeholder = NirExpr::new(
        NirExprKind::Block(NirBlock::empty(body_span)),
        body_type,
        body_span,
    );
    let original = std::mem::replace(body, placeholder);
    let mut stmts = Vec::with_capacity(2 + sync_stmts.len());
    stmts.push(NirStmt::new(
        NirStmtKind::Let {
            name: tmp_name.clone(),
            local_index: tmp_idx,
            is_mut: false,
            is_reactive: false,
            type_id: body_type,
            value: original,
            // The temp captures a fresh r-value; no deep value-copy is
            // appropriate (we just bind whatever the arm body produced).
            skip_value_copy: true,
        },
        body_span,
    ));
    stmts.extend(sync_stmts);
    stmts.push(NirStmt::new(
        NirStmtKind::Expr(NirExpr::new(
            NirExprKind::Local {
                index: tmp_idx,
                name: tmp_name,
            },
            body_type,
            body_span,
        )),
        body_span,
    ));
    *body = NirExpr::new(
        NirExprKind::Block(NirBlock::new(stmts, body_span)),
        body_type,
        body_span,
    );
    ctx.free_temp(tmp_idx, body_type);
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared sync calculation helpers (used by walk_call_expr)
// ─────────────────────────────────────────────────────────────────────────────

/// Check if an expression is an immutable ref to a local (`Unary{Ref, Local}`).
/// Also returns true for bare locals with `Ref(T)` type (after `ref_elim`).
fn is_immut_ref_arg(expr: &NirExpr, type_table: &TypeTable) -> bool {
    match &expr.kind {
        NirExprKind::Unary {
            op: NirUnaryOp::Ref,
            ..
        } => true,
        NirExprKind::Local { .. } => {
            matches!(type_table.get(expr.type_id), ResolvedType::Ref(inner)
                if !matches!(type_table.get(*inner), ResolvedType::MutRef(_)))
        }
        _ => false,
    }
}

/// For a call argument that might be a scalarized local, determine which
/// fields need syncing based on the callee's field usage cache.
fn add_sync_fields_for_arg(
    arg_expr: &NirExpr,
    func_ref: &FunctionRef,
    param_position: u32,
    candidates: &[ScalarizeCandidate],
    type_table: &TypeTable,
    cache: &FieldUsageCache,
    is_immut_ref: bool,
    result: &mut SyncFields,
) {
    let Some(local_idx) = extract_gc_local_index(arg_expr, type_table) else {
        return;
    };
    let has_scalarized = candidates.iter().any(|c| c.local_index == local_idx);
    if !has_scalarized {
        return;
    }

    let cache_key = (func_ref.module_source.clone(), func_ref.name.clone());
    let callee = cache.get(&cache_key);

    // If the callee's parameter at this position is typed `&T` (not
    // `&mut T`), the callee cannot mutate through the reference — re-read
    // is unnecessary regardless of how the caller typed the argument.
    let callee_immut = callee.is_some_and(|e| e.immut_ref_params.contains(&param_position));
    let is_immut_ref = is_immut_ref || callee_immut;

    let mut add_field = |local_idx: u32, field_idx: u32| {
        result.write_back.insert((local_idx, field_idx));
        if !is_immut_ref {
            result.re_read.insert((local_idx, field_idx));
        }
    };

    // An entry with empty `params` is treated as a cache miss — no
    // field-usage info, so be conservative.
    match callee.filter(|e| !e.params.is_empty()) {
        Some(entry) => match entry.params.get(&param_position) {
            Some(Some(field_set)) => {
                // Precise: only sync fields the callee accesses.
                for c in candidates {
                    if c.local_index == local_idx && field_set.contains(&c.field_index) {
                        add_field(c.local_index, c.field_index);
                    }
                }
            }
            Some(None) => {
                // Conservative: callee passes the struct further; all
                // fields may be touched.
                for c in candidates {
                    if c.local_index == local_idx {
                        add_field(c.local_index, c.field_index);
                    }
                }
            }
            None => {
                // Param at this position not struct-typed in callee, or
                // not tracked. No fields need syncing.
            }
        },
        None => {
            // Callee not in cache — conservative.
            for c in candidates {
                if c.local_index == local_idx {
                    add_field(c.local_index, c.field_index);
                }
            }
        }
    }
}

fn extract_gc_local_index(expr: &NirExpr, type_table: &TypeTable) -> Option<u32> {
    match &expr.kind {
        NirExprKind::Local { index, .. } => {
            if is_gc_heap_type(expr.type_id, type_table) {
                Some(*index)
            } else {
                None
            }
        }
        NirExprKind::Unary {
            op: NirUnaryOp::MutRef | NirUnaryOp::Ref,
            expr: inner,
        } => {
            if let NirExprKind::Local { index, .. } = &inner.kind {
                if is_gc_heap_type(inner.type_id, type_table) {
                    Some(*index)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn add_all_fields_for_local(
    local_idx: u32,
    candidates: &[ScalarizeCandidate],
    result: &mut IndexSet<(u32, u32)>,
) {
    for c in candidates {
        if c.local_index == local_idx {
            result.insert((c.local_index, c.field_index));
        }
    }
}

fn is_gc_heap_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Struct { .. } => true,
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            is_gc_heap_type(*inner, type_table)
        }
        _ => false,
    }
}
