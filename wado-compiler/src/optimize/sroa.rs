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

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::name::ModuleSource;
use crate::tir::{
    FunctionRef, TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TirStructField,
    TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;
use crate::token::Span;

/// Maps (`module_source`, `func_name`) → set of parameter indices that have `stores` declared.
type StoresLookup = IndexMap<(ModuleSource, String), IndexSet<usize>>;

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
    /// The type of the aggregate (needed for reconstruction at escape sites)
    aggregate_type_id: TypeId,
    /// The struct name (for `StructLiteral` reconstruction; empty for tuples)
    struct_name: String,
}

/// Build a lookup table mapping (`module_source`, `func_name`) → set of stored param indices.
fn build_stores_lookup(project: &FlatPackage) -> StoresLookup {
    let mut lookup = StoresLookup::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if func.stores.is_empty() {
            continue;
        }
        let stored_indices: IndexSet<usize> = func
            .params
            .iter()
            .enumerate()
            .filter(|(_, param)| func.stores.iter().any(|s| s == &param.name))
            .map(|(i, _)| i)
            .collect();
        if !stored_indices.is_empty() {
            lookup.insert(
                (func.module_source.clone(), func.name.clone()),
                stored_indices,
            );
        }
    }
    lookup
}

/// Apply SROA to all functions in the project.
pub fn scalar_replace_aggregates(project: &mut FlatPackage) -> bool {
    let stores_lookup = build_stores_lookup(project);
    let type_table = project.type_table.borrow();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let module_source = func.module_source.clone();
        changed |= sroa_in_function(&mut func, &type_table, &stores_lookup, &module_source);
    }
    changed
}

/// Apply SROA within a single function.
fn sroa_in_function(
    func: &mut TirFunction,
    type_table: &TypeTable,
    stores_lookup: &StoresLookup,
    current_module: &ModuleSource,
) -> bool {
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

    // Step 2b: For escaped candidates, check if all escapes are "soft" (reconstructible).
    // Soft escapes: used as call argument, returned, or used in struct/tuple literal.
    // Hard escapes: address taken, closure capture, bare local assignment, etc.
    // With stores-aware analysis: &candidate in call args to non-stores functions is soft.
    let soft_escaped =
        find_soft_escaped_locals(body, &candidates, &escaped, stores_lookup, current_module);

    // Filter candidates: non-escaped are "safe", soft-escaped are "reconstruct".
    // Locals in stores_aliased_locals are treated as hard-escaped (their references
    // were stored by an inlined stores function or a previously decomposed struct).
    let mut safe_set: IndexSet<u32> = IndexSet::default();
    let mut reconstruct_set: IndexSet<u32> = IndexSet::default();
    for c in &candidates {
        if func.stores_aliased_locals.contains(&c.local_index) {
            continue; // Blocked: reference was stored, unsafe to decompose
        }
        if !escaped.contains(&c.local_index) {
            safe_set.insert(c.local_index);
        } else if soft_escaped.contains(&c.local_index) {
            reconstruct_set.insert(c.local_index);
        }
    }

    // Combined set of all SROA'd candidates (safe + reconstruct)
    let all_sroa: IndexSet<u32> = safe_set
        .iter()
        .chain(reconstruct_set.iter())
        .copied()
        .collect();
    if all_sroa.is_empty() {
        return false;
    }

    // Step 3: Allocate scalar locals for each field of each SROA'd candidate.
    // Map: (original_local, field_index) → new_local_index
    let mut field_local_map: IndexMap<(u32, u32), u32> = IndexMap::default();
    // Map: (original_local, field_index) → (new_local_name, field_type)
    let mut field_info_map: IndexMap<(u32, u32), (String, TypeId)> = IndexMap::default();

    for candidate in &candidates {
        if !all_sroa.contains(&candidate.local_index) {
            continue;
        }
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

    // Collect mutability and reconstruction info for SROA'd candidates.
    let mut candidate_mut: IndexMap<u32, bool> = IndexMap::default();
    let mut reconstruct_info: IndexMap<u32, ReconstructInfo> = IndexMap::default();
    for candidate in &candidates {
        if !all_sroa.contains(&candidate.local_index) {
            continue;
        }
        candidate_mut.insert(candidate.local_index, candidate.is_mut);
        if reconstruct_set.contains(&candidate.local_index) {
            reconstruct_info.insert(
                candidate.local_index,
                ReconstructInfo {
                    struct_name: candidate.struct_name.clone(),
                    aggregate_type_id: candidate.aggregate_type_id,
                    fields: candidate.fields.clone(),
                },
            );
        }
    }

    // Step 3b: Mark locals referenced via &local in decomposed struct fields.
    // When SROA decomposes `let h = S { field: &p }` into `__sroa_h_field = &p`,
    // future ref_elim may eliminate the `&p`, hiding the aliasing from future SROA.
    // Mark `p` as stores-aliased so it won't be decomposed in later iterations.
    mark_ref_field_locals_as_aliased(body, &all_sroa, &mut func.stores_aliased_locals);

    // Step 4: Rewrite — expand Let statements and replace field accesses.
    rewrite_block(
        body,
        &all_sroa,
        &field_local_map,
        &field_info_map,
        &candidate_mut,
        &reconstruct_info,
    );

    true
}

/// When SROA decomposes a struct with `&local` field values, mark those locals
/// as stores-aliased to prevent future SROA from decomposing them.
fn mark_ref_field_locals_as_aliased(
    body: &TirBlock,
    decomposed: &IndexSet<u32>,
    stores_aliased: &mut IndexSet<u32>,
) {
    for stmt in &body.stmts {
        mark_ref_fields_in_stmt(stmt, decomposed, stores_aliased);
    }
}

fn mark_ref_fields_in_stmt(
    stmt: &TirStmt,
    decomposed: &IndexSet<u32>,
    stores_aliased: &mut IndexSet<u32>,
) {
    match &stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            if decomposed.contains(local_index) {
                // This candidate is being decomposed — scan field values for &local
                collect_ref_locals_in_fields(value, stores_aliased);
            }
            mark_ref_fields_in_expr(value, decomposed, stores_aliased);
        }
        TirStmtKind::Expr(expr) => {
            mark_ref_fields_in_expr(expr, decomposed, stores_aliased);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                mark_ref_fields_in_expr(v, decomposed, stores_aliased);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            mark_ref_fields_in_expr(condition, decomposed, stores_aliased);
            mark_ref_field_locals_as_aliased(then_block, decomposed, stores_aliased);
            if let Some(eb) = else_block {
                mark_ref_field_locals_as_aliased(eb, decomposed, stores_aliased);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            mark_ref_field_locals_as_aliased(body, decomposed, stores_aliased);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            mark_ref_fields_in_expr(scrutinee, decomposed, stores_aliased);
            mark_ref_field_locals_as_aliased(then_block, decomposed, stores_aliased);
            if let Some(eb) = else_block {
                mark_ref_field_locals_as_aliased(eb, decomposed, stores_aliased);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                mark_ref_fields_in_expr(v, decomposed, stores_aliased);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => {
            mark_ref_fields_in_expr(value, decomposed, stores_aliased);
        }
        TirStmtKind::Continue => {}
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn mark_ref_fields_in_expr(
    expr: &TirExpr,
    decomposed: &IndexSet<u32>,
    stores_aliased: &mut IndexSet<u32>,
) {
    match &expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            mark_ref_field_locals_as_aliased(block, decomposed, stores_aliased);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            mark_ref_fields_in_expr(condition, decomposed, stores_aliased);
            mark_ref_field_locals_as_aliased(then_branch, decomposed, stores_aliased);
            if let Some(eb) = else_branch {
                mark_ref_field_locals_as_aliased(eb, decomposed, stores_aliased);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            mark_ref_fields_in_expr(inner, decomposed, stores_aliased);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    mark_ref_fields_in_expr(guard, decomposed, stores_aliased);
                }
                mark_ref_fields_in_expr(&arm.body, decomposed, stores_aliased);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            mark_ref_fields_in_expr(scrutinee, decomposed, stores_aliased);
            for arm in arms {
                mark_ref_field_locals_as_aliased(arm, decomposed, stores_aliased);
            }
            mark_ref_field_locals_as_aliased(default, decomposed, stores_aliased);
        }
        TirExprKind::Binary { left, right, .. } => {
            mark_ref_fields_in_expr(left, decomposed, stores_aliased);
            mark_ref_fields_in_expr(right, decomposed, stores_aliased);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::ClosureToCanonical { functor: inner, .. } => {
            mark_ref_fields_in_expr(inner, decomposed, stores_aliased);
        }
        TirExprKind::Assign { target, value } => {
            mark_ref_fields_in_expr(target, decomposed, stores_aliased);
            mark_ref_fields_in_expr(value, decomposed, stores_aliased);
        }
        TirExprKind::Index { expr: inner, index } => {
            mark_ref_fields_in_expr(inner, decomposed, stores_aliased);
            mark_ref_fields_in_expr(index, decomposed, stores_aliased);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                mark_ref_fields_in_expr(&arg.expr, decomposed, stores_aliased);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                mark_ref_fields_in_expr(arg, decomposed, stores_aliased);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            mark_ref_fields_in_expr(receiver, decomposed, stores_aliased);
            for arg in args {
                mark_ref_fields_in_expr(&arg.expr, decomposed, stores_aliased);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            mark_ref_fields_in_expr(callee, decomposed, stores_aliased);
            for arg in args {
                mark_ref_fields_in_expr(arg, decomposed, stores_aliased);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                mark_ref_fields_in_expr(&field.value, decomposed, stores_aliased);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                mark_ref_fields_in_expr(elem, decomposed, stores_aliased);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                mark_ref_fields_in_expr(p, decomposed, stores_aliased);
            }
        }
        TirExprKind::Closure { body, .. } => {
            mark_ref_fields_in_expr(body, decomposed, stores_aliased);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            mark_ref_fields_in_expr(value, decomposed, stores_aliased);
        }
        // Leaf nodes
        TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!("WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase")
        }
    }
}

/// Collect local indices from `&local` expressions in struct/tuple literal fields.
fn collect_ref_locals_in_fields(expr: &TirExpr, stores_aliased: &mut IndexSet<u32>) {
    match &expr.kind {
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                extract_ref_local(&field.value, stores_aliased);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                extract_ref_local(elem, stores_aliased);
            }
        }
        _ => {}
    }
}

/// If `expr` is `&local` or `&mut local`, add the local index to the set.
fn extract_ref_local(expr: &TirExpr, stores_aliased: &mut IndexSet<u32>) {
    if let TirExprKind::Unary { op, expr: inner } = &expr.kind
        && matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
        && let TirExprKind::Local { index, .. } = &inner.kind
    {
        stores_aliased.insert(*index);
    }
}

/// Info needed to reconstruct a struct literal from SROA'd fields at escape sites.
struct ReconstructInfo {
    struct_name: String,
    aggregate_type_id: TypeId,
    fields: Vec<(String, TypeId)>,
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
            match &value.kind {
                TirExprKind::StructLiteral {
                    struct_name,
                    fields,
                    ..
                } => {
                    let field_info: Vec<(String, TypeId)> = fields
                        .iter()
                        .map(|f| (f.name.clone(), f.value.type_id))
                        .collect();
                    candidates.push(SroaCandidate {
                        local_index: *local_index,
                        local_name: name.clone(),
                        fields: field_info,
                        is_mut: *is_mut,
                        aggregate_type_id: value.type_id,
                        struct_name: struct_name.clone(),
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
                        aggregate_type_id: value.type_id,
                        struct_name: String::new(),
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
        TirStmtKind::IfLet {
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
        TirStmtKind::LetDestructure { value, .. } => {
            collect_candidates_in_expr(value, type_table, candidates);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
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
    let mut checker = EscapeChecker {
        candidates: &candidate_set,
        escaped: IndexSet::default(),
    };
    checker.visit_block(body);
    checker.escaped
}

/// Visitor that marks candidate locals as escaped if they appear outside of
/// field-access positions.
///
/// A local is "safe" (non-escaping) if it only appears as:
/// - `FieldAccess { expr: Local { index: candidate }, .. }` (field read)
/// - `Assign { target: FieldAccess { expr: Local { .. }, .. }, .. }` (field write)
///
/// Any other use of the local (passed to function, returned, address taken,
/// captured by a closure, etc.) is an escape.
struct EscapeChecker<'a> {
    candidates: &'a IndexSet<u32>,
    escaped: IndexSet<u32>,
}

impl TirRefVisitor for EscapeChecker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
            _ => self.walk_stmt(stmt),
        }
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            // FieldAccess on a candidate local is safe — don't mark the base local as
            // escaped and don't recurse into the base (which is the local itself).
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
                if is_candidate_local(inner, self.candidates).is_some() {
                    return;
                }
                self.visit_expr(inner);
            }
            // Assign to a field of a candidate is safe for the target side.
            TirExprKind::Assign { target, value } => {
                if let TirExprKind::FieldAccess { expr: inner, .. }
                | TirExprKind::TupleSpread { expr: inner }
                | TirExprKind::TupleZip { expr: inner }
                | TirExprKind::TypePackExpansion {
                    call_expr: inner, ..
                } = &target.kind
                    && is_candidate_local(inner, self.candidates).is_some()
                {
                    self.visit_expr(value);
                    return;
                }
                self.visit_expr(target);
                self.visit_expr(value);
            }
            // A bare Local reference to a candidate in any other position → escape.
            TirExprKind::Local { index, .. } => {
                if self.candidates.contains(index) {
                    self.escaped.insert(*index);
                }
            }
            // Address taken → definitely escape.
            TirExprKind::Unary { op, expr: inner } => {
                if matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
                    && let TirExprKind::Local { index, .. } = &inner.kind
                    && self.candidates.contains(index)
                {
                    self.escaped.insert(*index);
                    return;
                }
                self.visit_expr(inner);
            }
            // Closure captures → escape.
            TirExprKind::Closure { body, captures, .. } => {
                for capture in captures {
                    if self.candidates.contains(&capture.outer_index) {
                        self.escaped.insert(capture.outer_index);
                    }
                }
                self.visit_expr(body);
            }
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
            _ => self.walk_expr(expr),
        }
    }
}

/// Determine which escaped candidates have ONLY "soft" escapes (reconstructible).
///
/// A "soft escape" means the candidate is used as:
/// - A function call argument (`Call`, `MethodCall`, `StaticCall`), possibly wrapped in Move
/// - A return value
///
/// These can be handled by reconstructing the struct literal from SROA'd fields.
/// Candidates with hard escapes (address taken, closure capture, etc.) are excluded.
fn find_soft_escaped_locals(
    body: &TirBlock,
    candidates: &[SroaCandidate],
    escaped: &IndexSet<u32>,
    stores_lookup: &StoresLookup,
    current_module: &ModuleSource,
) -> IndexSet<u32> {
    // Only check candidates that actually escaped
    let escaped_candidates: IndexSet<u32> = candidates
        .iter()
        .map(|c| c.local_index)
        .filter(|idx| escaped.contains(idx))
        .collect();
    if escaped_candidates.is_empty() {
        return IndexSet::default();
    }

    // Does every non-field-access use of this candidate appear in a soft position
    // (return value, break value, or immutable ref to a non-stores callee)?
    let hard_escaped = {
        let mut checker = SoftEscapeChecker {
            candidates: &escaped_candidates,
            hard_escaped: IndexSet::default(),
            stores_lookup,
            current_module,
            soft_allowed: false,
        };
        checker.visit_block(body);
        checker.hard_escaped
    };

    // Soft-escaped = escaped but NOT hard-escaped, AND has at least one field access.
    let has_field_access = {
        let mut checker = FieldAccessChecker {
            candidates: &escaped_candidates,
            has_access: IndexSet::default(),
        };
        checker.visit_block(body);
        checker.has_access
    };

    escaped_candidates
        .into_iter()
        .filter(|idx| !hard_escaped.contains(idx) && has_field_access.contains(idx))
        .collect()
}

/// Visitor that records which escaped candidates have at least one field-access use.
/// Used to filter out candidates whose only uses are in call arguments or return
/// values — SROA would not help those, since there are no loads/stores to eliminate.
struct FieldAccessChecker<'a> {
    candidates: &'a IndexSet<u32>,
    has_access: IndexSet<u32>,
}

impl TirRefVisitor for FieldAccessChecker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            // The original walker ignored these stmt kinds entirely — preserve.
            TirStmtKind::LetDestructure { .. }
            | TirStmtKind::TaskReturn { .. }
            | TirStmtKind::VariadicForOf { .. } => {}
            _ => self.walk_stmt(stmt),
        }
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
                if let Some(idx) = is_candidate_local(inner, self.candidates) {
                    self.has_access.insert(idx);
                    return;
                }
                self.visit_expr(inner);
            }
            TirExprKind::Assign { target, value } => {
                if let TirExprKind::FieldAccess { expr: inner, .. }
                | TirExprKind::TupleSpread { expr: inner }
                | TirExprKind::TupleZip { expr: inner }
                | TirExprKind::TypePackExpansion {
                    call_expr: inner, ..
                } = &target.kind
                    && let Some(idx) = is_candidate_local(inner, self.candidates)
                {
                    self.has_access.insert(idx);
                    self.visit_expr(value);
                    return;
                }
                self.visit_expr(target);
                self.visit_expr(value);
            }
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
            _ => self.walk_expr(expr),
        }
    }
}

/// Visitor that checks escaped candidates for "hard" escapes (non-reconstructible).
/// A candidate is marked hard-escaped if any use is non-reconstructible.
///
/// `soft_allowed` is set by `visit_stmt` when visiting a Return/Break value and
/// consumed by the first call to `visit_expr` on the top-level expression only,
/// so that only a bare `Local` at the very top of a Return/Break value is treated
/// as a "soft" escape.  Call arguments are NOT soft: in Wasm GC, structs are
/// heap-allocated and passed by reference, so a callee receiving a reconstructed
/// (new) object would modify that fresh copy instead of the original — losing the
/// mutation.  The exception is `&candidate` passed to a callee that does not
/// declare `stores` for that parameter, which is always safe.
struct SoftEscapeChecker<'a> {
    candidates: &'a IndexSet<u32>,
    hard_escaped: IndexSet<u32>,
    stores_lookup: &'a StoresLookup,
    current_module: &'a ModuleSource,
    soft_allowed: bool,
}

impl TirRefVisitor for SoftEscapeChecker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Return { value: Some(v) } | TirStmtKind::Break { value: Some(v), .. } => {
                self.soft_allowed = true;
                self.visit_expr(v);
                self.soft_allowed = false;
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
            _ => self.walk_stmt(stmt),
        }
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        // Consume the soft-context flag at the top of the first visit only.
        // All recursive visits see soft=false, mirroring the original walker's
        // `in_soft_context=false` for non-top-level children.
        let soft = std::mem::replace(&mut self.soft_allowed, false);
        match &expr.kind {
            // FieldAccess on candidate is always safe — skip recursion into the base.
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
                if is_candidate_local(inner, self.candidates).is_some() {
                    return;
                }
                self.visit_expr(inner);
            }
            // Assign to field of candidate is safe — only recurse into value.
            TirExprKind::Assign { target, value } => {
                if let TirExprKind::FieldAccess { expr: inner, .. }
                | TirExprKind::TupleSpread { expr: inner }
                | TirExprKind::TupleZip { expr: inner }
                | TirExprKind::TypePackExpansion {
                    call_expr: inner, ..
                } = &target.kind
                    && is_candidate_local(inner, self.candidates).is_some()
                {
                    self.visit_expr(value);
                    return;
                }
                self.visit_expr(target);
                self.visit_expr(value);
            }
            // Bare Local in a soft context (return/break value) is OK.
            // Bare Local anywhere else → hard escape.
            TirExprKind::Local { index, .. } => {
                if self.candidates.contains(index) && !soft {
                    self.hard_escaped.insert(*index);
                }
            }
            // Address taken → hard escape.  The `&candidate` as a non-stores call
            // argument exception is handled by the Call/MethodCall arms below,
            // which skip visiting such args entirely.
            TirExprKind::Unary { op, expr: inner } => {
                if matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
                    && let TirExprKind::Local { index, .. } = &inner.kind
                    && self.candidates.contains(index)
                {
                    self.hard_escaped.insert(*index);
                    return;
                }
                self.visit_expr(inner);
            }
            // Closure captures → hard escape.
            TirExprKind::Closure { body, captures, .. } => {
                for capture in captures {
                    if self.candidates.contains(&capture.outer_index) {
                        self.hard_escaped.insert(capture.outer_index);
                    }
                }
                self.visit_expr(body);
            }
            // Call / MethodCall: skip `&candidate` args to non-stores callees.
            TirExprKind::Call { func, args, .. } => {
                for (i, arg) in args.iter().enumerate() {
                    if is_immut_ref_to_candidate(&arg.expr, self.candidates)
                        && !callee_stores_param_at(func, i, self.current_module, self.stores_lookup)
                    {
                        continue;
                    }
                    self.visit_expr(&arg.expr);
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                // &self on non-stores method → safe, skip receiver.
                if !is_immut_ref_to_candidate(receiver, self.candidates)
                    || callee_stores_param_at(func, 0, self.current_module, self.stores_lookup)
                {
                    self.visit_expr(receiver);
                }
                for (i, arg) in args.iter().enumerate() {
                    if is_immut_ref_to_candidate(&arg.expr, self.candidates)
                        && !callee_stores_param_at(
                            func,
                            i + 1,
                            self.current_module,
                            self.stores_lookup,
                        )
                    {
                        continue;
                    }
                    self.visit_expr(&arg.expr);
                }
            }
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
            _ => self.walk_expr(expr),
        }
    }
}

/// Check if an expression is `&candidate` (immutable ref to a candidate local).
fn is_immut_ref_to_candidate(expr: &TirExpr, candidates: &IndexSet<u32>) -> bool {
    if let TirExprKind::Unary { op, expr: inner } = &expr.kind
        && matches!(op, TirUnaryOp::Ref)
        && let TirExprKind::Local { index, .. } = &inner.kind
        && candidates.contains(index)
    {
        return true;
    }
    false
}

/// Check if a callee stores the parameter at the given index.
/// Returns true (conservative) if the callee is unknown or declares `stores` for the param.
fn callee_stores_param_at(
    func_ref: &FunctionRef,
    param_index: usize,
    current_module: &ModuleSource,
    stores_lookup: &StoresLookup,
) -> bool {
    let target_module = if func_ref.module_source.is_entry_point() {
        current_module.clone()
    } else {
        func_ref.module_source.clone()
    };
    let key = (target_module, func_ref.name.clone());
    match stores_lookup.get(&key) {
        Some(stored_indices) => stored_indices.contains(&param_index as &usize),
        None => false, // No stores declaration → param is not stored
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
    reconstruct_info: &IndexMap<u32, ReconstructInfo>,
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
                    expand_struct_let(
                        value,
                        local_idx,
                        is_mut,
                        span,
                        safe_set,
                        field_map,
                        info_map,
                        candidate_mut,
                        reconstruct_info,
                        &mut new_stmts,
                    );
                }
                _ => unreachable!("candidate must be Let statement"),
            }
            continue;
        }

        // Not a candidate Let — rewrite field accesses in the statement.
        rewrite_stmt(
            &mut stmt,
            safe_set,
            field_map,
            info_map,
            candidate_mut,
            reconstruct_info,
        );
        new_stmts.push(stmt);
    }

    block.stmts = new_stmts;
}

/// Expand a Let value (`StructLiteral`, `TupleLiteral`, or Move-wrapped) into per-field Lets.
fn expand_struct_let(
    value: TirExpr,
    local_idx: u32,
    is_mut: bool,
    span: Span,
    safe_set: &IndexSet<u32>,
    field_map: &IndexMap<(u32, u32), u32>,
    info_map: &IndexMap<(u32, u32), (String, TypeId)>,
    candidate_mut: &IndexMap<u32, bool>,
    reconstruct_info: &IndexMap<u32, ReconstructInfo>,
    new_stmts: &mut Vec<TirStmt>,
) {
    match value.kind {
        TirExprKind::StructLiteral { fields, .. } => {
            let mut sorted_fields: Vec<_> = fields.into_iter().collect();
            sorted_fields.sort_by_key(|f| f.field_index);
            for mut field in sorted_fields {
                rewrite_expr(
                    &mut field.value,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
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
                        // The original struct/tuple literal was a fresh value, so
                        // its fields don't need value_copy — the field expressions
                        // are directly consumed by the fresh construction. Without
                        // this flag, the WIR builder would insert a deep copy for
                        // each field, breaking reference sharing semantics.
                        skip_value_copy: true,
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
                    reconstruct_info,
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
                        skip_value_copy: true,
                    },
                    span,
                ));
            }
        }
        _ => unreachable!("candidate must be struct or tuple literal"),
    }
}

fn rewrite_stmt(
    stmt: &mut TirStmt,
    safe_set: &IndexSet<u32>,
    field_map: &IndexMap<(u32, u32), u32>,
    info_map: &IndexMap<(u32, u32), (String, TypeId)>,
    candidate_mut: &IndexMap<u32, bool>,
    reconstruct_info: &IndexMap<u32, ReconstructInfo>,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            rewrite_expr(
                value,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirStmtKind::Expr(expr) => {
            rewrite_expr(
                expr,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                rewrite_expr(
                    v,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
                );
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_expr(
                condition,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            rewrite_block(
                then_block,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            if let Some(eb) = else_block {
                rewrite_block(
                    eb,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
                );
            }
        }
        TirStmtKind::Loop { body } => {
            rewrite_block(
                body,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            rewrite_block(
                block,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rewrite_expr(
                scrutinee,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            rewrite_block(
                then_block,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            if let Some(eb) = else_block {
                rewrite_block(
                    eb,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
                );
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_expr(
                    v,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
                );
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetDestructure { value, .. } => {
            rewrite_expr(
                value,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

/// Rewrite an expression: replace `s.field` with the corresponding scalar local,
/// and reconstruct struct literals at soft-escape sites.
fn rewrite_expr(
    expr: &mut TirExpr,
    safe_set: &IndexSet<u32>,
    field_map: &IndexMap<(u32, u32), u32>,
    info_map: &IndexMap<(u32, u32), (String, TypeId)>,
    candidate_mut: &IndexMap<u32, bool>,
    reconstruct_info: &IndexMap<u32, ReconstructInfo>,
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
            rewrite_expr(
                value,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            return;
        }
    }

    // Reconstruct: bare Local reference to a soft-escape candidate → re-materialize struct/tuple
    if let TirExprKind::Local { index, .. } = &expr.kind
        && let Some(info) = reconstruct_info.get(index)
    {
        let idx = *index;
        let span = expr.span;
        reconstruct_aggregate(expr, idx, info, field_map, info_map, span);
        return;
    }

    // Recurse into child expressions
    match &mut expr.kind {
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            rewrite_expr(
                inner,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirExprKind::Assign { target, value } => {
            rewrite_expr(
                target,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            rewrite_expr(
                value,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirExprKind::Binary { left, right, .. } => {
            rewrite_expr(
                left,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            rewrite_expr(
                right,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirExprKind::Unary { expr: inner, .. } => {
            rewrite_expr(
                inner,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirExprKind::Cast { expr: inner, .. } => {
            rewrite_expr(
                inner,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                rewrite_expr(
                    &mut arg.expr,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
                );
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                rewrite_expr(
                    arg,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
                );
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            rewrite_expr(
                receiver,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            for arg in args {
                rewrite_expr(
                    &mut arg.expr,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
                );
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            rewrite_expr(
                callee,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            for arg in args {
                rewrite_expr(
                    arg,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
                );
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            rewrite_expr(
                functor,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            rewrite_expr(
                inner,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            rewrite_expr(
                index,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            rewrite_block(
                block,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_expr(
                condition,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            rewrite_block(
                then_branch,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            if let Some(eb) = else_branch {
                rewrite_block(
                    eb,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
                );
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            rewrite_expr(
                inner,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    rewrite_expr(
                        guard,
                        safe_set,
                        field_map,
                        info_map,
                        candidate_mut,
                        reconstruct_info,
                    );
                }
                rewrite_expr(
                    &mut arm.body,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
                );
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
                    reconstruct_info,
                );
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                rewrite_expr(
                    elem,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
                );
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                rewrite_expr(
                    p,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
                );
            }
        }
        TirExprKind::Closure { body, .. } => {
            rewrite_expr(
                body,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            rewrite_expr(
                value,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
            rewrite_expr(
                expr,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirExprKind::VariantPayload { expr, .. } => {
            rewrite_expr(
                expr,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            rewrite_expr(
                scrutinee,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
            for arm in arms {
                rewrite_block(
                    arm,
                    safe_set,
                    field_map,
                    info_map,
                    candidate_mut,
                    reconstruct_info,
                );
            }
            rewrite_block(
                default,
                safe_set,
                field_map,
                info_map,
                candidate_mut,
                reconstruct_info,
            );
        }
        // Leaf nodes — nothing to rewrite
        TirExprKind::Local { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. }
        | TirExprKind::TemplateString { .. } => {}
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!("WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase")
        }
    }
}

/// Build a reconstructed struct or tuple literal from SROA'd scalar locals.
fn reconstruct_aggregate(
    expr: &mut TirExpr,
    local_idx: u32,
    info: &ReconstructInfo,
    field_map: &IndexMap<(u32, u32), u32>,
    info_map: &IndexMap<(u32, u32), (String, TypeId)>,
    span: Span,
) {
    if info.struct_name.is_empty() {
        // Tuple reconstruction
        let elements: Vec<TirExpr> = info
            .fields
            .iter()
            .enumerate()
            .map(|(i, (_, type_id))| {
                let key = (local_idx, i as u32);
                let field_local = field_map[&key];
                let (field_name, _) = &info_map[&key];
                TirExpr {
                    kind: TirExprKind::Local {
                        index: field_local,
                        name: field_name.clone(),
                    },
                    type_id: *type_id,
                    span,
                }
            })
            .collect();
        expr.kind = TirExprKind::TupleLiteral { elements };
    } else {
        // Struct reconstruction
        let fields: Vec<TirStructField> = info
            .fields
            .iter()
            .enumerate()
            .map(|(i, (name, type_id))| {
                let key = (local_idx, i as u32);
                let field_local = field_map[&key];
                let (field_name, _) = &info_map[&key];
                TirStructField {
                    name: name.clone(),
                    value: TirExpr {
                        kind: TirExprKind::Local {
                            index: field_local,
                            name: field_name.clone(),
                        },
                        type_id: *type_id,
                        span,
                    },
                    field_index: i as u32,
                }
            })
            .collect();
        expr.kind = TirExprKind::StructLiteral {
            struct_type: info.aggregate_type_id,
            struct_name: info.struct_name.clone(),
            fields,
        };
    }
}
