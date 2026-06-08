//! Loop-Invariant Code Motion (LICM) for Wado NIR
//!
//! This module hoists loop-invariant computations out of loops to improve performance.
//! It identifies field accesses on variables that don't change within a loop and moves
//! those accesses before the loop.
//!
//! The pass reads and mutates the arena [`Body`] directly. The hoist-candidate
//! and replacement walks share a `*_child_nodes` enumerator that mirrors the
//! tree walk's child set exactly (expression and block children, excluding
//! patterns); `collect_modified_vars` keeps its own walk because it special-
//! cases assignments, calls, and pattern bindings.

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::nir::{NirFunction, NirLocal, NirUnaryOp};
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, ExprNode, PatKind, StmtId, StmtKind, StmtNode,
};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, GatedPass};

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
pub fn apply_licm(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let type_table = project.type_table.borrow();
    let len = project.functions.len();
    gate.run_gated(GatedPass::Licm, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        licm_function(&mut func, &type_table)
    })
}

/// Apply LICM to a function
fn licm_function(func: &mut NirFunction, type_table: &TypeTable) -> bool {
    if func.body.is_none() {
        return false;
    }
    let mut local_count = func.local_count;
    // The local list is read (original local types) *and* grown (hoist locals,
    // including second-level ones) during the walk, so thread an owned clone and
    // write it back once the body borrow ends.
    let mut locals = func.locals.clone();
    let mut outer_aliases: Vec<(u32, u32)> = Vec::new();
    let changed = {
        let body = func.body.as_mut().unwrap();
        let root = body.root;
        licm_block(
            body,
            root,
            &mut local_count,
            &mut locals,
            type_table,
            &mut outer_aliases,
        )
    };
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
/// candidates targeting the other alias.
fn licm_block(
    body: &mut Body,
    block: BlockId,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    outer_aliases: &mut Vec<(u32, u32)>,
) -> bool {
    let mut changed = false;
    let mut new_stmts = Vec::new();

    for s in std::mem::take(&mut body.blocks[block].stmts) {
        // Classify without holding the borrow across the mutable recursion.
        enum Shape {
            Loop(BlockId),
            If(BlockId, Option<BlockId>),
            Labeled(BlockId),
            Let(u32, ExprId),
            Other,
        }
        let shape = match &body.stmts[s].kind {
            StmtKind::Loop { body: lb } => Shape::Loop(*lb),
            StmtKind::If {
                then_block,
                else_block,
                ..
            } => Shape::If(*then_block, *else_block),
            StmtKind::LabeledBlock { block, .. } => Shape::Labeled(*block),
            StmtKind::Let {
                local_index, value, ..
            } => Shape::Let(*local_index, *value),
            _ => Shape::Other,
        };

        match shape {
            Shape::Loop(lb) => {
                let empty_set = IndexSet::default();
                let hoist_stmts = licm_loop(
                    body,
                    lb,
                    local_count,
                    locals,
                    type_table,
                    &empty_set,
                    outer_aliases,
                );
                if !hoist_stmts.is_empty() {
                    changed = true;
                }
                new_stmts.extend(hoist_stmts);
                new_stmts.push(s);
            }
            Shape::If(then_b, else_b) => {
                // Sharing the alias accumulator across sibling branches is safe:
                // aliasing is monotone-correct (extra aliases only cause
                // conservative misses, never wrong hoists).
                changed |= licm_block(body, then_b, local_count, locals, type_table, outer_aliases);
                if let Some(eb) = else_b {
                    changed |= licm_block(body, eb, local_count, locals, type_table, outer_aliases);
                }
                new_stmts.push(s);
            }
            Shape::Labeled(inner) => {
                changed |= licm_block(body, inner, local_count, locals, type_table, outer_aliases);
                new_stmts.push(s);
            }
            Shape::Let(local_index, value) => {
                // Track outer-scope aliases so a subsequent loop's LICM can see them.
                if let Some(src_idx) = extract_alias_source(body, value)
                    && is_gc_heap_type(body.exprs[value].type_id, type_table)
                {
                    outer_aliases.push((local_index, src_idx));
                }
                new_stmts.push(s);
            }
            Shape::Other => {
                new_stmts.push(s);
            }
        }
    }

    body.blocks[block].stmts = new_stmts;
    changed
}

/// Apply LICM to a single loop, returning hoisting statement ids to prepend.
fn licm_loop(
    body: &mut Body,
    loop_body: BlockId,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    type_table: &TypeTable,
    extra_modified: &IndexSet<u32>,
    outer_aliases: &[(u32, u32)],
) -> Vec<StmtId> {
    let mut all_hoist_stmts = Vec::new();

    // Run LICM iteratively until no more candidates are found (second-level
    // hoisting), bounded to avoid pathological cases.
    const MAX_LICM_ITERATIONS: usize = 10;
    for _iteration in 0..MAX_LICM_ITERATIONS {
        // Step 1: Collect all variables modified in the loop.
        let mut modified_vars = ModifiedVars::default();
        modified_vars.extend_full(extra_modified);
        for &(a, b) in outer_aliases {
            modified_vars.add_alias(a, b);
        }
        collect_modified_vars_in_block(body, loop_body, &mut modified_vars, type_table);

        // Step 2: Collect immutable reference bindings for look-through.
        let ref_bindings = collect_immutable_ref_bindings(body, loop_body, type_table);

        // Step 3: Find field accesses that can be hoisted.
        let mut candidates = Vec::new();
        let mut seen = IndexSet::default();
        let mut next_local = *local_count;
        find_hoist_candidates_in_block(
            body,
            loop_body,
            &modified_vars,
            &ref_bindings,
            &mut candidates,
            &mut seen,
            &mut next_local,
        );

        // Step 3.5: Drop `x.f` candidates where `x` is a reference and that
        // pointee field is written elsewhere in the loop.
        candidates.retain(|c| {
            let root_ty = if (c.local_index as usize) < locals.len() {
                locals[c.local_index as usize].type_id
            } else {
                c.type_id
            };
            !modified_vars.is_reference_field_aliasing_written(root_ty, c.field_index, type_table)
        });

        if candidates.is_empty() {
            // Field-hoisting has converged for this loop. Try hoisting maximal
            // loop-invariant pure-arithmetic subexpressions (e.g. the
            // `_licm_end - _licm_start` a scan loop recomputes in its guard
            // every iteration). Runs here, after field-hoisting, so the
            // `_licm_*` locals it created are visible as loop-invariant
            // operands.
            if hoist_invariant_arith(
                body,
                loop_body,
                &modified_vars,
                local_count,
                locals,
                &mut all_hoist_stmts,
            ) {
                continue;
            }
            break;
        }

        // Renumber the surviving hoist locals contiguously from `*local_count`.
        next_local = *local_count;
        for candidate in &mut candidates {
            candidate.new_local_index = next_local;
            next_local += 1;
        }

        // Step 4: Create hoisting statements.
        for candidate in &candidates {
            let local_type_id = if (candidate.local_index as usize) < locals.len() {
                locals[candidate.local_index as usize].type_id
            } else {
                candidate.type_id
            };

            // Build `local.field` as fresh arena nodes.
            let local_expr = body.exprs.push(ExprNode {
                kind: ExprKind::Local {
                    index: candidate.local_index,
                    name: candidate.local_name.clone(),
                },
                type_id: local_type_id,
                span: Span::new(0, 0, 0, 0),
            });
            let field_access_expr = body.exprs.push(ExprNode {
                kind: ExprKind::FieldAccess {
                    expr: local_expr,
                    field_index: candidate.field_index,
                    field_name: candidate.field_name.clone(),
                },
                type_id: candidate.type_id,
                span: Span::new(0, 0, 0, 0),
            });

            let hoist_name = format!(
                "_licm_{}_{}",
                candidate.field_name, candidate.new_local_index
            );
            let hoist_stmt = body.stmts.push(StmtNode {
                kind: StmtKind::Let {
                    name: hoist_name.clone(),
                    local_index: candidate.new_local_index,
                    is_mut: false,
                    is_reactive: false,
                    type_id: candidate.type_id,
                    value: field_access_expr,
                    skip_value_copy: true,
                },
                span: Span::new(0, 0, 0, 0),
            });
            all_hoist_stmts.push(hoist_stmt);

            // Add the local entry mirroring the let above.
            locals.push(NirLocal {
                name: hoist_name,
                type_id: candidate.type_id,
                is_mut: false,
            });
        }

        *local_count = next_local;

        // Step 5: Replace field accesses in the loop body with the hoisted locals.
        replace_hoisted_in_block(body, loop_body, &candidates, &ref_bindings);
    }

    // Nested loops: recurse. The nested `licm_block` accumulates aliases from
    // the outer loop's `let` statements on its own walk.
    let mut nested_aliases: Vec<(u32, u32)> = outer_aliases.to_vec();
    licm_block(
        body,
        loop_body,
        local_count,
        locals,
        type_table,
        &mut nested_aliases,
    );

    all_hoist_stmts
}

// ---------------------------------------------------------------------------
// Shared child enumeration (expression + block children, patterns excluded)
// ---------------------------------------------------------------------------

enum Child {
    Expr(ExprId),
    Block(BlockId),
}

/// The expression / block children of an expression, in walk order, *excluding*
/// patterns. Mirrors the child set of the tree `find_hoist`/`replace_hoist`/
/// `collect_licm_ref` walks (a `Match` yields its scrutinee plus each arm's
/// guard and body, never the arm pattern).
fn expr_child_nodes(body: &Body, e: ExprId) -> Vec<Child> {
    match &body.exprs[e].kind {
        ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::ClosureToCanonical { functor: inner, .. }
        | ExprKind::GlobalVarSet { value: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. } => vec![Child::Expr(*inner)],
        ExprKind::Binary { left, right, .. }
        | ExprKind::Assign {
            target: left,
            value: right,
        }
        | ExprKind::Index {
            expr: left,
            index: right,
        } => vec![Child::Expr(*left), Child::Expr(*right)],
        ExprKind::Call { args, .. } => args.iter().map(|a| Child::Expr(a.expr)).collect(),
        ExprKind::MethodCall { receiver, args, .. } => std::iter::once(Child::Expr(*receiver))
            .chain(args.iter().map(|a| Child::Expr(a.expr)))
            .collect(),
        ExprKind::CmRawCall { args, .. } => args.iter().map(|a| Child::Expr(*a)).collect(),
        ExprKind::IndirectCall { callee, args } => std::iter::once(Child::Expr(*callee))
            .chain(args.iter().map(|a| Child::Expr(*a)))
            .collect(),
        ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => vec![Child::Block(*b)],
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let mut v = vec![Child::Expr(*condition), Child::Block(*then_branch)];
            if let Some(eb) = else_branch {
                v.push(Child::Block(*eb));
            }
            v
        }
        ExprKind::StructLiteral { fields, .. } => {
            fields.iter().map(|f| Child::Expr(f.value)).collect()
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            elements.iter().map(|e| Child::Expr(*e)).collect()
        }
        ExprKind::VariantConstruct { payload, .. } => {
            payload.iter().map(|p| Child::Expr(*p)).collect()
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => std::iter::once(Child::Expr(*scrutinee))
            .chain(arms.iter().map(|a| Child::Block(*a)))
            .chain(std::iter::once(Child::Block(*default)))
            .collect(),
        ExprKind::Match { expr, arms } => {
            let mut v = vec![Child::Expr(*expr)];
            for arm in arms {
                v.push(Child::Expr(arm.body));
                if let Some(g) = arm.guard {
                    v.push(Child::Expr(g));
                }
            }
            v
        }
        // Leaves.
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
        | ExprKind::EnumConstruct { .. } => vec![],
    }
}

/// The expression / block children of a statement, in walk order, excluding the
/// `LetDestructure` pattern (matching the tree `find_hoist`/`replace_hoist`/
/// `collect_licm_ref` statement walks).
fn stmt_child_nodes(body: &Body, s: StmtId) -> Vec<Child> {
    match &body.stmts[s].kind {
        StmtKind::Let { value, .. }
        | StmtKind::Expr(value)
        | StmtKind::LetDestructure { value, .. } => vec![Child::Expr(*value)],
        StmtKind::Return { value } | StmtKind::Break { value, .. } => {
            value.iter().map(|v| Child::Expr(*v)).collect()
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut v = vec![Child::Expr(*condition), Child::Block(*then_block)];
            if let Some(eb) = else_block {
                v.push(Child::Block(*eb));
            }
            v
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            vec![Child::Block(*b)]
        }
        StmtKind::Continue => vec![],
    }
}

// ---------------------------------------------------------------------------
// Modified-variable collection (special-cased walk)
// ---------------------------------------------------------------------------

fn collect_modified_vars_in_block(
    body: &Body,
    block: BlockId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    for s in &body.blocks[block].stmts {
        collect_modified_vars_in_stmt(body, *s, modified, type_table);
    }
}

/// Mark a local as fully modified if it has a GC struct type and is passed to a
/// function call (callees can mutate any field). Immutable `&T` locals are
/// skipped — no callee can mutate the pointee through them.
fn mark_gc_local_as_fully_modified(
    body: &Body,
    e: ExprId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    if let ExprKind::Local { index, .. } = &body.exprs[e].kind
        && is_gc_heap_type(body.exprs[e].type_id, type_table)
    {
        if let ResolvedType::Ref(inner) = type_table.get(body.exprs[e].type_id)
            && !matches!(type_table.get(*inner), ResolvedType::MutRef(_))
        {
            return;
        }
        modified.insert_full(*index);
    }
}

/// Walk through reference wrappers and tail-return blocks to find the source
/// local a let-binding aliases. Alias-precision-only: missing an alias is a
/// soundness bug, extra aliases are at worst a missed optimisation.
fn extract_alias_source(body: &Body, e: ExprId) -> Option<u32> {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } => extract_alias_source(body, *inner),
        ExprKind::Block(block) => {
            let tail = *body.blocks[*block].stmts.last()?;
            let StmtKind::Expr(tail_expr) = &body.stmts[tail].kind else {
                return None;
            };
            extract_alias_source(body, *tail_expr)
        }
        ExprKind::LabeledBlock { label, block, .. } => {
            let last = *body.blocks[*block].stmts.last()?;
            let StmtKind::Break {
                label: Some(brk_label),
                value: Some(brk_value),
            } = &body.stmts[last].kind
            else {
                return None;
            };
            if brk_label != label {
                return None;
            }
            extract_alias_source(body, *brk_value)
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

/// Mark a local as fully modified, traversing through unary ops and nested
/// field accesses to the root.
fn mark_local_as_fully_modified(body: &Body, e: ExprId, modified: &mut ModifiedVars) {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => {
            modified.insert_full(*index);
        }
        ExprKind::FieldAccess { expr: inner, .. } | ExprKind::Unary { expr: inner, .. } => {
            mark_local_as_fully_modified(body, *inner, modified);
        }
        _ => {}
    }
}

/// A chain of field accesses bottoming out at a `Local` (`a`, `a.b`, `a.b.c`),
/// with no `Index`, deref, or call.
fn is_pure_field_chain(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { .. } => true,
        ExprKind::FieldAccess { expr: inner, .. } => is_pure_field_chain(body, *inner),
        _ => false,
    }
}

/// Strip all `Ref`/`MutRef` wrappers, returning the pointee type.
fn strip_references(type_id: TypeId, type_table: &TypeTable) -> TypeId {
    match type_table.get(type_id) {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            strip_references(*inner, type_table)
        }
        _ => type_id,
    }
}

/// If `expr` is a `&mut`-reference to a struct passed to a call, record its
/// pointee as clobbered.
fn record_mut_ref_clobber(
    body: &Body,
    e: ExprId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    let mut ty = body.exprs[e].type_id;
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
/// type of the assigned object.
fn record_written_field_type(
    body: &Body,
    target: ExprId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    if let ExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &body.exprs[target].kind
    {
        let pointee = strip_references(body.exprs[*inner].type_id, type_table);
        modified.insert_written_field_type(pointee, *field_index);
    }
}

/// Mark what is modified by an assignment target.
fn mark_assignment_target_as_modified(
    body: &Body,
    e: ExprId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => {
            modified.insert_full(*index);
        }
        ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            let inner = *inner;
            let field_index = *field_index;
            record_written_field_type(body, e, modified, type_table);
            if let ExprKind::Local { index, .. } = &body.exprs[inner].kind {
                modified.insert_field(*index, field_index);
            } else if is_pure_field_chain(body, inner) {
                // `a.b.c = x` mutates `*a.b`, not a field of the root `a`.
            } else {
                mark_local_as_fully_modified(body, inner, modified);
            }
        }
        ExprKind::Unary { expr: inner, .. } => {
            mark_local_as_fully_modified(body, *inner, modified);
        }
        _ => {}
    }
}

fn collect_modified_vars_in_stmt(
    body: &Body,
    s: StmtId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    match &body.stmts[s].kind {
        StmtKind::Let {
            local_index, value, ..
        } => {
            let local_index = *local_index;
            let value = *value;
            modified.insert_full(local_index);
            if let Some(src_idx) = extract_alias_source(body, value)
                && is_gc_heap_type(body.exprs[value].type_id, type_table)
            {
                modified.add_alias(local_index, src_idx);
            }
            collect_modified_vars_in_expr(body, value, modified, type_table);
        }
        StmtKind::Expr(expr) => {
            collect_modified_vars_in_expr(body, *expr, modified, type_table);
        }
        StmtKind::Return { value } => {
            if let Some(v) = value {
                collect_modified_vars_in_expr(body, *v, modified, type_table);
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let condition = *condition;
            let then_block = *then_block;
            let else_block = *else_block;
            collect_modified_vars_in_expr(body, condition, modified, type_table);
            collect_modified_vars_in_block(body, then_block, modified, type_table);
            if let Some(eb) = else_block {
                collect_modified_vars_in_block(body, eb, modified, type_table);
            }
        }
        StmtKind::Loop { body: lb } => {
            collect_modified_vars_in_block(body, *lb, modified, type_table);
        }
        StmtKind::LabeledBlock { block, .. } => {
            collect_modified_vars_in_block(body, *block, modified, type_table);
        }
        StmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_modified_vars_in_expr(body, *v, modified, type_table);
            }
        }
        StmtKind::Continue => {}
        StmtKind::LetDestructure { pattern, value, .. } => {
            let pattern = *pattern;
            let value = *value;
            collect_pattern_bindings(body, pattern, modified);
            collect_modified_vars_in_expr(body, value, modified, type_table);
        }
    }
}

/// Collect all local variable indices bound by a pattern.
fn collect_pattern_bindings(
    body: &Body,
    pat: crate::nir_arena::PatId,
    modified: &mut ModifiedVars,
) {
    match &body.pats[pat].kind {
        PatKind::Binding { local_index, .. } => {
            modified.insert_full(*local_index);
        }
        PatKind::Variant { bindings, .. } => {
            let bindings = bindings.clone();
            for b in bindings {
                collect_pattern_bindings(body, b, modified);
            }
        }
        PatKind::Tuple(patterns, _) => {
            let patterns = patterns.clone();
            for p in patterns {
                collect_pattern_bindings(body, p, modified);
            }
        }
        PatKind::Struct { fields, .. } => {
            let fields: Vec<_> = fields.iter().map(|f| f.pattern).collect();
            for p in fields {
                collect_pattern_bindings(body, p, modified);
            }
        }
        PatKind::Or(alternatives) => {
            let alternatives = alternatives.clone();
            for p in alternatives {
                collect_pattern_bindings(body, p, modified);
            }
        }
        PatKind::Wildcard
        | PatKind::Literal(_)
        | PatKind::Enum { .. }
        | PatKind::ConstantValue { .. }
        | PatKind::Range { .. } => {}
    }
}

fn collect_modified_vars_in_expr(
    body: &Body,
    e: ExprId,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    match &body.exprs[e].kind {
        ExprKind::Assign { target, value } => {
            let target = *target;
            let value = *value;
            mark_assignment_target_as_modified(body, target, modified, type_table);
            collect_modified_vars_in_expr(body, target, modified, type_table);
            collect_modified_vars_in_expr(body, value, modified, type_table);
        }
        ExprKind::Binary { left, right, .. } => {
            let left = *left;
            let right = *right;
            collect_modified_vars_in_expr(body, left, modified, type_table);
            collect_modified_vars_in_expr(body, right, modified, type_table);
        }
        ExprKind::Unary { op, expr: inner } => {
            let inner = *inner;
            if matches!(op, NirUnaryOp::MutRef)
                && matches!(body.exprs[inner].kind, ExprKind::Local { .. })
            {
                mark_local_as_fully_modified(body, inner, modified);
            }
            collect_modified_vars_in_expr(body, inner, modified, type_table);
        }
        ExprKind::Cast { expr: inner, .. } => {
            collect_modified_vars_in_expr(body, *inner, modified, type_table);
        }
        ExprKind::Call { args, .. } => {
            let arg_ids: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            for a in arg_ids {
                mark_gc_local_as_fully_modified(body, a, modified, type_table);
                record_mut_ref_clobber(body, a, modified, type_table);
                collect_modified_vars_in_expr(body, a, modified, type_table);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            let receiver = *receiver;
            let arg_ids: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            mark_gc_local_as_fully_modified(body, receiver, modified, type_table);
            record_mut_ref_clobber(body, receiver, modified, type_table);
            collect_modified_vars_in_expr(body, receiver, modified, type_table);
            for a in arg_ids {
                mark_gc_local_as_fully_modified(body, a, modified, type_table);
                record_mut_ref_clobber(body, a, modified, type_table);
                collect_modified_vars_in_expr(body, a, modified, type_table);
            }
        }
        ExprKind::CmRawCall { args, .. } => {
            let arg_ids = args.clone();
            for a in arg_ids {
                collect_modified_vars_in_expr(body, a, modified, type_table);
            }
        }
        ExprKind::FieldAccess { expr: inner, .. } => {
            collect_modified_vars_in_expr(body, *inner, modified, type_table);
        }
        ExprKind::Index { expr: inner, index } => {
            let inner = *inner;
            let index = *index;
            collect_modified_vars_in_expr(body, inner, modified, type_table);
            collect_modified_vars_in_expr(body, index, modified, type_table);
        }
        ExprKind::Block(block) => {
            collect_modified_vars_in_block(body, *block, modified, type_table);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let condition = *condition;
            let then_branch = *then_branch;
            let else_branch = *else_branch;
            collect_modified_vars_in_expr(body, condition, modified, type_table);
            collect_modified_vars_in_block(body, then_branch, modified, type_table);
            if let Some(eb) = else_branch {
                collect_modified_vars_in_block(body, eb, modified, type_table);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            let vals: Vec<ExprId> = fields.iter().map(|f| f.value).collect();
            for v in vals {
                collect_modified_vars_in_expr(body, v, modified, type_table);
            }
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            let elements = elements.clone();
            for el in elements {
                collect_modified_vars_in_expr(body, el, modified, type_table);
            }
        }
        ExprKind::IndirectCall { callee, args } => {
            let callee = *callee;
            let arg_ids = args.clone();
            collect_modified_vars_in_expr(body, callee, modified, type_table);
            for a in arg_ids {
                mark_gc_local_as_fully_modified(body, a, modified, type_table);
                collect_modified_vars_in_expr(body, a, modified, type_table);
            }
        }
        ExprKind::ClosureToCanonical { functor, .. } => {
            collect_modified_vars_in_expr(body, *functor, modified, type_table);
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                collect_modified_vars_in_expr(body, *p, modified, type_table);
            }
        }
        ExprKind::LabeledBlock { block, .. } => {
            collect_modified_vars_in_block(body, *block, modified, type_table);
        }
        ExprKind::GlobalVarSet { value, .. } => {
            collect_modified_vars_in_expr(body, *value, modified, type_table);
        }
        ExprKind::VariantTag { expr } | ExprKind::VariantTest { expr, .. } => {
            collect_modified_vars_in_expr(body, *expr, modified, type_table);
        }
        ExprKind::VariantPayload { expr, .. } => {
            collect_modified_vars_in_expr(body, *expr, modified, type_table);
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
            collect_modified_vars_in_expr(body, scrutinee, modified, type_table);
            for arm in arms {
                collect_modified_vars_in_block(body, arm, modified, type_table);
            }
            collect_modified_vars_in_block(body, default, modified, type_table);
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
            let arm_data: Vec<(crate::nir_arena::PatId, Option<ExprId>, ExprId)> =
                arms.iter().map(|a| (a.pattern, a.guard, a.body)).collect();
            collect_modified_vars_in_expr(body, expr, modified, type_table);
            for (pattern, guard, body_expr) in arm_data {
                collect_pattern_bindings(body, pattern, modified);
                if let Some(g) = guard {
                    collect_modified_vars_in_expr(body, g, modified, type_table);
                }
                collect_modified_vars_in_expr(body, body_expr, modified, type_table);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Immutable-reference binding collection
// ---------------------------------------------------------------------------

/// Information about an immutable reference binding: `let ref_var: &T = &source_var`
#[derive(Debug, Clone)]
struct LicmRefBinding {
    source_index: u32,
    source_name: String,
}

fn collect_immutable_ref_bindings(
    body: &Body,
    block: BlockId,
    type_table: &TypeTable,
) -> IndexMap<u32, LicmRefBinding> {
    let mut bindings = IndexMap::default();
    collect_licm_ref_bindings_in_block(body, block, type_table, &mut bindings);
    bindings
}

fn collect_licm_ref_bindings_in_block(
    body: &Body,
    block: BlockId,
    type_table: &TypeTable,
    bindings: &mut IndexMap<u32, LicmRefBinding>,
) {
    for s in &body.blocks[block].stmts {
        collect_licm_ref_bindings_in_stmt(body, *s, type_table, bindings);
    }
}

fn collect_licm_ref_bindings_in_stmt(
    body: &Body,
    s: StmtId,
    type_table: &TypeTable,
    bindings: &mut IndexMap<u32, LicmRefBinding>,
) {
    // `let x: &T = &y` (immutable ref to a local) records `x -> y`.
    if let StmtKind::Let {
        local_index,
        value,
        type_id,
        ..
    } = &body.stmts[s].kind
    {
        let local_index = *local_index;
        let value = *value;
        if matches!(type_table.get(*type_id), ResolvedType::Ref(_))
            && let ExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr: source,
            } = &body.exprs[value].kind
            && let ExprKind::Local {
                index: source_idx,
                name: source_name,
            } = &body.exprs[*source].kind
        {
            bindings.insert(
                local_index,
                LicmRefBinding {
                    source_index: *source_idx,
                    source_name: source_name.clone(),
                },
            );
        }
    }
    // Recurse into the statement's expression / block children.
    for child in stmt_child_nodes(body, s) {
        match child {
            Child::Expr(e) => collect_licm_ref_bindings_in_expr(body, e, type_table, bindings),
            Child::Block(b) => collect_licm_ref_bindings_in_block(body, b, type_table, bindings),
        }
    }
}

fn collect_licm_ref_bindings_in_expr(
    body: &Body,
    e: ExprId,
    type_table: &TypeTable,
    bindings: &mut IndexMap<u32, LicmRefBinding>,
) {
    for child in expr_child_nodes(body, e) {
        match child {
            Child::Expr(c) => collect_licm_ref_bindings_in_expr(body, c, type_table, bindings),
            Child::Block(b) => collect_licm_ref_bindings_in_block(body, b, type_table, bindings),
        }
    }
}

// ---------------------------------------------------------------------------
// Hoist-candidate detection
// ---------------------------------------------------------------------------

/// Represents a hoistable expression with its replacement info.
#[derive(Debug)]
struct HoistCandidate {
    local_index: u32,
    local_name: String,
    field_index: u32,
    field_name: String,
    type_id: TypeId,
    new_local_index: u32,
}

fn find_hoist_candidates_in_block(
    body: &Body,
    block: BlockId,
    modified_vars: &ModifiedVars,
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut IndexSet<(u32, u32)>,
    next_local: &mut u32,
) {
    for s in &body.blocks[block].stmts {
        find_hoist_candidates_in_stmt(
            body,
            *s,
            modified_vars,
            ref_bindings,
            candidates,
            seen,
            next_local,
        );
    }
}

fn find_hoist_candidates_in_stmt(
    body: &Body,
    s: StmtId,
    modified_vars: &ModifiedVars,
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut IndexSet<(u32, u32)>,
    next_local: &mut u32,
) {
    for child in stmt_child_nodes(body, s) {
        match child {
            Child::Expr(e) => find_hoist_candidates_in_expr(
                body,
                e,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            ),
            Child::Block(b) => find_hoist_candidates_in_block(
                body,
                b,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            ),
        }
    }
}

fn find_hoist_candidates_in_expr(
    body: &Body,
    e: ExprId,
    modified_vars: &ModifiedVars,
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut IndexSet<(u32, u32)>,
    next_local: &mut u32,
) {
    // The key pattern: field access on a loop-invariant local.
    if let ExprKind::FieldAccess {
        expr: inner,
        field_index,
        field_name,
    } = &body.exprs[e].kind
        && let ExprKind::Local { index, name } = &body.exprs[*inner].kind
    {
        let field_index = *field_index;
        // Case 1: direct access on a loop-invariant local.
        if modified_vars.is_field_hoistable(*index, field_index) {
            let key = (*index, field_index);
            if !seen.contains(&key) {
                seen.insert(key);
                candidates.push(HoistCandidate {
                    local_index: *index,
                    local_name: name.clone(),
                    field_index,
                    field_name: field_name.clone(),
                    type_id: body.exprs[e].type_id,
                    new_local_index: *next_local,
                });
                *next_local += 1;
            }
        }
        // Case 2: access through an immutable reference to a loop-invariant local.
        else if let Some(ref_binding) = ref_bindings.get(index)
            && modified_vars.is_field_hoistable(ref_binding.source_index, field_index)
        {
            let key = (ref_binding.source_index, field_index);
            if !seen.contains(&key) {
                seen.insert(key);
                candidates.push(HoistCandidate {
                    local_index: ref_binding.source_index,
                    local_name: ref_binding.source_name.clone(),
                    field_index,
                    field_name: field_name.clone(),
                    type_id: body.exprs[e].type_id,
                    new_local_index: *next_local,
                });
                *next_local += 1;
            }
        }
    }
    // Recurse into children.
    for child in expr_child_nodes(body, e) {
        match child {
            Child::Expr(c) => find_hoist_candidates_in_expr(
                body,
                c,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            ),
            Child::Block(b) => find_hoist_candidates_in_block(
                body,
                b,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Loop-invariant pure-arithmetic hoisting
// ---------------------------------------------------------------------------

/// Binary ops that are pure and total (cannot trap, no side effects), so a
/// loop-invariant instance can be speculatively computed once in the
/// pre-header. `Div` / `Mod` are excluded (trap on a zero divisor — hoisting
/// out of a possibly-zero-iteration loop could trap where the original would
/// not). `RefEq` / `RefNotEq` are excluded (reference operands, not arithmetic).
fn is_hoistable_binop(op: crate::nir::NirBinaryOp) -> bool {
    use crate::nir::NirBinaryOp::{
        Add, And, BitAnd, BitOr, BitXor, Eq, Gt, GtEq, Lt, LtEq, Mul, NotEq, Or, Shl, Shr, Sub,
    };
    matches!(
        op,
        Add | Sub
            | Mul
            | Eq
            | NotEq
            | Lt
            | LtEq
            | Gt
            | GtEq
            | And
            | Or
            | BitAnd
            | BitOr
            | BitXor
            | Shl
            | Shr
    )
}

/// Whether `e` is a pure-arithmetic tree whose every leaf is a loop-invariant
/// scalar local or a numeric/bool/char literal. Such a tree evaluates to the
/// same value on every iteration. A `Local` is invariant when
/// `collect_modified_vars` did not mark it fully modified — which covers
/// reassignment, `&mut`/`&` borrows, by-reference call args, and loop-body
/// `let`/pattern bindings.
///
/// `Cast` is deliberately excluded: a float→int cast lowers to the trapping
/// `i32.trunc_f64_s` family (not `trunc_sat`), so hoisting one to the
/// pre-header could trap on a NaN/out-of-range value where a zero-iteration
/// loop never would — the same trap-soundness reason `Div`/`Mod` are excluded.
fn is_invariant_arith(body: &Body, e: ExprId, modified: &ModifiedVars) -> bool {
    match &body.exprs[e].kind {
        ExprKind::IntLiteral { .. }
        | ExprKind::FloatLiteral { .. }
        | ExprKind::BoolLiteral(_)
        | ExprKind::CharLiteral(_) => true,
        ExprKind::Local { index, .. } => !modified.fully.contains(index),
        ExprKind::Binary { left, op, right } => {
            is_hoistable_binop(*op)
                && is_invariant_arith(body, *left, modified)
                && is_invariant_arith(body, *right, modified)
        }
        ExprKind::Unary { op, expr } => {
            matches!(op, NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot)
                && is_invariant_arith(body, *expr, modified)
        }
        _ => false,
    }
}

/// Whether the arithmetic tree contains at least one `Local` leaf. A
/// constant-only tree is left for constant folding — hoisting it gains nothing.
fn arith_has_local(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { .. } => true,
        ExprKind::Binary { left, right, .. } => {
            arith_has_local(body, *left) || arith_has_local(body, *right)
        }
        ExprKind::Unary { expr, .. } => arith_has_local(body, *expr),
        _ => false,
    }
}

/// Whether `e` is a compound (`Binary` / `Unary`) loop-invariant arithmetic
/// expression worth hoisting into a pre-loop temp.
fn is_hoistable_invariant_compound(body: &Body, e: ExprId, modified: &ModifiedVars) -> bool {
    let compound = matches!(
        &body.exprs[e].kind,
        ExprKind::Binary { .. } | ExprKind::Unary { .. }
    );
    compound && is_invariant_arith(body, e, modified) && arith_has_local(body, e)
}

/// Structural equality over the hoistable-arithmetic grammar (`Local`,
/// numeric/bool/char literals, `Binary` / `Unary`). Used to dedup equal
/// invariant expressions into a single hoisted temp.
fn arith_exprs_equal(body: &Body, a: ExprId, b: ExprId) -> bool {
    if body.exprs[a].type_id != body.exprs[b].type_id {
        return false;
    }
    match (&body.exprs[a].kind, &body.exprs[b].kind) {
        (ExprKind::Local { index: i1, .. }, ExprKind::Local { index: i2, .. }) => i1 == i2,
        (ExprKind::IntLiteral { value: v1, .. }, ExprKind::IntLiteral { value: v2, .. }) => {
            v1 == v2
        }
        (ExprKind::FloatLiteral { value: v1, .. }, ExprKind::FloatLiteral { value: v2, .. }) => {
            v1.to_bits() == v2.to_bits()
        }
        (ExprKind::BoolLiteral(x), ExprKind::BoolLiteral(y)) => x == y,
        (ExprKind::CharLiteral(x), ExprKind::CharLiteral(y)) => x == y,
        (
            ExprKind::Binary {
                left: l1,
                op: o1,
                right: r1,
            },
            ExprKind::Binary {
                left: l2,
                op: o2,
                right: r2,
            },
        ) => o1 == o2 && arith_exprs_equal(body, *l1, *l2) && arith_exprs_equal(body, *r1, *r2),
        (ExprKind::Unary { op: o1, expr: e1 }, ExprKind::Unary { op: o2, expr: e2 }) => {
            o1 == o2 && arith_exprs_equal(body, *e1, *e2)
        }
        _ => false,
    }
}

/// Collect the maximal loop-invariant arithmetic subexpressions in `block`.
/// "Maximal" means a hoistable expression whose parent is not itself
/// hoistable, so each whole invariant tree is hoisted once. Nested loops are
/// skipped — the recursive `licm_loop` call hoists each nested loop's own
/// invariants into that loop's pre-header.
fn collect_invariant_arith_in_block(
    body: &Body,
    block: BlockId,
    modified: &ModifiedVars,
    out: &mut Vec<ExprId>,
) {
    for s in &body.blocks[block].stmts {
        collect_invariant_arith_in_stmt(body, *s, modified, out);
    }
}

fn collect_invariant_arith_in_stmt(
    body: &Body,
    s: StmtId,
    modified: &ModifiedVars,
    out: &mut Vec<ExprId>,
) {
    // Do not descend into nested loops: their invariant expressions are
    // hoisted to their own pre-header by the recursive `licm_loop` call.
    if matches!(body.stmts[s].kind, StmtKind::Loop { .. }) {
        return;
    }
    for child in stmt_child_nodes(body, s) {
        match child {
            Child::Expr(e) => collect_invariant_arith_in_expr(body, e, modified, out),
            Child::Block(b) => collect_invariant_arith_in_block(body, b, modified, out),
        }
    }
}

fn collect_invariant_arith_in_expr(
    body: &Body,
    e: ExprId,
    modified: &ModifiedVars,
    out: &mut Vec<ExprId>,
) {
    if is_hoistable_invariant_compound(body, e, modified) {
        out.push(e);
        return; // maximal: do not recurse into a hoisted tree's children.
    }
    for child in expr_child_nodes(body, e) {
        match child {
            Child::Expr(c) => collect_invariant_arith_in_expr(body, c, modified, out),
            Child::Block(b) => collect_invariant_arith_in_block(body, b, modified, out),
        }
    }
}

/// Hoist maximal loop-invariant pure-arithmetic subexpressions out of
/// `loop_body`, structurally deduping equal expressions into one temp each.
/// Returns whether anything was hoisted. The pre-header `let`s are appended
/// to `all_hoist_stmts` (prepended before the loop by the caller).
fn hoist_invariant_arith(
    body: &mut Body,
    loop_body: BlockId,
    modified: &ModifiedVars,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
    all_hoist_stmts: &mut Vec<StmtId>,
) -> bool {
    let mut found = Vec::new();
    collect_invariant_arith_in_block(body, loop_body, modified, &mut found);
    if found.is_empty() {
        return false;
    }

    // Group occurrences by structural equality (representative = first seen).
    let mut groups: Vec<Vec<ExprId>> = Vec::new();
    'next: for e in found {
        for g in &mut groups {
            if arith_exprs_equal(body, g[0], e) {
                g.push(e);
                continue 'next;
            }
        }
        groups.push(vec![e]);
    }

    for occ in groups {
        let rep = occ[0];
        let type_id = body.exprs[rep].type_id;
        let new_idx = *local_count;
        *local_count += 1;
        let name = format!("_licm_arith_{new_idx}");

        // Clone the representative into the pre-header `let` *before* rewriting
        // the in-loop occurrences (which include `rep` itself) to a `Local`.
        let value = body.clone_expr(rep);
        let let_stmt = body.stmts.push(StmtNode {
            kind: StmtKind::Let {
                name: name.clone(),
                local_index: new_idx,
                is_mut: false,
                is_reactive: false,
                type_id,
                value,
                skip_value_copy: true,
            },
            span: Span::new(0, 0, 0, 0),
        });
        all_hoist_stmts.push(let_stmt);
        locals.push(NirLocal {
            name: name.clone(),
            type_id,
            is_mut: false,
        });

        for o in occ {
            body.exprs[o].kind = ExprKind::Local {
                index: new_idx,
                name: name.clone(),
            };
        }
    }

    true
}

// ---------------------------------------------------------------------------
// Replace hoisted field accesses with the hoisted locals
// ---------------------------------------------------------------------------

fn replace_hoisted_in_block(
    body: &mut Body,
    block: BlockId,
    candidates: &[HoistCandidate],
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
) {
    for s in body.blocks[block].stmts.clone() {
        replace_hoisted_in_stmt(body, s, candidates, ref_bindings);
    }
}

fn replace_hoisted_in_stmt(
    body: &mut Body,
    s: StmtId,
    candidates: &[HoistCandidate],
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
) {
    for child in stmt_child_nodes(body, s) {
        match child {
            Child::Expr(e) => replace_hoisted_in_expr(body, e, candidates, ref_bindings),
            Child::Block(b) => replace_hoisted_in_block(body, b, candidates, ref_bindings),
        }
    }
}

fn replace_hoisted_in_expr(
    body: &mut Body,
    e: ExprId,
    candidates: &[HoistCandidate],
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
) {
    // First, check if this expression matches a hoist candidate.
    let matched = if let ExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &body.exprs[e].kind
        && let ExprKind::Local { index, .. } = &body.exprs[*inner].kind
    {
        let index = *index;
        let field_index = *field_index;
        // Case 1: direct match — local.field where local is the hoisted source.
        let direct = candidates
            .iter()
            .find(|c| c.local_index == index && c.field_index == field_index);
        if let Some(c) = direct {
            Some((c.new_local_index, c.field_name.clone()))
        } else if let Some(ref_binding) = ref_bindings.get(&index) {
            // Case 2: look through immutable reference — ref_var.field.
            candidates
                .iter()
                .find(|c| c.local_index == ref_binding.source_index && c.field_index == field_index)
                .map(|c| (c.new_local_index, c.field_name.clone()))
        } else {
            None
        }
    } else {
        None
    };
    if let Some((new_local_index, field_name)) = matched {
        body.exprs[e].kind = ExprKind::Local {
            index: new_local_index,
            name: format!("_licm_{field_name}_{new_local_index}"),
        };
        return;
    }

    // Recurse into sub-expressions / sub-blocks.
    for child in expr_child_nodes(body, e) {
        match child {
            Child::Expr(c) => replace_hoisted_in_expr(body, c, candidates, ref_bindings),
            Child::Block(b) => replace_hoisted_in_block(body, b, candidates, ref_bindings),
        }
    }
}
