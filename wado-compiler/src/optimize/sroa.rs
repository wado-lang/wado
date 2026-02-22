//! Scalar Replacement of Aggregates (SROA) optimization for Wado TIR
//!
//! This pass eliminates struct and tuple allocations when the aggregate is only
//! used for field access. After inlining exposes patterns like:
//!
//! ```text
//! let s = MyStruct { x: expr1, y: expr2 };
//! let a = s.x;
//! let b = s.y;
//! ```
//!
//! SROA decomposes the struct into individual scalar locals:
//!
//! ```text
//! let __sroa_s_x = expr1;
//! let __sroa_s_y = expr2;
//! let a = __sroa_s_x;
//! let b = __sroa_s_y;
//! ```
//!
//! Copy propagation then eliminates the trivial copies.
//!
//! This is the single most impactful optimization for WasmGC-targeting compilers,
//! as struct allocations are GC-managed heap objects.

use crate::project::Project;
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TirUnaryOp, TypeId,
    TypeTable,
};
use indexmap::IndexMap;
use indexmap::IndexSet;

/// Information about a struct/tuple local that may be decomposable.
struct SroaCandidate {
    /// Local index of the original aggregate variable
    local_index: u32,
    /// Name of the original variable
    local_name: String,
    /// Per-field info: (`field_name`, `field_type_id`)
    fields: Vec<(String, TypeId)>,
    /// Whether the original let binding was mutable
    is_mut: bool,
}

/// Apply SROA to all functions in the project.
pub fn scalar_replace_aggregates(project: &mut Project) -> bool {
    let mut changed = false;
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            changed |= sroa_in_function(&mut func, &type_table);
        }
    }
    changed
}

/// Apply SROA within a single function.
fn sroa_in_function(func: &mut TirFunction, type_table: &TypeTable) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };

    // Step 1: Identify candidate Let bindings (struct/tuple literals).
    let candidates = collect_candidates(&body.stmts, type_table);
    if candidates.is_empty() {
        return false;
    }

    // Step 2: Escape analysis — check every use of each candidate.
    let escaped = find_escaped_locals(body, &candidates);

    // Filter to non-escaped candidates.
    let safe: Vec<SroaCandidate> = candidates
        .into_iter()
        .filter(|c| !escaped.contains(&c.local_index))
        .collect();
    if safe.is_empty() {
        return false;
    }

    // Step 3: Allocate scalar locals for each field of each safe candidate.
    // Map: (original_local, field_index) → new_local_index
    let mut field_local_map: IndexMap<(u32, u32), u32> = IndexMap::new();
    // Map: (original_local, field_index) → (new_local_name, field_type)
    let mut field_info_map: IndexMap<(u32, u32), (String, TypeId)> = IndexMap::new();
    let safe_set: IndexSet<u32> = safe.iter().map(|c| c.local_index).collect();

    for candidate in &safe {
        let base = func.local_count;
        for (i, (field_name, field_type)) in candidate.fields.iter().enumerate() {
            let new_index = base + i as u32;
            field_local_map.insert((candidate.local_index, i as u32), new_index);
            let new_name = format!("__sroa_{}_{}", candidate.local_name, field_name);
            field_info_map.insert((candidate.local_index, i as u32), (new_name, *field_type));
            func.local_types.push(*field_type);
        }
        func.local_count += candidate.fields.len() as u32;
    }

    // Collect mutability info for safe candidates.
    let mut candidate_mut: IndexMap<u32, bool> = IndexMap::new();
    for candidate in &safe {
        candidate_mut.insert(candidate.local_index, candidate.is_mut);
    }

    // Step 4: Rewrite — expand Let statements and replace field accesses.
    rewrite_block(
        body,
        &safe_set,
        &field_local_map,
        &field_info_map,
        &candidate_mut,
    );

    true
}

/// Collect SROA candidates from `Let` statements binding struct/tuple literals.
fn collect_candidates(stmts: &[TirStmt], type_table: &TypeTable) -> Vec<SroaCandidate> {
    let mut candidates = Vec::new();
    collect_candidates_in_stmts(stmts, type_table, &mut candidates);
    candidates
}

fn collect_candidates_in_stmts(
    stmts: &[TirStmt],
    type_table: &TypeTable,
    candidates: &mut Vec<SroaCandidate>,
) {
    for stmt in stmts {
        collect_candidates_in_stmt(stmt, type_table, candidates);
    }
}

fn collect_candidates_in_stmt(
    stmt: &TirStmt,
    type_table: &TypeTable,
    candidates: &mut Vec<SroaCandidate>,
) {
    match &stmt.kind {
        TirStmtKind::Let {
            name,
            local_index,
            is_mut,
            value,
            ..
        } => {
            // Check if value is a struct literal or tuple literal
            match &value.kind {
                TirExprKind::StructLiteral { fields, .. } => {
                    let field_info: Vec<(String, TypeId)> = fields
                        .iter()
                        .map(|f| (f.name.clone(), f.value.type_id))
                        .collect();
                    candidates.push(SroaCandidate {
                        local_index: *local_index,
                        local_name: name.clone(),
                        fields: field_info,
                        is_mut: *is_mut,
                    });
                }
                TirExprKind::TupleLiteral { elements, .. } => {
                    let field_info: Vec<(String, TypeId)> = elements
                        .iter()
                        .enumerate()
                        .map(|(i, e)| (i.to_string(), e.type_id))
                        .collect();
                    candidates.push(SroaCandidate {
                        local_index: *local_index,
                        local_name: name.clone(),
                        fields: field_info,
                        is_mut: *is_mut,
                    });
                }
                _ => {}
            }
            // Also recurse into the value expression (for nested blocks etc.)
            collect_candidates_in_expr(value, type_table, candidates);
        }
        TirStmtKind::Expr(expr) => {
            collect_candidates_in_expr(expr, type_table, candidates);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_candidates_in_expr(v, type_table, candidates);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_candidates_in_expr(condition, type_table, candidates);
            collect_candidates_in_stmts(&then_block.stmts, type_table, candidates);
            if let Some(eb) = else_block {
                collect_candidates_in_stmts(&eb.stmts, type_table, candidates);
            }
        }
        TirStmtKind::Loop { body } => {
            collect_candidates_in_stmts(&body.stmts, type_table, candidates);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_candidates_in_stmts(&block.stmts, type_table, candidates);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_candidates_in_expr(scrutinee, type_table, candidates);
            collect_candidates_in_stmts(&then_block.stmts, type_table, candidates);
            if let Some(eb) = else_block {
                collect_candidates_in_stmts(&eb.stmts, type_table, candidates);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_candidates_in_expr(v, type_table, candidates);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            collect_candidates_in_expr(value, type_table, candidates);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

fn collect_candidates_in_expr(
    expr: &TirExpr,
    type_table: &TypeTable,
    candidates: &mut Vec<SroaCandidate>,
) {
    match &expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            collect_candidates_in_stmts(&block.stmts, type_table, candidates);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_candidates_in_expr(condition, type_table, candidates);
            collect_candidates_in_stmts(&then_branch.stmts, type_table, candidates);
            if let Some(eb) = else_branch {
                collect_candidates_in_stmts(&eb.stmts, type_table, candidates);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            collect_candidates_in_expr(inner, type_table, candidates);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_candidates_in_expr(guard, type_table, candidates);
                }
                collect_candidates_in_expr(&arm.body, type_table, candidates);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_candidates_in_expr(scrutinee, type_table, candidates);
            for arm in arms {
                collect_candidates_in_stmts(&arm.stmts, type_table, candidates);
            }
            collect_candidates_in_stmts(&default.stmts, type_table, candidates);
        }
        TirExprKind::Closure { body, .. } => {
            collect_candidates_in_expr(body, type_table, candidates);
        }
        // Other expression kinds don't contain Let statements
        _ => {}
    }
}

/// Escape analysis: find all candidate locals that escape (used in non-field-access positions).
fn find_escaped_locals(body: &TirBlock, candidates: &[SroaCandidate]) -> IndexSet<u32> {
    let candidate_set: IndexSet<u32> = candidates.iter().map(|c| c.local_index).collect();
    let mut escaped = IndexSet::new();
    check_escape_in_block(body, &candidate_set, &mut escaped);
    escaped
}

fn check_escape_in_block(
    block: &TirBlock,
    candidates: &IndexSet<u32>,
    escaped: &mut IndexSet<u32>,
) {
    for stmt in &block.stmts {
        check_escape_in_stmt(stmt, candidates, escaped);
    }
}

fn check_escape_in_stmt(stmt: &TirStmt, candidates: &IndexSet<u32>, escaped: &mut IndexSet<u32>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            check_escape_in_expr(value, candidates, escaped);
        }
        TirStmtKind::Expr(expr) => {
            check_escape_in_expr(expr, candidates, escaped);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                check_escape_in_expr(v, candidates, escaped);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            check_escape_in_expr(condition, candidates, escaped);
            check_escape_in_block(then_block, candidates, escaped);
            if let Some(eb) = else_block {
                check_escape_in_block(eb, candidates, escaped);
            }
        }
        TirStmtKind::Loop { body } => {
            check_escape_in_block(body, candidates, escaped);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            check_escape_in_block(block, candidates, escaped);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            check_escape_in_expr(scrutinee, candidates, escaped);
            check_escape_in_block(then_block, candidates, escaped);
            if let Some(eb) = else_block {
                check_escape_in_block(eb, candidates, escaped);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                check_escape_in_expr(v, candidates, escaped);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            check_escape_in_expr(value, candidates, escaped);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

/// Check if an expression causes any candidate locals to escape.
///
/// A local is "safe" (non-escaping) if it only appears as:
/// - `FieldAccess { expr: Local { index: candidate }, .. }` (field read)
/// - `FieldAccess { expr: Move { expr: Local { index: candidate } }, .. }` (field read through move)
/// - `Assign { target: FieldAccess { expr: Local { .. }, .. }, .. }` (field write)
///
/// Any other use of the local (passed to function, returned, address taken, etc.) is an escape.
fn check_escape_in_expr(expr: &TirExpr, candidates: &IndexSet<u32>, escaped: &mut IndexSet<u32>) {
    match &expr.kind {
        // FieldAccess on a candidate local is safe — don't mark the base local as escaped.
        // But do recurse into the non-base subexpressions.
        TirExprKind::FieldAccess { expr: inner, .. } => {
            if is_candidate_local(inner, candidates).is_some() {
                // Safe: field read on candidate. Don't recurse into inner (it's the local itself).
                return;
            }
            // If inner is Move { Local { candidate } }, also safe.
            if let TirExprKind::Move { expr: move_inner } = &inner.kind
                && is_candidate_local(move_inner, candidates).is_some()
            {
                return;
            }
            // Not a candidate — recurse normally
            check_escape_in_expr(inner, candidates, escaped);
        }

        // Assign to a field of a candidate is safe for the target side.
        TirExprKind::Assign { target, value } => {
            if let TirExprKind::FieldAccess { expr: inner, .. } = &target.kind
                && is_candidate_local(inner, candidates).is_some()
            {
                // Safe: field write. Only recurse into value, not target.
                check_escape_in_expr(value, candidates, escaped);
                return;
            }
            // Assign to a candidate local as a whole → escape
            check_escape_in_expr(target, candidates, escaped);
            check_escape_in_expr(value, candidates, escaped);
        }

        // Move wrapping a candidate is not an escape by itself —
        // it will be handled by the parent (FieldAccess checks for Move { Local }).
        // But if Move wraps a candidate and is NOT inside a FieldAccess, it IS an escape.
        // This case is handled naturally: if we reach here, it means the Move is not
        // inside a FieldAccess (that case returned early above), so we recurse normally.
        TirExprKind::Move { expr: inner } => {
            check_escape_in_expr(inner, candidates, escaped);
        }

        // A bare Local reference to a candidate in any other position → escape
        TirExprKind::Local { index, .. } => {
            if candidates.contains(index) {
                escaped.insert(*index);
            }
        }

        // Address taken → definitely escape
        TirExprKind::Unary { op, expr: inner } => {
            if matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
                && let TirExprKind::Local { index, .. } = &inner.kind
                && candidates.contains(index)
            {
                escaped.insert(*index);
                return;
            }
            check_escape_in_expr(inner, candidates, escaped);
        }

        // Closure captures → escape
        TirExprKind::Closure { body, captures, .. } => {
            for capture in captures {
                if candidates.contains(&capture.outer_index) {
                    escaped.insert(capture.outer_index);
                }
            }
            check_escape_in_expr(body, candidates, escaped);
        }

        // Recurse into all other expression kinds
        TirExprKind::Binary { left, right, .. } => {
            check_escape_in_expr(left, candidates, escaped);
            check_escape_in_expr(right, candidates, escaped);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                check_escape_in_expr(arg, candidates, escaped);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            check_escape_in_expr(receiver, candidates, escaped);
            for arg in args {
                check_escape_in_expr(arg, candidates, escaped);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            check_escape_in_expr(callee, candidates, escaped);
            for arg in args {
                check_escape_in_expr(arg, candidates, escaped);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            check_escape_in_expr(functor, candidates, escaped);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            check_escape_in_expr(inner, candidates, escaped);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            check_escape_in_expr(inner, candidates, escaped);
            check_escape_in_expr(index, candidates, escaped);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            check_escape_in_block(block, candidates, escaped);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            check_escape_in_expr(condition, candidates, escaped);
            check_escape_in_block(then_branch, candidates, escaped);
            if let Some(eb) = else_branch {
                check_escape_in_block(eb, candidates, escaped);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            check_escape_in_expr(inner, candidates, escaped);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    check_escape_in_expr(guard, candidates, escaped);
                }
                check_escape_in_expr(&arm.body, candidates, escaped);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                check_escape_in_expr(&field.value, candidates, escaped);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } | TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                check_escape_in_expr(elem, candidates, escaped);
            }
        }
        TirExprKind::OptionSome { value } => {
            check_escape_in_expr(value, candidates, escaped);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                check_escape_in_expr(p, candidates, escaped);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            check_escape_in_expr(value, candidates, escaped);
        }
        TirExprKind::IsNotNull { expr }
        | TirExprKind::UnwrapOption { expr, .. }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. } => {
            check_escape_in_expr(expr, candidates, escaped);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            check_escape_in_expr(expr, candidates, escaped);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            check_escape_in_expr(scrutinee, candidates, escaped);
            for arm in arms {
                check_escape_in_block(arm, candidates, escaped);
            }
            check_escape_in_block(default, candidates, escaped);
        }
        // Leaf nodes — no locals to check
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Check if an expression is a `Local` node referencing a candidate.
fn is_candidate_local(expr: &TirExpr, candidates: &IndexSet<u32>) -> Option<u32> {
    if let TirExprKind::Local { index, .. } = &expr.kind
        && candidates.contains(index)
    {
        return Some(*index);
    }
    None
}

/// Rewrite a block: expand Let statements for candidates and replace field accesses.
fn rewrite_block(
    block: &mut TirBlock,
    safe_set: &IndexSet<u32>,
    field_map: &IndexMap<(u32, u32), u32>,
    info_map: &IndexMap<(u32, u32), (String, TypeId)>,
    candidate_mut: &IndexMap<u32, bool>,
) {
    // Process statements, potentially expanding one statement into multiple.
    let old_stmts = std::mem::take(&mut block.stmts);
    let mut new_stmts = Vec::with_capacity(old_stmts.len());

    for mut stmt in old_stmts {
        if let TirStmtKind::Let { local_index, .. } = &stmt.kind
            && safe_set.contains(local_index)
        {
            let local_idx = *local_index;
            let span = stmt.span;
            let is_mut = candidate_mut.get(&local_idx).copied().unwrap_or(false);

            // Expand into per-field Let statements
            match stmt.kind {
                TirStmtKind::Let { value, .. } => {
                    match value.kind {
                        TirExprKind::StructLiteral { fields, .. } => {
                            // Sort fields by field_index to match the candidate's field order
                            let mut sorted_fields: Vec<_> = fields.into_iter().collect();
                            sorted_fields.sort_by_key(|f| f.field_index);
                            for mut field in sorted_fields {
                                // Rewrite references to other SROA'd locals
                                // within the field value expression.
                                rewrite_expr(
                                    &mut field.value,
                                    safe_set,
                                    field_map,
                                    info_map,
                                    candidate_mut,
                                );
                                let key = (local_idx, field.field_index);
                                let new_local = field_map[&key];
                                let (new_name, field_type) = &info_map[&key];
                                new_stmts.push(TirStmt::new(
                                    TirStmtKind::Let {
                                        name: new_name.clone(),
                                        local_index: new_local,
                                        is_mut,
                                        is_reactive: false,
                                        type_id: *field_type,
                                        value: field.value,
                                    },
                                    span,
                                ));
                            }
                        }
                        TirExprKind::TupleLiteral { elements, .. } => {
                            for (i, mut elem) in elements.into_iter().enumerate() {
                                rewrite_expr(
                                    &mut elem,
                                    safe_set,
                                    field_map,
                                    info_map,
                                    candidate_mut,
                                );
                                let key = (local_idx, i as u32);
                                let new_local = field_map[&key];
                                let (new_name, field_type) = &info_map[&key];
                                new_stmts.push(TirStmt::new(
                                    TirStmtKind::Let {
                                        name: new_name.clone(),
                                        local_index: new_local,
                                        is_mut,
                                        is_reactive: false,
                                        type_id: *field_type,
                                        value: elem,
                                    },
                                    span,
                                ));
                            }
                        }
                        _ => unreachable!("candidate must be struct or tuple literal"),
                    }
                }
                _ => unreachable!("candidate must be Let statement"),
            }
            continue;
        }

        // Not a candidate Let — rewrite field accesses in the statement.
        rewrite_stmt(&mut stmt, safe_set, field_map, info_map, candidate_mut);
        new_stmts.push(stmt);
    }

    block.stmts = new_stmts;
}

fn rewrite_stmt(
    stmt: &mut TirStmt,
    safe_set: &IndexSet<u32>,
    field_map: &IndexMap<(u32, u32), u32>,
    info_map: &IndexMap<(u32, u32), (String, TypeId)>,
    candidate_mut: &IndexMap<u32, bool>,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            rewrite_expr(value, safe_set, field_map, info_map, candidate_mut);
        }
        TirStmtKind::Expr(expr) => {
            rewrite_expr(expr, safe_set, field_map, info_map, candidate_mut);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                rewrite_expr(v, safe_set, field_map, info_map, candidate_mut);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_expr(condition, safe_set, field_map, info_map, candidate_mut);
            rewrite_block(then_block, safe_set, field_map, info_map, candidate_mut);
            if let Some(eb) = else_block {
                rewrite_block(eb, safe_set, field_map, info_map, candidate_mut);
            }
        }
        TirStmtKind::Loop { body } => {
            rewrite_block(body, safe_set, field_map, info_map, candidate_mut);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            rewrite_block(block, safe_set, field_map, info_map, candidate_mut);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rewrite_expr(scrutinee, safe_set, field_map, info_map, candidate_mut);
            rewrite_block(then_block, safe_set, field_map, info_map, candidate_mut);
            if let Some(eb) = else_block {
                rewrite_block(eb, safe_set, field_map, info_map, candidate_mut);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_expr(v, safe_set, field_map, info_map, candidate_mut);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            rewrite_expr(value, safe_set, field_map, info_map, candidate_mut);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

/// Rewrite an expression: replace `s.field` with the corresponding scalar local.
fn rewrite_expr(
    expr: &mut TirExpr,
    safe_set: &IndexSet<u32>,
    field_map: &IndexMap<(u32, u32), u32>,
    info_map: &IndexMap<(u32, u32), (String, TypeId)>,
    candidate_mut: &IndexMap<u32, bool>,
) {
    // Check for field access on a candidate local: s.field → scalar local
    if let TirExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &expr.kind
    {
        // Direct: candidate.field
        if let Some(local_idx) = is_candidate_local(inner, safe_set) {
            let key = (local_idx, *field_index);
            if let Some(&new_local) = field_map.get(&key) {
                let (new_name, _) = &info_map[&key];
                expr.kind = TirExprKind::Local {
                    index: new_local,
                    name: new_name.clone(),
                };
                return;
            }
        }
        // Through Move: Move { candidate }.field
        if let TirExprKind::Move { expr: move_inner } = &inner.kind
            && let Some(local_idx) = is_candidate_local(move_inner, safe_set)
        {
            let field_idx = *field_index;
            let key = (local_idx, field_idx);
            if let Some(&new_local) = field_map.get(&key) {
                let (new_name, _) = &info_map[&key];
                expr.kind = TirExprKind::Local {
                    index: new_local,
                    name: new_name.clone(),
                };
                return;
            }
        }
    }

    // Check for field write: candidate.field = value → scalar_local = value
    if let TirExprKind::Assign { target, value } = &mut expr.kind
        && let TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } = &target.kind
        && let Some(local_idx) = is_candidate_local(inner, safe_set)
    {
        let key = (local_idx, *field_index);
        if let Some(&new_local) = field_map.get(&key) {
            let (new_name, _) = &info_map[&key];
            // Rewrite: Assign { target: FieldAccess, value } → Assign { target: Local, value }
            target.kind = TirExprKind::Local {
                index: new_local,
                name: new_name.clone(),
            };
            rewrite_expr(value, safe_set, field_map, info_map, candidate_mut);
            return;
        }
    }

    // Recurse into child expressions
    match &mut expr.kind {
        TirExprKind::FieldAccess { expr: inner, .. } => {
            rewrite_expr(inner, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::Assign { target, value } => {
            rewrite_expr(target, safe_set, field_map, info_map, candidate_mut);
            rewrite_expr(value, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::Binary { left, right, .. } => {
            rewrite_expr(left, safe_set, field_map, info_map, candidate_mut);
            rewrite_expr(right, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            rewrite_expr(inner, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            rewrite_expr(inner, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::Move { expr: inner } => {
            rewrite_expr(inner, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                rewrite_expr(arg, safe_set, field_map, info_map, candidate_mut);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            rewrite_expr(receiver, safe_set, field_map, info_map, candidate_mut);
            for arg in args {
                rewrite_expr(arg, safe_set, field_map, info_map, candidate_mut);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            rewrite_expr(callee, safe_set, field_map, info_map, candidate_mut);
            for arg in args {
                rewrite_expr(arg, safe_set, field_map, info_map, candidate_mut);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            rewrite_expr(functor, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            rewrite_expr(inner, safe_set, field_map, info_map, candidate_mut);
            rewrite_expr(index, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            rewrite_block(block, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_expr(condition, safe_set, field_map, info_map, candidate_mut);
            rewrite_block(then_branch, safe_set, field_map, info_map, candidate_mut);
            if let Some(eb) = else_branch {
                rewrite_block(eb, safe_set, field_map, info_map, candidate_mut);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            rewrite_expr(inner, safe_set, field_map, info_map, candidate_mut);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    rewrite_expr(guard, safe_set, field_map, info_map, candidate_mut);
                }
                rewrite_expr(&mut arm.body, safe_set, field_map, info_map, candidate_mut);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                rewrite_expr(
                    &mut field.value,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                );
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } | TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                rewrite_expr(elem, safe_set, field_map, info_map, candidate_mut);
            }
        }
        TirExprKind::OptionSome { value } => {
            rewrite_expr(value, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                rewrite_expr(p, safe_set, field_map, info_map, candidate_mut);
            }
        }
        TirExprKind::Closure { body, .. } => {
            rewrite_expr(body, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            rewrite_expr(value, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::IsNotNull { expr }
        | TirExprKind::UnwrapOption { expr, .. }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. } => {
            rewrite_expr(expr, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            rewrite_expr(expr, safe_set, field_map, info_map, candidate_mut);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            rewrite_expr(scrutinee, safe_set, field_map, info_map, candidate_mut);
            for arm in arms {
                rewrite_block(arm, safe_set, field_map, info_map, candidate_mut);
            }
            rewrite_block(default, safe_set, field_map, info_map, candidate_mut);
        }
        // Leaf nodes — nothing to rewrite
        TirExprKind::Local { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}
