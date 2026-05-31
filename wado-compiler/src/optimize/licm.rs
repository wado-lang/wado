//! Loop-Invariant Code Motion (LICM) for Wado NIR
//!
//! This module hoists loop-invariant computations out of loops to improve performance.
//! It identifies field accesses on variables that don't change within a loop and moves
//! those accesses before the loop.

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::nir::{
    NirBlock, NirExpr, NirExprKind, NirFunction, NirLocal, NirPattern, NirStmt, NirStmtKind,
    NirUnaryOp,
};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

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
    /// `(pointee_type, field_index)` for every field written in the loop. Wado
    /// references alias, so a write through one `&T` is seen through any other;
    /// the `(local, field)` tracking above misses writes via a different alias.
    /// Used by `is_reference_field_aliasing_written`.
    written_field_types: IndexSet<(TypeId, u32)>,
    /// Pointee struct types passed by `&mut` to a call/method in the loop: the
    /// callee may write *any* field, so no field of that type is invariant.
    clobbered_pointee_types: IndexSet<TypeId>,
}

impl ModifiedVars {
    fn insert_full(&mut self, local_idx: u32) {
        self.fully.insert(local_idx);
    }

    fn insert_field(&mut self, local_idx: u32, field_idx: u32) {
        self.fields.insert((local_idx, field_idx));
    }

    fn insert_written_field_type(&mut self, pointee: TypeId, field_idx: u32) {
        self.written_field_types.insert((pointee, field_idx));
    }

    fn insert_clobbered_pointee_type(&mut self, pointee: TypeId) {
        self.clobbered_pointee_types.insert(pointee);
    }

    /// True when hoisting `x.field_idx` is unsound: `x` is a reference whose
    /// pointee's `field_idx` is written in the loop — directly via an alias, or
    /// opaquely by a call that received the pointee by `&mut`. By-value roots
    /// are covered by the `fully`/`fields`/alias machinery.
    fn is_reference_field_aliasing_written(
        &self,
        root_type: TypeId,
        field_idx: u32,
        type_table: &TypeTable,
    ) -> bool {
        match type_table.get(root_type) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                let pointee = strip_references(*inner, type_table);
                self.clobbered_pointee_types.contains(&pointee)
                    || self.written_field_types.contains(&(pointee, field_idx))
            }
            _ => false,
        }
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
pub fn apply_licm(project: &mut NirPackage) -> bool {
    let mut changed = false;
    let type_table = project.type_table.borrow();
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= licm_function(&mut func, &type_table);
    }
    changed
}

/// Apply LICM to a function
fn licm_function(func: &mut NirFunction, type_table: &TypeTable) -> bool {
    let Some(ref mut body) = func.body else {
        return false;
    };
    let mut local_count = func.local_count;
    let mut locals = func.locals.clone();
    let mut outer_aliases: Vec<(u32, u32)> = Vec::new();
    let changed = licm_block(
        body,
        &mut local_count,
        &mut locals,
        type_table,
        &mut outer_aliases,
    );
    func.local_count = local_count;
    func.locals = locals;
    changed
}

/// Apply LICM to all loops in a block.
///
/// `outer_aliases` accumulates `let x = y` (and `&y` / `&mut y` / labeled-
/// or plain-block tail equivalents) pairs from let-statements that
/// precede each loop. The fixpoint loop in `licm_loop` consumes these so
/// that a write to one alias inside the loop body invalidates hoist
/// candidates targeting the other alias — otherwise LICM hoists a snapshot
/// of `local63.pos` (where `local63` is a `&mut Parser` alias of the real
/// `local0`) and the body's `local63.pos = local63.pos + 1` write isn't
/// recognised as invalidating the snapshot.
fn licm_block(
    block: &mut NirBlock,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    outer_aliases: &mut Vec<(u32, u32)>,
) -> bool {
    let mut changed = false;
    let mut new_stmts = Vec::new();

    for mut stmt in std::mem::take(&mut block.stmts) {
        match &mut stmt.kind {
            NirStmtKind::Loop { body } => {
                // Apply LICM to the loop body
                let empty_set = IndexSet::default();
                let hoist_stmts = licm_loop(
                    body,
                    local_count,
                    locals,
                    type_table,
                    &empty_set,
                    outer_aliases,
                );

                if !hoist_stmts.is_empty() {
                    changed = true;
                }

                // Prepend hoisting statements
                new_stmts.extend(hoist_stmts);
                new_stmts.push(stmt);
            }
            NirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                // Recurse into if branches. The conservative thing is to
                // clone the alias accumulator so sibling branches don't
                // see each other's aliases, but since aliasing is
                // monotone-correct (extra aliases never cause wrong
                // hoists, only conservative misses), sharing the same
                // accumulator is safe.
                changed |= licm_block(then_block, local_count, locals, type_table, outer_aliases);
                if let Some(eb) = else_block {
                    changed |= licm_block(eb, local_count, locals, type_table, outer_aliases);
                }
                new_stmts.push(stmt);
            }
            NirStmtKind::LabeledBlock { block: inner, .. } => {
                changed |= licm_block(inner, local_count, locals, type_table, outer_aliases);
                new_stmts.push(stmt);
            }
            NirStmtKind::Let {
                local_index, value, ..
            } => {
                // Track outer-scope aliases so a subsequent loop's LICM
                // can see them. See `outer_aliases` doc above.
                if let Some(src_idx) = extract_alias_source(value)
                    && is_gc_heap_type(value.type_id, type_table)
                {
                    outer_aliases.push((*local_index, src_idx));
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
    loop_body: &mut NirBlock,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    extra_modified: &IndexSet<u32>,
    outer_aliases: &[(u32, u32)],
) -> Vec<NirStmt> {
    let mut all_hoist_stmts = Vec::new();

    // Run LICM iteratively until no more candidates are found
    // This enables second-level hoisting (e.g., hoisting _licm_entries.repr after _licm_entries)
    // Limit iterations to prevent pathological cases
    const MAX_LICM_ITERATIONS: usize = 10;
    for _iteration in 0..MAX_LICM_ITERATIONS {
        // Step 1: Collect all variables modified in the loop
        let mut modified_vars = ModifiedVars::default();
        modified_vars.extend_full(extra_modified);
        // Seed with pre-loop aliases (e.g. `let p = &mut outer_p`) so a
        // write through one alias inside the loop body invalidates hoist
        // candidates targeting the other.
        for &(a, b) in outer_aliases {
            modified_vars.add_alias(a, b);
        }
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

        // Step 3.5: Drop `x.f` candidates where `x` is a reference and that
        // pointee field is written elsewhere in the loop — an aliasing reference
        // may write it. Makes the nested-chain relaxation sound and fixes the
        // pre-existing two-aliased-`&mut` case.
        candidates.retain(|c| {
            let root_ty = if (c.local_index as usize) < locals.len() {
                locals[c.local_index as usize].type_id
            } else {
                c.type_id
            };
            !modified_vars.is_reference_field_aliasing_written(root_ty, c.field_index, type_table)
        });

        if candidates.is_empty() {
            break;
        }

        // Renumber the surviving hoist locals contiguously from `*local_count`:
        // dropped candidates left gaps in the indices `find_hoist_candidates`
        // provisionally assigned, and `locals.push` below appends densely.
        next_local = *local_count;
        for candidate in &mut candidates {
            candidate.new_local_index = next_local;
            next_local += 1;
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
            let field_access_expr = NirExpr::new(
                NirExprKind::FieldAccess {
                    expr: Box::new(NirExpr::new(
                        NirExprKind::Local {
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
            let hoist_stmt = NirStmt::new(
                NirStmtKind::Let {
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
            locals.push(NirLocal {
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

    // Also need to handle nested loops - apply LICM recursively.
    // The nested loop sees the outer-loop's body as its own outer scope,
    // so threading `outer_aliases` is unnecessary here — the nested
    // `licm_block` will accumulate aliases from the outer loop's `let`
    // statements on its own walk.
    let mut nested_aliases: Vec<(u32, u32)> = outer_aliases.to_vec();
    licm_block(
        loop_body,
        local_count,
        locals,
        type_table,
        &mut nested_aliases,
    );

    all_hoist_stmts
}

/// Collect all local variable indices that are modified (assigned) in a block.
fn collect_modified_vars_in_block(
    block: &NirBlock,
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
    expr: &NirExpr,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    if let NirExprKind::Local { index, .. } = &expr.kind
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

/// Walk through reference-introducing wrappers and tail-return blocks to
/// find the source local that a let-binding actually aliases.
///
/// Handles:
/// - `Local(y)` — direct
/// - `&y` / `&mut y` — reference (the address backs the same heap object)
/// - `LabeledBlock { label, block: { stmts; break label: V } }` — value-yielding
///   labeled block, recurse into `V`
/// - `Block { stmts; Expr(V) }` — plain block whose tail expression is `V`,
///   produced by `branch_prune`'s C3 flatten of the above. Recurse into `V`.
///
/// The walk is alias-precision-only: missing an alias is a soundness bug
/// (LICM may hoist a mutated field), but adding too many aliases is at
/// worst a missed optimisation. The recursion stops on the first
/// non-aliasing shape; it never traverses through `FieldAccess`, `Index`,
/// `Call`, etc.
fn extract_alias_source(expr: &NirExpr) -> Option<u32> {
    match &expr.kind {
        NirExprKind::Local { index, .. } => Some(*index),
        NirExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } => extract_alias_source(inner),
        NirExprKind::Block(block) => {
            let tail = block.stmts.last()?;
            let NirStmtKind::Expr(tail_expr) = &tail.kind else {
                return None;
            };
            extract_alias_source(tail_expr)
        }
        NirExprKind::LabeledBlock { label, block, .. } => {
            // Find the matching tail break and recurse on its value.
            // Conservatively only accept the LAST stmt being the break;
            // multi-break shapes don't have a single alias source.
            let last = block.stmts.last()?;
            let NirStmtKind::Break {
                label: Some(brk_label),
                value: Some(brk_value),
            } = &last.kind
            else {
                return None;
            };
            if brk_label != label {
                return None;
            }
            extract_alias_source(brk_value)
        }
        _ => None,
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
fn mark_local_as_fully_modified(expr: &NirExpr, modified: &mut ModifiedVars) {
    match &expr.kind {
        NirExprKind::Local { index, .. } => {
            modified.insert_full(*index);
        }
        NirExprKind::FieldAccess { expr: inner, .. } => {
            mark_local_as_fully_modified(inner, modified);
        }
        NirExprKind::Unary { expr: inner, .. } => {
            mark_local_as_fully_modified(inner, modified);
        }
        _ => {}
    }
}

/// A chain of field accesses bottoming out at a `Local` (`a`, `a.b`, `a.b.c`),
/// with no `Index`, deref, or call. A write through such a chain mutates an
/// inner object rather than reassigning a field of the root local.
fn is_pure_field_chain(expr: &NirExpr) -> bool {
    match &expr.kind {
        NirExprKind::Local { .. } => true,
        NirExprKind::FieldAccess { expr: inner, .. } => is_pure_field_chain(inner),
        _ => false,
    }
}

/// Strip all `Ref`/`MutRef` wrappers, returning the pointee type — the key for
/// type-based aliasing-write tracking (references to one pointee alias).
fn strip_references(type_id: TypeId, type_table: &TypeTable) -> TypeId {
    match type_table.get(type_id) {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            strip_references(*inner, type_table)
        }
        _ => type_id,
    }
}

/// If `expr` is a `&mut`-reference to a struct passed to a call, record its
/// pointee as clobbered: the callee may write any field of it, so no field of
/// that struct type is loop-invariant. Immutable `&T` args cannot mutate, and
/// by-value struct args are independent copies, so neither is recorded.
fn record_mut_ref_clobber(expr: &NirExpr, modified: &mut ModifiedVars, type_table: &TypeTable) {
    let mut ty = expr.type_id;
    // Peel `Ref(MutRef(..))` etc.; a `&mut` anywhere in the wrapper chain means
    // the callee can reach a mutable handle to the inner struct.
    let mut saw_mut = false;
    loop {
        match type_table.get(ty) {
            ResolvedType::MutRef(inner) => {
                saw_mut = true;
                ty = *inner;
            }
            ResolvedType::Ref(inner) => ty = *inner,
            _ => break,
        }
    }
    if saw_mut && matches!(type_table.get(ty), ResolvedType::Struct { .. }) {
        modified.insert_clobbered_pointee_type(ty);
    }
}

/// Record a field-access write into `written_field_types`, keyed by the pointee
/// type of the assigned object (`a.f`, `a.b.c`, `arr[i].c`, `(*p).c`).
fn record_written_field_type(
    target: &NirExpr,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    if let NirExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &target.kind
    {
        let pointee = strip_references(inner.type_id, type_table);
        modified.insert_written_field_type(pointee, *field_index);
    }
}

/// Mark what is modified by an assignment target. Field assignments (`buf.len =
/// x`) mark only that `(local, field)`, so LICM can still hoist `buf.repr`;
/// deeper/direct assignments mark the root fully. Every field-access write also
/// records its pointee-type key (`record_written_field_type`) so an aliasing
/// reference hoist is blocked (see `is_reference_field_aliasing_written`).
fn mark_assignment_target_as_modified(
    expr: &NirExpr,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    match &expr.kind {
        NirExprKind::Local { index, .. } => {
            // Direct assignment: `buf = x` — fully modified
            modified.insert_full(*index);
        }
        NirExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            // Type-keyed write for aliasing reference hoists, any chain shape.
            record_written_field_type(expr, modified, type_table);
            if let NirExprKind::Local { index, .. } = &inner.kind {
                // `buf.field = x` — only that field is modified.
                modified.insert_field(*index, *field_index);
            } else if is_pure_field_chain(inner) {
                // `a.b.c = x` mutates `*a.b`, not any field of the root `a`, so
                // don't mark `a` fully (keeps sibling chains `a.b.d` hoistable).
                // LICM peels one level per iteration; once `a.b` is a local the
                // write becomes single-level. Cross-reference aliasing of `*a.b`
                // is handled by the type-keyed write recorded above.
            } else {
                // `a[i].c = x`, `(*p).c = x`: conservatively mark root fully.
                mark_local_as_fully_modified(inner, modified);
            }
        }
        NirExprKind::Unary { expr: inner, .. } => {
            // E.g., `*ptr = x` — mark root local as fully modified
            mark_local_as_fully_modified(inner, modified);
        }
        _ => {}
    }
}

fn collect_modified_vars_in_stmt(
    stmt: &NirStmt,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    match &stmt.kind {
        NirStmtKind::Let {
            local_index, value, ..
        } => {
            // Let statements define new variables, mark them as modified
            // (they're not invariant within the loop where they're defined)
            modified.insert_full(*local_index);
            // Track GC aliases: `let a = b` (or `&b` / `&mut b` /
            // labeled-block or plain-block tailing in `b`) where `b` is a
            // local with GC type means `a` and `b` point to the same
            // heap object — so a write through one is observable through
            // the other.
            if let Some(src_idx) = extract_alias_source(value)
                && is_gc_heap_type(value.type_id, type_table)
            {
                modified.add_alias(*local_index, src_idx);
            }
            // Also check the value expression for mutable references
            collect_modified_vars_in_expr(value, modified, type_table);
        }
        NirStmtKind::Expr(expr) => {
            collect_modified_vars_in_expr(expr, modified, type_table);
        }
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_modified_vars_in_expr(v, modified, type_table);
            }
        }
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } => {
            collect_modified_vars_in_block(body, modified, type_table);
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            collect_modified_vars_in_block(block, modified, type_table);
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_modified_vars_in_expr(v, modified, type_table);
            }
        }
        NirStmtKind::Continue => {}
        NirStmtKind::LetDestructure { pattern, value, .. } => {
            // Collect pattern bindings as they are assigned
            collect_pattern_bindings(pattern, modified);
            // Also check the value expression for mutable references
            collect_modified_vars_in_expr(value, modified, type_table);
        }
    }
}

/// Collect all local variable indices bound by a pattern.
/// These variables are assigned fresh each time the pattern matches.
fn collect_pattern_bindings(pattern: &NirPattern, modified: &mut ModifiedVars) {
    match pattern {
        NirPattern::Binding { local_index, .. } => {
            modified.insert_full(*local_index);
        }
        NirPattern::Variant { bindings, .. } => {
            for binding in bindings {
                collect_pattern_bindings(binding, modified);
            }
        }
        NirPattern::Tuple(patterns, _) => {
            for p in patterns {
                collect_pattern_bindings(p, modified);
            }
        }
        NirPattern::Struct { fields, .. } => {
            for field in fields {
                collect_pattern_bindings(&field.pattern, modified);
            }
        }
        NirPattern::Wildcard
        | NirPattern::Literal(_)
        | NirPattern::Enum { .. }
        | NirPattern::ConstantValue { .. }
        | NirPattern::Range { .. } => {
            // No bindings
        }
        NirPattern::Or(alternatives) => {
            for p in alternatives {
                collect_pattern_bindings(p, modified);
            }
        }
    }
}

fn collect_modified_vars_in_expr(
    expr: &NirExpr,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    match &expr.kind {
        NirExprKind::Assign { target, value } => {
            // Mark the assignment target appropriately.
            // Field assignment (buf.field = x) only marks that specific field as modified,
            // enabling LICM to still hoist other fields of the same object.
            mark_assignment_target_as_modified(target, modified, type_table);
            collect_modified_vars_in_expr(target, modified, type_table);
            collect_modified_vars_in_expr(value, modified, type_table);
        }
        NirExprKind::Binary { left, right, .. } => {
            collect_modified_vars_in_expr(left, modified, type_table);
            collect_modified_vars_in_expr(right, modified, type_table);
        }
        NirExprKind::Unary { op, expr } => {
            // &mut local: the local may be reassigned through the ref (boxed primitives).
            // &mut local.field: in Wasm GC, this just reads the GC reference stored in the
            // field — neither the parent local nor the field reference itself changes.
            // Only mark the root as fully modified for a direct &mut local, not &mut local.field.
            if matches!(op, NirUnaryOp::MutRef) && matches!(expr.kind, NirExprKind::Local { .. }) {
                mark_local_as_fully_modified(expr, modified);
            }
            collect_modified_vars_in_expr(expr, modified, type_table);
        }
        NirExprKind::Cast { expr, .. } => {
            collect_modified_vars_in_expr(expr, modified, type_table);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                mark_gc_local_as_fully_modified(&arg.expr, modified, type_table);
                record_mut_ref_clobber(&arg.expr, modified, type_table);
                collect_modified_vars_in_expr(&arg.expr, modified, type_table);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            mark_gc_local_as_fully_modified(receiver, modified, type_table);
            record_mut_ref_clobber(receiver, modified, type_table);
            collect_modified_vars_in_expr(receiver, modified, type_table);
            for arg in args {
                mark_gc_local_as_fully_modified(&arg.expr, modified, type_table);
                record_mut_ref_clobber(&arg.expr, modified, type_table);
                collect_modified_vars_in_expr(&arg.expr, modified, type_table);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_modified_vars_in_expr(arg, modified, type_table);
            }
        }
        NirExprKind::FieldAccess { expr, .. } => {
            collect_modified_vars_in_expr(expr, modified, type_table);
        }
        NirExprKind::Index { expr, index } => {
            collect_modified_vars_in_expr(expr, modified, type_table);
            collect_modified_vars_in_expr(index, modified, type_table);
        }
        NirExprKind::Block(block) => {
            collect_modified_vars_in_block(block, modified, type_table);
        }
        NirExprKind::If {
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
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_modified_vars_in_expr(&field.value, modified, type_table);
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_modified_vars_in_expr(elem, modified, type_table);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            collect_modified_vars_in_expr(callee, modified, type_table);
            for arg in args {
                mark_gc_local_as_fully_modified(arg, modified, type_table);
                collect_modified_vars_in_expr(arg, modified, type_table);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            collect_modified_vars_in_expr(functor, modified, type_table);
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                collect_modified_vars_in_expr(payload_expr, modified, type_table);
            }
        }
        NirExprKind::LabeledBlock { block, .. } => {
            collect_modified_vars_in_block(block, modified, type_table);
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            collect_modified_vars_in_expr(value, modified, type_table);
        }
        NirExprKind::VariantTag { expr } | NirExprKind::VariantTest { expr, .. } => {
            collect_modified_vars_in_expr(expr, modified, type_table);
        }
        NirExprKind::VariantPayload { expr, .. } => {
            collect_modified_vars_in_expr(expr, modified, type_table);
        }
        NirExprKind::Switch {
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
            collect_modified_vars_in_expr(expr, modified, type_table);
            for arm in arms {
                collect_pattern_bindings(&arm.pattern, modified);
                if let Some(guard) = &arm.guard {
                    collect_modified_vars_in_expr(guard, modified, type_table);
                }
                collect_modified_vars_in_expr(&arm.body, modified, type_table);
            }
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
    block: &NirBlock,
    type_table: &TypeTable,
) -> IndexMap<u32, LicmRefBinding> {
    let mut bindings = IndexMap::default();
    collect_licm_ref_bindings_in_block(block, type_table, &mut bindings);
    bindings
}

fn collect_licm_ref_bindings_in_block(
    block: &NirBlock,
    type_table: &TypeTable,
    bindings: &mut IndexMap<u32, LicmRefBinding>,
) {
    for stmt in &block.stmts {
        collect_licm_ref_bindings_in_stmt(stmt, type_table, bindings);
    }
}

fn collect_licm_ref_bindings_in_stmt(
    stmt: &NirStmt,
    type_table: &TypeTable,
    bindings: &mut IndexMap<u32, LicmRefBinding>,
) {
    match &stmt.kind {
        NirStmtKind::Let {
            local_index,
            value,
            type_id,
            ..
        } => {
            // Check if this is: let x: &T = &y (immutable ref to a local)
            if matches!(type_table.get(*type_id), ResolvedType::Ref(_))
                && let NirExprKind::Unary {
                    op: NirUnaryOp::Ref,
                    expr: source,
                } = &value.kind
                && let NirExprKind::Local {
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
        NirStmtKind::Expr(expr) => {
            collect_licm_ref_bindings_in_expr(expr, type_table, bindings);
        }
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_licm_ref_bindings_in_expr(v, type_table, bindings);
            }
        }
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } => {
            collect_licm_ref_bindings_in_block(body, type_table, bindings);
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            collect_licm_ref_bindings_in_block(block, type_table, bindings);
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_licm_ref_bindings_in_expr(v, type_table, bindings);
            }
        }
        NirStmtKind::Continue => {}
        NirStmtKind::LetDestructure { value, .. } => {
            collect_licm_ref_bindings_in_expr(value, type_table, bindings);
        }
    }
}

fn collect_licm_ref_bindings_in_expr(
    expr: &NirExpr,
    type_table: &TypeTable,
    bindings: &mut IndexMap<u32, LicmRefBinding>,
) {
    // Recurse into all sub-expressions to find nested let bindings
    match &expr.kind {
        NirExprKind::Block(block) => {
            collect_licm_ref_bindings_in_block(block, type_table, bindings);
        }
        NirExprKind::LabeledBlock { block, .. } => {
            collect_licm_ref_bindings_in_block(block, type_table, bindings);
        }
        NirExprKind::If {
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
        NirExprKind::Binary { left, right, .. } => {
            collect_licm_ref_bindings_in_expr(left, type_table, bindings);
            collect_licm_ref_bindings_in_expr(right, type_table, bindings);
        }
        NirExprKind::Unary { expr: inner, .. } => {
            collect_licm_ref_bindings_in_expr(inner, type_table, bindings);
        }
        NirExprKind::FieldAccess { expr: inner, .. } => {
            collect_licm_ref_bindings_in_expr(inner, type_table, bindings);
        }
        NirExprKind::Index { expr: inner, index } => {
            collect_licm_ref_bindings_in_expr(inner, type_table, bindings);
            collect_licm_ref_bindings_in_expr(index, type_table, bindings);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                collect_licm_ref_bindings_in_expr(&arg.expr, type_table, bindings);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            collect_licm_ref_bindings_in_expr(receiver, type_table, bindings);
            for arg in args {
                collect_licm_ref_bindings_in_expr(&arg.expr, type_table, bindings);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_licm_ref_bindings_in_expr(arg, type_table, bindings);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            collect_licm_ref_bindings_in_expr(callee, type_table, bindings);
            for arg in args {
                collect_licm_ref_bindings_in_expr(arg, type_table, bindings);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            collect_licm_ref_bindings_in_expr(functor, type_table, bindings);
        }
        NirExprKind::Assign { target, value } => {
            collect_licm_ref_bindings_in_expr(target, type_table, bindings);
            collect_licm_ref_bindings_in_expr(value, type_table, bindings);
        }
        NirExprKind::Cast { expr: inner, .. } => {
            collect_licm_ref_bindings_in_expr(inner, type_table, bindings);
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_licm_ref_bindings_in_expr(&field.value, type_table, bindings);
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_licm_ref_bindings_in_expr(elem, type_table, bindings);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                collect_licm_ref_bindings_in_expr(payload_expr, type_table, bindings);
            }
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            collect_licm_ref_bindings_in_expr(value, type_table, bindings);
        }
        NirExprKind::VariantTag { expr } | NirExprKind::VariantTest { expr, .. } => {
            collect_licm_ref_bindings_in_expr(expr, type_table, bindings);
        }
        NirExprKind::VariantPayload { expr, .. } => {
            collect_licm_ref_bindings_in_expr(expr, type_table, bindings);
        }
        NirExprKind::Switch {
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
            collect_licm_ref_bindings_in_expr(expr, type_table, bindings);
            for arm in arms {
                collect_licm_ref_bindings_in_expr(&arm.body, type_table, bindings);
                if let Some(guard) = &arm.guard {
                    collect_licm_ref_bindings_in_expr(guard, type_table, bindings);
                }
            }
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
    block: &NirBlock,
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
    stmt: &NirStmt,
    modified_vars: &ModifiedVars,
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut IndexSet<(u32, u32)>,
    next_local: &mut u32,
) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } => {
            find_hoist_candidates_in_expr(
                value,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        NirStmtKind::Expr(expr) => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        NirStmtKind::Return { value } => {
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
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } => {
            find_hoist_candidates_in_block(
                body,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            find_hoist_candidates_in_block(
                block,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        NirStmtKind::Break { value, .. } => {
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
        NirStmtKind::Continue => {}
        NirStmtKind::LetDestructure { value, .. } => {
            find_hoist_candidates_in_expr(
                value,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
    }
}

fn find_hoist_candidates_in_expr(
    expr: &NirExpr,
    modified_vars: &ModifiedVars,
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut IndexSet<(u32, u32)>,
    next_local: &mut u32,
) {
    match &expr.kind {
        // This is the key pattern: field access on a loop-invariant local
        NirExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => {
            if let NirExprKind::Local { index, name } = &inner.kind {
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
        NirExprKind::Binary { left, right, .. } => {
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
        NirExprKind::Unary { expr, .. } => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        NirExprKind::Assign { target, value } => {
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
        NirExprKind::Cast { expr, .. } => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        NirExprKind::Call { args, .. } => {
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
        NirExprKind::MethodCall { receiver, args, .. } => {
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
        NirExprKind::CmRawCall { args, .. } => {
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
        NirExprKind::Index { expr, index } => {
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
        NirExprKind::Block(block) => {
            find_hoist_candidates_in_block(
                block,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        NirExprKind::If {
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
        NirExprKind::StructLiteral { fields, .. } => {
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
        NirExprKind::TupleLiteral { elements } => {
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
        NirExprKind::IndirectCall { callee, args, .. } => {
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
        NirExprKind::ClosureToCanonical { functor, .. } => {
            find_hoist_candidates_in_expr(
                functor,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        NirExprKind::VariantConstruct { payload, .. } => {
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
        NirExprKind::LabeledBlock { block, .. } => {
            find_hoist_candidates_in_block(
                block,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            find_hoist_candidates_in_expr(
                value,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        NirExprKind::VariantTag { expr } | NirExprKind::VariantTest { expr, .. } => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        NirExprKind::VariantPayload { expr, .. } => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        NirExprKind::Switch {
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
    }
}

/// Replace field accesses with references to hoisted locals
fn replace_hoisted_in_block(
    block: &mut NirBlock,
    candidates: &[HoistCandidate],
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
) {
    for stmt in &mut block.stmts {
        replace_hoisted_in_stmt(stmt, candidates, ref_bindings);
    }
}

fn replace_hoisted_in_stmt(
    stmt: &mut NirStmt,
    candidates: &[HoistCandidate],
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
) {
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. } => {
            replace_hoisted_in_expr(value, candidates, ref_bindings);
        }
        NirStmtKind::Expr(expr) => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
        }
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                replace_hoisted_in_expr(v, candidates, ref_bindings);
            }
        }
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } => {
            replace_hoisted_in_block(body, candidates, ref_bindings);
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            replace_hoisted_in_block(block, candidates, ref_bindings);
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                replace_hoisted_in_expr(v, candidates, ref_bindings);
            }
        }
        NirStmtKind::Continue => {}
        NirStmtKind::LetDestructure { value, .. } => {
            replace_hoisted_in_expr(value, candidates, ref_bindings);
        }
    }
}

fn replace_hoisted_in_expr(
    expr: &mut NirExpr,
    candidates: &[HoistCandidate],
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
) {
    // First, check if this expression matches a hoist candidate
    if let NirExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &expr.kind
        && let NirExprKind::Local { index, .. } = &inner.kind
    {
        // Case 1: Direct match - local.field where local is the hoisted source
        for candidate in candidates {
            if candidate.local_index == *index && candidate.field_index == *field_index {
                // Replace with a reference to the hoisted local
                expr.kind = NirExprKind::Local {
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
                    expr.kind = NirExprKind::Local {
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
        NirExprKind::FieldAccess { expr: inner, .. } => {
            replace_hoisted_in_expr(inner, candidates, ref_bindings);
        }
        NirExprKind::Binary { left, right, .. } => {
            replace_hoisted_in_expr(left, candidates, ref_bindings);
            replace_hoisted_in_expr(right, candidates, ref_bindings);
        }
        NirExprKind::Unary { expr, .. } => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
        }
        NirExprKind::Assign { target, value } => {
            replace_hoisted_in_expr(target, candidates, ref_bindings);
            replace_hoisted_in_expr(value, candidates, ref_bindings);
        }
        NirExprKind::Cast { expr, .. } => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                replace_hoisted_in_expr(&mut arg.expr, candidates, ref_bindings);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            replace_hoisted_in_expr(receiver, candidates, ref_bindings);
            for arg in args {
                replace_hoisted_in_expr(&mut arg.expr, candidates, ref_bindings);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                replace_hoisted_in_expr(arg, candidates, ref_bindings);
            }
        }
        NirExprKind::Index { expr, index } => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
            replace_hoisted_in_expr(index, candidates, ref_bindings);
        }
        NirExprKind::Block(block) => {
            replace_hoisted_in_block(block, candidates, ref_bindings);
        }
        NirExprKind::If {
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
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                replace_hoisted_in_expr(&mut field.value, candidates, ref_bindings);
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                replace_hoisted_in_expr(elem, candidates, ref_bindings);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            replace_hoisted_in_expr(callee, candidates, ref_bindings);
            for arg in args {
                replace_hoisted_in_expr(arg, candidates, ref_bindings);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            replace_hoisted_in_expr(functor, candidates, ref_bindings);
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                replace_hoisted_in_expr(payload_expr, candidates, ref_bindings);
            }
        }
        NirExprKind::LabeledBlock { block, .. } => {
            replace_hoisted_in_block(block, candidates, ref_bindings);
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            replace_hoisted_in_expr(value, candidates, ref_bindings);
        }
        NirExprKind::VariantTag { expr } | NirExprKind::VariantTest { expr, .. } => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
        }
        NirExprKind::VariantPayload { expr, .. } => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
        }
        NirExprKind::Switch {
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
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
            for arm in arms {
                replace_hoisted_in_expr(&mut arm.body, candidates, ref_bindings);
                if let Some(guard) = &mut arm.guard {
                    replace_hoisted_in_expr(guard, candidates, ref_bindings);
                }
            }
        }
    }
}
