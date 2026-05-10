//! Loop-Invariant Code Motion (LICM) for Wado TIR
//!
//! This module hoists loop-invariant computations out of loops to improve performance.
//! It identifies field accesses on variables that don't change within a loop and moves
//! those accesses before the loop.

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::tir::{
    ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal, TirPattern, TirStmt,
    TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};

/// Tracks which variables and fields are modified within a loop.
///
/// Distinguishes between full-object modification (e.g., `buf = new_string`, `&mut buf`)
/// and field-level modification (e.g., `buf.len = buf.len + 1`), enabling LICM to
/// hoist field accesses like `buf.repr` even when `buf.len` is modified.
///
/// Also tracks GC reference aliases: when `let a = b` copies a GC struct reference,
/// `a` and `b` point to the same heap object. Modifications through one alias must
/// prevent hoisting field accesses on the other.
#[derive(Default)]
struct ModifiedVars {
    /// Locals that are fully modified (assigned as a whole, passed as &mut, etc.).
    fully: IndexSet<u32>,
    /// (`local_index`, `field_index`) pairs where only a specific field is modified.
    fields: IndexSet<(u32, u32)>,
    /// GC alias pairs: if `(a, b)` is present, `a` and `b` may point to the same object.
    aliases: Vec<(u32, u32)>,
}

impl ModifiedVars {
    fn insert_full(&mut self, local_idx: u32) {
        self.fully.insert(local_idx);
    }

    fn insert_field(&mut self, local_idx: u32, field_idx: u32) {
        self.fields.insert((local_idx, field_idx));
    }

    fn extend_full(&mut self, other: &IndexSet<u32>) {
        self.fully.extend(other.iter().copied());
    }

    fn add_alias(&mut self, a: u32, b: u32) {
        self.aliases.push((a, b));
    }

    /// Collect all locals that alias with `local_idx` (transitively).
    fn alias_set(&self, local_idx: u32) -> IndexSet<u32> {
        let mut set = IndexSet::default();
        set.insert(local_idx);
        let mut changed = true;
        while changed {
            changed = false;
            for &(a, b) in &self.aliases {
                if set.contains(&a) && set.insert(b) {
                    changed = true;
                }
                if set.contains(&b) && set.insert(a) {
                    changed = true;
                }
            }
        }
        set
    }

    /// Returns true if the given local is not fully modified AND
    /// the specific field of that local is not field-modified,
    /// considering all aliases of the local.
    fn is_field_hoistable(&self, local_idx: u32, field_idx: u32) -> bool {
        let aliases = self.alias_set(local_idx);
        for &idx in &aliases {
            if self.fully.contains(&idx) || self.fields.contains(&(idx, field_idx)) {
                return false;
            }
        }
        true
    }
}

/// Apply Loop-Invariant Code Motion to all functions in the project.
pub fn apply_licm(project: &mut FlatPackage) -> bool {
    let mut changed = false;
    let type_table = project.type_table.borrow();
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= licm_function(&mut func, &type_table);
    }
    changed
}

/// Apply LICM to a function
fn licm_function(func: &mut TirFunction, type_table: &TypeTable) -> bool {
    let Some(ref mut body) = func.body else {
        return false;
    };
    let mut local_count = func.local_count;
    let mut locals = func.locals.clone();
    let changed = licm_block(body, &mut local_count, &mut locals, type_table);
    func.local_count = local_count;
    func.locals = locals;
    changed
}

/// Apply LICM to all loops in a block
fn licm_block(
    block: &mut TirBlock,
    local_count: &mut u32,
    locals: &mut Vec<TirLocal>,
    type_table: &TypeTable,
) -> bool {
    let mut changed = false;
    let mut new_stmts = Vec::new();

    for mut stmt in std::mem::take(&mut block.stmts) {
        match &mut stmt.kind {
            TirStmtKind::Loop { body } => {
                // Apply LICM to the loop body
                let empty_set = IndexSet::default();
                let hoist_stmts = licm_loop(body, local_count, locals, type_table, &empty_set);

                if !hoist_stmts.is_empty() {
                    changed = true;
                }

                // Prepend hoisting statements
                new_stmts.extend(hoist_stmts);
                new_stmts.push(stmt);
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                // Recurse into if branches
                changed |= licm_block(then_block, local_count, locals, type_table);
                if let Some(eb) = else_block {
                    changed |= licm_block(eb, local_count, locals, type_table);
                }
                new_stmts.push(stmt);
            }
            TirStmtKind::LabeledBlock { block: inner, .. } => {
                changed |= licm_block(inner, local_count, locals, type_table);
                new_stmts.push(stmt);
            }
            TirStmtKind::IfLet {
                then_block,
                else_block,
                ..
            } => {
                changed |= licm_block(then_block, local_count, locals, type_table);
                if let Some(eb) = else_block {
                    changed |= licm_block(eb, local_count, locals, type_table);
                }
                new_stmts.push(stmt);
            }
            // Other statements don't contain loops at the statement level
            _ => {
                new_stmts.push(stmt);
            }
        }
    }

    block.stmts = new_stmts;
    changed
}

/// Apply LICM to a single loop, returning hoisting statements to prepend
/// `extra_modified` contains variables that are implicitly modified (e.g., for-of binding)
/// Runs iteratively until no more candidates are found (for second-level hoisting)
fn licm_loop(
    loop_body: &mut TirBlock,
    local_count: &mut u32,
    locals: &mut Vec<TirLocal>,
    type_table: &TypeTable,
    extra_modified: &IndexSet<u32>,
) -> Vec<TirStmt> {
    let mut all_hoist_stmts = Vec::new();

    // Run LICM iteratively until no more candidates are found
    // This enables second-level hoisting (e.g., hoisting _licm_entries.repr after _licm_entries)
    // Limit iterations to prevent pathological cases
    const MAX_LICM_ITERATIONS: usize = 10;
    for _iteration in 0..MAX_LICM_ITERATIONS {
        // Step 1: Collect all variables modified in the loop
        let mut modified_vars = ModifiedVars::default();
        modified_vars.extend_full(extra_modified);
        collect_modified_vars_in_block(loop_body, &mut modified_vars, type_table);

        // Step 2: Collect immutable reference bindings for look-through optimization
        // This allows hoisting field accesses like `self.field` where `self: &T = &source`
        let ref_bindings = collect_immutable_ref_bindings(loop_body, type_table);

        // Step 3: Find field accesses that can be hoisted
        let mut candidates = Vec::new();
        let mut seen = IndexSet::default();
        let mut next_local = *local_count;
        find_hoist_candidates_in_block(
            loop_body,
            &modified_vars,
            &ref_bindings,
            &mut candidates,
            &mut seen,
            &mut next_local,
        );

        if candidates.is_empty() {
            break;
        }

        // Step 4: Create hoisting statements
        for candidate in &candidates {
            // Get the type of the original local to build the field access expression
            let local_type_id = if (candidate.local_index as usize) < locals.len() {
                locals[candidate.local_index as usize].type_id
            } else {
                // Fallback: use the candidate's type_id
                candidate.type_id
            };

            // Create field access expression: local.field
            let field_access_expr = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(TirExpr::new(
                        TirExprKind::Local {
                            index: candidate.local_index,
                            name: candidate.local_name.clone(),
                        },
                        local_type_id,
                        crate::token::Span::new(0, 0, 0, 0),
                    )),
                    field_index: candidate.field_index,
                    field_name: candidate.field_name.clone(),
                },
                candidate.type_id,
                crate::token::Span::new(0, 0, 0, 0),
            );

            // Create let statement for the hoisted value
            let hoist_name = format!(
                "_licm_{}_{}",
                candidate.field_name, candidate.new_local_index
            );
            let hoist_stmt = TirStmt::new(
                TirStmtKind::Let {
                    name: hoist_name.clone(),
                    local_index: candidate.new_local_index,
                    is_mut: false,
                    is_reactive: false,
                    type_id: candidate.type_id,
                    value: field_access_expr,
                    skip_value_copy: true,
                },
                crate::token::Span::new(0, 0, 0, 0),
            );
            all_hoist_stmts.push(hoist_stmt);

            // Add the local entry mirroring the let above
            locals.push(TirLocal {
                name: hoist_name,
                type_id: candidate.type_id,
                is_mut: false,
            });
        }

        // Update local count
        *local_count = next_local;

        // Step 5: Replace field accesses in the loop body with references to hoisted locals
        replace_hoisted_in_block(loop_body, &candidates, &ref_bindings);
    }

    // Also need to handle nested loops - apply LICM recursively
    licm_block(loop_body, local_count, locals, type_table);

    all_hoist_stmts
}

/// Collect all local variable indices that are modified (assigned) in a block.
fn collect_modified_vars_in_block(
    block: &TirBlock,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    for stmt in &block.stmts {
        collect_modified_vars_in_stmt(stmt, modified, type_table);
    }
}

/// Mark a local as fully modified if it has a GC struct type and is passed to a function call.
///
/// In Wasm GC, struct values are passed by reference. A callee receiving a GC struct
/// can modify any of its fields (e.g., `String::grow` reassigns `self.repr`).
/// This prevents LICM from hoisting field accesses on locals that may be mutated
/// by function calls within the loop.
///
/// Exception: locals whose type is `Ref(T)` (immutable reference) are skipped.
/// No function can modify the underlying struct through an immutable reference,
/// so field accesses on such locals remain loop-invariant across calls.
fn mark_gc_local_as_fully_modified(
    expr: &TirExpr,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    if let TirExprKind::Local { index, .. } = &expr.kind
        && is_gc_heap_type(expr.type_id, type_table)
    {
        // Immutable reference locals (`&T`) cannot be used by a callee to modify
        // the underlying struct. Skip marking them as modified.
        // Only skip `Ref(Struct/GenericInstance)` — not `Ref(MutRef(...))` which
        // could allow modification through the inner mutable reference.
        if let ResolvedType::Ref(inner) = type_table.get(expr.type_id)
            && !matches!(type_table.get(*inner), ResolvedType::MutRef(_))
        {
            return;
        }
        modified.insert_full(*index);
    }
}

/// Check if a type is a GC heap type whose fields can be mutated by a callee.
fn is_gc_heap_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => true,
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            is_gc_heap_type(*inner, type_table)
        }
        _ => false,
    }
}

/// Mark a local as fully modified (e.g., passed as &mut, direct assignment).
/// Traverses through unary ops and nested field accesses, always marking the root as fully modified.
fn mark_local_as_fully_modified(expr: &TirExpr, modified: &mut ModifiedVars) {
    match &expr.kind {
        TirExprKind::Local { index, .. } => {
            modified.insert_full(*index);
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            mark_local_as_fully_modified(inner, modified);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            mark_local_as_fully_modified(inner, modified);
        }
        _ => {}
    }
}

/// Mark what is modified by an assignment target expression.
///
/// Distinguishes between field assignments (`buf.len = x` marks only the `len` field
/// of `buf` as modified) and direct/deeper assignments (marks the root local as fully
/// modified). This enables LICM to hoist `buf.repr` even when `buf.len` is assigned.
fn mark_assignment_target_as_modified(expr: &TirExpr, modified: &mut ModifiedVars) {
    match &expr.kind {
        TirExprKind::Local { index, .. } => {
            // Direct assignment: `buf = x` — fully modified
            modified.insert_full(*index);
        }
        TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            if let TirExprKind::Local { index, .. } = &inner.kind {
                // Single-level field assignment: `buf.field = x` — only that field is modified
                modified.insert_field(*index, *field_index);
            } else {
                // Deeper nesting: `a.b.c = x` — conservatively mark root as fully modified
                mark_local_as_fully_modified(inner, modified);
            }
        }
        TirExprKind::Unary { expr: inner, .. } => {
            // E.g., `*ptr = x` — mark root local as fully modified
            mark_local_as_fully_modified(inner, modified);
        }
        _ => {}
    }
}

fn collect_modified_vars_in_stmt(
    stmt: &TirStmt,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    match &stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            // Let statements define new variables, mark them as modified
            // (they're not invariant within the loop where they're defined)
            modified.insert_full(*local_index);
            // Track GC aliases: `let a = b` where b is a local with GC type
            // means a and b point to the same heap object.
            if let TirExprKind::Local { index: src_idx, .. } = &value.kind
                && is_gc_heap_type(value.type_id, type_table)
            {
                modified.add_alias(*local_index, *src_idx);
            }
            // Also check the value expression for mutable references
            collect_modified_vars_in_expr(value, modified, type_table);
        }
        TirStmtKind::Expr(expr) => {
            collect_modified_vars_in_expr(expr, modified, type_table);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_modified_vars_in_expr(v, modified, type_table);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_modified_vars_in_expr(condition, modified, type_table);
            collect_modified_vars_in_block(then_block, modified, type_table);
            if let Some(eb) = else_block {
                collect_modified_vars_in_block(eb, modified, type_table);
            }
        }
        TirStmtKind::Loop { body } => {
            collect_modified_vars_in_block(body, modified, type_table);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_modified_vars_in_block(block, modified, type_table);
        }
        TirStmtKind::IfLet {
            scrutinee,
            pattern,
            then_block,
            else_block,
            ..
        } => {
            collect_modified_vars_in_expr(scrutinee, modified, type_table);
            // Pattern bindings introduce new variables that are assigned fresh each iteration
            // Mark them as modified so LICM doesn't hoist accesses to them
            collect_pattern_bindings(pattern, modified);
            collect_modified_vars_in_block(then_block, modified, type_table);
            if let Some(eb) = else_block {
                collect_modified_vars_in_block(eb, modified, type_table);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_modified_vars_in_expr(v, modified, type_table);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetDestructure { pattern, value, .. } => {
            // Collect pattern bindings as they are assigned
            collect_pattern_bindings(pattern, modified);
            // Also check the value expression for mutable references
            collect_modified_vars_in_expr(value, modified, type_table);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

/// Collect all local variable indices bound by a pattern.
/// These variables are assigned fresh each time the pattern matches.
fn collect_pattern_bindings(pattern: &TirPattern, modified: &mut ModifiedVars) {
    match pattern {
        TirPattern::Binding { local_index, .. } => {
            modified.insert_full(*local_index);
        }
        TirPattern::Variant { bindings, .. } => {
            for binding in bindings {
                collect_pattern_bindings(binding, modified);
            }
        }
        TirPattern::Tuple(patterns, _) => {
            for p in patterns {
                collect_pattern_bindings(p, modified);
            }
        }
        TirPattern::Struct { fields, .. } => {
            for field in fields {
                collect_pattern_bindings(&field.pattern, modified);
            }
        }
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::ConstantValue { .. }
        | TirPattern::Range { .. } => {
            // No bindings
        }
        TirPattern::Or(alternatives) => {
            for p in alternatives {
                collect_pattern_bindings(p, modified);
            }
        }
    }
}

fn collect_modified_vars_in_expr(
    expr: &TirExpr,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    match &expr.kind {
        TirExprKind::Assign { target, value } => {
            // Mark the assignment target appropriately.
            // Field assignment (buf.field = x) only marks that specific field as modified,
            // enabling LICM to still hoist other fields of the same object.
            mark_assignment_target_as_modified(target, modified);
            collect_modified_vars_in_expr(target, modified, type_table);
            collect_modified_vars_in_expr(value, modified, type_table);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_modified_vars_in_expr(left, modified, type_table);
            collect_modified_vars_in_expr(right, modified, type_table);
        }
        TirExprKind::Unary { op, expr } => {
            // &mut local: the local may be reassigned through the ref (boxed primitives).
            // &mut local.field: in Wasm GC, this just reads the GC reference stored in the
            // field — neither the parent local nor the field reference itself changes.
            // Only mark the root as fully modified for a direct &mut local, not &mut local.field.
            if matches!(op, TirUnaryOp::MutRef) && matches!(expr.kind, TirExprKind::Local { .. }) {
                mark_local_as_fully_modified(expr, modified);
            }
            collect_modified_vars_in_expr(expr, modified, type_table);
        }
        TirExprKind::Cast { expr, .. } => {
            collect_modified_vars_in_expr(expr, modified, type_table);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                mark_gc_local_as_fully_modified(&arg.expr, modified, type_table);
                collect_modified_vars_in_expr(&arg.expr, modified, type_table);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            mark_gc_local_as_fully_modified(receiver, modified, type_table);
            collect_modified_vars_in_expr(receiver, modified, type_table);
            for arg in args {
                mark_gc_local_as_fully_modified(&arg.expr, modified, type_table);
                collect_modified_vars_in_expr(&arg.expr, modified, type_table);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_modified_vars_in_expr(arg, modified, type_table);
            }
        }
        TirExprKind::FieldAccess { expr, .. }
        | TirExprKind::TupleSpread { expr }
        | TirExprKind::TupleZip { expr }
        | TirExprKind::TypePackExpansion {
            call_expr: expr, ..
        } => {
            collect_modified_vars_in_expr(expr, modified, type_table);
        }
        TirExprKind::Index { expr, index } => {
            collect_modified_vars_in_expr(expr, modified, type_table);
            collect_modified_vars_in_expr(index, modified, type_table);
        }
        TirExprKind::Block(block) => {
            collect_modified_vars_in_block(block, modified, type_table);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_modified_vars_in_expr(condition, modified, type_table);
            collect_modified_vars_in_block(then_branch, modified, type_table);
            if let Some(eb) = else_branch {
                collect_modified_vars_in_block(eb, modified, type_table);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_modified_vars_in_expr(&field.value, modified, type_table);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_modified_vars_in_expr(elem, modified, type_table);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_modified_vars_in_expr(callee, modified, type_table);
            for arg in args {
                mark_gc_local_as_fully_modified(arg, modified, type_table);
                collect_modified_vars_in_expr(arg, modified, type_table);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_modified_vars_in_expr(functor, modified, type_table);
        }
        TirExprKind::Closure { body, .. } => {
            collect_modified_vars_in_expr(body, modified, type_table);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                collect_modified_vars_in_expr(payload_expr, modified, type_table);
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_modified_vars_in_block(block, modified, type_table);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_modified_vars_in_expr(value, modified, type_table);
        }
        TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
            collect_modified_vars_in_expr(expr, modified, type_table);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            collect_modified_vars_in_expr(expr, modified, type_table);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_modified_vars_in_expr(scrutinee, modified, type_table);
            for arm in arms {
                collect_modified_vars_in_block(arm, modified, type_table);
            }
            collect_modified_vars_in_block(default, modified, type_table);
        }
        // Leaf nodes
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
            collect_modified_vars_in_expr(expr, modified, type_table);
            for arm in arms {
                collect_pattern_bindings(&arm.pattern, modified);
                if let Some(guard) = &arm.guard {
                    collect_modified_vars_in_expr(guard, modified, type_table);
                }
                collect_modified_vars_in_expr(&arm.body, modified, type_table);
            }
        }
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Information about an immutable reference binding: `let ref_var: &T = &source_var`
#[derive(Debug, Clone)]
struct LicmRefBinding {
    /// The source local index that this reference points to
    source_index: u32,
    /// The source local name (for creating hoist statements)
    source_name: String,
}

/// Collect immutable reference bindings in a block.
/// These are patterns like: `let self: &T = &source_var`
/// Returns a map from `ref_local_index` -> `source_local_index`
fn collect_immutable_ref_bindings(
    block: &TirBlock,
    type_table: &TypeTable,
) -> IndexMap<u32, LicmRefBinding> {
    let mut bindings = IndexMap::default();
    collect_licm_ref_bindings_in_block(block, type_table, &mut bindings);
    bindings
}

fn collect_licm_ref_bindings_in_block(
    block: &TirBlock,
    type_table: &TypeTable,
    bindings: &mut IndexMap<u32, LicmRefBinding>,
) {
    for stmt in &block.stmts {
        collect_licm_ref_bindings_in_stmt(stmt, type_table, bindings);
    }
}

fn collect_licm_ref_bindings_in_stmt(
    stmt: &TirStmt,
    type_table: &TypeTable,
    bindings: &mut IndexMap<u32, LicmRefBinding>,
) {
    match &stmt.kind {
        TirStmtKind::Let {
            local_index,
            value,
            type_id,
            ..
        } => {
            // Check if this is: let x: &T = &y (immutable ref to a local)
            if matches!(type_table.get(*type_id), ResolvedType::Ref(_))
                && let TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: source,
                } = &value.kind
                && let TirExprKind::Local {
                    index: source_idx,
                    name: source_name,
                } = &source.kind
            {
                bindings.insert(
                    *local_index,
                    LicmRefBinding {
                        source_index: *source_idx,
                        source_name: source_name.clone(),
                    },
                );
            }
            // Recurse into the value expression (for nested blocks)
            collect_licm_ref_bindings_in_expr(value, type_table, bindings);
        }
        TirStmtKind::Expr(expr) => {
            collect_licm_ref_bindings_in_expr(expr, type_table, bindings);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_licm_ref_bindings_in_expr(v, type_table, bindings);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_licm_ref_bindings_in_expr(condition, type_table, bindings);
            collect_licm_ref_bindings_in_block(then_block, type_table, bindings);
            if let Some(eb) = else_block {
                collect_licm_ref_bindings_in_block(eb, type_table, bindings);
            }
        }
        TirStmtKind::Loop { body } => {
            collect_licm_ref_bindings_in_block(body, type_table, bindings);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_licm_ref_bindings_in_block(block, type_table, bindings);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_licm_ref_bindings_in_expr(scrutinee, type_table, bindings);
            collect_licm_ref_bindings_in_block(then_block, type_table, bindings);
            if let Some(eb) = else_block {
                collect_licm_ref_bindings_in_block(eb, type_table, bindings);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_licm_ref_bindings_in_expr(v, type_table, bindings);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetDestructure { value, .. } => {
            collect_licm_ref_bindings_in_expr(value, type_table, bindings);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn collect_licm_ref_bindings_in_expr(
    expr: &TirExpr,
    type_table: &TypeTable,
    bindings: &mut IndexMap<u32, LicmRefBinding>,
) {
    // Recurse into all sub-expressions to find nested let bindings
    match &expr.kind {
        TirExprKind::Block(block) => {
            collect_licm_ref_bindings_in_block(block, type_table, bindings);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_licm_ref_bindings_in_block(block, type_table, bindings);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_licm_ref_bindings_in_expr(condition, type_table, bindings);
            collect_licm_ref_bindings_in_block(then_branch, type_table, bindings);
            if let Some(eb) = else_branch {
                collect_licm_ref_bindings_in_block(eb, type_table, bindings);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_licm_ref_bindings_in_expr(left, type_table, bindings);
            collect_licm_ref_bindings_in_expr(right, type_table, bindings);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            collect_licm_ref_bindings_in_expr(inner, type_table, bindings);
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            collect_licm_ref_bindings_in_expr(inner, type_table, bindings);
        }
        TirExprKind::Index { expr: inner, index } => {
            collect_licm_ref_bindings_in_expr(inner, type_table, bindings);
            collect_licm_ref_bindings_in_expr(index, type_table, bindings);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                collect_licm_ref_bindings_in_expr(&arg.expr, type_table, bindings);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_licm_ref_bindings_in_expr(receiver, type_table, bindings);
            for arg in args {
                collect_licm_ref_bindings_in_expr(&arg.expr, type_table, bindings);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_licm_ref_bindings_in_expr(arg, type_table, bindings);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_licm_ref_bindings_in_expr(callee, type_table, bindings);
            for arg in args {
                collect_licm_ref_bindings_in_expr(arg, type_table, bindings);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_licm_ref_bindings_in_expr(functor, type_table, bindings);
        }
        TirExprKind::Assign { target, value } => {
            collect_licm_ref_bindings_in_expr(target, type_table, bindings);
            collect_licm_ref_bindings_in_expr(value, type_table, bindings);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            collect_licm_ref_bindings_in_expr(inner, type_table, bindings);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_licm_ref_bindings_in_expr(&field.value, type_table, bindings);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_licm_ref_bindings_in_expr(elem, type_table, bindings);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_licm_ref_bindings_in_expr(body, type_table, bindings);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                collect_licm_ref_bindings_in_expr(payload_expr, type_table, bindings);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_licm_ref_bindings_in_expr(value, type_table, bindings);
        }
        TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
            collect_licm_ref_bindings_in_expr(expr, type_table, bindings);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            collect_licm_ref_bindings_in_expr(expr, type_table, bindings);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_licm_ref_bindings_in_expr(scrutinee, type_table, bindings);
            for arm in arms {
                collect_licm_ref_bindings_in_block(arm, type_table, bindings);
            }
            collect_licm_ref_bindings_in_block(default, type_table, bindings);
        }
        // Leaf nodes - no nested expressions
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
            collect_licm_ref_bindings_in_expr(expr, type_table, bindings);
            for arm in arms {
                collect_licm_ref_bindings_in_expr(&arm.body, type_table, bindings);
                if let Some(guard) = &arm.guard {
                    collect_licm_ref_bindings_in_expr(guard, type_table, bindings);
                }
            }
        }
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Represents a hoistable expression with its replacement info
#[derive(Debug)]
struct HoistCandidate {
    /// The original expression pattern to match (field access on a local)
    local_index: u32,
    /// The name of the local variable (for unparsing)
    local_name: String,
    field_index: u32,
    field_name: String,
    /// The type of the field access result
    type_id: TypeId,
    /// The new local index to use for the hoisted value
    new_local_index: u32,
}

/// Find field accesses on loop-invariant expressions that can be hoisted.
/// Returns a list of candidates to hoist.
fn find_hoist_candidates_in_block(
    block: &TirBlock,
    modified_vars: &ModifiedVars,
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut IndexSet<(u32, u32)>, // (local_index, field_index) pairs already seen
    next_local: &mut u32,
) {
    for stmt in &block.stmts {
        find_hoist_candidates_in_stmt(
            stmt,
            modified_vars,
            ref_bindings,
            candidates,
            seen,
            next_local,
        );
    }
}

fn find_hoist_candidates_in_stmt(
    stmt: &TirStmt,
    modified_vars: &ModifiedVars,
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut IndexSet<(u32, u32)>,
    next_local: &mut u32,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            find_hoist_candidates_in_expr(
                value,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirStmtKind::Expr(expr) => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                find_hoist_candidates_in_expr(
                    v,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            find_hoist_candidates_in_expr(
                condition,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            find_hoist_candidates_in_block(
                then_block,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            if let Some(eb) = else_block {
                find_hoist_candidates_in_block(
                    eb,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirStmtKind::Loop { body } => {
            find_hoist_candidates_in_block(
                body,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            find_hoist_candidates_in_block(
                block,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            find_hoist_candidates_in_expr(
                scrutinee,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            find_hoist_candidates_in_block(
                then_block,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            if let Some(eb) = else_block {
                find_hoist_candidates_in_block(
                    eb,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                find_hoist_candidates_in_expr(
                    v,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetDestructure { value, .. } => {
            find_hoist_candidates_in_expr(
                value,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
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

fn find_hoist_candidates_in_expr(
    expr: &TirExpr,
    modified_vars: &ModifiedVars,
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut IndexSet<(u32, u32)>,
    next_local: &mut u32,
) {
    match &expr.kind {
        // This is the key pattern: field access on a loop-invariant local
        TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => {
            if let TirExprKind::Local { index, name } = &inner.kind {
                // Case 1: Direct access on a loop-invariant local.
                // A field is hoistable if neither the whole local nor that specific field
                // is modified (field-level tracking lets us hoist buf.repr even when buf.len
                // is modified by assignment).
                if modified_vars.is_field_hoistable(*index, *field_index) {
                    let key = (*index, *field_index);
                    if !seen.contains(&key) {
                        seen.insert(key);
                        candidates.push(HoistCandidate {
                            local_index: *index,
                            local_name: name.clone(),
                            field_index: *field_index,
                            field_name: field_name.clone(),
                            type_id: expr.type_id,
                            new_local_index: *next_local,
                        });
                        *next_local += 1;
                    }
                }
                // Case 2: Access through an immutable reference to a loop-invariant local
                // e.g., `let self: &T = &source; ... self.field ...`
                // Since &T guarantees immutability, self.field == source.field
                else if let Some(ref_binding) = ref_bindings.get(index)
                    && modified_vars.is_field_hoistable(ref_binding.source_index, *field_index)
                {
                    let key = (ref_binding.source_index, *field_index);
                    if !seen.contains(&key) {
                        seen.insert(key);
                        candidates.push(HoistCandidate {
                            local_index: ref_binding.source_index,
                            local_name: ref_binding.source_name.clone(),
                            field_index: *field_index,
                            field_name: field_name.clone(),
                            type_id: expr.type_id,
                            new_local_index: *next_local,
                        });
                        *next_local += 1;
                    }
                }
            }
            // Still recurse into inner expression
            find_hoist_candidates_in_expr(
                inner,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Binary { left, right, .. } => {
            find_hoist_candidates_in_expr(
                left,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            find_hoist_candidates_in_expr(
                right,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Unary { expr, .. } => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Assign { target, value } => {
            find_hoist_candidates_in_expr(
                target,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            find_hoist_candidates_in_expr(
                value,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Cast { expr, .. } => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                find_hoist_candidates_in_expr(
                    &arg.expr,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            find_hoist_candidates_in_expr(
                receiver,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            for arg in args {
                find_hoist_candidates_in_expr(
                    &arg.expr,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                find_hoist_candidates_in_expr(
                    arg,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::Index { expr, index } => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            find_hoist_candidates_in_expr(
                index,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Block(block) => {
            find_hoist_candidates_in_block(
                block,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            find_hoist_candidates_in_expr(
                condition,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            find_hoist_candidates_in_block(
                then_branch,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            if let Some(eb) = else_branch {
                find_hoist_candidates_in_block(
                    eb,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                find_hoist_candidates_in_expr(
                    &field.value,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                find_hoist_candidates_in_expr(
                    elem,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            find_hoist_candidates_in_expr(
                inner,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            find_hoist_candidates_in_expr(
                callee,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            for arg in args {
                find_hoist_candidates_in_expr(
                    arg,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            find_hoist_candidates_in_expr(
                functor,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Closure { body, .. } => {
            find_hoist_candidates_in_expr(
                body,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                find_hoist_candidates_in_expr(
                    payload_expr,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            find_hoist_candidates_in_block(
                block,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            find_hoist_candidates_in_expr(
                value,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::VariantPayload { expr, .. } => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            find_hoist_candidates_in_expr(
                scrutinee,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            for arm in arms {
                find_hoist_candidates_in_block(
                    arm,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
            find_hoist_candidates_in_block(
                default,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        // Leaf nodes
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
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            for arm in arms {
                find_hoist_candidates_in_expr(
                    &arm.body,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
                if let Some(guard) = &arm.guard {
                    find_hoist_candidates_in_expr(
                        guard,
                        modified_vars,
                        ref_bindings,
                        candidates,
                        seen,
                        next_local,
                    );
                }
            }
        }
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Replace field accesses with references to hoisted locals
fn replace_hoisted_in_block(
    block: &mut TirBlock,
    candidates: &[HoistCandidate],
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
) {
    for stmt in &mut block.stmts {
        replace_hoisted_in_stmt(stmt, candidates, ref_bindings);
    }
}

fn replace_hoisted_in_stmt(
    stmt: &mut TirStmt,
    candidates: &[HoistCandidate],
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            replace_hoisted_in_expr(value, candidates, ref_bindings);
        }
        TirStmtKind::Expr(expr) => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                replace_hoisted_in_expr(v, candidates, ref_bindings);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            replace_hoisted_in_expr(condition, candidates, ref_bindings);
            replace_hoisted_in_block(then_block, candidates, ref_bindings);
            if let Some(eb) = else_block {
                replace_hoisted_in_block(eb, candidates, ref_bindings);
            }
        }
        TirStmtKind::Loop { body } => {
            replace_hoisted_in_block(body, candidates, ref_bindings);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            replace_hoisted_in_block(block, candidates, ref_bindings);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            replace_hoisted_in_expr(scrutinee, candidates, ref_bindings);
            replace_hoisted_in_block(then_block, candidates, ref_bindings);
            if let Some(eb) = else_block {
                replace_hoisted_in_block(eb, candidates, ref_bindings);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                replace_hoisted_in_expr(v, candidates, ref_bindings);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetDestructure { value, .. } => {
            replace_hoisted_in_expr(value, candidates, ref_bindings);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn replace_hoisted_in_expr(
    expr: &mut TirExpr,
    candidates: &[HoistCandidate],
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
) {
    // First, check if this expression matches a hoist candidate
    if let TirExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &expr.kind
        && let TirExprKind::Local { index, .. } = &inner.kind
    {
        // Case 1: Direct match - local.field where local is the hoisted source
        for candidate in candidates {
            if candidate.local_index == *index && candidate.field_index == *field_index {
                // Replace with a reference to the hoisted local
                expr.kind = TirExprKind::Local {
                    index: candidate.new_local_index,
                    name: format!(
                        "_licm_{}_{}",
                        candidate.field_name, candidate.new_local_index
                    ),
                };
                return;
            }
        }
        // Case 2: Look through immutable reference - ref_var.field where ref_var = &source
        if let Some(ref_binding) = ref_bindings.get(index) {
            for candidate in candidates {
                if candidate.local_index == ref_binding.source_index
                    && candidate.field_index == *field_index
                {
                    // Replace with a reference to the hoisted local
                    expr.kind = TirExprKind::Local {
                        index: candidate.new_local_index,
                        name: format!(
                            "_licm_{}_{}",
                            candidate.field_name, candidate.new_local_index
                        ),
                    };
                    return;
                }
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
            replace_hoisted_in_expr(inner, candidates, ref_bindings);
        }
        TirExprKind::Binary { left, right, .. } => {
            replace_hoisted_in_expr(left, candidates, ref_bindings);
            replace_hoisted_in_expr(right, candidates, ref_bindings);
        }
        TirExprKind::Unary { expr, .. } => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
        }
        TirExprKind::Assign { target, value } => {
            replace_hoisted_in_expr(target, candidates, ref_bindings);
            replace_hoisted_in_expr(value, candidates, ref_bindings);
        }
        TirExprKind::Cast { expr, .. } => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                replace_hoisted_in_expr(&mut arg.expr, candidates, ref_bindings);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            replace_hoisted_in_expr(receiver, candidates, ref_bindings);
            for arg in args {
                replace_hoisted_in_expr(&mut arg.expr, candidates, ref_bindings);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                replace_hoisted_in_expr(arg, candidates, ref_bindings);
            }
        }
        TirExprKind::Index { expr, index } => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
            replace_hoisted_in_expr(index, candidates, ref_bindings);
        }
        TirExprKind::Block(block) => {
            replace_hoisted_in_block(block, candidates, ref_bindings);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            replace_hoisted_in_expr(condition, candidates, ref_bindings);
            replace_hoisted_in_block(then_branch, candidates, ref_bindings);
            if let Some(eb) = else_branch {
                replace_hoisted_in_block(eb, candidates, ref_bindings);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                replace_hoisted_in_expr(&mut field.value, candidates, ref_bindings);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                replace_hoisted_in_expr(elem, candidates, ref_bindings);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            replace_hoisted_in_expr(callee, candidates, ref_bindings);
            for arg in args {
                replace_hoisted_in_expr(arg, candidates, ref_bindings);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            replace_hoisted_in_expr(functor, candidates, ref_bindings);
        }
        TirExprKind::Closure { body, .. } => {
            replace_hoisted_in_expr(body, candidates, ref_bindings);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                replace_hoisted_in_expr(payload_expr, candidates, ref_bindings);
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            replace_hoisted_in_block(block, candidates, ref_bindings);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            replace_hoisted_in_expr(value, candidates, ref_bindings);
        }
        TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            replace_hoisted_in_expr(scrutinee, candidates, ref_bindings);
            for arm in arms {
                replace_hoisted_in_block(arm, candidates, ref_bindings);
            }
            replace_hoisted_in_block(default, candidates, ref_bindings);
        }
        // Leaf nodes
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
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
            for arm in arms {
                replace_hoisted_in_expr(&mut arm.body, candidates, ref_bindings);
                if let Some(guard) = &mut arm.guard {
                    replace_hoisted_in_expr(guard, candidates, ref_bindings);
                }
            }
        }
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}
