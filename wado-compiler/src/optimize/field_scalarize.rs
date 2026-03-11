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
//! 6. Insert re-read `_hfs_field_N = obj.field` after function calls that receive `obj`
//! 7. Insert final write-back `obj.field = _hfs_field_N` after the loop
//!
//! This converts GC struct.get/struct.set into wasm local.get/local.set in hot loops.

use crate::project::Project;
use crate::tir::{
    ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TirUnaryOp,
    TypeId, TypeTable,
};
use indexmap::{IndexMap, IndexSet};

const MIN_ACCESS_COUNT: usize = 4;

pub fn scalarize_hot_fields(project: &mut Project) -> bool {
    let mut changed = false;
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            changed |= scalarize_function(&mut func, &type_table);
        }
    }
    changed
}

fn scalarize_function(func: &mut TirFunction, type_table: &TypeTable) -> bool {
    let Some(ref mut body) = func.body else {
        return false;
    };
    let mut local_count = func.local_count;
    let mut local_types = func.local_types.clone();
    let changed = scalarize_block(body, &mut local_count, &mut local_types, type_table);
    func.local_count = local_count;
    func.local_types = local_types;
    changed
}

fn scalarize_block(
    block: &mut TirBlock,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &TypeTable,
) -> bool {
    let mut changed = false;
    let mut new_stmts = Vec::new();

    for mut stmt in std::mem::take(&mut block.stmts) {
        match &mut stmt.kind {
            TirStmtKind::Loop { body } => {
                // Recurse into inner blocks/loops first.
                changed |= scalarize_block(body, local_count, local_types, type_table);
                // Try to scalarize hot fields at this loop level.
                let result =
                    scalarize_loop(body, local_count, local_types, type_table);
                if !result.pre_stmts.is_empty() {
                    changed = true;
                    new_stmts.extend(result.pre_stmts);
                    new_stmts.push(stmt);
                    new_stmts.extend(result.post_stmts);
                } else {
                    new_stmts.push(stmt);
                }
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                changed |= scalarize_block(then_block, local_count, local_types, type_table);
                if let Some(eb) = else_block {
                    changed |= scalarize_block(eb, local_count, local_types, type_table);
                }
                new_stmts.push(stmt);
            }
            TirStmtKind::LabeledBlock { block: inner, .. } => {
                changed |= scalarize_block(inner, local_count, local_types, type_table);
                new_stmts.push(stmt);
            }
            TirStmtKind::IfPattern {
                then_block,
                else_block,
                ..
            } => {
                changed |= scalarize_block(then_block, local_count, local_types, type_table);
                if let Some(eb) = else_block {
                    changed |= scalarize_block(eb, local_count, local_types, type_table);
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
    local_types: &mut Vec<TypeId>,
    type_table: &TypeTable,
) -> ScalarizeResult {
    // Step 1: Count field accesses (reads + writes) in the loop body
    let mut access_counts: IndexMap<(u32, u32), FieldAccessInfo> = IndexMap::new();
    count_field_accesses_in_block(loop_body, &mut access_counts, type_table);

    // Step 2: Find which locals are passed to function calls (and thus could be aliased)
    let mut call_passed_locals: IndexSet<u32> = IndexSet::new();
    collect_call_passed_locals_in_block(loop_body, &mut call_passed_locals, type_table);

    // Step 3: Select candidates - fields accessed frequently enough,
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
        local_types.push(info.field_type_id);
        next_local += 1;
    }

    if candidates.is_empty() {
        return ScalarizeResult {
            pre_stmts: Vec::new(),
            post_stmts: Vec::new(),
        };
    }

    *local_count = next_local;

    // Step 4: Create pre-loop load statements
    let span = crate::token::Span::new(0, 0, 0, 0);
    let mut pre_stmts = Vec::new();
    for c in &candidates {
        let local_type_id = if (c.local_index as usize) < local_types.len() {
            local_types[c.local_index as usize]
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

    // Step 5: Create post-loop write-back statements
    let mut post_stmts = Vec::new();
    for c in &candidates {
        post_stmts.push(make_write_back_stmt(c, span));
    }

    // Step 6: Replace field accesses in the loop body, and insert write-back/re-read
    // around function calls that pass the struct
    replace_in_block(loop_body, &candidates, &call_passed_locals, local_types, type_table);

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

#[derive(Debug, Clone)]
struct FieldAccessInfo {
    local_name: String,
    field_name: String,
    local_type_id: TypeId,
    field_type_id: TypeId,
    read_count: usize,
    write_count: usize,
    local_fully_assigned: bool,
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
        TirStmtKind::Loop { body } => {
            count_field_accesses_in_block(body, counts, type_table);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            count_field_accesses_in_block(block, counts, type_table);
        }
        TirStmtKind::IfPattern {
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
        TirStmtKind::LetPattern { value, .. } => {
            count_field_accesses_in_expr(value, counts, false, type_table);
        }
        TirStmtKind::TaskReturn { .. } => {}
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
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::Match { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {}
    }
}

fn mark_local_fully_assigned(
    local_idx: u32,
    counts: &mut IndexMap<(u32, u32), FieldAccessInfo>,
) {
    for (&(li, _fi), info) in counts.iter_mut() {
        if li == local_idx {
            info.local_fully_assigned = true;
        }
    }
}

fn collect_call_passed_locals_in_block(
    block: &TirBlock,
    locals: &mut IndexSet<u32>,
    type_table: &TypeTable,
) {
    for stmt in &block.stmts {
        collect_call_passed_locals_in_stmt(stmt, locals, type_table);
    }
}

fn collect_call_passed_locals_in_stmt(
    stmt: &TirStmt,
    locals: &mut IndexSet<u32>,
    type_table: &TypeTable,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            collect_call_passed_locals_in_expr(value, locals, type_table);
        }
        TirStmtKind::Expr(expr) => {
            collect_call_passed_locals_in_expr(expr, locals, type_table);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_call_passed_locals_in_expr(v, locals, type_table);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_call_passed_locals_in_expr(condition, locals, type_table);
            collect_call_passed_locals_in_block(then_block, locals, type_table);
            if let Some(eb) = else_block {
                collect_call_passed_locals_in_block(eb, locals, type_table);
            }
        }
        TirStmtKind::Loop { body } => {
            collect_call_passed_locals_in_block(body, locals, type_table);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_call_passed_locals_in_block(block, locals, type_table);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_call_passed_locals_in_expr(scrutinee, locals, type_table);
            collect_call_passed_locals_in_block(then_block, locals, type_table);
            if let Some(eb) = else_block {
                collect_call_passed_locals_in_block(eb, locals, type_table);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_call_passed_locals_in_expr(v, locals, type_table);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            collect_call_passed_locals_in_expr(value, locals, type_table);
        }
        TirStmtKind::TaskReturn { .. } => {}
    }
}

fn collect_call_passed_locals_in_expr(
    expr: &TirExpr,
    locals: &mut IndexSet<u32>,
    type_table: &TypeTable,
) {
    match &expr.kind {
        TirExprKind::Call { args, .. } => {
            for arg in args {
                collect_gc_local_from_expr(&arg.expr, locals, type_table);
                collect_call_passed_locals_in_expr(&arg.expr, locals, type_table);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_gc_local_from_expr(receiver, locals, type_table);
            collect_call_passed_locals_in_expr(receiver, locals, type_table);
            for arg in args {
                collect_gc_local_from_expr(&arg.expr, locals, type_table);
                collect_call_passed_locals_in_expr(&arg.expr, locals, type_table);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_call_passed_locals_in_expr(callee, locals, type_table);
            for arg in args {
                collect_gc_local_from_expr(arg, locals, type_table);
                collect_call_passed_locals_in_expr(arg, locals, type_table);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_call_passed_locals_in_expr(left, locals, type_table);
            collect_call_passed_locals_in_expr(right, locals, type_table);
        }
        TirExprKind::Unary { expr, .. } => {
            collect_call_passed_locals_in_expr(expr, locals, type_table);
        }
        TirExprKind::Assign { target, value } => {
            collect_call_passed_locals_in_expr(target, locals, type_table);
            collect_call_passed_locals_in_expr(value, locals, type_table);
        }
        TirExprKind::Cast { expr, .. } => {
            collect_call_passed_locals_in_expr(expr, locals, type_table);
        }
        TirExprKind::FieldAccess { expr, .. } => {
            collect_call_passed_locals_in_expr(expr, locals, type_table);
        }
        TirExprKind::Index { expr, index } => {
            collect_call_passed_locals_in_expr(expr, locals, type_table);
            collect_call_passed_locals_in_expr(index, locals, type_table);
        }
        TirExprKind::Block(block) => {
            collect_call_passed_locals_in_block(block, locals, type_table);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_call_passed_locals_in_expr(condition, locals, type_table);
            collect_call_passed_locals_in_block(then_branch, locals, type_table);
            if let Some(eb) = else_branch {
                collect_call_passed_locals_in_block(eb, locals, type_table);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_call_passed_locals_in_expr(&field.value, locals, type_table);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_call_passed_locals_in_expr(elem, locals, type_table);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_call_passed_locals_in_expr(arg, locals, type_table);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_call_passed_locals_in_expr(functor, locals, type_table);
        }
        TirExprKind::Closure { body, .. } => {
            collect_call_passed_locals_in_expr(body, locals, type_table);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                collect_call_passed_locals_in_expr(p, locals, type_table);
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_call_passed_locals_in_block(block, locals, type_table);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_call_passed_locals_in_expr(value, locals, type_table);
        }
        TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
            collect_call_passed_locals_in_expr(expr, locals, type_table);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            collect_call_passed_locals_in_expr(expr, locals, type_table);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_call_passed_locals_in_expr(scrutinee, locals, type_table);
            for arm in arms {
                collect_call_passed_locals_in_block(arm, locals, type_table);
            }
            collect_call_passed_locals_in_block(default, locals, type_table);
        }
        _ => {}
    }
}

fn collect_gc_local_from_expr(
    expr: &TirExpr,
    locals: &mut IndexSet<u32>,
    type_table: &TypeTable,
) {
    match &expr.kind {
        TirExprKind::Local { index, .. } => {
            if is_gc_heap_type(expr.type_id, type_table) {
                locals.insert(*index);
            }
        }
        // &mut local — used as method receiver or passed by mutable reference
        TirExprKind::Unary {
            op: TirUnaryOp::MutRef,
            expr: inner,
        } => {
            if let TirExprKind::Local { index, .. } = &inner.kind {
                if is_gc_heap_type(inner.type_id, type_table) {
                    locals.insert(*index);
                }
            }
        }
        _ => {}
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

fn replace_in_block(
    block: &mut TirBlock,
    candidates: &[ScalarizeCandidate],
    call_passed_locals: &IndexSet<u32>,
    local_types: &[TypeId],
    type_table: &TypeTable,
) {
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
                replace_in_expr(condition, candidates);
                replace_in_block(then_block, candidates, call_passed_locals, local_types, type_table);
                if let Some(eb) = else_block {
                    replace_in_block(eb, candidates, call_passed_locals, local_types, type_table);
                }
                new_stmts.push(stmt);
                continue;
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                replace_in_expr(scrutinee, candidates);
                replace_in_block(then_block, candidates, call_passed_locals, local_types, type_table);
                if let Some(eb) = else_block {
                    replace_in_block(eb, candidates, call_passed_locals, local_types, type_table);
                }
                new_stmts.push(stmt);
                continue;
            }
            TirStmtKind::Loop { body } => {
                replace_in_block(body, candidates, call_passed_locals, local_types, type_table);
                new_stmts.push(stmt);
                continue;
            }
            TirStmtKind::LabeledBlock { block: inner, .. } => {
                replace_in_block(inner, candidates, call_passed_locals, local_types, type_table);
                new_stmts.push(stmt);
                continue;
            }
            _ => {}
        }

        // Insert write-back before return statements, since return exits the
        // function and would skip the post-loop write-back.
        if matches!(stmt.kind, TirStmtKind::Return { .. }) {
            replace_in_stmt(&mut stmt, candidates);
            new_stmts.extend(make_write_back_stmts(candidates, span));
            new_stmts.push(stmt);
            continue;
        }

        // For leaf statements: check if this statement contains a function call
        // that passes a scalarized local, and add write-back/re-read around it.
        let needs_sync = stmt_has_call_with_scalarized_local(&stmt, candidates, call_passed_locals, type_table);

        if needs_sync {
            for c in candidates {
                if call_passed_locals.contains(&c.local_index) {
                    new_stmts.push(make_write_back_stmt(c, span));
                }
            }
        }

        replace_in_stmt(&mut stmt, candidates);
        new_stmts.push(stmt);

        if needs_sync {
            for c in candidates {
                if call_passed_locals.contains(&c.local_index) {
                    new_stmts.push(make_re_read_stmt(c, span));
                }
            }
        }
    }

    block.stmts = new_stmts;
}

fn stmt_has_call_with_scalarized_local(
    stmt: &TirStmt,
    candidates: &[ScalarizeCandidate],
    call_passed_locals: &IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    let scalarized_locals: IndexSet<u32> = candidates
        .iter()
        .filter(|c| call_passed_locals.contains(&c.local_index))
        .map(|c| c.local_index)
        .collect();

    if scalarized_locals.is_empty() {
        return false;
    }

    expr_has_call_passing_locals_stmt(stmt, &scalarized_locals, type_table)
}

fn expr_has_call_passing_locals_stmt(
    stmt: &TirStmt,
    locals: &IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            expr_has_call_passing_locals(value, locals, type_table)
        }
        TirStmtKind::Expr(expr) => expr_has_call_passing_locals(expr, locals, type_table),
        TirStmtKind::Return { value } => value
            .as_ref()
            .is_some_and(|v| expr_has_call_passing_locals(v, locals, type_table)),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_has_call_passing_locals(condition, locals, type_table)
                || block_has_call_passing_locals(then_block, locals, type_table)
                || else_block
                    .as_ref()
                    .is_some_and(|eb| block_has_call_passing_locals(eb, locals, type_table))
        }
        TirStmtKind::Loop { body } => block_has_call_passing_locals(body, locals, type_table),
        TirStmtKind::LabeledBlock { block, .. } => {
            block_has_call_passing_locals(block, locals, type_table)
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            expr_has_call_passing_locals(scrutinee, locals, type_table)
                || block_has_call_passing_locals(then_block, locals, type_table)
                || else_block
                    .as_ref()
                    .is_some_and(|eb| block_has_call_passing_locals(eb, locals, type_table))
        }
        TirStmtKind::Break { value, .. } => value
            .as_ref()
            .is_some_and(|v| expr_has_call_passing_locals(v, locals, type_table)),
        TirStmtKind::Continue => false,
        TirStmtKind::LetPattern { value, .. } => {
            expr_has_call_passing_locals(value, locals, type_table)
        }
        TirStmtKind::TaskReturn { .. } => false,
    }
}

fn block_has_call_passing_locals(
    block: &TirBlock,
    locals: &IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    block
        .stmts
        .iter()
        .any(|s| expr_has_call_passing_locals_stmt(s, locals, type_table))
}

fn expr_is_gc_local_in_set(
    expr: &TirExpr,
    locals: &IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    match &expr.kind {
        TirExprKind::Local { index, .. } => {
            is_gc_heap_type(expr.type_id, type_table) && locals.contains(index)
        }
        TirExprKind::Unary {
            op: TirUnaryOp::MutRef,
            expr: inner,
        } => {
            if let TirExprKind::Local { index, .. } = &inner.kind {
                is_gc_heap_type(inner.type_id, type_table) && locals.contains(index)
            } else {
                false
            }
        }
        _ => false,
    }
}

fn expr_has_call_passing_locals(
    expr: &TirExpr,
    locals: &IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    match &expr.kind {
        TirExprKind::Call { args, .. } => {
            let has_match = args
                .iter()
                .any(|arg| expr_is_gc_local_in_set(&arg.expr, locals, type_table));
            if has_match {
                return true;
            }
            args.iter()
                .any(|arg| expr_has_call_passing_locals(&arg.expr, locals, type_table))
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            let has_match = expr_is_gc_local_in_set(receiver, locals, type_table)
                || args
                    .iter()
                    .any(|arg| expr_is_gc_local_in_set(&arg.expr, locals, type_table));
            if has_match {
                return true;
            }
            expr_has_call_passing_locals(receiver, locals, type_table)
                || args
                    .iter()
                    .any(|arg| expr_has_call_passing_locals(&arg.expr, locals, type_table))
        }
        TirExprKind::Binary { left, right, .. } => {
            expr_has_call_passing_locals(left, locals, type_table)
                || expr_has_call_passing_locals(right, locals, type_table)
        }
        TirExprKind::Unary { expr, .. } => {
            expr_has_call_passing_locals(expr, locals, type_table)
        }
        TirExprKind::Assign { target, value } => {
            expr_has_call_passing_locals(target, locals, type_table)
                || expr_has_call_passing_locals(value, locals, type_table)
        }
        TirExprKind::Cast { expr, .. } => {
            expr_has_call_passing_locals(expr, locals, type_table)
        }
        TirExprKind::FieldAccess { expr, .. } => {
            expr_has_call_passing_locals(expr, locals, type_table)
        }
        TirExprKind::Index { expr, index } => {
            expr_has_call_passing_locals(expr, locals, type_table)
                || expr_has_call_passing_locals(index, locals, type_table)
        }
        TirExprKind::Block(block) => block_has_call_passing_locals(block, locals, type_table),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_has_call_passing_locals(condition, locals, type_table)
                || block_has_call_passing_locals(then_branch, locals, type_table)
                || else_branch
                    .as_ref()
                    .is_some_and(|eb| block_has_call_passing_locals(eb, locals, type_table))
        }
        TirExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|f| expr_has_call_passing_locals(&f.value, locals, type_table)),
        TirExprKind::TupleLiteral { elements } => elements
            .iter()
            .any(|e| expr_has_call_passing_locals(e, locals, type_table)),
        TirExprKind::LabeledBlock { block, .. } => {
            block_has_call_passing_locals(block, locals, type_table)
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            expr_has_call_passing_locals(callee, locals, type_table)
                || args
                    .iter()
                    .any(|arg| expr_has_call_passing_locals(arg, locals, type_table))
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            expr_has_call_passing_locals(scrutinee, locals, type_table)
                || arms
                    .iter()
                    .any(|arm| block_has_call_passing_locals(arm, locals, type_table))
                || block_has_call_passing_locals(default, locals, type_table)
        }
        _ => false,
    }
}

fn replace_in_stmt(stmt: &mut TirStmt, candidates: &[ScalarizeCandidate]) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            replace_in_expr(value, candidates);
        }
        TirStmtKind::Expr(expr) => {
            replace_in_expr(expr, candidates);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                replace_in_expr(v, candidates);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            replace_in_expr(condition, candidates);
            replace_in_block_stmts(then_block, candidates);
            if let Some(eb) = else_block {
                replace_in_block_stmts(eb, candidates);
            }
        }
        TirStmtKind::Loop { body } => {
            replace_in_block_stmts(body, candidates);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            replace_in_block_stmts(block, candidates);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            replace_in_expr(scrutinee, candidates);
            replace_in_block_stmts(then_block, candidates);
            if let Some(eb) = else_block {
                replace_in_block_stmts(eb, candidates);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                replace_in_expr(v, candidates);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            replace_in_expr(value, candidates);
        }
        TirStmtKind::TaskReturn { .. } => {}
    }
}

fn replace_in_block_stmts(block: &mut TirBlock, candidates: &[ScalarizeCandidate]) {
    for stmt in &mut block.stmts {
        replace_in_stmt(stmt, candidates);
    }
}

fn make_write_back_stmts(
    candidates: &[ScalarizeCandidate],
    span: crate::token::Span,
) -> Vec<TirStmt> {
    candidates.iter().map(|c| make_write_back_stmt(c, span)).collect()
}

fn replace_in_expr(expr: &mut TirExpr, candidates: &[ScalarizeCandidate]) {
    // Check if this is an assignment to a scalarized field: obj.field = val
    if let TirExprKind::Assign { target, value } = &mut expr.kind {
        if let TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } = &target.kind
        {
            if let TirExprKind::Local { index, .. } = &inner.kind {
                for c in candidates {
                    if c.local_index == *index && c.field_index == *field_index {
                        // Replace obj.field = val with _hfs_local = val
                        replace_in_expr(value, candidates);
                        let new_target = TirExpr::new(
                            TirExprKind::Local {
                                index: c.new_local_index,
                                name: format!("_hfs_{}_{}", c.field_name, c.new_local_index),
                            },
                            c.type_id,
                            expr.span,
                        );
                        *target = Box::new(new_target);
                        return;
                    }
                }
            }
        }
        replace_in_expr(target, candidates);
        replace_in_expr(value, candidates);
        return;
    }

    // Check if this is a read of a scalarized field: obj.field
    if let TirExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &expr.kind
    {
        if let TirExprKind::Local { index, .. } = &inner.kind {
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
    }

    // Recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::FieldAccess { expr: inner, .. } => {
            replace_in_expr(inner, candidates);
        }
        TirExprKind::Binary { left, right, .. } => {
            replace_in_expr(left, candidates);
            replace_in_expr(right, candidates);
        }
        TirExprKind::Unary { expr, .. } => {
            replace_in_expr(expr, candidates);
        }
        TirExprKind::Cast { expr, .. } => {
            replace_in_expr(expr, candidates);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                replace_in_expr(&mut arg.expr, candidates);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            replace_in_expr(receiver, candidates);
            for arg in args {
                replace_in_expr(&mut arg.expr, candidates);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                replace_in_expr(arg, candidates);
            }
        }
        TirExprKind::Index { expr, index } => {
            replace_in_expr(expr, candidates);
            replace_in_expr(index, candidates);
        }
        TirExprKind::Block(block) => {
            replace_in_block_stmts(block, candidates);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            replace_in_expr(condition, candidates);
            replace_in_block_stmts(then_branch, candidates);
            if let Some(eb) = else_branch {
                replace_in_block_stmts(eb, candidates);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                replace_in_expr(&mut field.value, candidates);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                replace_in_expr(elem, candidates);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            replace_in_expr(callee, candidates);
            for arg in args {
                replace_in_expr(arg, candidates);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            replace_in_expr(functor, candidates);
        }
        TirExprKind::Closure { body, .. } => {
            replace_in_expr(body, candidates);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                replace_in_expr(p, candidates);
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            replace_in_block_stmts(block, candidates);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            replace_in_expr(value, candidates);
        }
        TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
            replace_in_expr(expr, candidates);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            replace_in_expr(expr, candidates);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            replace_in_expr(scrutinee, candidates);
            for arm in arms {
                replace_in_block_stmts(arm, candidates);
            }
            replace_in_block_stmts(default, candidates);
        }
        TirExprKind::Assign { target, value } => {
            replace_in_expr(target, candidates);
            replace_in_expr(value, candidates);
        }
        _ => {}
    }
}
