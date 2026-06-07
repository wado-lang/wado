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
use crate::nir::{FunctionRef, NirFunction, NirLocal, NirUnaryOp};
use crate::nir_arena::{
    ArmData, BlockId, Body, ExprId, ExprKind, NodeRef, PatId, PatKind, StmtId, StmtKind,
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
    // Read-only field-usage scan over the arena body.
    let Some(arena) = func.body.as_ref() else {
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

    let mut cx = ParamUsageCtx {
        struct_params: &struct_param_locals,
        field_sets: &mut field_sets,
        conservative_params: &mut conservative_locals,
        type_table,
    };
    collect_param_field_usage_in_block(arena, arena.root, &mut cx);

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

/// Threaded read-only context for the parameter field-usage scan that
/// builds the `FieldUsageCache`.
struct ParamUsageCtx<'a> {
    struct_params: &'a IndexSet<u32>,
    field_sets: &'a mut IndexMap<u32, IndexSet<u32>>,
    conservative_params: &'a mut IndexSet<u32>,
    type_table: &'a TypeTable,
}

fn collect_param_field_usage_in_block(body: &Body, block: BlockId, cx: &mut ParamUsageCtx) {
    for sid in body.blocks[block].stmts.clone() {
        collect_param_field_usage_in_stmt(body, sid, cx);
    }
}

fn collect_param_field_usage_in_stmt(body: &Body, stmt: StmtId, cx: &mut ParamUsageCtx) {
    match &body.stmts[stmt].kind {
        StmtKind::Let { value, .. }
        | StmtKind::LetDestructure { value, .. }
        | StmtKind::Expr(value) => {
            collect_param_field_usage_in_expr(body, *value, cx);
        }
        StmtKind::Return { value } | StmtKind::Break { value, .. } => {
            if let Some(v) = *value {
                collect_param_field_usage_in_expr(body, v, cx);
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            collect_param_field_usage_in_expr(body, condition, cx);
            collect_param_field_usage_in_block(body, then_block, cx);
            if let Some(eb) = else_block {
                collect_param_field_usage_in_block(body, eb, cx);
            }
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            collect_param_field_usage_in_block(body, *b, cx);
        }
        StmtKind::Continue => {}
    }
}

fn collect_param_field_usage_in_expr(body: &Body, e: ExprId, cx: &mut ParamUsageCtx) {
    match &body.exprs[e].kind {
        ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            // Track field access on struct param: `self.field` or `(&mut self).field`
            let (inner, field_index) = (*inner, *field_index);
            if let Some(idx) = extract_local_index(body, inner)
                && cx.struct_params.contains(&idx)
            {
                cx.field_sets.entry(idx).or_default().insert(field_index);
                return;
            }
            collect_param_field_usage_in_expr(body, inner, cx);
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            // Check for `self.field = val` (field assignment)
            if let ExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } = &body.exprs[target].kind
            {
                let (inner, field_index) = (*inner, *field_index);
                if let Some(idx) = extract_local_index(body, inner)
                    && cx.struct_params.contains(&idx)
                {
                    cx.field_sets.entry(idx).or_default().insert(field_index);
                    collect_param_field_usage_in_expr(body, value, cx);
                    return;
                }
            }
            // Check for full local assignment `param = val` → conservative
            if let ExprKind::Local { index, .. } = &body.exprs[target].kind
                && cx.struct_params.contains(index)
            {
                cx.conservative_params.insert(*index);
            }
            collect_param_field_usage_in_expr(body, target, cx);
            collect_param_field_usage_in_expr(body, value, cx);
        }
        ExprKind::Call { args, .. } => {
            // If a struct param is passed as argument → conservative
            for aid in args.iter().map(|a| a.expr).collect::<Vec<_>>() {
                mark_if_param_passed(body, aid, cx);
                collect_param_field_usage_in_expr(body, aid, cx);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            // If a struct param is the receiver → conservative (self passed to another method)
            let receiver = *receiver;
            let arg_ids: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            mark_if_param_passed(body, receiver, cx);
            collect_param_field_usage_in_expr(body, receiver, cx);
            for aid in arg_ids {
                mark_if_param_passed(body, aid, cx);
                collect_param_field_usage_in_expr(body, aid, cx);
            }
        }
        ExprKind::IndirectCall { callee, args, .. } => {
            let callee = *callee;
            let arg_ids = args.clone();
            collect_param_field_usage_in_expr(body, callee, cx);
            for aid in arg_ids {
                mark_if_param_passed(body, aid, cx);
                collect_param_field_usage_in_expr(body, aid, cx);
            }
        }
        ExprKind::CmRawCall { args, .. } => {
            for aid in args.clone() {
                collect_param_field_usage_in_expr(body, aid, cx);
            }
        }
        ExprKind::Binary { left, right, .. } => {
            let (left, right) = (*left, *right);
            collect_param_field_usage_in_expr(body, left, cx);
            collect_param_field_usage_in_expr(body, right, cx);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Cast { expr, .. } => {
            collect_param_field_usage_in_expr(body, *expr, cx);
        }
        ExprKind::Index { expr, index } => {
            let (expr, index) = (*expr, *index);
            collect_param_field_usage_in_expr(body, expr, cx);
            collect_param_field_usage_in_expr(body, index, cx);
        }
        ExprKind::Block(block) => {
            collect_param_field_usage_in_block(body, *block, cx);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
            collect_param_field_usage_in_expr(body, condition, cx);
            collect_param_field_usage_in_block(body, then_branch, cx);
            if let Some(eb) = else_branch {
                collect_param_field_usage_in_block(body, eb, cx);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            for fid in fields.iter().map(|f| f.value).collect::<Vec<_>>() {
                collect_param_field_usage_in_expr(body, fid, cx);
            }
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            for eid in elements.clone() {
                collect_param_field_usage_in_expr(body, eid, cx);
            }
        }
        ExprKind::ClosureToCanonical { functor, .. } => {
            collect_param_field_usage_in_expr(body, *functor, cx);
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = *payload {
                collect_param_field_usage_in_expr(body, p, cx);
            }
        }
        ExprKind::LabeledBlock { block, .. } => {
            collect_param_field_usage_in_block(body, *block, cx);
        }
        ExprKind::GlobalVarSet { value, .. } => {
            collect_param_field_usage_in_expr(body, *value, cx);
        }
        ExprKind::VariantTag { expr }
        | ExprKind::VariantTest { expr, .. }
        | ExprKind::VariantPayload { expr, .. } => {
            collect_param_field_usage_in_expr(body, *expr, cx);
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let scrutinee = *scrutinee;
            let arms = arms.clone();
            let default = *default;
            collect_param_field_usage_in_expr(body, scrutinee, cx);
            for arm in arms {
                collect_param_field_usage_in_block(body, arm, cx);
            }
            collect_param_field_usage_in_block(body, default, cx);
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
        ExprKind::Match { expr, arms } => {
            let expr = *expr;
            let arms = arms.clone();
            collect_param_field_usage_in_expr(body, expr, cx);
            for arm in &arms {
                if let Some(guard) = arm.guard {
                    collect_param_field_usage_in_expr(body, guard, cx);
                }
                collect_param_field_usage_in_expr(body, arm.body, cx);
            }
        }
    }
}

/// Extract local index from a local expression or `&mut local`.
fn extract_local_index(body: &Body, e: ExprId) -> Option<u32> {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => {
            if let ExprKind::Local { index, .. } = &body.exprs[*inner].kind {
                Some(*index)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// If `e` is a struct param (or &mut of one), mark it as conservative.
fn mark_if_param_passed(body: &Body, e: ExprId, cx: &mut ParamUsageCtx) {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => {
            if cx.struct_params.contains(index)
                && is_gc_heap_type(body.exprs[e].type_id, cx.type_table)
            {
                cx.conservative_params.insert(*index);
            }
        }
        ExprKind::Unary {
            op: NirUnaryOp::MutRef | NirUnaryOp::Ref,
            expr: inner,
        } => {
            let inner = *inner;
            if let ExprKind::Local { index, .. } = &body.exprs[inner].kind
                && cx.struct_params.contains(index)
                && is_gc_heap_type(body.exprs[inner].type_id, cx.type_table)
            {
                cx.conservative_params.insert(*index);
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

/// Run the loop scalarizer on the arena loop body `lb` in place, returning
/// the pre / post statements that wrap the loop as arena statement ids.
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
    let inside_loop_locals = collect_locals_introduced_in_block(body, lb);
    let result = scalarize_loop(
        body,
        lb,
        &inside_loop_locals,
        local_count,
        locals,
        type_table,
        cache,
        analysis,
    );
    (result.pre_stmts, result.post_stmts)
}

struct ScalarizeResult {
    pre_stmts: Vec<StmtId>,
    post_stmts: Vec<StmtId>,
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

#[allow(clippy::too_many_arguments)]
fn scalarize_loop(
    body: &mut Body,
    loop_body: BlockId,
    inside_loop_locals: &IndexSet<u32>,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    cache: &FieldUsageCache,
    analysis: &HfsAnalysis<'_>,
) -> ScalarizeResult {
    // Step 1: Count field accesses (reads + writes) in the loop body
    let mut access_counts: IndexMap<(u32, u32), FieldAccessInfo> = IndexMap::default();
    count_field_accesses_in_block(body, loop_body, &mut access_counts, type_table);

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
    let mut pre_stmts: Vec<StmtId> = Vec::new();
    for c in &candidates {
        let local_type_id = if (c.local_index as usize) < locals.len() {
            locals[c.local_index as usize].type_id
        } else {
            c.type_id
        };
        let base = push_expr(
            body,
            ExprKind::Local {
                index: c.local_index,
                name: c.local_name.clone(),
            },
            local_type_id,
            span,
        );
        let field = push_expr(
            body,
            ExprKind::FieldAccess {
                expr: base,
                field_index: c.field_index,
                field_name: c.field_name.clone(),
            },
            c.type_id,
            span,
        );
        let load_stmt = push_stmt(
            body,
            StmtKind::Let {
                name: format!("__hfs_{}_{}", c.field_name, c.new_local_index),
                local_index: c.new_local_index,
                is_mut: true,
                is_reactive: false,
                type_id: c.type_id,
                value: field,
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
        body,
        loop_body,
        &candidates,
        locals,
        local_count,
        type_table,
        cache,
    );
    let post_stmts: Vec<StmtId> = Vec::new();

    ScalarizeResult {
        pre_stmts,
        post_stmts,
    }
}

/// Push a fresh expression node into the arena and return its id.
fn push_expr(body: &mut Body, kind: ExprKind, type_id: TypeId, span: crate::token::Span) -> ExprId {
    body.exprs.push(crate::nir_arena::ExprNode {
        kind,
        type_id,
        span,
    })
}

/// Push a fresh statement node into the arena and return its id.
fn push_stmt(body: &mut Body, kind: StmtKind, span: crate::token::Span) -> StmtId {
    body.stmts.push(crate::nir_arena::StmtNode { kind, span })
}

/// Build a `Local` expression node for the scalar `__hfs_F` local.
fn scalar_local_expr(body: &mut Body, c: &ScalarizeCandidate, span: crate::token::Span) -> ExprId {
    push_expr(
        body,
        ExprKind::Local {
            index: c.new_local_index,
            name: format!("__hfs_{}_{}", c.field_name, c.new_local_index),
        },
        c.type_id,
        span,
    )
}

/// Build a `local.field` field-access expression node for the candidate.
fn field_access_expr(body: &mut Body, c: &ScalarizeCandidate, span: crate::token::Span) -> ExprId {
    let base = push_expr(
        body,
        ExprKind::Local {
            index: c.local_index,
            name: c.local_name.clone(),
        },
        c.local_type_id,
        span,
    );
    push_expr(
        body,
        ExprKind::FieldAccess {
            expr: base,
            field_index: c.field_index,
            field_name: c.field_name.clone(),
        },
        c.type_id,
        span,
    )
}

/// `local.field = __hfs_F;` — commit the scalar back to the GC field.
fn make_write_back_stmt(
    body: &mut Body,
    c: &ScalarizeCandidate,
    span: crate::token::Span,
) -> StmtId {
    let target = field_access_expr(body, c, span);
    let value = scalar_local_expr(body, c, span);
    let assign = push_expr(body, ExprKind::Assign { target, value }, c.type_id, span);
    push_stmt(body, StmtKind::Expr(assign), span)
}

/// `__hfs_F = local.field;` — refresh the scalar from the GC field.
fn make_re_read_stmt(body: &mut Body, c: &ScalarizeCandidate, span: crate::token::Span) -> StmtId {
    let target = scalar_local_expr(body, c, span);
    let value = field_access_expr(body, c, span);
    let assign = push_expr(body, ExprKind::Assign { target, value }, c.type_id, span);
    push_stmt(body, StmtKind::Expr(assign), span)
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
    body: &Body,
    block: BlockId,
    counts: &mut IndexMap<(u32, u32), FieldAccessInfo>,
    type_table: &TypeTable,
) {
    for sid in body.blocks[block].stmts.clone() {
        count_field_accesses_in_stmt(body, sid, counts, type_table);
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
    body: &Body,
    stmt: StmtId,
    counts: &mut IndexMap<(u32, u32), FieldAccessInfo>,
    type_table: &TypeTable,
) {
    match &body.stmts[stmt].kind {
        StmtKind::Let { value, .. } => {
            // A Let inside a loop defines a new local variable. Unlike Assign,
            // it doesn't reassign an existing local. We don't need to mark
            // anything as fully assigned here — only process the value expression.
            //
            // However, if the value is a Local reference (e.g. `__local_47 = pos`),
            // this creates an alias. Any field modifications through the alias
            // won't be tracked by the scalarization, so mark the original as aliased.
            let value = *value;
            if let ExprKind::Local { index, .. } = &body.exprs[value].kind
                && is_gc_heap_type(body.exprs[value].type_id, type_table)
            {
                mark_local_aliased(*index, counts);
            }
            count_field_accesses_in_expr(body, value, counts, false, false, type_table);
        }
        StmtKind::Expr(expr) => {
            count_field_accesses_in_expr(body, *expr, counts, false, false, type_table);
        }
        StmtKind::Return { value } => {
            if let Some(v) = *value {
                count_field_accesses_in_expr(body, v, counts, false, false, type_table);
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            count_field_accesses_in_expr(body, condition, counts, false, false, type_table);
            count_field_accesses_in_block(body, then_block, counts, type_table);
            if let Some(eb) = else_block {
                count_field_accesses_in_block(body, eb, counts, type_table);
            }
        }
        StmtKind::Loop { body: _ } => {
            // Do NOT recurse into nested loops. Each loop level is processed
            // independently by its own scalarize_loop call in scalarize_block.
            // Recursing here would cause outer-level HFS to hoist fields that
            // are only accessed inside an inner loop, potentially before the
            // struct containing them is even initialized.
        }
        StmtKind::LabeledBlock { block, .. } => {
            count_field_accesses_in_block(body, *block, counts, type_table);
        }
        StmtKind::Break { value, .. } => {
            if let Some(v) = *value {
                count_field_accesses_in_expr(body, v, counts, false, false, type_table);
            }
        }
        StmtKind::Continue => {}
        StmtKind::LetDestructure { value, .. } => {
            count_field_accesses_in_expr(body, *value, counts, false, false, type_table);
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
    body: &Body,
    e: ExprId,
    counts: &mut IndexMap<(u32, u32), FieldAccessInfo>,
    is_assign_target: bool,
    in_call_arg: bool,
    type_table: &TypeTable,
) {
    match &body.exprs[e].kind {
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            count_field_accesses_in_expr(body, target, counts, true, false, type_table);
            count_field_accesses_in_expr(body, value, counts, false, false, type_table);
            // If target is a direct local assignment, mark it fully assigned
            if let ExprKind::Local { index, .. } = &body.exprs[target].kind {
                mark_local_fully_assigned(*index, counts);
            }
            // If value is a local reference (e.g., `other = pos`), the source is aliased
            if let ExprKind::Local { index, .. } = &body.exprs[value].kind
                && is_gc_heap_type(body.exprs[value].type_id, type_table)
            {
                mark_local_aliased(*index, counts);
            }
        }
        ExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => {
            // Match both `local.field` and `(&mut local).field` patterns.
            // The latter occurs for `&mut local.field` which NIR represents as
            // FieldAccess { expr: Unary { MutRef, Local { ... } }, field }.
            let (inner, field_index, field_name) = (*inner, *field_index, field_name.clone());
            let local_info = match &body.exprs[inner].kind {
                ExprKind::Local { index, name } => {
                    Some((*index, name.clone(), body.exprs[inner].type_id))
                }
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr: ref_inner,
                } => {
                    let ref_inner = *ref_inner;
                    if let ExprKind::Local { index, name } = &body.exprs[ref_inner].kind {
                        Some((*index, name.clone(), body.exprs[ref_inner].type_id))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some((index, name, local_type_id)) = local_info {
                let key = (index, field_index);
                let field_type_id = body.exprs[e].type_id;
                let info = counts.entry(key).or_insert_with(|| FieldAccessInfo {
                    local_name: name,
                    field_name,
                    local_type_id,
                    field_type_id,
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
                count_field_accesses_in_expr(body, inner, counts, false, false, type_table);
            }
        }
        ExprKind::Binary { left, right, .. } => {
            let (left, right) = (*left, *right);
            count_field_accesses_in_expr(body, left, counts, false, false, type_table);
            count_field_accesses_in_expr(body, right, counts, false, false, type_table);
        }
        ExprKind::Unary { op, expr } => {
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
            let (op, inner) = (*op, *expr);
            if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
                && let ExprKind::Local { index, .. } = &body.exprs[inner].kind
            {
                if !in_call_arg && is_gc_heap_type(body.exprs[inner].type_id, type_table) {
                    mark_local_aliased(*index, counts);
                }
                return;
            }
            count_field_accesses_in_expr(body, inner, counts, false, false, type_table);
        }
        ExprKind::Cast { expr, .. } => {
            count_field_accesses_in_expr(body, *expr, counts, false, false, type_table);
        }
        ExprKind::Call { args, .. } => {
            for aid in args.iter().map(|a| a.expr).collect::<Vec<_>>() {
                count_field_accesses_in_expr(body, aid, counts, false, true, type_table);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            let receiver = *receiver;
            let arg_ids: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            count_field_accesses_in_expr(body, receiver, counts, false, true, type_table);
            for aid in arg_ids {
                count_field_accesses_in_expr(body, aid, counts, false, true, type_table);
            }
        }
        ExprKind::CmRawCall { args, .. } => {
            for aid in args.clone() {
                count_field_accesses_in_expr(body, aid, counts, false, true, type_table);
            }
        }
        ExprKind::Index { expr, index } => {
            let (expr, index) = (*expr, *index);
            count_field_accesses_in_expr(body, expr, counts, false, false, type_table);
            count_field_accesses_in_expr(body, index, counts, false, false, type_table);
        }
        ExprKind::Block(block) => {
            count_field_accesses_in_block(body, *block, counts, type_table);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
            count_field_accesses_in_expr(body, condition, counts, false, false, type_table);
            count_field_accesses_in_block(body, then_branch, counts, type_table);
            if let Some(eb) = else_branch {
                count_field_accesses_in_block(body, eb, counts, type_table);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            for fid in fields.iter().map(|f| f.value).collect::<Vec<_>>() {
                count_field_accesses_in_expr(body, fid, counts, false, false, type_table);
            }
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            for eid in elements.clone() {
                count_field_accesses_in_expr(body, eid, counts, false, false, type_table);
            }
        }
        ExprKind::IndirectCall { callee, args, .. } => {
            let callee = *callee;
            let arg_ids = args.clone();
            count_field_accesses_in_expr(body, callee, counts, false, false, type_table);
            for aid in arg_ids {
                count_field_accesses_in_expr(body, aid, counts, false, true, type_table);
            }
        }
        ExprKind::ClosureToCanonical { functor, .. } => {
            count_field_accesses_in_expr(body, *functor, counts, false, false, type_table);
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = *payload {
                count_field_accesses_in_expr(body, p, counts, false, false, type_table);
            }
        }
        ExprKind::LabeledBlock { block, .. } => {
            count_field_accesses_in_block(body, *block, counts, type_table);
        }
        ExprKind::GlobalVarSet { value, .. } => {
            count_field_accesses_in_expr(body, *value, counts, false, false, type_table);
        }
        ExprKind::VariantTag { expr } | ExprKind::VariantTest { expr, .. } => {
            count_field_accesses_in_expr(body, *expr, counts, false, false, type_table);
        }
        ExprKind::VariantPayload { expr, .. } => {
            count_field_accesses_in_expr(body, *expr, counts, false, false, type_table);
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
            count_field_accesses_in_expr(body, scrutinee, counts, false, false, type_table);
            for arm in arms {
                count_field_accesses_in_block(body, arm, counts, type_table);
            }
            count_field_accesses_in_block(body, default, counts, type_table);
        }
        ExprKind::IntLiteral { .. }
        | ExprKind::FloatLiteral { .. }
        | ExprKind::BoolLiteral(_)
        | ExprKind::CharLiteral(_)
        | ExprKind::StringLiteral(_)
        | ExprKind::BytesLiteral(_)
        | ExprKind::Null
        | ExprKind::Unit
        | ExprKind::GlobalVarGet { .. }
        | ExprKind::EnumConstruct { .. } => {}
        ExprKind::Local { index, .. } => {
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
            if !in_call_arg
                && !is_assign_target
                && is_gc_heap_type(body.exprs[e].type_id, type_table)
            {
                mark_local_aliased(*index, counts);
            }
        }
        ExprKind::Match { expr, arms } => {
            let expr = *expr;
            let arms = arms.clone();
            count_field_accesses_in_expr(body, expr, counts, false, false, type_table);
            for arm in &arms {
                if let Some(guard) = arm.guard {
                    count_field_accesses_in_expr(body, guard, counts, false, false, type_table);
                }
                count_field_accesses_in_expr(body, arm.body, counts, false, false, type_table);
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

/// The per-candidate loop-entry / back-edge state.
///
/// A candidate whose field the loop body never accesses through the GC
/// reference — no call takes the local by `&`/`&mut`, and the field is never
/// `&`/`&mut`-referenced directly — is *call-clean*: its only field
/// interaction is the scalar write-back. For such a candidate
/// the field can stay `ScalarOnly` across the back-edge — the per-iteration
/// write-back the back-edge invariant would otherwise force is pure waste.
/// Its write-back is deferred to the loop's escape points (`return`,
/// `break`), which `commit_scalar_for_escape` already covers, so the field
/// is canonical for any reader after the loop. Candidates that *are* read
/// through the reference inside the loop keep the `Both` entry / body-end
/// force so the read observes an up-to-date field.
fn entry_states_for(candidates: &[ScalarizeCandidate], deferrable: &[bool]) -> ScalarStates {
    candidates
        .iter()
        .enumerate()
        .map(|(i, _)| {
            if deferrable[i] {
                CanonState::ScalarOnly
            } else {
                CanonState::Both
            }
        })
        .collect()
}

/// Determine, per candidate, whether its field is never accessed through the
/// GC reference anywhere in the loop body — neither by a call that takes the
/// candidate's local by `&`/`&mut` (such a callee could read the field), nor
/// by a direct `&`/`&mut` of the field itself (`&mut self.f`, which lets a
/// callee bypass the scalar). Plain field reads/writes don't count: the walker
/// rewrites them to scalar ops, so they never require the field to be
/// canonical.
fn compute_deferrable_candidates(
    body: &Body,
    block: BlockId,
    candidates: &[ScalarizeCandidate],
    type_table: &TypeTable,
    cache: &FieldUsageCache,
) -> Vec<bool> {
    let mut touched: IndexSet<(u32, u32)> = IndexSet::default();
    collect_call_touched_node(
        body,
        NodeRef::Block(block),
        candidates,
        type_table,
        cache,
        &mut touched,
    );
    candidates
        .iter()
        .map(|c| !touched.contains(&(c.local_index, c.field_index)))
        .collect()
}

/// Recursively visit every node under `node`, recording which `(local,
/// field)` candidates are touched through the GC reference: a call that
/// takes the local by `&`/`&mut`, or a direct `&self.f` / `&mut self.f`.
fn collect_call_touched_node(
    body: &Body,
    node: NodeRef,
    candidates: &[ScalarizeCandidate],
    type_table: &TypeTable,
    cache: &FieldUsageCache,
    touched: &mut IndexSet<(u32, u32)>,
) {
    if let NodeRef::Expr(e) = node {
        if matches!(
            &body.exprs[e].kind,
            ExprKind::Call { .. }
                | ExprKind::MethodCall { .. }
                | ExprKind::IndirectCall { .. }
                | ExprKind::CmRawCall { .. }
        ) {
            let mut sync = SyncFields {
                write_back: IndexSet::default(),
                re_read: IndexSet::default(),
            };
            accumulate_call_sync(body, e, candidates, type_table, cache, &mut sync);
            touched.extend(sync.write_back.iter().copied());
            touched.extend(sync.re_read.iter().copied());
        }
        // Taking a reference to the candidate's exact field — `&self.f` /
        // `&mut self.f` — lets a callee read or write the field through the
        // reference, bypassing the scalar. `accumulate_call_sync` only
        // tracks whole-local `&`/`&mut` args, so guard the field-ref shape
        // here: such a candidate must NOT be deferred (its field is not
        // call-clean). Conservative — at worst it forgoes the deferral.
        if let ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } = &body.exprs[e].kind
            && let ExprKind::FieldAccess {
                expr: base,
                field_index,
                ..
            } = &body.exprs[*inner].kind
            && let ExprKind::Local { index, .. } = &body.exprs[*base].kind
        {
            touched.insert((*index, *field_index));
        }
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_call_touched_node(body, c, candidates, type_table, cache, touched);
    }
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

/// Top-level entry: walk the loop body with each candidate's state
/// initialized to its loop-entry state, then converge body-exit back to that
/// same entry state so the loop's back-edge invariant holds.
///
/// Call-clean candidates (`deferrable`) enter as `ScalarOnly` and stay
/// `ScalarOnly` across the back-edge, so the body-end converge is a no-op
/// for them — the per-iteration write-back is eliminated and the field is
/// instead committed at the loop's escape points (`return` / `break`, via
/// `commit_scalar_for_escape`). All other candidates keep the classic
/// `Both` entry / body-end force-`Both` discipline.
#[allow(clippy::too_many_arguments)]
fn process_loop_body(
    body: &mut Body,
    block: BlockId,
    candidates: &[ScalarizeCandidate],
    locals: &mut Vec<NirLocal>,
    local_count: &mut u32,
    type_table: &TypeTable,
    cache: &FieldUsageCache,
) {
    let deferrable = compute_deferrable_candidates(body, block, candidates, type_table, cache);
    let entry = entry_states_for(candidates, &deferrable);
    let mut states = entry.clone();
    let mut ctx = WalkCtx {
        candidates,
        type_table,
        cache,
        locals,
        local_count,
        temp_pool: IndexMap::default(),
        label_break_states: IndexMap::default(),
    };
    walk_block(body, block, &mut states, &mut ctx);
    let span = crate::token::Span::new(0, 0, 0, 0);
    // Body-end: converge every candidate back to its loop-entry state so the
    // loop's back-edge state matches the entry established by the pre-load.
    // For deferrable (`ScalarOnly`-entry) candidates this is a no-op; for the
    // rest it forces `Both`. Insert one sync per candidate that diverged.
    let body_end = sync_to_targets(body, &mut states, &entry, &ctx, span);
    body.blocks[block].stmts.extend(body_end);
}

// ─────────────────────────────────────────────────────────────────────────────
// State transition helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Emit sync stmts to bring every candidate from its current state to
/// `target`, mutating `states` accordingly. Returns the stmts in the
/// order they should be inserted (stable wrt candidate index).
fn sync_to_target(
    body: &mut Body,
    states: &mut ScalarStates,
    target: CanonState,
    ctx: &WalkCtx,
    span: crate::token::Span,
) -> Vec<StmtId> {
    let mut out = Vec::new();
    for (i, c) in ctx.candidates.iter().enumerate() {
        if let Some(stmt) = state_transition_stmt(body, states[i], target, c, span) {
            out.push(stmt);
        }
        states[i] = target;
    }
    out
}

/// Like `sync_to_target`, but each candidate converges to its own target
/// state (used for the per-candidate back-edge invariant: deferrable
/// candidates target `ScalarOnly`, the rest `Both`).
fn sync_to_targets(
    body: &mut Body,
    states: &mut ScalarStates,
    targets: &[CanonState],
    ctx: &WalkCtx,
    span: crate::token::Span,
) -> Vec<StmtId> {
    let mut out = Vec::new();
    for (i, c) in ctx.candidates.iter().enumerate() {
        if let Some(stmt) = state_transition_stmt(body, states[i], targets[i], c, span) {
            out.push(stmt);
        }
        states[i] = targets[i];
    }
    out
}

/// Emit a single sync stmt for one candidate transitioning from `from` to
/// `to`. Returns None if no sync is needed.
fn state_transition_stmt(
    body: &mut Body,
    from: CanonState,
    to: CanonState,
    c: &ScalarizeCandidate,
    span: crate::token::Span,
) -> Option<StmtId> {
    use CanonState::{Both, FieldOnly, ScalarOnly};
    match (from, to) {
        (Both, Both) | (ScalarOnly, ScalarOnly) | (FieldOnly, FieldOnly) => None,
        // Tightening the label without crossing canonical sides — no sync.
        (Both, ScalarOnly | FieldOnly) => None,
        // ScalarOnly → Both / FieldOnly: scalar is canonical, field is
        // stale; commit scalar to field via write-back.
        (ScalarOnly, Both | FieldOnly) => Some(make_write_back_stmt(body, c, span)),
        // FieldOnly → Both / ScalarOnly: field is canonical, scalar is
        // stale; refresh scalar from field via re-read.
        (FieldOnly, Both | ScalarOnly) => Some(make_re_read_stmt(body, c, span)),
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
    body: &mut Body,
    block: BlockId,
    from: &ScalarStates,
    to: &ScalarStates,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    debug_assert_eq!(from.len(), to.len());
    let mut sync_stmts: Vec<StmtId> = Vec::new();
    for (i, c) in ctx.candidates.iter().enumerate() {
        if let Some(stmt) = state_transition_stmt(body, from[i], to[i], c, span) {
            sync_stmts.push(stmt);
        }
    }
    if sync_stmts.is_empty() {
        return;
    }
    append_sync_preserving_block_value(body, block, sync_stmts, ctx);
}

/// Append `sync_stmts` to `block`, preserving the block's trailing value
/// if it has one. If the block's last stmt is a non-unit value-producing
/// `Expr(e)`, the trailing expr is captured into a pooled temp before
/// the sync stmts run, and a final `Local(temp)` stmt restores the
/// block's value contract. Otherwise (empty block, non-Expr trailing
/// stmt, or unit-typed trailing Expr) the sync stmts are simply
/// appended.
fn append_sync_preserving_block_value(
    body: &mut Body,
    block: BlockId,
    sync_stmts: Vec<StmtId>,
    ctx: &mut WalkCtx,
) {
    let trailing_is_value = match body.blocks[block].stmts.last() {
        Some(&s) => matches!(
            &body.stmts[s].kind,
            StmtKind::Expr(e) if body.exprs[*e].type_id != TypeTable::UNIT
        ),
        None => false,
    };
    if !trailing_is_value {
        body.blocks[block].stmts.extend(sync_stmts);
        return;
    }
    let last_sid = body.blocks[block]
        .stmts
        .pop()
        .expect("checked non-empty above");
    let last_span = body.stmts[last_sid].span;
    let StmtKind::Expr(value_expr) = body.stmts[last_sid].kind else {
        unreachable!("checked Expr above")
    };
    let body_type = body.exprs[value_expr].type_id;
    let tmp_idx = ctx.alloc_temp(body_type);
    let tmp_name = ctx.temp_name(tmp_idx);
    let let_sid = push_stmt(
        body,
        StmtKind::Let {
            name: tmp_name.clone(),
            local_index: tmp_idx,
            is_mut: false,
            is_reactive: false,
            type_id: body_type,
            value: value_expr,
            skip_value_copy: true,
        },
        last_span,
    );
    body.blocks[block].stmts.push(let_sid);
    body.blocks[block].stmts.extend(sync_stmts);
    let local_e = push_expr(
        body,
        ExprKind::Local {
            index: tmp_idx,
            name: tmp_name,
        },
        body_type,
        last_span,
    );
    let trailing_sid = push_stmt(body, StmtKind::Expr(local_e), last_span);
    body.blocks[block].stmts.push(trailing_sid);
    ctx.free_temp(tmp_idx, body_type);
}

/// Build a fresh block holding only the convergence stmts from `from`
/// to `to`. Used to synthesize an else-branch when the original was
/// absent and the implicit no-op path needs sync.
fn build_convergence_block(
    body: &mut Body,
    from: &ScalarStates,
    to: &ScalarStates,
    ctx: &WalkCtx,
    span: crate::token::Span,
) -> BlockId {
    let mut stmts: Vec<StmtId> = Vec::new();
    for (i, c) in ctx.candidates.iter().enumerate() {
        if let Some(stmt) = state_transition_stmt(body, from[i], to[i], c, span) {
            stmts.push(stmt);
        }
    }
    body.blocks
        .push(crate::nir_arena::BlockNode { stmts, span })
}

/// True if any of the candidates' state in `from` differs from `to`.
fn states_differ(from: &ScalarStates, to: &ScalarStates) -> bool {
    debug_assert_eq!(from.len(), to.len());
    from.iter().zip(to.iter()).any(|(a, b)| a != b)
}

// ─────────────────────────────────────────────────────────────────────────────
// Statement walker
// ─────────────────────────────────────────────────────────────────────────────

fn walk_block(body: &mut Body, block: BlockId, states: &mut ScalarStates, ctx: &mut WalkCtx) {
    let span = crate::token::Span::new(0, 0, 0, 0);
    let stmts = std::mem::take(&mut body.blocks[block].stmts);
    let mut new_stmts: Vec<StmtId> = Vec::new();
    for sid in stmts {
        walk_stmt(body, sid, states, &mut new_stmts, ctx, span);
    }
    body.blocks[block].stmts = new_stmts;
}

fn walk_stmt(
    body: &mut Body,
    sid: StmtId,
    states: &mut ScalarStates,
    out: &mut Vec<StmtId>,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    match &body.stmts[sid].kind {
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            walk_expr(body, condition, states, true, out, ctx);
            walk_branching_block_if(body, sid, then_block, else_block, states, ctx, span);
            out.push(sid);
        }
        StmtKind::Loop { body: lb } => {
            let lb = *lb;
            walk_nested_loop(body, lb, states, out, ctx, span);
            out.push(sid);
        }
        StmtKind::LabeledBlock {
            label,
            block: inner,
        } => {
            let (label, inner) = (label.clone(), *inner);
            walk_labeled_block(body, &label, inner, states, ctx);
            out.push(sid);
        }
        StmtKind::Return { value } => {
            let value = *value;
            if let Some(v) = value {
                walk_expr(body, v, states, true, out, ctx);
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
                let return_value = value.expect("checked Some above");
                let return_type = body.exprs[return_value].type_id;
                let tmp_idx = ctx.alloc_temp(return_type);
                let tmp_name = ctx.temp_name(tmp_idx);
                let let_sid = push_stmt(
                    body,
                    StmtKind::Let {
                        name: tmp_name.clone(),
                        local_index: tmp_idx,
                        is_mut: false,
                        is_reactive: false,
                        type_id: return_type,
                        value: return_value,
                        skip_value_copy: true,
                    },
                    span,
                );
                out.push(let_sid);
                commit_scalar_for_escape(body, states, out, ctx, span);
                let local_e = push_expr(
                    body,
                    ExprKind::Local {
                        index: tmp_idx,
                        name: tmp_name,
                    },
                    return_type,
                    span,
                );
                body.stmts[sid].kind = StmtKind::Return {
                    value: Some(local_e),
                };
                out.push(sid);
                ctx.free_temp(tmp_idx, return_type);
            } else {
                commit_scalar_for_escape(body, states, out, ctx, span);
                out.push(sid);
            }
        }
        StmtKind::Break { value, label } => {
            let (value, label) = (*value, label.clone());
            if let Some(v) = value {
                walk_expr(body, v, states, true, out, ctx);
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
                if let Some(bucket) = ctx.label_break_states.get_mut(&l) {
                    bucket.push(states.clone());
                } else {
                    // A labeled break to a label not registered during this
                    // loop-body walk targets a block *enclosing* the loop, so
                    // it escapes the HFS scope just like a `return` or an
                    // unlabeled break. Commit `ScalarOnly` candidates so the
                    // field is canonical for readers after the loop — this is
                    // what keeps deferring the per-iteration back-edge
                    // write-back sound for breaks that exit the loop. (No-op
                    // when the state is already `Both`, so it cannot
                    // reintroduce the commit-on-every-labeled-break regression:
                    // those breaks target *registered* in-loop labels and take
                    // the branch above.)
                    commit_scalar_for_escape(body, states, out, ctx, span);
                }
            } else {
                commit_scalar_for_escape(body, states, out, ctx, span);
            }
            out.push(sid);
        }
        StmtKind::Continue => {
            // `continue` jumps back to the loop header, skipping the
            // body-end force-Both. Drive every candidate to `Both` so
            // the next iteration's invariant (every candidate is `Both`
            // at body entry) holds at runtime as well as at
            // compile-time.
            let sync_stmts = sync_to_target(body, states, CanonState::Both, ctx, span);
            out.extend(sync_stmts);
            out.push(sid);
        }
        StmtKind::Let { value, .. } => {
            let value = *value;
            walk_expr(body, value, states, true, out, ctx);
            out.push(sid);
        }
        StmtKind::LetDestructure { value, .. } => {
            let value = *value;
            walk_expr(body, value, states, true, out, ctx);
            out.push(sid);
        }
        StmtKind::Expr(expr) => {
            let expr = *expr;
            walk_expr(body, expr, states, false, out, ctx);
            out.push(sid);
        }
    }
}

/// Commit every scalar-canonical candidate to the field for an escape
/// path (return / non-enclosing break). After this, every candidate's
/// state has its field canonical (write-back was inserted only where
/// needed).
fn commit_scalar_for_escape(
    body: &mut Body,
    states: &mut ScalarStates,
    out: &mut Vec<StmtId>,
    ctx: &WalkCtx,
    span: crate::token::Span,
) {
    for (i, c) in ctx.candidates.iter().enumerate() {
        if states[i] == CanonState::ScalarOnly {
            let stmt = make_write_back_stmt(body, c, span);
            out.push(stmt);
            // The escape leaves the loop scope; subsequent code does not
            // observe `__hfs_F`, but recording the post-commit state keeps
            // the invariant for any downstream walker logic.
            states[i] = CanonState::Both;
        }
    }
}

/// Walk an If/IfLet's branches with cloned entry state, compute the
/// per-candidate join target, and insert convergence sync at the end of
/// each branch. `if_sid` is the `If` statement whose `else_block` is
/// updated when the implicit no-op path needs a synthesized else.
fn walk_branching_block_if(
    body: &mut Body,
    if_sid: StmtId,
    then_block: BlockId,
    else_block: Option<BlockId>,
    states: &mut ScalarStates,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    let entry = states.clone();
    let mut then_states = entry.clone();
    walk_block(body, then_block, &mut then_states, ctx);
    let (else_states, has_else) = if let Some(eb) = else_block {
        let mut s = entry.clone();
        walk_block(body, eb, &mut s, ctx);
        (s, true)
    } else {
        // The implicit no-op path leaves state unchanged from entry.
        (entry.clone(), false)
    };
    let target = pick_join_targets(&[&then_states, &else_states]);
    insert_convergence_at_block_end(body, then_block, &then_states, &target, ctx, span);
    if has_else {
        let eb = else_block.expect("has_else");
        insert_convergence_at_block_end(body, eb, &else_states, &target, ctx, span);
    } else if states_differ(&entry, &target) {
        // The implicit no-op path needs sync to converge — synthesize
        // an else-block holding the convergence stmts.
        let new_eb = build_convergence_block(body, &entry, &target, ctx, span);
        if let StmtKind::If { else_block, .. } = &mut body.stmts[if_sid].kind {
            *else_block = Some(new_eb);
        }
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
    body: &mut Body,
    block: BlockId,
    states: &mut ScalarStates,
    out: &mut Vec<StmtId>,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    // Pre-recurse: commit any `ScalarOnly` outer candidate so inner
    // reads (and a nested HFS's pre-load) observe an up-to-date field.
    for i in 0..ctx.candidates.len() {
        if states[i] == CanonState::ScalarOnly {
            let stmt = make_write_back_stmt(body, &ctx.candidates[i], span);
            out.push(stmt);
            states[i] = CanonState::Both;
        }
    }
    let entry_states = states.clone();
    let mut body_exit_states = states.clone();
    walk_block(body, block, &mut body_exit_states, ctx);
    // Snapshot for post-loop JOIN before we overwrite body_exit with
    // the body-end sync.
    let body_exit_pre_sync = body_exit_states.clone();
    // Restore back-edge invariant: drive body_exit back to entry so
    // iter 2+ starts at the same state the walker analyzed.
    for i in 0..ctx.candidates.len() {
        if let Some(stmt) = state_transition_stmt(
            body,
            body_exit_states[i],
            entry_states[i],
            &ctx.candidates[i],
            span,
        ) {
            body.blocks[block].stmts.push(stmt);
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
    body: &mut Body,
    e: ExprId,
    states: &mut ScalarStates,
    result_used: bool,
    out: &mut Vec<StmtId>,
    ctx: &mut WalkCtx,
) {
    let span = body.exprs[e].span;

    // Field assignment: `local.field = value` becomes `__hfs_F = value`.
    if let ExprKind::Assign { target, value } = &body.exprs[e].kind {
        let (target, value) = (*target, *value);
        if let Some((cand_idx, c)) = field_assign_to_candidate(body, target, ctx) {
            // Walk RHS first (state may transition through it).
            walk_expr(body, value, states, true, out, ctx);
            // Commit the assignment: rewrite target in place and update state.
            body.exprs[target].kind = ExprKind::Local {
                index: c.new_local_index,
                name: format!("__hfs_{}_{}", c.field_name, c.new_local_index),
            };
            body.exprs[target].type_id = c.type_id;
            states[cand_idx] = CanonState::ScalarOnly;
            return;
        }
        // Not a scalarized field assign. Fall through to general recursion.
        walk_expr(body, target, states, true, out, ctx);
        walk_expr(body, value, states, true, out, ctx);
        return;
    }

    // Field read: `local.field` becomes `__hfs_F`. Requires scalar canonical;
    // insert re-read at stmt level if state is FieldOnly.
    if let Some((cand_idx, c)) = field_read_to_candidate(body, e, ctx) {
        if !states[cand_idx].scalar_canonical() {
            let stmt = make_re_read_stmt(body, &c, span);
            out.push(stmt);
            states[cand_idx] = CanonState::Both;
        }
        body.exprs[e].kind = ExprKind::Local {
            index: c.new_local_index,
            name: format!("__hfs_{}_{}", c.field_name, c.new_local_index),
        };
        return;
    }

    // Call sites: handle separately.
    if matches!(
        &body.exprs[e].kind,
        ExprKind::Call { .. } | ExprKind::MethodCall { .. } | ExprKind::IndirectCall { .. }
    ) {
        walk_call_expr(body, e, states, out, ctx);
        return;
    }

    // Other expressions: recurse into sub-expressions in evaluation order,
    // letting state transitions propagate naturally.
    walk_other_expr_kinds(body, e, states, result_used, out, ctx);
}

/// Match a target expression that's a scalarized field's `local.field`
/// in an assignment. Returns the candidate index + a clone of the
/// candidate. (Cloning avoids holding a borrow on `ctx` across the
/// caller's mutations.)
fn field_assign_to_candidate(
    body: &Body,
    target: ExprId,
    ctx: &WalkCtx,
) -> Option<(usize, ScalarizeCandidate)> {
    if let ExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &body.exprs[target].kind
        && let ExprKind::Local { index, .. } = &body.exprs[*inner].kind
    {
        for (i, c) in ctx.candidates.iter().enumerate() {
            if c.local_index == *index && c.field_index == *field_index {
                return Some((i, c.clone()));
            }
        }
    }
    None
}

fn field_read_to_candidate(
    body: &Body,
    e: ExprId,
    ctx: &WalkCtx,
) -> Option<(usize, ScalarizeCandidate)> {
    if let ExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &body.exprs[e].kind
        && let ExprKind::Local { index, .. } = &body.exprs[*inner].kind
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
    body: &mut Body,
    e: ExprId,
    states: &mut ScalarStates,
    out: &mut Vec<StmtId>,
    ctx: &mut WalkCtx,
) {
    let span = body.exprs[e].span;
    // Compute the call's field effects BEFORE recursing into args (the
    // computation looks at direct args; recursion may wrap nested calls
    // and obscure the args' shape from extract_gc_local_index).
    let effects = compute_call_field_effects(body, e, ctx);
    // Recurse into args. Their walk may emit its own sync at stmt level,
    // and may transition states for fields touched by nested calls.
    recurse_into_call_args(body, e, states, out, ctx);
    // After args have been walked, commit pre-call state for THIS call.
    // (Only the call itself contributes; nested calls already handled.)
    for &i in &effects.read_required {
        if !states[i].field_canonical() {
            let c = ctx.candidates[i].clone();
            let stmt = make_write_back_stmt(body, &c, span);
            out.push(stmt);
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
    body: &mut Body,
    e: ExprId,
    states: &mut ScalarStates,
    out: &mut Vec<StmtId>,
    ctx: &mut WalkCtx,
) {
    let (receiver, callee, arg_ids): (Option<ExprId>, Option<ExprId>, Vec<ExprId>) =
        match &body.exprs[e].kind {
            ExprKind::Call { args, .. } => (None, None, args.iter().map(|a| a.expr).collect()),
            ExprKind::MethodCall { receiver, args, .. } => {
                (Some(*receiver), None, args.iter().map(|a| a.expr).collect())
            }
            ExprKind::IndirectCall { callee, args, .. } => (None, Some(*callee), args.clone()),
            _ => unreachable!("recurse_into_call_args called on non-call expr"),
        };
    if let Some(callee) = callee {
        walk_expr(body, callee, states, true, out, ctx);
    }
    if let Some(receiver) = receiver {
        walk_expr(body, receiver, states, true, out, ctx);
    }
    for aid in arg_ids {
        walk_expr(body, aid, states, true, out, ctx);
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

fn compute_call_field_effects(body: &Body, call: ExprId, ctx: &WalkCtx) -> CallFieldEffects {
    let mut sync = SyncFields {
        write_back: IndexSet::default(),
        re_read: IndexSet::default(),
    };
    accumulate_call_sync(
        body,
        call,
        ctx.candidates,
        ctx.type_table,
        ctx.cache,
        &mut sync,
    );
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
    body: &Body,
    call: ExprId,
    candidates: &[ScalarizeCandidate],
    type_table: &TypeTable,
    cache: &FieldUsageCache,
    result: &mut SyncFields,
) {
    match &body.exprs[call].kind {
        ExprKind::Call { func, args, .. } => {
            let func = func.clone();
            let arg_ids: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            for (arg_position, aid) in arg_ids.into_iter().enumerate() {
                let immut_ref = is_immut_ref_arg(body, aid, type_table);
                add_sync_fields_for_arg(
                    body,
                    aid,
                    &func,
                    arg_position as u32,
                    candidates,
                    type_table,
                    cache,
                    immut_ref,
                    result,
                );
            }
        }
        ExprKind::MethodCall {
            func,
            receiver,
            args,
            ..
        } => {
            let func = func.clone();
            let receiver = *receiver;
            let arg_ids: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            let immut_ref = is_immut_ref_arg(body, receiver, type_table);
            add_sync_fields_for_arg(
                body, receiver, &func, 0, candidates, type_table, cache, immut_ref, result,
            );
            for (arg_position, aid) in arg_ids.into_iter().enumerate() {
                let immut_ref = is_immut_ref_arg(body, aid, type_table);
                add_sync_fields_for_arg(
                    body,
                    aid,
                    &func,
                    (arg_position + 1) as u32,
                    candidates,
                    type_table,
                    cache,
                    immut_ref,
                    result,
                );
            }
        }
        ExprKind::IndirectCall { args, .. } => {
            for aid in args.clone() {
                if let Some(local_idx) = extract_gc_local_index(body, aid, type_table) {
                    add_all_fields_for_local(local_idx, candidates, &mut result.write_back);
                    if !is_immut_ref_arg(body, aid, type_table) {
                        add_all_fields_for_local(local_idx, candidates, &mut result.re_read);
                    }
                }
            }
        }
        ExprKind::CmRawCall { .. } => {
            // CmRawCall is a lowered Wasm import — its args are primitive
            // Wasm types, never struct refs.
        }
        // The remaining NirExprKind variants are not call sites. Both callers
        // (`compute_call_field_effects` via `walk_call_expr`, and
        // `compute_deferrable_candidates`' `CallTouchVisitor`) gate on the
        // `Call` / `MethodCall` / `IndirectCall` / `CmRawCall` shape before
        // dispatching here. Fail loud if a future refactor pushes a different
        // shape into here, rather than silently producing no sync.
        _ => unreachable!("accumulate_call_sync called on non-call expression"),
    }
}

/// Recurse into non-call, non-field-access expression kinds. Field
/// rewrites and call wrappings are handled by the higher-level
/// `walk_expr`; this helper just propagates the walk through structural
/// kinds.
fn walk_other_expr_kinds(
    body: &mut Body,
    e: ExprId,
    states: &mut ScalarStates,
    result_used: bool,
    out: &mut Vec<StmtId>,
    ctx: &mut WalkCtx,
) {
    let span = body.exprs[e].span;
    match &body.exprs[e].kind {
        ExprKind::FieldAccess { expr: inner, .. } => {
            let inner = *inner;
            walk_expr(body, inner, states, true, out, ctx);
        }
        ExprKind::Binary { left, right, .. } => {
            let (left, right) = (*left, *right);
            walk_expr(body, left, states, true, out, ctx);
            walk_expr(body, right, states, true, out, ctx);
        }
        ExprKind::Unary { expr: inner, .. } => {
            let inner = *inner;
            walk_expr(body, inner, states, true, out, ctx);
        }
        ExprKind::Cast { expr: inner, .. } => {
            let inner = *inner;
            walk_expr(body, inner, states, true, out, ctx);
        }
        ExprKind::Index { expr: inner, index } => {
            let (inner, index) = (*inner, *index);
            walk_expr(body, inner, states, true, out, ctx);
            walk_expr(body, index, states, true, out, ctx);
        }
        ExprKind::Block(block) => {
            let block = *block;
            walk_inline_block(body, block, states, result_used, ctx);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
            walk_expr(body, condition, states, true, out, ctx);
            walk_expr_branches_if(
                body,
                e,
                then_branch,
                else_branch,
                states,
                result_used,
                ctx,
                span,
            );
        }
        ExprKind::StructLiteral { fields, .. } => {
            for fid in fields.iter().map(|f| f.value).collect::<Vec<_>>() {
                walk_expr(body, fid, states, true, out, ctx);
            }
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            for eid in elements.clone() {
                walk_expr(body, eid, states, true, out, ctx);
            }
        }
        ExprKind::ClosureToCanonical { functor, .. } => {
            let functor = *functor;
            walk_expr(body, functor, states, true, out, ctx);
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = *payload {
                walk_expr(body, p, states, true, out, ctx);
            }
        }
        ExprKind::LabeledBlock { label, block, .. } => {
            let (label, block) = (label.clone(), *block);
            walk_labeled_block(body, &label, block, states, ctx);
        }
        ExprKind::GlobalVarSet { value, .. } => {
            let value = *value;
            walk_expr(body, value, states, true, out, ctx);
        }
        ExprKind::VariantTag { expr: inner } | ExprKind::VariantTest { expr: inner, .. } => {
            let inner = *inner;
            walk_expr(body, inner, states, true, out, ctx);
        }
        ExprKind::VariantPayload { expr: inner, .. } => {
            let inner = *inner;
            walk_expr(body, inner, states, true, out, ctx);
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let scrutinee = *scrutinee;
            let arms = arms.clone();
            let default = *default;
            walk_expr(body, scrutinee, states, true, out, ctx);
            walk_expr_branches_switch(body, &arms, default, states, ctx, span);
        }
        ExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            let scrutinee = *scrutinee;
            let arms = arms.clone();
            walk_expr(body, scrutinee, states, true, out, ctx);
            walk_expr_branches_match(body, &arms, states, result_used, ctx, span);
        }
        ExprKind::CmRawCall { args, .. } => {
            for aid in args.clone() {
                walk_expr(body, aid, states, true, out, ctx);
            }
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
        ExprKind::Assign { .. } => {
            unreachable!("Assign handled at the top of walk_expr")
        }
        ExprKind::Call { .. } | ExprKind::MethodCall { .. } | ExprKind::IndirectCall { .. } => {
            unreachable!("call exprs handled by walk_call_expr in walk_expr")
        }
    }
}

/// Walk a Block expression at the position where it's ENCLOSED inside
/// another expression. Sync stmts emitted inside the inner block stay
/// inside the block (its own stmt sequence), so `walk_block` does the
/// right thing.
fn walk_inline_block(
    body: &mut Body,
    block: BlockId,
    states: &mut ScalarStates,
    _result_used: bool,
    ctx: &mut WalkCtx,
) {
    walk_block(body, block, states, ctx);
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
    body: &mut Body,
    label: &str,
    block: BlockId,
    states: &mut ScalarStates,
    ctx: &mut WalkCtx,
) {
    let prior = ctx.label_break_states.insert(label.to_string(), Vec::new());
    walk_block(body, block, states, ctx);
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

/// Walk an If used in expression position (then/else are blocks). `if_e`
/// is the `If` expression whose `else_branch` is updated when the implicit
/// no-op path needs a synthesized else.
#[allow(clippy::too_many_arguments)]
fn walk_expr_branches_if(
    body: &mut Body,
    if_e: ExprId,
    then_branch: BlockId,
    else_branch: Option<BlockId>,
    states: &mut ScalarStates,
    _result_used: bool,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    let entry = states.clone();
    let mut then_states = entry.clone();
    walk_block(body, then_branch, &mut then_states, ctx);
    let (else_states, has_else) = if let Some(eb) = else_branch {
        let mut s = entry.clone();
        walk_block(body, eb, &mut s, ctx);
        (s, true)
    } else {
        (entry.clone(), false)
    };
    let target = pick_join_targets(&[&then_states, &else_states]);
    insert_convergence_at_block_end(body, then_branch, &then_states, &target, ctx, span);
    if has_else {
        let eb = else_branch.expect("has_else");
        insert_convergence_at_block_end(body, eb, &else_states, &target, ctx, span);
    } else if states_differ(&entry, &target) {
        let new_eb = build_convergence_block(body, &entry, &target, ctx, span);
        if let ExprKind::If { else_branch, .. } = &mut body.exprs[if_e].kind {
            *else_branch = Some(new_eb);
        }
    }
    *states = target;
}

/// Walk a Switch expression: arms and default are blocks.
fn walk_expr_branches_switch(
    body: &mut Body,
    arms: &[BlockId],
    default: BlockId,
    states: &mut ScalarStates,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    let entry = states.clone();
    let mut arm_states: Vec<ScalarStates> = Vec::new();
    for &arm in arms {
        let mut s = entry.clone();
        walk_block(body, arm, &mut s, ctx);
        arm_states.push(s);
    }
    let mut default_states = entry;
    walk_block(body, default, &mut default_states, ctx);
    let mut all_refs: Vec<&ScalarStates> = arm_states.iter().collect();
    all_refs.push(&default_states);
    let target = pick_join_targets(&all_refs);
    for (&arm, exit) in arms.iter().zip(arm_states.iter()) {
        insert_convergence_at_block_end(body, arm, exit, &target, ctx, span);
    }
    insert_convergence_at_block_end(body, default, &default_states, &target, ctx, span);
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
    body: &mut Body,
    arms: &[ArmData],
    states: &mut ScalarStates,
    result_used: bool,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    let entry = states.clone();
    let mut accumulated_pre = entry;
    let mut arm_states: Vec<ScalarStates> = Vec::with_capacity(arms.len());
    for arm in arms {
        let mut s = accumulated_pre.clone();
        // Guard (if any). Pre-stmts emitted by walking the guard must
        // run BEFORE the guard's expression evaluates; wrap the guard
        // in a Block to hold them.
        let has_guard = arm.guard.is_some();
        if let Some(guard) = arm.guard {
            let mut guard_pre: Vec<StmtId> = Vec::new();
            walk_expr(body, guard, &mut s, true, &mut guard_pre, ctx);
            if !guard_pre.is_empty() {
                wrap_expr_with_prefix(body, guard, guard_pre);
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
        let mut body_pre: Vec<StmtId> = Vec::new();
        walk_expr(body, arm.body, &mut s, result_used, &mut body_pre, ctx);
        if !body_pre.is_empty() {
            wrap_expr_with_prefix(body, arm.body, body_pre);
        }
        arm_states.push(s);
    }
    let arm_refs: Vec<&ScalarStates> = arm_states.iter().collect();
    let target = pick_join_targets(&arm_refs);
    for (arm, exit) in arms.iter().zip(arm_states.iter()) {
        // Emit convergence sync at the end of the arm body.
        emit_convergence_at_arm_body_end(body, arm.body, exit, &target, ctx, span);
    }
    *states = target;
}

/// Prepend prefix stmts into an expression's evaluation by wrapping the
/// expression node `e` in place into a Block holding the prefix stmts
/// followed by the original expression (moved to a fresh node) as its
/// value-producing stmt. The node id `e` is preserved so its parent still
/// references it; it now carries a `Block` of the same type id.
fn wrap_expr_with_prefix(body: &mut Body, e: ExprId, prefix: Vec<StmtId>) {
    let expr_type = body.exprs[e].type_id;
    let expr_span = body.exprs[e].span;
    // Move the original expression into a fresh node, leaving `e` to be
    // rewritten as the wrapping Block.
    let original_kind = std::mem::replace(&mut body.exprs[e].kind, ExprKind::Unit);
    let original = push_expr(body, original_kind, expr_type, expr_span);
    let expr_stmt = push_stmt(body, StmtKind::Expr(original), expr_span);
    let mut stmts = prefix;
    stmts.push(expr_stmt);
    let blk = body.blocks.push(crate::nir_arena::BlockNode {
        stmts,
        span: expr_span,
    });
    body.exprs[e].kind = ExprKind::Block(blk);
}

/// Insert convergence sync at the end of an arm body (the expression node
/// `arm_e`). If the body is already a Block, append the sync stmts
/// directly. Otherwise wrap the body in a Block { `body_as_stmt`; sync }
/// (when body is unit-typed) or Block { let __tmp = body; sync; __tmp }
/// (when non-unit, so the temp preserves the value across the trailing
/// sync). The temp uses the per-type pool.
fn emit_convergence_at_arm_body_end(
    body: &mut Body,
    arm_e: ExprId,
    from: &ScalarStates,
    to: &ScalarStates,
    ctx: &mut WalkCtx,
    span: crate::token::Span,
) {
    let mut sync_stmts: Vec<StmtId> = Vec::new();
    for (i, c) in ctx.candidates.iter().enumerate() {
        if let Some(stmt) = state_transition_stmt(body, from[i], to[i], c, span) {
            sync_stmts.push(stmt);
        }
    }
    if sync_stmts.is_empty() {
        return;
    }
    // Existing Block bodies: append sync to the block, preserving the
    // block's trailing value if it has one (a non-unit Expr stmt).
    if let ExprKind::Block(block) = body.exprs[arm_e].kind {
        append_sync_preserving_block_value(body, block, sync_stmts, ctx);
        return;
    }
    let body_type = body.exprs[arm_e].type_id;
    let body_span = body.exprs[arm_e].span;
    if body_type == TypeTable::UNIT {
        // Unit body: Block { Expr(body); sync... }
        let original_kind = std::mem::replace(&mut body.exprs[arm_e].kind, ExprKind::Unit);
        let original = push_expr(body, original_kind, body_type, body_span);
        let expr_stmt = push_stmt(body, StmtKind::Expr(original), body_span);
        let mut stmts = Vec::with_capacity(1 + sync_stmts.len());
        stmts.push(expr_stmt);
        stmts.extend(sync_stmts);
        let blk = body.blocks.push(crate::nir_arena::BlockNode {
            stmts,
            span: body_span,
        });
        body.exprs[arm_e].kind = ExprKind::Block(blk);
        return;
    }
    // Non-unit body: capture into a pooled temp so the Block evaluates
    // to the original body's value after sync.
    let tmp_idx = ctx.alloc_temp(body_type);
    let tmp_name = ctx.temp_name(tmp_idx);
    let original_kind = std::mem::replace(&mut body.exprs[arm_e].kind, ExprKind::Unit);
    let original = push_expr(body, original_kind, body_type, body_span);
    let let_stmt = push_stmt(
        body,
        StmtKind::Let {
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
    );
    let mut stmts = Vec::with_capacity(2 + sync_stmts.len());
    stmts.push(let_stmt);
    stmts.extend(sync_stmts);
    let local_e = push_expr(
        body,
        ExprKind::Local {
            index: tmp_idx,
            name: tmp_name,
        },
        body_type,
        body_span,
    );
    let trailing_stmt = push_stmt(body, StmtKind::Expr(local_e), body_span);
    stmts.push(trailing_stmt);
    let blk = body.blocks.push(crate::nir_arena::BlockNode {
        stmts,
        span: body_span,
    });
    body.exprs[arm_e].kind = ExprKind::Block(blk);
    ctx.free_temp(tmp_idx, body_type);
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared sync calculation helpers (used by walk_call_expr)
// ─────────────────────────────────────────────────────────────────────────────

/// Check if an expression is an immutable ref to a local (`Unary{Ref, Local}`).
/// Also returns true for bare locals with `Ref(T)` type (after `ref_elim`).
fn is_immut_ref_arg(body: &Body, e: ExprId, type_table: &TypeTable) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Unary {
            op: NirUnaryOp::Ref,
            ..
        } => true,
        ExprKind::Local { .. } => {
            matches!(type_table.get(body.exprs[e].type_id), ResolvedType::Ref(inner)
                if !matches!(type_table.get(*inner), ResolvedType::MutRef(_)))
        }
        _ => false,
    }
}

/// For a call argument that might be a scalarized local, determine which
/// fields need syncing based on the callee's field usage cache.
#[allow(clippy::too_many_arguments)]
fn add_sync_fields_for_arg(
    body: &Body,
    arg_expr: ExprId,
    func_ref: &FunctionRef,
    param_position: u32,
    candidates: &[ScalarizeCandidate],
    type_table: &TypeTable,
    cache: &FieldUsageCache,
    is_immut_ref: bool,
    result: &mut SyncFields,
) {
    let Some(local_idx) = extract_gc_local_index(body, arg_expr, type_table) else {
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

fn extract_gc_local_index(body: &Body, e: ExprId, type_table: &TypeTable) -> Option<u32> {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => {
            if is_gc_heap_type(body.exprs[e].type_id, type_table) {
                Some(*index)
            } else {
                None
            }
        }
        ExprKind::Unary {
            op: NirUnaryOp::MutRef | NirUnaryOp::Ref,
            expr: inner,
        } => {
            let inner = *inner;
            if let ExprKind::Local { index, .. } = &body.exprs[inner].kind {
                if is_gc_heap_type(body.exprs[inner].type_id, type_table) {
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
