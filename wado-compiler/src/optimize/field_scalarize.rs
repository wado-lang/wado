//! Hot Field Scalarization for Wado TIR
//!
//! This pass promotes frequently-accessed struct fields to local variables within loops.
//! Unlike LICM which handles loop-invariant fields, this pass handles fields that are
//! both read and written inside the loop body.
//!
//! For a field `obj.field` that is accessed N times in a loop:
//! 1. Create a mutable local `_hfs_field_N` before the loop
//! 2. Load: `_hfs_field_N = obj.field`
//! 3. Replace all `obj.field` reads with `_hfs_field_N`
//! 4. Replace all `obj.field = val` writes with `_hfs_field_N = val`
//! 5. Insert write-back `obj.field = _hfs_field_N` before function calls that receive `obj`
//!    (only for fields the callee actually accesses)
//! 6. Insert re-read `_hfs_field_N = obj.field` after function calls that receive `obj`
//!    (only for fields the callee actually accesses)
//! 7. Insert final write-back `obj.field = _hfs_field_N` after the loop
//!
//! This converts GC struct.get/struct.set into wasm local.get/local.set in hot loops.
//!
//! ## Field-Selective Sync
//!
//! When a scalarized struct is passed to a function call, only fields that the callee
//! actually accesses need to be written back before and re-read after the call.
//! A pre-computed `FieldUsageCache` maps each function to the set of fields it accesses
//! on each struct-typed parameter. If the callee cannot be resolved or passes the struct
//! transitively to another unknown call, all fields are conservatively synced.

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::ModuleSource;
use crate::tir::{
    FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal, TirPattern,
    TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};

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

pub fn scalarize_hot_fields(project: &mut FlatPackage) -> bool {
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

fn build_field_usage_cache(project: &FlatPackage) -> FieldUsageCache {
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
    func: &TirFunction,
    type_table: &TypeTable,
) -> IndexMap<u32, ParamFieldUsage> {
    let Some(ref body) = func.body else {
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
    block: &TirBlock,
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
    stmt: &TirStmt,
    struct_params: &IndexSet<u32>,
    field_sets: &mut IndexMap<u32, IndexSet<u32>>,
    conservative_params: &mut IndexSet<u32>,
    type_table: &TypeTable,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            collect_param_field_usage_in_expr(
                value,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirStmtKind::Expr(expr) => {
            collect_param_field_usage_in_expr(
                expr,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirStmtKind::Return { value } => {
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
        TirStmtKind::If {
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
        TirStmtKind::Loop { body } => {
            collect_param_field_usage_in_block(
                body,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_param_field_usage_in_block(
                block,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_param_field_usage_in_expr(
                scrutinee,
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
        TirStmtKind::Break { value, .. } => {
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
        TirStmtKind::Continue
        | TirStmtKind::TaskReturn { .. }
        | TirStmtKind::VariadicForOf { .. } => {}
        TirStmtKind::LetDestructure { value, .. } => {
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
    expr: &TirExpr,
    struct_params: &IndexSet<u32>,
    field_sets: &mut IndexMap<u32, IndexSet<u32>>,
    conservative_params: &mut IndexSet<u32>,
    type_table: &TypeTable,
) {
    match &expr.kind {
        TirExprKind::FieldAccess {
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
        TirExprKind::Assign { target, value } => {
            // Check for `self.field = val` (field assignment)
            if let TirExprKind::FieldAccess {
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
            if let TirExprKind::Local { index, .. } = &target.kind
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
        TirExprKind::Call { args, .. } => {
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
        TirExprKind::MethodCall { receiver, args, .. } => {
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
        TirExprKind::IndirectCall { callee, args, .. } => {
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
        TirExprKind::CmRawCall { args, .. } => {
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
        TirExprKind::Binary { left, right, .. } => {
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
        TirExprKind::Unary { expr, .. } => {
            collect_param_field_usage_in_expr(
                expr,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirExprKind::Cast { expr, .. } => {
            collect_param_field_usage_in_expr(
                expr,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirExprKind::Index { expr, index } => {
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
        TirExprKind::Block(block) => {
            collect_param_field_usage_in_block(
                block,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirExprKind::If {
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
        TirExprKind::StructLiteral { fields, .. } => {
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
        TirExprKind::TupleLiteral { elements } => {
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
        TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            collect_param_field_usage_in_expr(
                inner,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_param_field_usage_in_expr(
                functor,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirExprKind::Closure { body, .. } => {
            collect_param_field_usage_in_expr(
                body,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirExprKind::VariantConstruct { payload, .. } => {
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
        TirExprKind::LabeledBlock { block, .. } => {
            collect_param_field_usage_in_block(
                block,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_param_field_usage_in_expr(
                value,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
            collect_param_field_usage_in_expr(
                expr,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirExprKind::VariantPayload { expr, .. } => {
            collect_param_field_usage_in_expr(
                expr,
                struct_params,
                field_sets,
                conservative_params,
                type_table,
            );
        }
        TirExprKind::Switch {
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
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
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
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::Match { expr, arms } => {
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
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Extract local index from a local expression or `&mut local`.
fn extract_local_index(expr: &TirExpr) -> Option<u32> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        TirExprKind::Unary {
            op: TirUnaryOp::MutRef,
            expr: inner,
        } => {
            if let TirExprKind::Local { index, .. } = &inner.kind {
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
    expr: &TirExpr,
    struct_params: &IndexSet<u32>,
    conservative_params: &mut IndexSet<u32>,
    type_table: &TypeTable,
) {
    match &expr.kind {
        TirExprKind::Local { index, .. } => {
            if struct_params.contains(index) && is_gc_heap_type(expr.type_id, type_table) {
                conservative_params.insert(*index);
            }
        }
        TirExprKind::Unary {
            op: TirUnaryOp::MutRef | TirUnaryOp::Ref,
            expr: inner,
        } => {
            if let TirExprKind::Local { index, .. } = &inner.kind
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
    func: &mut TirFunction,
    type_table: &TypeTable,
    cache: &FieldUsageCache,
) -> bool {
    let Some(ref mut body) = func.body else {
        return false;
    };
    let mut local_count = func.local_count;
    let mut locals = func.locals.clone();
    let changed = scalarize_block(body, &mut local_count, &mut locals, type_table, cache);
    func.local_count = local_count;
    func.locals = locals;
    changed
}

fn scalarize_block(
    block: &mut TirBlock,
    local_count: &mut u32,
    locals: &mut Vec<TirLocal>,
    type_table: &TypeTable,
    cache: &FieldUsageCache,
) -> bool {
    let mut changed = false;
    let mut new_stmts = Vec::new();

    for mut stmt in std::mem::take(&mut block.stmts) {
        match &mut stmt.kind {
            TirStmtKind::Loop { body } => {
                // Recurse into inner blocks/loops first.
                changed |= scalarize_block(body, local_count, locals, type_table, cache);
                // Try to scalarize hot fields at this loop level.
                let result = scalarize_loop(body, local_count, locals, type_table, cache);
                if result.pre_stmts.is_empty() {
                    new_stmts.push(stmt);
                } else {
                    changed = true;
                    new_stmts.extend(result.pre_stmts);
                    new_stmts.push(stmt);
                    new_stmts.extend(result.post_stmts);
                }
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                changed |= scalarize_block(then_block, local_count, locals, type_table, cache);
                if let Some(eb) = else_block {
                    changed |= scalarize_block(eb, local_count, locals, type_table, cache);
                }
                new_stmts.push(stmt);
            }
            TirStmtKind::LabeledBlock { block: inner, .. } => {
                changed |= scalarize_block(inner, local_count, locals, type_table, cache);
                new_stmts.push(stmt);
            }
            TirStmtKind::IfLet {
                then_block,
                else_block,
                ..
            } => {
                changed |= scalarize_block(then_block, local_count, locals, type_table, cache);
                if let Some(eb) = else_block {
                    changed |= scalarize_block(eb, local_count, locals, type_table, cache);
                }
                new_stmts.push(stmt);
            }
            _ => {
                new_stmts.push(stmt);
            }
        }
    }

    block.stmts = new_stmts;
    changed
}

struct ScalarizeResult {
    pre_stmts: Vec<TirStmt>,
    post_stmts: Vec<TirStmt>,
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
    loop_body: &mut TirBlock,
    local_count: &mut u32,
    locals: &mut Vec<TirLocal>,
    type_table: &TypeTable,
    cache: &FieldUsageCache,
) -> ScalarizeResult {
    // Step 1: Count field accesses (reads + writes) in the loop body
    let mut access_counts: IndexMap<(u32, u32), FieldAccessInfo> = IndexMap::default();
    count_field_accesses_in_block(loop_body, &mut access_counts, type_table);

    // Step 1b: Collect locals introduced inside the loop body. These cannot
    // be safely scalarized at this loop level — their owning storage (the
    // GC struct ref) is unbound at the loop's pre-header where the
    // hoisted `let _hfs_field = local.field;` would run, producing a
    // null-reference trap. Locals declared in the parent scope (i.e., not
    // listed here) are fine to scalarize.
    let inside_loop_locals = collect_locals_introduced_in_block(loop_body);

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
        locals.push(TirLocal {
            name: format!("_hfs_{}_{}", info.field_name, next_local),
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

        let load_stmt = TirStmt::new(
            TirStmtKind::Let {
                name: format!("_hfs_{}_{}", c.field_name, c.new_local_index),
                local_index: c.new_local_index,
                is_mut: true,
                is_reactive: false,
                type_id: c.type_id,
                value: TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(TirExpr::new(
                            TirExprKind::Local {
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

    // Step 4: Create post-loop write-back statements
    let mut post_stmts = Vec::new();
    for c in &candidates {
        post_stmts.push(make_write_back_stmt(c, span));
    }

    // Step 5: Replace field accesses in the loop body, and insert write-back/re-read
    // around function calls (only for fields the callee actually accesses).
    // Track labels that lexically ENCLOSE the current position inside the loop body:
    // a break targeting one of those stays in the HFS scope (no write-back needed),
    // while a break to any other label escapes and requires a write-back. Using a
    // dynamic set matches TIR break resolution, which always binds to the nearest
    // enclosing label — so a sibling LabeledBlock with the same name (e.g. one
    // brought in by inlining) never shadows an outer break target.
    let enclosing_labels = IndexSet::default();
    replace_in_block(
        loop_body,
        &candidates,
        locals,
        type_table,
        cache,
        &enclosing_labels,
        0,
    );

    ScalarizeResult {
        pre_stmts,
        post_stmts,
    }
}

fn make_write_back_stmt(c: &ScalarizeCandidate, span: crate::token::Span) -> TirStmt {
    TirStmt::new(
        TirStmtKind::Expr(TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(TirExpr::new(
                            TirExprKind::Local {
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
                value: Box::new(TirExpr::new(
                    TirExprKind::Local {
                        index: c.new_local_index,
                        name: format!("_hfs_{}_{}", c.field_name, c.new_local_index),
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

fn make_re_read_stmt(c: &ScalarizeCandidate, span: crate::token::Span) -> TirStmt {
    TirStmt::new(
        TirStmtKind::Expr(TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(TirExpr::new(
                    TirExprKind::Local {
                        index: c.new_local_index,
                        name: format!("_hfs_{}_{}", c.field_name, c.new_local_index),
                    },
                    c.type_id,
                    span,
                )),
                value: Box::new(TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(TirExpr::new(
                            TirExprKind::Local {
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
    block: &TirBlock,
    counts: &mut IndexMap<(u32, u32), FieldAccessInfo>,
    type_table: &TypeTable,
) {
    for stmt in &block.stmts {
        count_field_accesses_in_stmt(stmt, counts, type_table);
    }
}

/// Collects every local index introduced (by `Let`, `LetDestructure`, match-
/// arm patterns, etc.) anywhere inside `block`, walking through nested
/// arms / blocks but NOT through nested `Loop` bodies (those are processed
/// independently by their own `scalarize_loop` call). Used to filter out
/// locals whose owning storage is unbound at the loop's pre-header — those
/// locals must not be scalarized at this loop level, otherwise the hoisted
/// `let _hfs_field = local.field;` null-derefs at runtime.
fn collect_locals_introduced_in_block(block: &TirBlock) -> IndexSet<u32> {
    struct Collector {
        out: IndexSet<u32>,
    }

    impl Collector {
        fn visit_pattern(&mut self, pattern: &TirPattern) {
            match pattern {
                TirPattern::Wildcard
                | TirPattern::Literal(_)
                | TirPattern::Enum { .. }
                | TirPattern::ConstantValue { .. }
                | TirPattern::Range { .. } => {}
                TirPattern::Binding { local_index, .. } => {
                    self.out.insert(*local_index);
                }
                TirPattern::Tuple(patterns, _) | TirPattern::Or(patterns) => {
                    for p in patterns {
                        self.visit_pattern(p);
                    }
                }
                TirPattern::Variant { bindings, .. } => {
                    for p in bindings {
                        self.visit_pattern(p);
                    }
                }
                TirPattern::Struct { fields, .. } => {
                    for f in fields {
                        self.visit_pattern(&f.pattern);
                    }
                }
            }
        }
    }

    impl crate::tir_visitor::TirRefVisitor for Collector {
        fn visit_stmt(&mut self, stmt: &TirStmt) {
            match &stmt.kind {
                TirStmtKind::Let { local_index, .. } => {
                    self.out.insert(*local_index);
                }
                TirStmtKind::LetDestructure { pattern, .. } => {
                    self.visit_pattern(pattern);
                }
                TirStmtKind::IfLet { pattern, .. } => {
                    self.visit_pattern(pattern);
                }
                TirStmtKind::Loop { .. } => {
                    // Skip nested loops: their locals are processed by their
                    // own scalarize_loop pass and are not visible at *this*
                    // loop's pre-header anyway.
                    return;
                }
                _ => {}
            }
            self.walk_stmt(stmt);
        }

        fn visit_expr(&mut self, expr: &TirExpr) {
            if let TirExprKind::Match { arms, .. } = &expr.kind {
                for arm in arms {
                    self.visit_pattern(&arm.pattern);
                }
            }
            self.walk_expr(expr);
        }
    }

    let mut c = Collector {
        out: IndexSet::default(),
    };
    crate::tir_visitor::TirRefVisitor::visit_block(&mut c, block);
    c.out
}

fn count_field_accesses_in_stmt(
    stmt: &TirStmt,
    counts: &mut IndexMap<(u32, u32), FieldAccessInfo>,
    type_table: &TypeTable,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            // A Let inside a loop defines a new local variable. Unlike Assign,
            // it doesn't reassign an existing local. We don't need to mark
            // anything as fully assigned here — only process the value expression.
            //
            // However, if the value is a Local reference (e.g. `__local_47 = pos`),
            // this creates an alias. Any field modifications through the alias
            // won't be tracked by the scalarization, so mark the original as aliased.
            if let TirExprKind::Local { index, .. } = &value.kind
                && is_gc_heap_type(value.type_id, type_table)
            {
                mark_local_aliased(*index, counts);
            }
            count_field_accesses_in_expr(value, counts, false, type_table);
        }
        TirStmtKind::Expr(expr) => {
            count_field_accesses_in_expr(expr, counts, false, type_table);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                count_field_accesses_in_expr(v, counts, false, type_table);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            count_field_accesses_in_expr(condition, counts, false, type_table);
            count_field_accesses_in_block(then_block, counts, type_table);
            if let Some(eb) = else_block {
                count_field_accesses_in_block(eb, counts, type_table);
            }
        }
        TirStmtKind::Loop { body: _ } => {
            // Do NOT recurse into nested loops. Each loop level is processed
            // independently by its own scalarize_loop call in scalarize_block.
            // Recursing here would cause outer-level HFS to hoist fields that
            // are only accessed inside an inner loop, potentially before the
            // struct containing them is even initialized.
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            count_field_accesses_in_block(block, counts, type_table);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            count_field_accesses_in_expr(scrutinee, counts, false, type_table);
            count_field_accesses_in_block(then_block, counts, type_table);
            if let Some(eb) = else_block {
                count_field_accesses_in_block(eb, counts, type_table);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                count_field_accesses_in_expr(v, counts, false, type_table);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetDestructure { value, .. } => {
            count_field_accesses_in_expr(value, counts, false, type_table);
        }
        TirStmtKind::TaskReturn { .. } => {}
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn count_field_accesses_in_expr(
    expr: &TirExpr,
    counts: &mut IndexMap<(u32, u32), FieldAccessInfo>,
    is_assign_target: bool,
    type_table: &TypeTable,
) {
    match &expr.kind {
        TirExprKind::Assign { target, value } => {
            count_field_accesses_in_expr(target, counts, true, type_table);
            count_field_accesses_in_expr(value, counts, false, type_table);
            // If target is a direct local assignment, mark it fully assigned
            if let TirExprKind::Local { index, .. } = &target.kind {
                mark_local_fully_assigned(*index, counts);
            }
            // If value is a local reference (e.g., `other = pos`), the source is aliased
            if let TirExprKind::Local { index, .. } = &value.kind
                && is_gc_heap_type(value.type_id, type_table)
            {
                mark_local_aliased(*index, counts);
            }
        }
        TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => {
            // Match both `local.field` and `(&mut local).field` patterns.
            // The latter occurs for `&mut local.field` which TIR represents as
            // FieldAccess { expr: Unary { MutRef, Local { ... } }, field }.
            let local_info = match &inner.kind {
                TirExprKind::Local { index, name } => Some((*index, name.clone(), inner.type_id)),
                TirExprKind::Unary {
                    op: TirUnaryOp::MutRef,
                    expr: ref_inner,
                } => {
                    if let TirExprKind::Local { index, name } = &ref_inner.kind {
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
                count_field_accesses_in_expr(inner, counts, false, type_table);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            count_field_accesses_in_expr(left, counts, false, type_table);
            count_field_accesses_in_expr(right, counts, false, type_table);
        }
        TirExprKind::Unary { op: _, expr } => {
            // &mut local does NOT mark the local as fully assigned.
            // Taking a mutable reference for method calls or passing to functions
            // is handled by write-back/re-read around calls via call_passed_locals.
            // Only direct assignment (local = value) should mark as fully assigned.
            count_field_accesses_in_expr(expr, counts, false, type_table);
        }
        TirExprKind::Cast { expr, .. } => {
            count_field_accesses_in_expr(expr, counts, false, type_table);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                count_field_accesses_in_expr(&arg.expr, counts, false, type_table);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            count_field_accesses_in_expr(receiver, counts, false, type_table);
            for arg in args {
                count_field_accesses_in_expr(&arg.expr, counts, false, type_table);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                count_field_accesses_in_expr(arg, counts, false, type_table);
            }
        }
        TirExprKind::Index { expr, index } => {
            count_field_accesses_in_expr(expr, counts, false, type_table);
            count_field_accesses_in_expr(index, counts, false, type_table);
        }
        TirExprKind::Block(block) => {
            count_field_accesses_in_block(block, counts, type_table);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            count_field_accesses_in_expr(condition, counts, false, type_table);
            count_field_accesses_in_block(then_branch, counts, type_table);
            if let Some(eb) = else_branch {
                count_field_accesses_in_block(eb, counts, type_table);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                count_field_accesses_in_expr(&field.value, counts, false, type_table);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                count_field_accesses_in_expr(elem, counts, false, type_table);
            }
        }
        TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            count_field_accesses_in_expr(inner, counts, false, type_table);
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            count_field_accesses_in_expr(callee, counts, false, type_table);
            for arg in args {
                count_field_accesses_in_expr(arg, counts, false, type_table);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            count_field_accesses_in_expr(functor, counts, false, type_table);
        }
        TirExprKind::Closure { body, .. } => {
            count_field_accesses_in_expr(body, counts, false, type_table);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                count_field_accesses_in_expr(p, counts, false, type_table);
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            count_field_accesses_in_block(block, counts, type_table);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            count_field_accesses_in_expr(value, counts, false, type_table);
        }
        TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
            count_field_accesses_in_expr(expr, counts, false, type_table);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            count_field_accesses_in_expr(expr, counts, false, type_table);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            count_field_accesses_in_expr(scrutinee, counts, false, type_table);
            for arm in arms {
                count_field_accesses_in_block(arm, counts, type_table);
            }
            count_field_accesses_in_block(default, counts, type_table);
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
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::Match { expr, arms } => {
            count_field_accesses_in_expr(expr, counts, false, type_table);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    count_field_accesses_in_expr(guard, counts, false, type_table);
                }
                count_field_accesses_in_expr(&arm.body, counts, false, type_table);
            }
        }
        TirExprKind::TemplateString { .. } => {}
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
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
// Replacement pass: replace field accesses and insert field-selective sync
// ─────────────────────────────────────────────────────────────────────────────

/// Context needed to insert write-back/re-read inside `LabeledBlock` expressions.
struct ReplaceCtx<'a> {
    locals: &'a [TirLocal],
    type_table: &'a TypeTable,
    cache: &'a FieldUsageCache,
    /// Labels of `LabeledBlock`s that lexically enclose the current position.
    /// A `break` whose target is in this set stays within the HFS scope; any
    /// other target escapes and requires a write-back before the break.
    enclosing_labels: &'a IndexSet<String>,
    loop_depth: usize,
}

fn replace_in_block(
    block: &mut TirBlock,
    candidates: &[ScalarizeCandidate],
    locals: &[TirLocal],
    type_table: &TypeTable,
    cache: &FieldUsageCache,
    enclosing_labels: &IndexSet<String>,
    loop_depth: usize,
) {
    let ctx = ReplaceCtx {
        locals,
        type_table,
        cache,
        enclosing_labels,
        loop_depth,
    };
    let span = crate::token::Span::new(0, 0, 0, 0);
    let mut new_stmts = Vec::new();

    for mut stmt in std::mem::take(&mut block.stmts) {
        // For compound statements (if/loop/labeled-block), recurse into their
        // inner blocks to place write-back/re-read as close to the actual calls
        // as possible, rather than wrapping the entire compound statement.
        match &mut stmt.kind {
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                // Calls inside the condition can modify scalarized fields (e.g.
                // `if !self.expect_byte(b)` where expect_byte mutates self.pos).
                // Insert write-back before the If and re-read after it.
                let mut cond_sync = SyncFields {
                    write_back: IndexSet::default(),
                    re_read: IndexSet::default(),
                };
                compute_sync_fields_in_expr(
                    condition,
                    candidates,
                    type_table,
                    cache,
                    &mut cond_sync,
                );
                for c in candidates {
                    if cond_sync
                        .write_back
                        .contains(&(c.local_index, c.field_index))
                    {
                        new_stmts.push(make_write_back_stmt(c, span));
                    }
                }
                replace_in_expr(condition, candidates, &ctx);
                replace_in_block(
                    then_block,
                    candidates,
                    locals,
                    type_table,
                    cache,
                    enclosing_labels,
                    loop_depth,
                );
                if let Some(eb) = else_block {
                    replace_in_block(
                        eb,
                        candidates,
                        locals,
                        type_table,
                        cache,
                        enclosing_labels,
                        loop_depth,
                    );
                }
                new_stmts.push(stmt);
                for c in candidates {
                    if cond_sync.re_read.contains(&(c.local_index, c.field_index)) {
                        new_stmts.push(make_re_read_stmt(c, span));
                    }
                }
                continue;
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                // Same as If: calls in the scrutinee can modify scalarized fields.
                let mut scrut_sync = SyncFields {
                    write_back: IndexSet::default(),
                    re_read: IndexSet::default(),
                };
                compute_sync_fields_in_expr(
                    scrutinee,
                    candidates,
                    type_table,
                    cache,
                    &mut scrut_sync,
                );
                for c in candidates {
                    if scrut_sync
                        .write_back
                        .contains(&(c.local_index, c.field_index))
                    {
                        new_stmts.push(make_write_back_stmt(c, span));
                    }
                }
                replace_in_expr(scrutinee, candidates, &ctx);
                replace_in_block(
                    then_block,
                    candidates,
                    locals,
                    type_table,
                    cache,
                    enclosing_labels,
                    loop_depth,
                );
                if let Some(eb) = else_block {
                    replace_in_block(
                        eb,
                        candidates,
                        locals,
                        type_table,
                        cache,
                        enclosing_labels,
                        loop_depth,
                    );
                }
                new_stmts.push(stmt);
                for c in candidates {
                    if scrut_sync.re_read.contains(&(c.local_index, c.field_index)) {
                        new_stmts.push(make_re_read_stmt(c, span));
                    }
                }
                continue;
            }
            TirStmtKind::Loop { body } => {
                replace_in_block(
                    body,
                    candidates,
                    locals,
                    type_table,
                    cache,
                    enclosing_labels,
                    loop_depth + 1,
                );
                new_stmts.push(stmt);
                continue;
            }
            TirStmtKind::LabeledBlock {
                label,
                block: inner,
            } => {
                let mut extended = enclosing_labels.clone();
                extended.insert(label.clone());
                replace_in_block(
                    inner, candidates, locals, type_table, cache, &extended, loop_depth,
                );
                new_stmts.push(stmt);
                continue;
            }
            TirStmtKind::Expr(expr) if matches!(expr.kind, TirExprKind::Switch { .. }) => {
                // Switch arms are blocks that may contain function calls needing
                // write-back/re-read sync. We must recurse with replace_in_block
                // (not replace_in_block_stmts) so that sync is inserted around
                // calls inside the arms.
                if let TirExprKind::Switch {
                    scrutinee,
                    arms,
                    default,
                    ..
                } = &mut expr.kind
                {
                    replace_in_expr(scrutinee, candidates, &ctx);
                    for arm in arms {
                        replace_in_block(
                            arm,
                            candidates,
                            locals,
                            type_table,
                            cache,
                            enclosing_labels,
                            loop_depth,
                        );
                    }
                    replace_in_block(
                        default,
                        candidates,
                        locals,
                        type_table,
                        cache,
                        enclosing_labels,
                        loop_depth,
                    );
                }
                new_stmts.push(stmt);
                continue;
            }
            _ => {}
        }

        // Insert write-back before return/break statements that escape the HFS
        // scope. A `break` whose target is a label that lexically ENCLOSES this
        // statement stays within the HFS scope (the block's own exit handles
        // write-back). Any other `break` target escapes, so we must write back
        // first. Matching by lexical enclosure (not by name-anywhere-in-scope)
        // matches TIR break resolution and is required when inlining brings in
        // a sibling labeled block that reuses a label name.
        //
        // Exception: unlabeled `break` at loop_depth 0 exits the HFS loop
        // directly — the post-loop write-backs handle the sync, so we skip
        // inserting redundant write-backs here.
        if matches!(stmt.kind, TirStmtKind::Return { .. })
            || matches!(&stmt.kind, TirStmtKind::Break { label, .. }
                if !label.as_ref().is_some_and(|l| enclosing_labels.contains(l.as_str())))
        {
            replace_in_stmt(&mut stmt, candidates, &ctx);
            let skip_wb =
                loop_depth == 0 && matches!(&stmt.kind, TirStmtKind::Break { label: None, .. });
            if !skip_wb {
                new_stmts.extend(make_write_back_stmts(candidates, span));
            }
            new_stmts.push(stmt);
            continue;
        }

        // For leaf statements: compute which (local, field) pairs need sync
        // based on what the callee functions actually access.
        let sync_fields = compute_sync_fields(&stmt, candidates, type_table, cache);

        if !sync_fields.write_back.is_empty() {
            for c in candidates {
                if sync_fields
                    .write_back
                    .contains(&(c.local_index, c.field_index))
                {
                    new_stmts.push(make_write_back_stmt(c, span));
                }
            }
        }

        replace_in_stmt(&mut stmt, candidates, &ctx);
        new_stmts.push(stmt);

        if !sync_fields.re_read.is_empty() {
            for c in candidates {
                if sync_fields
                    .re_read
                    .contains(&(c.local_index, c.field_index))
                {
                    new_stmts.push(make_re_read_stmt(c, span));
                }
            }
        }
    }

    block.stmts = new_stmts;
}

/// Fields needing synchronization around function calls.
/// `write_back` fields need scalar → struct write before the call.
/// `re_read` fields need struct → scalar read after the call.
/// Immutable ref args only need write-back (callee can't modify through `&T`).
struct SyncFields {
    write_back: IndexSet<(u32, u32)>,
    re_read: IndexSet<(u32, u32)>,
}

/// Compute which (`local_index`, `field_index`) pairs need write-back/re-read
/// for calls in this statement. Uses the field usage cache to narrow down
/// to only fields the callee actually accesses.
fn compute_sync_fields(
    stmt: &TirStmt,
    candidates: &[ScalarizeCandidate],
    type_table: &TypeTable,
    cache: &FieldUsageCache,
) -> SyncFields {
    let mut result = SyncFields {
        write_back: IndexSet::default(),
        re_read: IndexSet::default(),
    };
    compute_sync_fields_in_stmt(stmt, candidates, type_table, cache, &mut result);
    result
}

fn compute_sync_fields_in_stmt(
    stmt: &TirStmt,
    candidates: &[ScalarizeCandidate],
    type_table: &TypeTable,
    cache: &FieldUsageCache,
    result: &mut SyncFields,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            compute_sync_fields_in_expr(value, candidates, type_table, cache, result);
        }
        TirStmtKind::Expr(expr) => {
            compute_sync_fields_in_expr(expr, candidates, type_table, cache, result);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                compute_sync_fields_in_expr(v, candidates, type_table, cache, result);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            compute_sync_fields_in_expr(condition, candidates, type_table, cache, result);
            for s in &then_block.stmts {
                compute_sync_fields_in_stmt(s, candidates, type_table, cache, result);
            }
            if let Some(eb) = else_block {
                for s in &eb.stmts {
                    compute_sync_fields_in_stmt(s, candidates, type_table, cache, result);
                }
            }
        }
        TirStmtKind::Loop { body } => {
            for s in &body.stmts {
                compute_sync_fields_in_stmt(s, candidates, type_table, cache, result);
            }
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            for s in &block.stmts {
                compute_sync_fields_in_stmt(s, candidates, type_table, cache, result);
            }
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            compute_sync_fields_in_expr(scrutinee, candidates, type_table, cache, result);
            for s in &then_block.stmts {
                compute_sync_fields_in_stmt(s, candidates, type_table, cache, result);
            }
            if let Some(eb) = else_block {
                for s in &eb.stmts {
                    compute_sync_fields_in_stmt(s, candidates, type_table, cache, result);
                }
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                compute_sync_fields_in_expr(v, candidates, type_table, cache, result);
            }
        }
        TirStmtKind::Continue
        | TirStmtKind::TaskReturn { .. }
        | TirStmtKind::VariadicForOf { .. } => {}
        TirStmtKind::LetDestructure { value, .. } => {
            compute_sync_fields_in_expr(value, candidates, type_table, cache, result);
        }
    }
}

/// Check if an expression is an immutable ref to a local (`Unary{Ref, Local}`).
/// Also returns true for bare locals with `Ref(T)` type (after `ref_elim`).
fn is_immut_ref_arg(expr: &TirExpr, type_table: &TypeTable) -> bool {
    match &expr.kind {
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            ..
        } => true,
        TirExprKind::Local { .. } => {
            matches!(type_table.get(expr.type_id), ResolvedType::Ref(inner)
                if !matches!(type_table.get(*inner), ResolvedType::MutRef(_)))
        }
        _ => false,
    }
}

fn compute_sync_fields_in_expr(
    expr: &TirExpr,
    candidates: &[ScalarizeCandidate],
    type_table: &TypeTable,
    cache: &FieldUsageCache,
    result: &mut SyncFields,
) {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
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
            for arg in args {
                compute_sync_fields_in_expr(&arg.expr, candidates, type_table, cache, result);
            }
        }
        TirExprKind::MethodCall {
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
            compute_sync_fields_in_expr(receiver, candidates, type_table, cache, result);
            for arg in args {
                compute_sync_fields_in_expr(&arg.expr, candidates, type_table, cache, result);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            for arg in args {
                if let Some(local_idx) = extract_gc_local_index(arg, type_table) {
                    add_all_fields_for_local(local_idx, candidates, &mut result.write_back);
                    if !is_immut_ref_arg(arg, type_table) {
                        add_all_fields_for_local(local_idx, candidates, &mut result.re_read);
                    }
                }
            }
            compute_sync_fields_in_expr(callee, candidates, type_table, cache, result);
            for arg in args {
                compute_sync_fields_in_expr(arg, candidates, type_table, cache, result);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                compute_sync_fields_in_expr(arg, candidates, type_table, cache, result);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            compute_sync_fields_in_expr(left, candidates, type_table, cache, result);
            compute_sync_fields_in_expr(right, candidates, type_table, cache, result);
        }
        TirExprKind::Unary { expr, .. } => {
            compute_sync_fields_in_expr(expr, candidates, type_table, cache, result);
        }
        TirExprKind::Assign { target, value } => {
            compute_sync_fields_in_expr(target, candidates, type_table, cache, result);
            compute_sync_fields_in_expr(value, candidates, type_table, cache, result);
        }
        TirExprKind::Cast { expr, .. } => {
            compute_sync_fields_in_expr(expr, candidates, type_table, cache, result);
        }
        TirExprKind::FieldAccess { expr, .. }
        | TirExprKind::TupleSpread { expr }
        | TirExprKind::TupleZip { expr }
        | TirExprKind::TypePackExpansion {
            call_expr: expr, ..
        } => {
            compute_sync_fields_in_expr(expr, candidates, type_table, cache, result);
        }
        TirExprKind::Index { expr, index } => {
            compute_sync_fields_in_expr(expr, candidates, type_table, cache, result);
            compute_sync_fields_in_expr(index, candidates, type_table, cache, result);
        }
        TirExprKind::Block(block) => {
            for s in &block.stmts {
                compute_sync_fields_in_stmt(s, candidates, type_table, cache, result);
            }
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            compute_sync_fields_in_expr(condition, candidates, type_table, cache, result);
            for s in &then_branch.stmts {
                compute_sync_fields_in_stmt(s, candidates, type_table, cache, result);
            }
            if let Some(eb) = else_branch {
                for s in &eb.stmts {
                    compute_sync_fields_in_stmt(s, candidates, type_table, cache, result);
                }
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                compute_sync_fields_in_expr(&field.value, candidates, type_table, cache, result);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                compute_sync_fields_in_expr(elem, candidates, type_table, cache, result);
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            for s in &block.stmts {
                compute_sync_fields_in_stmt(s, candidates, type_table, cache, result);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            compute_sync_fields_in_expr(scrutinee, candidates, type_table, cache, result);
            for arm in arms {
                for s in &arm.stmts {
                    compute_sync_fields_in_stmt(s, candidates, type_table, cache, result);
                }
            }
            for s in &default.stmts {
                compute_sync_fields_in_stmt(s, candidates, type_table, cache, result);
            }
        }
        TirExprKind::Match { expr, arms } => {
            compute_sync_fields_in_expr(expr, candidates, type_table, cache, result);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    compute_sync_fields_in_expr(guard, candidates, type_table, cache, result);
                }
                compute_sync_fields_in_expr(&arm.body, candidates, type_table, cache, result);
            }
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let crate::tir::TirTemplatePart::Interpolation { expr, .. } = part {
                    compute_sync_fields_in_expr(expr, candidates, type_table, cache, result);
                }
            }
        }
        TirExprKind::Closure { body, .. } => {
            compute_sync_fields_in_expr(body, candidates, type_table, cache, result);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                compute_sync_fields_in_expr(p, candidates, type_table, cache, result);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            compute_sync_fields_in_expr(functor, candidates, type_table, cache, result);
        }
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. }
        | TirExprKind::GlobalVarSet { value: expr, .. } => {
            compute_sync_fields_in_expr(expr, candidates, type_table, cache, result);
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
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// For a call argument that might be a scalarized local, determine which fields
/// need syncing based on the callee's field usage cache.
fn add_sync_fields_for_arg(
    arg_expr: &TirExpr,
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

    // Check if this local has any scalarized fields
    let has_scalarized = candidates.iter().any(|c| c.local_index == local_idx);
    if !has_scalarized {
        return;
    }

    // Look up the callee's entry in the cache.
    let cache_key = (func_ref.module_source.clone(), func_ref.name.clone());
    let callee = cache.get(&cache_key);

    // If the callee's parameter at this position is typed `&T` (not `&mut T`),
    // the callee cannot mutate through the reference — re-read is unnecessary
    // regardless of how the caller typed the argument. This matters for
    // `self.method()` calls where `self: &mut T` is coerced to an `&T` parameter.
    let callee_immut = callee.is_some_and(|e| e.immut_ref_params.contains(&param_position));
    let is_immut_ref = is_immut_ref || callee_immut;

    // Helper: add fields to write_back and optionally re_read.
    // Immutable ref args (`&T`) only need write-back — the callee cannot modify
    // through an immutable reference, so re-read is unnecessary.
    let mut add_field = |local_idx: u32, field_idx: u32| {
        result.write_back.insert((local_idx, field_idx));
        if !is_immut_ref {
            result.re_read.insert((local_idx, field_idx));
        }
    };

    // Treat an entry with empty `params` the same as a cache miss — we have no
    // field usage info, so we must be conservative.
    match callee.filter(|e| !e.params.is_empty()) {
        Some(entry) => {
            match entry.params.get(&param_position) {
                Some(Some(field_set)) => {
                    // Precise: only sync fields the callee accesses
                    for c in candidates {
                        if c.local_index == local_idx && field_set.contains(&c.field_index) {
                            add_field(c.local_index, c.field_index);
                        }
                    }
                }
                Some(None) => {
                    // Conservative: callee passes the struct further, all fields potentially touched
                    for c in candidates {
                        if c.local_index == local_idx {
                            add_field(c.local_index, c.field_index);
                        }
                    }
                }
                None => {
                    // Param at this position is not struct-typed in callee, or not tracked.
                    // No fields need syncing (callee can't access fields of a non-struct param).
                }
            }
        }
        None => {
            // Callee not in cache (external/imported function, no body, etc.)
            // Conservative: sync all fields.
            for c in candidates {
                if c.local_index == local_idx {
                    add_field(c.local_index, c.field_index);
                }
            }
        }
    }
}

/// Extract local index from an expression if it's a GC-typed local or &mut local.
fn extract_gc_local_index(expr: &TirExpr, type_table: &TypeTable) -> Option<u32> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => {
            if is_gc_heap_type(expr.type_id, type_table) {
                Some(*index)
            } else {
                None
            }
        }
        TirExprKind::Unary {
            op: TirUnaryOp::MutRef | TirUnaryOp::Ref,
            expr: inner,
        } => {
            if let TirExprKind::Local { index, .. } = &inner.kind {
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

/// Add all scalarized fields for a local (conservative fallback).
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

// ─────────────────────────────────────────────────────────────────────────────
// Expression replacement (replace field accesses with scalarized locals)
// ─────────────────────────────────────────────────────────────────────────────

fn replace_in_stmt(stmt: &mut TirStmt, candidates: &[ScalarizeCandidate], ctx: &ReplaceCtx) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            replace_in_expr(value, candidates, ctx);
        }
        TirStmtKind::Expr(expr) => {
            replace_in_expr(expr, candidates, ctx);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                replace_in_expr(v, candidates, ctx);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            replace_in_expr(condition, candidates, ctx);
            replace_in_block_stmts(then_block, candidates, ctx);
            if let Some(eb) = else_block {
                replace_in_block_stmts(eb, candidates, ctx);
            }
        }
        TirStmtKind::Loop { body } => {
            replace_in_block_stmts(body, candidates, ctx);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            replace_in_block_stmts(block, candidates, ctx);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            replace_in_expr(scrutinee, candidates, ctx);
            replace_in_block_stmts(then_block, candidates, ctx);
            if let Some(eb) = else_block {
                replace_in_block_stmts(eb, candidates, ctx);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                replace_in_expr(v, candidates, ctx);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetDestructure { value, .. } => {
            replace_in_expr(value, candidates, ctx);
        }
        TirStmtKind::TaskReturn { .. } => {}
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn replace_in_block_stmts(
    block: &mut TirBlock,
    candidates: &[ScalarizeCandidate],
    ctx: &ReplaceCtx,
) {
    for stmt in &mut block.stmts {
        replace_in_stmt(stmt, candidates, ctx);
    }
}

fn make_write_back_stmts(
    candidates: &[ScalarizeCandidate],
    span: crate::token::Span,
) -> Vec<TirStmt> {
    candidates
        .iter()
        .map(|c| make_write_back_stmt(c, span))
        .collect()
}

fn replace_in_expr(expr: &mut TirExpr, candidates: &[ScalarizeCandidate], ctx: &ReplaceCtx) {
    // Check if this is an assignment to a scalarized field: obj.field = val
    if let TirExprKind::Assign { target, value } = &mut expr.kind {
        if let TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } = &target.kind
            && let TirExprKind::Local { index, .. } = &inner.kind
        {
            for c in candidates {
                if c.local_index == *index && c.field_index == *field_index {
                    // Replace obj.field = val with _hfs_local = val
                    replace_in_expr(value, candidates, ctx);
                    let new_target = TirExpr::new(
                        TirExprKind::Local {
                            index: c.new_local_index,
                            name: format!("_hfs_{}_{}", c.field_name, c.new_local_index),
                        },
                        c.type_id,
                        expr.span,
                    );
                    **target = new_target;
                    return;
                }
            }
        }
        replace_in_expr(target, candidates, ctx);
        replace_in_expr(value, candidates, ctx);
        return;
    }

    // Check if this is a read of a scalarized field: obj.field
    if let TirExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &expr.kind
        && let TirExprKind::Local { index, .. } = &inner.kind
    {
        for c in candidates {
            if c.local_index == *index && c.field_index == *field_index {
                // Replace obj.field with _hfs_local
                expr.kind = TirExprKind::Local {
                    index: c.new_local_index,
                    name: format!("_hfs_{}_{}", c.field_name, c.new_local_index),
                };
                return;
            }
        }
    }

    // Recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            replace_in_expr(inner, candidates, ctx);
        }
        TirExprKind::Binary { left, right, .. } => {
            replace_in_expr(left, candidates, ctx);
            replace_in_expr(right, candidates, ctx);
        }
        TirExprKind::Unary { expr, .. } => {
            replace_in_expr(expr, candidates, ctx);
        }
        TirExprKind::Cast { expr, .. } => {
            replace_in_expr(expr, candidates, ctx);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                replace_in_expr(&mut arg.expr, candidates, ctx);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            replace_in_expr(receiver, candidates, ctx);
            for arg in args {
                replace_in_expr(&mut arg.expr, candidates, ctx);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                replace_in_expr(arg, candidates, ctx);
            }
        }
        TirExprKind::Index { expr, index } => {
            replace_in_expr(expr, candidates, ctx);
            replace_in_expr(index, candidates, ctx);
        }
        TirExprKind::Block(block) => {
            replace_in_block_stmts(block, candidates, ctx);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            replace_in_expr(condition, candidates, ctx);
            replace_in_block_stmts(then_branch, candidates, ctx);
            if let Some(eb) = else_branch {
                replace_in_block_stmts(eb, candidates, ctx);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                replace_in_expr(&mut field.value, candidates, ctx);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                replace_in_expr(elem, candidates, ctx);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            replace_in_expr(callee, candidates, ctx);
            for arg in args {
                replace_in_expr(arg, candidates, ctx);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            replace_in_expr(functor, candidates, ctx);
        }
        TirExprKind::Closure { body, .. } => {
            replace_in_expr(body, candidates, ctx);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                replace_in_expr(p, candidates, ctx);
            }
        }
        TirExprKind::LabeledBlock { label, block, .. } => {
            // Use replace_in_block (with write-back/re-read sync) instead of
            // replace_in_block_stmts (field replacement only). This ensures
            // function calls inside labeled block expressions get proper
            // write-back/re-read around them, even when the labeled block
            // is nested inside a let value or other expression context.
            // Extend the enclosing-labels set with this block's label so that
            // a break targeting it is recognized as staying in the HFS scope.
            let mut extended = ctx.enclosing_labels.clone();
            extended.insert(label.clone());
            replace_in_block(
                block,
                candidates,
                ctx.locals,
                ctx.type_table,
                ctx.cache,
                &extended,
                ctx.loop_depth,
            );
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            replace_in_expr(value, candidates, ctx);
        }
        TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
            replace_in_expr(expr, candidates, ctx);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            replace_in_expr(expr, candidates, ctx);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            replace_in_expr(scrutinee, candidates, ctx);
            for arm in arms {
                replace_in_block_stmts(arm, candidates, ctx);
            }
            replace_in_block_stmts(default, candidates, ctx);
        }
        TirExprKind::Assign { target, value } => {
            replace_in_expr(target, candidates, ctx);
            replace_in_expr(value, candidates, ctx);
        }
        TirExprKind::Match { expr, arms } => {
            replace_in_expr(expr, candidates, ctx);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    replace_in_expr(guard, candidates, ctx);
                }
                replace_in_expr(&mut arm.body, candidates, ctx);
            }
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts {
                if let crate::tir::TirTemplatePart::Interpolation { expr, .. } = part {
                    replace_in_expr(expr, candidates, ctx);
                }
            }
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
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}
