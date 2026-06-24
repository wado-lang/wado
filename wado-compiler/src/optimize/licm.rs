//! Loop-Invariant Code Motion (LICM) for Wado NIR
//!
//! This module hoists loop-invariant computations out of loops to improve performance.
//! Two kinds of candidates move to the pre-header: field accesses on
//! variables the loop does not modify (legality via [`ModifiedVars`]), and
//! pure-arithmetic subtrees whose `Local` leaves are pre-header-stable —
//! each leaf's use-site `ValueId` equals the loop-entry snapshot value
//! ([`Engine::loop_entry_value`]), deduped by `ValueId` (see [`ArithHoist`]).
//!
//! Runs on the worklist rewrite engine (combine migration; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a [`Rule`]: a
//! per-function standalone engine session whose `apply_block` fires once at
//! the body root and applies LICM to every loop in the function. All
//! mutations route through the engine edit API (`alloc_expr`, `alloc_stmt`,
//! `alloc_local`, `clone_expr`, `set_block_stmts`, `replace_expr_kind`) so
//! the parent map and use index stay coherent.
//!
//! The hoist-candidate and replacement walks share a `*_child_nodes`
//! enumerator that mirrors the tree walk's child set exactly (expression and
//! block children, excluding patterns); `collect_modified_vars` keeps its own
//! walk because it special-cases assignments, calls, and pattern bindings.

use std::cell::Cell;

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::nir::{NirBinaryOp, NirFunction, NirUnaryOp};
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatKind, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueId;
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

    /// Whether `local_idx` (or any alias) is fully modified in the loop — i.e. it
    /// is **not** loop-invariant. An in-loop `let` counts as a modification
    /// (`collect_modified_vars` inserts the bound local), so a leaf bound inside
    /// the loop is correctly non-invariant. This is exactly the invariance the
    /// `ValueGraph`'s `use-site value == loop-entry value` check computed, so an
    /// arith-hoist leaf check can read it instead of querying `value_of`.
    fn local_modified(&self, local_idx: u32) -> bool {
        self.alias_set(local_idx)
            .iter()
            .any(|idx| self.fully.contains(idx))
    }
}

/// Apply Loop-Invariant Code Motion to all functions in the project.
pub fn apply_licm(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let type_table = project.type_table.borrow();
    let first_param_types = super::alias::first_param_types(project);
    let call_immutability = super::alias::CallImmutability::new(project, &type_table);
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::Licm, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
        }
        let rule = LicmRule {
            type_table: &type_table,
            applied: Cell::new(false),
        };
        let NirFunction {
            body,
            locals,
            params,
            address_taken_locals,
            stores_aliased_locals,
            ..
        } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let (aliased, untrackable, mut_escaped) = super::alias::builder_alias_sets(
            body,
            locals,
            address_taken_locals,
            stores_aliased_locals,
            &type_table,
            &first_param_types,
            &call_immutability,
        );
        let param_locals: Vec<u32> = params.iter().map(|p| p.local_index).collect();
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.set_alias_sets(aliased, untrackable, mut_escaped);
        engine.set_value_graph_type_table(&type_table);
        engine.set_param_locals(param_locals);
        let licm_changed = engine.run(&[&rule]);
        // Condition implication shares licm's session: licm hoists only
        // loop-invariant, move-safe code, so values are preserved and the
        // ValueGraph stays valid. cond-impl runs after licm here — the same
        // document order as the standalone passes — so it still sees the hoisted
        // body.
        let cond_changed = super::condition_implication::eliminate_at_root(&mut engine);
        licm_changed || cond_changed
    })
}

/// Standalone-session rule whose single `apply_block` performs the whole-
/// function LICM walk at the body root.
pub(super) struct LicmRule<'a> {
    type_table: &'a TypeTable,
    applied: Cell<bool>,
}

impl Rule for LicmRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        if self.applied.replace(true) {
            return false;
        }
        let root = engine.body.root;
        let mut outer_aliases: Vec<(u32, u32)> = Vec::new();
        licm_block(engine, root, self.type_table, &mut outer_aliases)
    }
}

/// Apply LICM to all loops in a block.
///
/// `outer_aliases` accumulates `let x = y` (and `&y` / `&mut y` / labeled-
/// or plain-block tail equivalents) pairs from let-statements that
/// precede each loop. The fixpoint loop in `licm_loop` consumes these so
/// that a write to one alias inside the loop body invalidates hoist
/// candidates targeting the other alias.
fn licm_block(
    engine: &mut Engine,
    block: BlockId,
    type_table: &TypeTable,
    outer_aliases: &mut Vec<(u32, u32)>,
) -> bool {
    let mut changed = false;
    let mut new_stmts = Vec::new();

    // Iterate a clone, not `mem::take`: `hoist_invariant_arith` rebuilds
    // the value graph from the body root mid-walk, so ancestor blocks must
    // stay populated.
    for s in engine.body.blocks[block].stmts.clone() {
        // Classify without holding the borrow across the mutable recursion.
        enum Shape {
            Loop(BlockId),
            If(BlockId, Option<BlockId>),
            Labeled(BlockId),
            Let(u32, Operand),
            Other,
        }
        let shape = match &engine.body.stmts[s].kind {
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
                let hoist_stmts = licm_loop(engine, lb, type_table, &empty_set, outer_aliases);
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
                changed |= licm_block(engine, then_b, type_table, outer_aliases);
                if let Some(eb) = else_b {
                    changed |= licm_block(engine, eb, type_table, outer_aliases);
                }
                new_stmts.push(s);
            }
            Shape::Labeled(inner) => {
                changed |= licm_block(engine, inner, type_table, outer_aliases);
                new_stmts.push(s);
            }
            Shape::Let(local_index, value) => {
                // Track outer-scope aliases so a subsequent loop's LICM can see them.
                if let Some(ve) = value.as_expr()
                    && let Some(src_idx) = extract_alias_source(engine.body, ve)
                    && is_gc_heap_type(engine.body.exprs[ve].type_id, type_table)
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

    engine.set_block_stmts(block, new_stmts);
    changed
}

/// Apply LICM to a single loop, returning hoisting statement ids to prepend.
fn licm_loop(
    engine: &mut Engine,
    loop_body: BlockId,
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
        collect_modified_vars_in_block(engine.body, loop_body, &mut modified_vars, type_table);

        // Step 2: Collect immutable reference bindings for look-through.
        let ref_bindings = collect_immutable_ref_bindings(engine.body, loop_body, type_table);

        // Step 3: Find field accesses that can be hoisted. `next_local` is a
        // local placeholder counter that `find_hoist_candidates_in_block`
        // increments to dedup candidates by (local, field); the actual local
        // indices are assigned at allocation time in step 4.
        let mut candidates = Vec::new();
        let mut seen = IndexSet::default();
        let mut next_local = engine.locals().len() as u32;
        find_hoist_candidates_in_block(
            engine.body,
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
            let locals = engine.locals();
            let root_ty = if (c.local_index as usize) < locals.len() {
                locals[c.local_index as usize].type_id
            } else {
                c.type_id
            };
            !modified_vars.is_reference_field_aliasing_written(root_ty, c.field_index, type_table)
        });

        if candidates.is_empty() {
            // Field-hoisting has converged for this loop. Try hoisting maximal
            // pre-header-stable pure-arithmetic subexpressions (e.g. the
            // `_licm_end - _licm_start` a scan loop recomputes in its guard
            // every iteration). Runs here, after field-hoisting, so the
            // `_licm_*` locals it created are visible as stable operands.
            if hoist_invariant_arith(engine, loop_body, &modified_vars, &mut all_hoist_stmts) {
                continue;
            }
            break;
        }

        // Step 4: Create hoisting statements. Each candidate gets its actual
        // `new_local_index` from `engine.alloc_local` (which also pushes the
        // `NirLocal` entry), so the surviving hoist locals are contiguous
        // from the function's current local count.
        for candidate in &mut candidates {
            let local_type_id = {
                let locals = engine.locals();
                if (candidate.local_index as usize) < locals.len() {
                    locals[candidate.local_index as usize].type_id
                } else {
                    candidate.type_id
                }
            };

            let hoist_name = format!("_licm_{}_{}", candidate.field_name, engine.locals().len());
            let new_local_index = engine.alloc_local(
                hoist_name.clone(),
                candidate.type_id,
                /* is_mut */ false,
            );
            candidate.new_local_index = new_local_index;

            // Build `local.field` as fresh arena nodes via the engine.
            let local_expr = engine.alloc_expr(
                ExprKind::Local {
                    index: candidate.local_index,
                    name: candidate.local_name.clone(),
                },
                local_type_id,
                Span::new(0, 0, 0, 0),
            );
            let field_access_expr = engine.alloc_expr(
                ExprKind::FieldAccess {
                    expr: local_expr.into(),
                    field_index: candidate.field_index,
                    field_name: candidate.field_name.clone(),
                },
                candidate.type_id,
                Span::new(0, 0, 0, 0),
            );
            let hoist_stmt = engine.alloc_stmt(
                StmtKind::Let {
                    name: hoist_name,
                    local_index: new_local_index,
                    is_mut: false,
                    is_reactive: false,
                    type_id: candidate.type_id,
                    value: field_access_expr.into(),
                    skip_value_copy: true,
                },
                Span::new(0, 0, 0, 0),
            );
            all_hoist_stmts.push(hoist_stmt);
        }

        // Step 5: Replace field accesses in the loop body with the hoisted locals.
        replace_hoisted_in_block(engine, loop_body, &candidates, &ref_bindings);
    }

    // Nested loops: recurse. The nested `licm_block` accumulates aliases from
    // the outer loop's `let` statements on its own walk.
    let mut nested_aliases: Vec<(u32, u32)> = outer_aliases.to_vec();
    licm_block(engine, loop_body, type_table, &mut nested_aliases);

    all_hoist_stmts
}

// ---------------------------------------------------------------------------
// Shared child enumeration (expression + block children, patterns excluded)
// ---------------------------------------------------------------------------

enum Child {
    Expr(ExprId),
    Block(BlockId),
}

fn op_child(op: Operand) -> Option<Child> {
    op.as_expr().map(Child::Expr)
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
        | ExprKind::VariantPayload { expr: inner, .. } => {
            inner.as_expr().map(Child::Expr).into_iter().collect()
        }
        ExprKind::Binary { left, right, .. }
        | ExprKind::Index {
            expr: left,
            index: right,
        } => [*left, *right].into_iter().filter_map(op_child).collect(),
        ExprKind::Assign { target, value } => [Some(Child::Expr(*target)), op_child(*value)]
            .into_iter()
            .flatten()
            .collect(),
        ExprKind::Call { args, .. } => args.iter().filter_map(|a| op_child(a.expr)).collect(),
        ExprKind::MethodCall { receiver, args, .. } => op_child(*receiver)
            .into_iter()
            .chain(args.iter().filter_map(|a| op_child(a.expr)))
            .collect(),
        ExprKind::CmRawCall { args, .. } => args.iter().filter_map(|a| op_child(*a)).collect(),
        ExprKind::IndirectCall { callee, args } => op_child(*callee)
            .into_iter()
            .chain(args.iter().filter_map(|a| op_child(*a)))
            .collect(),
        ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => vec![Child::Block(*b)],
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let mut v: Vec<Child> = op_child(*condition)
                .into_iter()
                .chain(std::iter::once(Child::Block(*then_branch)))
                .collect();
            if let Some(eb) = else_branch {
                v.push(Child::Block(*eb));
            }
            v
        }
        ExprKind::StructLiteral { fields, .. } => {
            fields.iter().filter_map(|f| op_child(f.value)).collect()
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            elements.iter().filter_map(|e| op_child(*e)).collect()
        }
        ExprKind::VariantConstruct { payload, .. } => {
            payload.iter().filter_map(|p| op_child(*p)).collect()
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => op_child(*scrutinee)
            .into_iter()
            .chain(arms.iter().map(|a| Child::Block(*a)))
            .chain(std::iter::once(Child::Block(*default)))
            .collect(),
        ExprKind::Match { expr, arms } => {
            let mut v: Vec<Child> = op_child(*expr).into_iter().collect();
            for arm in arms {
                if let Some(c) = op_child(arm.body) {
                    v.push(c);
                }
                if let Some(g) = arm.guard.and_then(op_child) {
                    v.push(g);
                }
            }
            v
        }
        // Leaves.
        ExprKind::BytesLiteral(_)
        | ExprKind::Dead
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
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            value.as_expr().map(Child::Expr).into_iter().collect()
        }
        StmtKind::Expr(value) => value.as_expr().map(Child::Expr).into_iter().collect(),
        StmtKind::Return { value } | StmtKind::Break { value, .. } => value
            .iter()
            .filter_map(|v| v.as_expr().map(Child::Expr))
            .collect(),
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            // A promoted (`Operand::Value`) condition has no skeleton child to
            // traverse; only an `Expr` condition contributes a child node.
            let mut v: Vec<Child> = condition
                .as_expr()
                .map(Child::Expr)
                .into_iter()
                .chain(std::iter::once(Child::Block(*then_block)))
                .collect();
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

fn mark_gc_local_as_fully_modified_operand(
    body: &Body,
    op: Operand,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    if let Some(e) = op.as_expr() {
        mark_gc_local_as_fully_modified(body, e, modified, type_table);
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
        } => inner
            .as_expr()
            .and_then(|ie| extract_alias_source(body, ie)),
        ExprKind::Block(block) => {
            let tail = *body.blocks[*block].stmts.last()?;
            let StmtKind::Expr(Operand::Expr(tail_expr)) = &body.stmts[tail].kind else {
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
            brk_value
                .as_expr()
                .and_then(|e| extract_alias_source(body, e))
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

fn mark_local_as_fully_modified_operand(body: &Body, op: Operand, modified: &mut ModifiedVars) {
    if let Some(e) = op.as_expr() {
        mark_local_as_fully_modified(body, e, modified);
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
            mark_local_as_fully_modified_operand(body, *inner, modified);
        }
        _ => {}
    }
}

/// A chain of field accesses bottoming out at a `Local` (`a`, `a.b`, `a.b.c`),
/// with no `Index`, deref, or call.
fn is_pure_field_chain(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { .. } => true,
        // A promoted `Operand::Value` receiver is not a pure local-read chain.
        ExprKind::FieldAccess { expr: inner, .. } => inner
            .as_expr()
            .is_some_and(|e| is_pure_field_chain(body, e)),
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

fn record_mut_ref_clobber_operand(
    body: &Body,
    op: Operand,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    if let Some(e) = op.as_expr() {
        record_mut_ref_clobber(body, e, modified, type_table);
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
        // A write place's receiver is never a promoted `Operand::Value`.
        && let Some(inner_e) = inner.as_expr()
    {
        let pointee = strip_references(body.exprs[inner_e].type_id, type_table);
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
            if let Some(inner_e) = inner.as_expr()
                && let ExprKind::Local { index, .. } = &body.exprs[inner_e].kind
            {
                modified.insert_field(*index, field_index);
            } else if inner
                .as_expr()
                .is_some_and(|ie| is_pure_field_chain(body, ie))
            {
                // `a.b.c = x` mutates `*a.b`, not a field of the root `a`.
            } else {
                // A promoted-value receiver (or other shape) falls back to the
                // conservative whole-local invalidation.
                mark_local_as_fully_modified_operand(body, inner, modified);
            }
        }
        ExprKind::Unary { expr: inner, .. } => {
            mark_local_as_fully_modified_operand(body, *inner, modified);
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
            if let Some(ve) = value.as_expr()
                && let Some(src_idx) = extract_alias_source(body, ve)
                && is_gc_heap_type(body.exprs[ve].type_id, type_table)
            {
                modified.add_alias(local_index, src_idx);
            }
            collect_modified_vars_in_operand(body, value, modified, type_table);
        }
        StmtKind::Expr(expr) => {
            collect_modified_vars_in_operand(body, *expr, modified, type_table);
        }
        StmtKind::Return { value } => {
            if let Some(v) = value {
                collect_modified_vars_in_operand(body, *v, modified, type_table);
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
            collect_modified_vars_in_operand(body, condition, modified, type_table);
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
                collect_modified_vars_in_operand(body, *v, modified, type_table);
            }
        }
        StmtKind::Continue => {}
        StmtKind::LetDestructure { pattern, value, .. } => {
            let pattern = *pattern;
            let value = *value;
            collect_pattern_bindings(body, pattern, modified);
            collect_modified_vars_in_operand(body, value, modified, type_table);
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

fn collect_modified_vars_in_operand(
    body: &Body,
    op: Operand,
    modified: &mut ModifiedVars,
    type_table: &TypeTable,
) {
    if let Some(e) = op.as_expr() {
        collect_modified_vars_in_expr(body, e, modified, type_table);
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
            collect_modified_vars_in_operand(body, value, modified, type_table);
        }
        ExprKind::Binary { left, right, .. } => {
            let left = *left;
            let right = *right;
            collect_modified_vars_in_operand(body, left, modified, type_table);
            collect_modified_vars_in_operand(body, right, modified, type_table);
        }
        ExprKind::Unary { op, expr: inner } => {
            let inner = *inner;
            if let Some(ie) = inner.as_expr()
                && matches!(op, NirUnaryOp::MutRef)
                && matches!(body.exprs[ie].kind, ExprKind::Local { .. })
            {
                mark_local_as_fully_modified(body, ie, modified);
            }
            collect_modified_vars_in_operand(body, inner, modified, type_table);
        }
        ExprKind::Cast { expr: inner, .. } => {
            collect_modified_vars_in_operand(body, *inner, modified, type_table);
        }
        ExprKind::Call { args, .. } => {
            let arg_ids: Vec<ExprId> = args.iter().filter_map(|a| a.expr.as_expr()).collect();
            for a in arg_ids {
                mark_gc_local_as_fully_modified(body, a, modified, type_table);
                record_mut_ref_clobber(body, a, modified, type_table);
                collect_modified_vars_in_expr(body, a, modified, type_table);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            let receiver = *receiver;
            let arg_ids: Vec<ExprId> = args.iter().filter_map(|a| a.expr.as_expr()).collect();
            mark_gc_local_as_fully_modified_operand(body, receiver, modified, type_table);
            record_mut_ref_clobber_operand(body, receiver, modified, type_table);
            collect_modified_vars_in_operand(body, receiver, modified, type_table);
            for a in arg_ids {
                mark_gc_local_as_fully_modified(body, a, modified, type_table);
                record_mut_ref_clobber(body, a, modified, type_table);
                collect_modified_vars_in_expr(body, a, modified, type_table);
            }
        }
        ExprKind::CmRawCall { args, .. } => {
            let arg_ids = args.clone();
            for a in arg_ids {
                collect_modified_vars_in_operand(body, a, modified, type_table);
            }
        }
        ExprKind::FieldAccess { expr: inner, .. } => {
            collect_modified_vars_in_operand(body, *inner, modified, type_table);
        }
        ExprKind::Index { expr: inner, index } => {
            let inner = *inner;
            let index = *index;
            collect_modified_vars_in_operand(body, inner, modified, type_table);
            collect_modified_vars_in_operand(body, index, modified, type_table);
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
            collect_modified_vars_in_operand(body, condition, modified, type_table);
            collect_modified_vars_in_block(body, then_branch, modified, type_table);
            if let Some(eb) = else_branch {
                collect_modified_vars_in_block(body, eb, modified, type_table);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            let vals: Vec<ExprId> = fields.iter().filter_map(|f| f.value.as_expr()).collect();
            for v in vals {
                collect_modified_vars_in_expr(body, v, modified, type_table);
            }
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            let elements = elements.clone();
            for el in elements {
                collect_modified_vars_in_operand(body, el, modified, type_table);
            }
        }
        ExprKind::IndirectCall { callee, args } => {
            let callee = *callee;
            let arg_ids = args.clone();
            collect_modified_vars_in_operand(body, callee, modified, type_table);
            for a in arg_ids {
                mark_gc_local_as_fully_modified_operand(body, a, modified, type_table);
                collect_modified_vars_in_operand(body, a, modified, type_table);
            }
        }
        ExprKind::ClosureToCanonical { functor, .. } => {
            collect_modified_vars_in_operand(body, *functor, modified, type_table);
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                collect_modified_vars_in_operand(body, *p, modified, type_table);
            }
        }
        ExprKind::LabeledBlock { block, .. } => {
            collect_modified_vars_in_block(body, *block, modified, type_table);
        }
        ExprKind::GlobalVarSet { value, .. } => {
            collect_modified_vars_in_operand(body, *value, modified, type_table);
        }
        ExprKind::VariantTag { expr } | ExprKind::VariantTest { expr, .. } => {
            collect_modified_vars_in_operand(body, *expr, modified, type_table);
        }
        ExprKind::VariantPayload { expr, .. } => {
            collect_modified_vars_in_operand(body, *expr, modified, type_table);
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
            collect_modified_vars_in_operand(body, scrutinee, modified, type_table);
            for arm in arms {
                collect_modified_vars_in_block(body, arm, modified, type_table);
            }
            collect_modified_vars_in_block(body, default, modified, type_table);
        }
        ExprKind::BytesLiteral(_)
        | ExprKind::Dead
        | ExprKind::Local { .. }
        | ExprKind::GlobalVarGet { .. }
        | ExprKind::EnumConstruct { .. } => {}
        ExprKind::Match { expr, arms } => {
            let expr = *expr;
            let arm_data: Vec<(crate::nir_arena::PatId, Option<ExprId>, Option<ExprId>)> = arms
                .iter()
                .map(|a| {
                    (
                        a.pattern,
                        a.guard.and_then(Operand::as_expr),
                        a.body.as_expr(),
                    )
                })
                .collect();
            collect_modified_vars_in_operand(body, expr, modified, type_table);
            for (pattern, guard, body_expr) in arm_data {
                collect_pattern_bindings(body, pattern, modified);
                if let Some(g) = guard {
                    collect_modified_vars_in_expr(body, g, modified, type_table);
                }
                if let Some(be) = body_expr {
                    collect_modified_vars_in_expr(body, be, modified, type_table);
                }
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
            && let Some(ve) = value.as_expr()
            && let ExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr: source,
            } = &body.exprs[ve].kind
            && let Some(se) = source.as_expr()
            && let ExprKind::Local {
                index: source_idx,
                name: source_name,
            } = &body.exprs[se].kind
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
        && let Some(inner_e) = inner.as_expr()
        && let ExprKind::Local { index, name } = &body.exprs[inner_e].kind
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

/// Whether `e`'s shape fits the hoistable-arithmetic grammar: a tree of pure,
/// total ops over `Local` leaves. A promoted (`Operand::Value`) leaf has no
/// skeleton expr and is treated as hoistable.
///
/// `Cast` is deliberately excluded: a float→int cast lowers to the trapping
/// `i32.trunc_f64_s` family (not `trunc_sat`), so hoisting one to the
/// pre-header could trap on a NaN/out-of-range value where a zero-iteration
/// loop never would — the same trap-soundness reason `Div`/`Mod` are excluded.
fn is_hoistable_arith_shape(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { .. } => true,
        ExprKind::Binary { left, op, right } => {
            is_hoistable_binop(*op)
                && left
                    .as_expr()
                    .is_none_or(|e| is_hoistable_arith_shape(body, e))
                && right
                    .as_expr()
                    .is_none_or(|e| is_hoistable_arith_shape(body, e))
        }
        ExprKind::Unary { op, expr } => {
            matches!(op, NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot)
                && expr
                    .as_expr()
                    .is_none_or(|e| is_hoistable_arith_shape(body, e))
        }
        _ => false,
    }
}

/// Collect every `Local` leaf of a hoistable-arithmetic tree.
fn collect_arith_local_leaves(body: &Body, e: ExprId, out: &mut Vec<(ExprId, u32)>) {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => out.push((e, *index)),
        ExprKind::Binary { left, right, .. } => {
            if let Some(le) = left.as_expr() {
                collect_arith_local_leaves(body, le, out);
            }
            if let Some(re) = right.as_expr() {
                collect_arith_local_leaves(body, re, out);
            }
        }
        ExprKind::Unary { expr, .. } => {
            if let Some(ie) = expr.as_expr() {
                collect_arith_local_leaves(body, ie, out);
            }
        }
        _ => {}
    }
}

/// A structural identity key for a hoistable-arith tree: kind / op / leaf-local
/// / promoted-operand-value, with commutative operands sorted so `a + b` and
/// `b + a` agree (matching the `ValueGraph` hash-cons). Once a tree's `Local`
/// leaves are all loop-invariant, two trees with the same key denote the same
/// value — so this replaces the `value_of` `ValueId` for the hoist dedup.
fn arith_structural_key(body: &Body, e: ExprId) -> String {
    let mut s = String::new();
    push_arith_key(body, e, &mut s);
    s
}

fn push_arith_key(body: &Body, e: ExprId, out: &mut String) {
    match &body.exprs[e].kind {
        ExprKind::Binary { left, op, right } => {
            let mut l = String::new();
            push_operand_key(body, *left, &mut l);
            let mut r = String::new();
            push_operand_key(body, *right, &mut r);
            // Commutative ops: order-independent so `a+b` ≡ `b+a`.
            if matches!(
                op,
                NirBinaryOp::Add
                    | NirBinaryOp::Mul
                    | NirBinaryOp::BitAnd
                    | NirBinaryOp::BitOr
                    | NirBinaryOp::BitXor
            ) && r < l
            {
                std::mem::swap(&mut l, &mut r);
            }
            out.push_str(&format!("B{op:?}({l},{r})"));
        }
        ExprKind::Unary { op, expr } => {
            let mut inner = String::new();
            push_operand_key(body, *expr, &mut inner);
            out.push_str(&format!("U{op:?}({inner})"));
        }
        ExprKind::Local { index, .. } => out.push_str(&format!("L{index}")),
        // Any other shape is not part of a hoistable-arith tree; key it by id so
        // it never spuriously dedups with another node.
        _ => out.push_str(&format!("E{}", e.index())),
    }
}

fn push_operand_key(body: &Body, op: Operand, out: &mut String) {
    match op {
        Operand::Expr(e) => push_arith_key(body, e, out),
        // A promoted operand is a frozen value: equal ids denote equal values.
        Operand::Value(v) => out.push_str(&format!("V{}", v.index())),
    }
}

/// Inputs shared by the arithmetic-hoist candidate walk.
struct ArithHoist<'a> {
    /// Locals bound by hoist `let`s whose statements are not in the tree
    /// yet (the caller prepends them after `licm_loop` returns): no entry
    /// value, but read-only pre-header temps are stable by construction.
    pending_hoist_locals: &'a IndexSet<u32>,
    /// Address-taken locals — writes through references are not modelled, so
    /// their use-site values cannot be trusted as loop-invariant.
    address_taken: &'a IndexSet<u32>,
    /// Loop-modified locals — a leaf is invariant iff none of its aliases are
    /// here (replaces the `value_of` `use == entry` invariance check).
    modified: &'a ModifiedVars,
}

impl ArithHoist<'_> {
    /// Whether `e` is a compound arithmetic expression that may move to
    /// the pre-header, returning its `ValueId` for dedup. Each `Local`
    /// leaf's use-site value must equal the pre-header entry value, so the
    /// hoisted clone computes what every occurrence reads — cross-iteration
    /// invariance alone would wrongly admit `loop { x = 5; … x + n … }`.
    fn candidate(&self, engine: &mut Engine, e: ExprId) -> Option<String> {
        let compound = matches!(
            &engine.body.exprs[e].kind,
            ExprKind::Binary { .. } | ExprKind::Unary { .. }
        );
        if !compound || !is_hoistable_arith_shape(engine.body, e) {
            return None;
        }
        let mut leaves: Vec<(ExprId, u32)> = Vec::new();
        collect_arith_local_leaves(engine.body, e, &mut leaves);
        // A constant-only tree is left for constant folding.
        if leaves.is_empty() {
            return None;
        }
        for (_leaf, idx) in leaves {
            if self.pending_hoist_locals.contains(&idx) {
                continue;
            }
            if self.address_taken.contains(&idx) {
                return None;
            }
            // The leaf must be loop-invariant. Read it from `modified_vars`
            // (value-graph-free) instead of `value(leaf) == loop_entry_value`.
            if self.modified.local_modified(idx) {
                return None;
            }
        }
        // With every `Local` leaf invariant, the structural key is exact
        // value-identity for the dedup (replaces `engine.value(e)`).
        Some(arith_structural_key(engine.body, e))
    }

    /// Collect the maximal hoistable arithmetic subexpressions in `block`,
    /// paired with their structural keys. "Maximal" means a hoistable expression
    /// whose parent is not itself hoistable, so each whole tree is hoisted
    /// once. Nested loops are skipped — the recursive `licm_loop` call
    /// hoists each nested loop's own invariants into that loop's pre-header.
    fn collect_in_block(
        &self,
        engine: &mut Engine,
        block: BlockId,
        out: &mut Vec<(ExprId, String)>,
    ) {
        for s in engine.body.blocks[block].stmts.clone() {
            self.collect_in_stmt(engine, s, out);
        }
    }

    fn collect_in_stmt(&self, engine: &mut Engine, s: StmtId, out: &mut Vec<(ExprId, String)>) {
        if matches!(engine.body.stmts[s].kind, StmtKind::Loop { .. }) {
            return;
        }
        for child in stmt_child_nodes(engine.body, s) {
            match child {
                Child::Expr(e) => self.collect_in_expr(engine, e, out),
                Child::Block(b) => self.collect_in_block(engine, b, out),
            }
        }
    }

    fn collect_in_expr(&self, engine: &mut Engine, e: ExprId, out: &mut Vec<(ExprId, String)>) {
        if let Some(key) = self.candidate(engine, e) {
            out.push((e, key));
            return; // maximal: do not recurse into a hoisted tree's children.
        }
        for child in expr_child_nodes(engine.body, e) {
            match child {
                Child::Expr(c) => self.collect_in_expr(engine, c, out),
                Child::Block(b) => self.collect_in_block(engine, b, out),
            }
        }
    }
}

/// Hoist maximal pre-header-stable pure-arithmetic subexpressions out of
/// `loop_body`, one temp per distinct `ValueId` (so copies share: `let t =
/// x; … t + y … x + y …`). The `let`s are appended to `all_hoist_stmts`,
/// which the caller prepends before the loop.
fn hoist_invariant_arith(
    engine: &mut Engine,
    loop_body: BlockId,
    modified: &ModifiedVars,
    all_hoist_stmts: &mut Vec<StmtId>,
) -> bool {
    // Earlier hoist rounds may have changed which locals are address-taken;
    // refresh that scan. The value graph is not rebuilt (build-once invariant):
    // an arith hoist appends a pre-header `let t = <invariant>` and never
    // reassigns an existing local, so every existing local's loop-entry value
    // stays valid; the new `t` simply has no entry and is not a candidate.
    engine.invalidate_address_taken();

    let mut pending_hoist_locals: IndexSet<u32> = IndexSet::default();
    for &s in all_hoist_stmts.iter() {
        if let StmtKind::Let { local_index, .. } = &engine.body.stmts[s].kind {
            pending_hoist_locals.insert(*local_index);
        }
    }
    let address_taken: IndexSet<u32> = engine.body_address_taken().clone();

    let walk = ArithHoist {
        pending_hoist_locals: &pending_hoist_locals,
        address_taken: &address_taken,
        modified,
    };
    let mut found: Vec<(ExprId, String)> = Vec::new();
    walk.collect_in_block(engine, loop_body, &mut found);
    if found.is_empty() {
        // No skeleton arith trees, but operand promotion may have left the
        // invariant as a bare `Operand::Value` slot (no skeleton expr) — hoist
        // those.
        let mut c = hoist_invariant_value_operands(engine, loop_body, all_hoist_stmts);
        c |= cse_loop_body(engine, loop_body, modified);
        return c;
    }

    // Group occurrences by (structural key, type): structurally-equal invariant
    // trees of equal type share one temp. The type key is belt-and-braces —
    // same-key trees over a shared `Local` leaf already agree on types.
    let mut groups: Vec<(String, TypeId, Vec<ExprId>)> = Vec::new();
    'next: for (e, key) in found {
        let ty = engine.body.exprs[e].type_id;
        for g in &mut groups {
            if g.0 == key && g.1 == ty {
                g.2.push(e);
                continue 'next;
            }
        }
        groups.push((key, ty, vec![e]));
    }

    for (_, type_id, occ) in groups {
        let rep = occ[0];
        let name = format!("_licm_arith_{}", engine.locals().len());
        let new_idx = engine.alloc_local(name.clone(), type_id, /* is_mut */ false);

        // Clone the representative into the pre-header `let` *before* rewriting
        // the in-loop occurrences (which include `rep` itself) to a `Local`.
        let value = engine.clone_expr(rep);
        let let_stmt = engine.alloc_stmt(
            StmtKind::Let {
                name: name.clone(),
                local_index: new_idx,
                is_mut: false,
                is_reactive: false,
                type_id,
                value: value.into(),
                skip_value_copy: true,
            },
            Span::new(0, 0, 0, 0),
        );
        all_hoist_stmts.push(let_stmt);

        for o in occ {
            engine.replace_expr_kind(
                o,
                ExprKind::Local {
                    index: new_idx,
                    name: name.clone(),
                },
            );
        }
    }

    hoist_invariant_value_operands(engine, loop_body, all_hoist_stmts);
    cse_loop_body(engine, loop_body, modified);
    true
}

/// Whether `idx` is in scope at the CSE insertion point (before `min_i`): it is
/// a loop-entry local, or bound by a top-level `let` of the loop body earlier.
fn cse_local_available(
    engine: &mut Engine,
    idx: u32,
    min_i: usize,
    toplevel_lets: &[(usize, u32)],
    loop_body: BlockId,
) -> bool {
    toplevel_lets.iter().any(|(i, l)| *i < min_i && *l == idx)
        || engine.loop_entry_value(loop_body, idx).is_some()
}

/// Whether cloning skeleton `e` at the insertion point is sound: every `Local`
/// leaf it reads is in scope there (see [`cse_local_available`]).
fn cse_clone_in_scope(
    engine: &mut Engine,
    e: ExprId,
    min_i: usize,
    toplevel_lets: &[(usize, u32)],
    loop_body: BlockId,
) -> bool {
    enum K {
        Local(u32),
        Bin(Operand, Operand),
        Un(Operand),
        Lit,
        No,
    }
    let k = match &engine.body.exprs[e].kind {
        ExprKind::Local { index, .. } => K::Local(*index),
        ExprKind::Binary { left, right, .. } => K::Bin(*left, *right),
        ExprKind::Unary { expr, .. } | ExprKind::Cast { expr, .. } => K::Un(*expr),
        ExprKind::BytesLiteral(_) => K::Lit,
        _ => K::No,
    };
    match k {
        K::Local(idx) => cse_local_available(engine, idx, min_i, toplevel_lets, loop_body),
        K::Bin(l, r) => {
            cse_operand_in_scope(engine, l, min_i, toplevel_lets, loop_body)
                && cse_operand_in_scope(engine, r, min_i, toplevel_lets, loop_body)
        }
        K::Un(o) => cse_operand_in_scope(engine, o, min_i, toplevel_lets, loop_body),
        K::Lit => true,
        K::No => false,
    }
}

fn cse_operand_in_scope(
    engine: &mut Engine,
    op: Operand,
    min_i: usize,
    toplevel_lets: &[(usize, u32)],
    loop_body: BlockId,
) -> bool {
    match op {
        Operand::Expr(e) => cse_clone_in_scope(engine, e, min_i, toplevel_lets, loop_body),
        Operand::Value(v) => {
            let rep = engine.body.values.find_imm(v);
            let mut leaves: IndexSet<u32> = IndexSet::default();
            engine.body.values.collect_opaque_locals(rep, &mut leaves);
            leaves
                .iter()
                .all(|&idx| cse_local_available(engine, idx, min_i, toplevel_lets, loop_body))
        }
    }
}

/// Common-subexpression elimination inside a loop body under operand promotion.
///
/// The value graph hash-conses a pure subexpression (`p * p` over a loop-carried
/// `p`) to one `ValueId`, so the two occurrences in a guard and the body share
/// an identity — but each is a distinct *skeleton* `Binary` expr the extractor
/// can not promote to a bare `Operand::Value` (a loop-carried local's value is
/// not reemittable at an arbitrary slot, so it stays a sourceless `Opaque`).
/// Each is therefore re-emitted. This restores the one-computation `__cse_N`
/// shape the standalone `cse` pass produced before hash-consing subsumed the
/// *deduplication* (but not the materialisation): bind a clone of the
/// subexpression to a temp placed before the earliest top-level statement that
/// contains an occurrence, and redirect every occurrence to read the temp.
///
/// Soundness — placement and availability:
/// - The temp lands before the earliest top-level statement of the loop body
///   that holds an occurrence, so it dominates every (later or equal) occurrence
///   in the body's linear statement list.
/// - A value with ≥2 occurrences sharing one `ValueId` reads the *same* leaf
///   values at each, so those leaves are in scope at all of them — hence bound
///   before the earliest occurrence's statement (or loop-carried / a param),
///   available where the temp is inserted. The clone re-emits the original
///   skeleton (a `local.get` of each leaf), so it computes exactly the shared
///   value. Trap-prone ops are excluded, so computing it once up front (possibly
///   on an iteration a conditional occurrence would have skipped) cannot trap.
fn cse_loop_body(engine: &mut Engine, loop_body: BlockId, modified: &ModifiedVars) -> bool {
    let stmts = engine.body.blocks[loop_body].stmts.clone();
    // Occurrences of each materialisable arith value, keyed by a value-graph-free
    // **structural key**, as (top-level stmt index, expr) in first-seen order.
    // Two structurally-equal trees denote the same value exactly when their
    // leaves hold the same values at both points; the per-run split below (no
    // leaf assigned across the span) establishes that without `value_of`,
    // replacing the value graph's per-point flow-sensitivity. Nested loops are
    // not descended.
    let mut occ: IndexMap<String, Vec<(usize, ExprId)>> = IndexMap::default();
    for (i, &s) in stmts.iter().enumerate() {
        let mut exprs = Vec::new();
        collect_stmt_exprs(engine.body, s, &mut exprs);
        for e in exprs {
            if !is_cse_candidate_expr(engine.body, e) {
                continue;
            }
            let mut leaves: Vec<(ExprId, u32)> = Vec::new();
            collect_arith_local_leaves(engine.body, e, &mut leaves);
            // A constant-only tree is left to const folding.
            if leaves.is_empty() {
                continue;
            }
            let key = arith_structural_key(engine.body, e);
            occ.entry(key).or_default().push((i, e));
        }
    }

    // Locals bound by a top-level `let` of the loop body, with their statement
    // index — the in-scope set at the insertion point grows as these precede it.
    let toplevel_lets: Vec<(usize, u32)> = stmts
        .iter()
        .enumerate()
        .filter_map(|(i, &s)| match engine.body.stmts[s].kind {
            StmtKind::Let { local_index, .. } => Some((i, local_index)),
            _ => None,
        })
        .collect();
    let address_taken: IndexSet<u32> = engine.body_address_taken().clone();

    // (stmt index, let) inserts and per-expr redirects, computed before any
    // mutation so indices stay stable.
    let mut inserts: Vec<(usize, StmtId)> = Vec::new();
    for (_key, occs) in occ {
        if occs.len() < 2 {
            continue;
        }
        // Leaves of this group (same key ⇒ same structure ⇒ same leaf locals).
        let mut leaves: Vec<(ExprId, u32)> = Vec::new();
        collect_arith_local_leaves(engine.body, occs[0].1, &mut leaves);
        let leaf_ids: Vec<u32> = leaves.iter().map(|(_, idx)| *idx).collect();
        // Address-taken leaves can be mutated through an alias at an unknown
        // point, which per-statement assignment tracking does not see — so an
        // address-taken leaf must be loop-**invariant** (never modified) for the
        // value to be stable. If any is modified, skip the whole group.
        if leaf_ids
            .iter()
            .any(|idx| address_taken.contains(idx) && modified.local_modified(*idx))
        {
            continue;
        }
        // Split the occurrences (in statement order) into maximal **runs** within
        // which no non-address-taken leaf is directly assigned across the span —
        // each run computes one value, soundly CSE'd into one temp. This replaces
        // the value graph's per-point flow-sensitivity (`p*p` before `p += 1` is
        // one value; after, a new one).
        let mut occs = occs;
        occs.sort_by_key(|(i, _)| *i);
        let mut runs: Vec<Vec<(usize, ExprId)>> = Vec::new();
        for (i, e) in occs {
            let start_new = match runs.last() {
                Some(run) => {
                    let lo = run[0].0;
                    (lo..=i).any(|si| {
                        let s = stmts[si];
                        leaf_ids.iter().any(|idx| {
                            !address_taken.contains(idx)
                                && local_assigned_in_stmt(engine.body, s, *idx)
                        })
                    })
                }
                None => true,
            };
            if start_new {
                runs.push(vec![(i, e)]);
            } else {
                runs.last_mut().unwrap().push((i, e));
            }
        }
        for occs in runs {
            if occs.len() < 2 {
                continue;
            }
            let ty = engine.body.exprs[occs[0].1].type_id;
            let min_i = occs.iter().map(|(i, _)| *i).min().unwrap();
            // Clone an occurrence whose skeleton is in scope at the insertion point
            // (before `min_i`): every `Local` leaf must be a loop-entry local or
            // bound by a top-level `let` before `min_i`. The value graph already
            // proved every occurrence equal, so any in-scope occurrence computes the
            // right value; cloning a bare alias read of an inner-scope local (e.g.
            // `let __cse = a` where `a` is bound inside a nested block) would read a
            // stale loop-carried value. Skip the value if none qualifies.
            let Some(&(_, src_expr)) = occs
                .iter()
                .find(|(_, e)| cse_clone_in_scope(engine, *e, min_i, &toplevel_lets, loop_body))
            else {
                continue;
            };
            let span = engine.body.exprs[src_expr].span;
            let name = format!("__cse_{}", engine.locals().len());
            let temp = engine.alloc_local(name.clone(), ty, /* is_mut */ false);
            // Clone the chosen occurrence's skeleton subtree for the temp's value
            // (the value itself is a sourceless-Opaque tree the extractor can not
            // re-emit; the skeleton can).
            let cloned = engine.clone_expr(src_expr);
            let let_stmt = engine.alloc_stmt(
                StmtKind::Let {
                    name: name.clone(),
                    local_index: temp,
                    is_mut: false,
                    is_reactive: false,
                    type_id: ty,
                    value: Operand::Expr(cloned),
                    skip_value_copy: true,
                },
                span,
            );
            // Redirect each occurrence to a *skeleton* `local.get __cse` (not a
            // promoted value): the temp is reassigned every iteration, so a value
            // operand `Opaque(Local(__cse))` would read as loop-invariant to the
            // arith hoist. `__cse` has no loop-entry value, so a skeleton read is
            // correctly treated as loop-carried.
            let mut any = false;
            for (_, e) in &occs {
                let lread = engine.alloc_expr(
                    ExprKind::Local {
                        index: temp,
                        name: name.clone(),
                    },
                    ty,
                    span,
                );
                any |= engine.redirect_expr(*e, Operand::Expr(lread));
            }
            if any {
                inserts.push((min_i, let_stmt));
            }
        }
    }
    if inserts.is_empty() {
        return false;
    }
    // Splice the temps in, each before its target statement.
    let mut new_stmts: Vec<StmtId> = Vec::new();
    for (i, &s) in stmts.iter().enumerate() {
        for (_, let_stmt) in inserts.iter().filter(|(mi, _)| *mi == i) {
            new_stmts.push(*let_stmt);
        }
        new_stmts.push(s);
    }
    engine.set_block_stmts(loop_body, new_stmts);
    true
}

/// Collect every expression under statement `s`, descending through blocks but
/// **not** into nested `Loop` bodies (keeping occurrences within one loop's
/// dominance scope).
fn collect_stmt_exprs(body: &Body, s: StmtId, out: &mut Vec<ExprId>) {
    if matches!(body.stmts[s].kind, StmtKind::Loop { .. }) {
        return;
    }
    for child in stmt_child_nodes(body, s) {
        match child {
            Child::Expr(e) => collect_expr_exprs(body, e, out),
            Child::Block(b) => {
                for &s2 in &body.blocks[b].stmts {
                    collect_stmt_exprs(body, s2, out);
                }
            }
        }
    }
}

fn collect_expr_exprs(body: &Body, e: ExprId, out: &mut Vec<ExprId>) {
    out.push(e);
    for child in expr_child_nodes(body, e) {
        match child {
            Child::Expr(c) => collect_expr_exprs(body, c, out),
            Child::Block(b) => {
                for &s2 in &body.blocks[b].stmts {
                    collect_stmt_exprs(body, s2, out);
                }
            }
        }
    }
}

/// Whether `v` is a pure arithmetic compound worth CSE-materialising: a
/// `Binary` / `Unary` with a non-trap-prone op. The leaves need no availability
/// check here — see [`cse_loop_body`]'s soundness note (shared scope of ≥2
/// occurrences) — only the root must be a compound, not a bare leaf.
/// Whether top-level statement `s` (its subtree, **not** descending nested
/// `Loop` bodies) directly assigns local `idx`: `idx = …` (an `Assign` rooting
/// at `idx`) or `let idx = …`. For a **non-address-taken** local this is the
/// only path that changes its value (no reference can alias it, and a call
/// cannot reach a non-escaping local) — so a `cse_loop_body` occurrence span
/// free of such assignments reads the same leaf value at every occurrence.
fn local_assigned_in_stmt(body: &Body, s: StmtId, idx: u32) -> bool {
    match &body.stmts[s].kind {
        StmtKind::Let {
            local_index, value, ..
        } => *local_index == idx || operand_assigns_local(body, *value, idx),
        StmtKind::Expr(op)
        | StmtKind::Return { value: Some(op) }
        | StmtKind::Break {
            value: Some(op), ..
        } => operand_assigns_local(body, *op, idx),
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            operand_assigns_local(body, *condition, idx)
                || block_assigns_local(body, *then_block, idx)
                || else_block.is_some_and(|b| block_assigns_local(body, b, idx))
        }
        StmtKind::LabeledBlock { block, .. } => block_assigns_local(body, *block, idx),
        // A nested `Loop` is not descended (its occurrences are out of this
        // body's CSE scope); conservatively any local it writes is already in
        // `modified_vars`, handled by the invariance fallback.
        StmtKind::Loop { .. }
        | StmtKind::Continue
        | StmtKind::Return { value: None }
        | StmtKind::Break { value: None, .. }
        | StmtKind::LetDestructure { .. } => false,
    }
}

fn block_assigns_local(body: &Body, b: BlockId, idx: u32) -> bool {
    body.blocks[b]
        .stmts
        .iter()
        .any(|&s| local_assigned_in_stmt(body, s, idx))
}

fn operand_assigns_local(body: &Body, op: Operand, idx: u32) -> bool {
    let Some(e) = op.as_expr() else { return false };
    expr_assigns_local(body, e, idx)
}

fn expr_assigns_local(body: &Body, e: ExprId, idx: u32) -> bool {
    if let ExprKind::Assign { target, .. } = &body.exprs[e].kind
        && super::arena_query::projection_root_local(body, *target) == Some(idx)
    {
        return true;
    }
    let mut found = false;
    body.for_each_child(NodeRef::Expr(e), |c| {
        if !found && let NodeRef::Expr(ce) = c {
            found = expr_assigns_local(body, ce, idx);
        }
    });
    found
}

/// A hoistable `Binary` / `Unary` arith expression — the CSE candidate shape,
/// checked structurally (value-graph-free).
fn is_cse_candidate_expr(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Binary { op, .. } => is_hoistable_binop(*op),
        ExprKind::Unary { op, .. } => {
            matches!(op, NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot)
        }
        _ => false,
    }
}

/// Hoist loop-invariant promoted *value* operands into a pre-header
/// `let _licm_arith_N`. Operand promotion can leave a loop-invariant compound
/// (e.g. `hi - lo`, born as a value before the loop) as a bare `Operand::Value`
/// slot with no skeleton expr, so [`ArithHoist`] (which scans skeleton trees)
/// never sees it. Materialise each distinct invariant value once in the
/// pre-header and redirect its in-loop slots to a read of the temp.
fn hoist_invariant_value_operands(
    engine: &mut Engine,
    loop_body: BlockId,
    all_hoist_stmts: &mut Vec<StmtId>,
) -> bool {
    use crate::nir_value_graph::OpaqueSource;

    let (expr_ids, stmt_ids) = collect_loop_subtree(engine.body, loop_body);

    // Phase 1: snapshot every operand slot in the subtree, in a fixed order.
    let mut ops: Vec<Operand> = Vec::new();
    for &e in &expr_ids {
        engine.body.map_expr_operands(e, &mut |op| {
            ops.push(op);
            op
        });
    }
    for &s in &stmt_ids {
        engine.body.map_stmt_operands(s, &mut |op| {
            ops.push(op);
            op
        });
    }

    // Locals available at the loop pre-header (where the hoisted `let` lands):
    // exactly those with a loop-entry value. A value whose `Opaque(Local)` leaf
    // names a local *bound inside* the loop (a pattern / while-let / nested
    // binding — the value graph gives it a fresh per-iteration Opaque, never a
    // `LoopPhi`) has no entry value, so hoisting it would compute the wrong
    // thing. Collect the candidate leaves first, then keep only the entry-live
    // ones — soundness gate for the hoist.
    let mut leaf_locals: IndexSet<u32> = IndexSet::default();
    for op in &ops {
        if let Operand::Value(v) = *op {
            let rep = engine.body.values.find_imm(v);
            engine
                .body
                .values
                .collect_opaque_locals(rep, &mut leaf_locals);
        }
    }
    let mut entry_locals: IndexSet<u32> = IndexSet::default();
    for idx in leaf_locals {
        if engine.loop_entry_value(loop_body, idx).is_some() {
            entry_locals.insert(idx);
        }
    }

    // Phase 2: pick the distinct invariant compound value reps, in first-seen
    // order, and materialise a pre-header temp + read value for each.
    let mut rep_read: IndexMap<ValueId, ValueId> = IndexMap::default();
    for op in &ops {
        let Operand::Value(v) = *op else { continue };
        let rep = engine.body.values.find_imm(v);
        if rep_read.contains_key(&rep)
            || !is_hoistable_value(&engine.body.values, rep, &entry_locals)
        {
            continue;
        }
        let Some(ty) = engine.body.values.type_of(rep) else {
            continue;
        };
        let name = format!("_licm_arith_{}", engine.locals().len());
        let temp = engine.alloc_local(name.clone(), ty, /* is_mut */ false);
        let read = engine
            .body
            .values
            .fresh_opaque_with_source(OpaqueSource::Local(temp));
        engine.body.values.set_type(read, ty);
        let let_stmt = engine.alloc_stmt(
            StmtKind::Let {
                name,
                local_index: temp,
                is_mut: false,
                is_reactive: false,
                type_id: ty,
                value: Operand::Value(rep),
                skip_value_copy: true,
            },
            Span::new(0, 0, 0, 0),
        );
        all_hoist_stmts.push(let_stmt);
        rep_read.insert(rep, read);
    }
    if rep_read.is_empty() {
        return false;
    }

    // Phase 3: precompute the new operand for each snapshot slot, then re-apply
    // in the same order (the closure touches no `Body` field, so no borrow
    // conflicts with the map).
    let new_ops: Vec<Operand> = ops
        .iter()
        .map(|op| match *op {
            Operand::Value(v) => match rep_read.get(&engine.body.values.find_imm(v)) {
                Some(&read) => Operand::Value(read),
                None => *op,
            },
            _ => *op,
        })
        .collect();
    let mut i = 0;
    for &e in &expr_ids {
        engine.body.map_expr_operands(e, &mut |_| {
            let r = new_ops[i];
            i += 1;
            r
        });
    }
    for &s in &stmt_ids {
        engine.body.map_stmt_operands(s, &mut |_| {
            let r = new_ops[i];
            i += 1;
            r
        });
    }
    true
}

/// Collect every expression and statement id reachable from `loop_body` (the
/// whole loop subtree, including nested loops — a pre-header temp dominates
/// them, so rewriting their slots stays sound).
fn collect_loop_subtree(body: &Body, loop_body: BlockId) -> (Vec<ExprId>, Vec<StmtId>) {
    let mut expr_ids = Vec::new();
    let mut stmt_ids = Vec::new();
    let mut block_work = vec![loop_body];
    while let Some(b) = block_work.pop() {
        for &s in &body.blocks[b].stmts {
            stmt_ids.push(s);
            for child in stmt_child_nodes(body, s) {
                match child {
                    Child::Block(nb) => block_work.push(nb),
                    Child::Expr(e) => collect_expr_subtree(body, e, &mut expr_ids, &mut block_work),
                }
            }
        }
    }
    (expr_ids, stmt_ids)
}

fn collect_expr_subtree(
    body: &Body,
    e: ExprId,
    expr_ids: &mut Vec<ExprId>,
    block_work: &mut Vec<BlockId>,
) {
    expr_ids.push(e);
    for child in expr_child_nodes(body, e) {
        match child {
            Child::Expr(c) => collect_expr_subtree(body, c, expr_ids, block_work),
            Child::Block(b) => block_work.push(b),
        }
    }
}

/// Whether `v` is a loop-invariant arithmetic compound worth hoisting: a
/// `Binary` / `Unary` root over leaves that are constants or `Opaque(Local)`
/// reads of pre-header-available locals (`entry_locals`), with at least one such
/// local leaf (a constant-only tree is left to const folding). Trap-prone ops
/// (`Div` / `Mod`) and flow-merge / heap kinds (`LoopPhi` / `Select` /
/// `FieldAccess` / `Opaque(Expr)` / `Cast`) are excluded — the same shape
/// `ArithHoist` admits.
fn is_hoistable_value(
    pool: &crate::nir_value_graph::ValuePool,
    v: ValueId,
    entry_locals: &IndexSet<u32>,
) -> bool {
    use crate::nir_value_graph::ValueKind;
    let compound = matches!(
        pool.kind(v),
        ValueKind::Binary { .. } | ValueKind::Unary { .. }
    );
    if !compound || !value_is_invariant(pool, v, entry_locals) {
        return false;
    }
    let mut leaves = IndexSet::default();
    pool.collect_opaque_locals(v, &mut leaves);
    !leaves.is_empty()
}

fn value_is_invariant(
    pool: &crate::nir_value_graph::ValuePool,
    v: ValueId,
    entry_locals: &IndexSet<u32>,
) -> bool {
    use crate::nir_value_graph::{OpaqueSource, ValueKind};
    match pool.kind(v) {
        ValueKind::Int(..)
        | ValueKind::Float(..)
        | ValueKind::Bool(_)
        | ValueKind::Char(_)
        | ValueKind::String(_)
        | ValueKind::Null
        | ValueKind::Unit => true,
        ValueKind::Opaque(oid) => match pool.opaque_source(*oid) {
            Some(OpaqueSource::Local(idx)) => entry_locals.contains(&idx),
            _ => false,
        },
        ValueKind::Binary { op, lhs, rhs, .. } => {
            is_hoistable_binop(*op)
                && value_is_invariant(pool, *lhs, entry_locals)
                && value_is_invariant(pool, *rhs, entry_locals)
        }
        ValueKind::Unary { op, operand, .. } => {
            matches!(op, NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot)
                && value_is_invariant(pool, *operand, entry_locals)
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Replace hoisted field accesses with the hoisted locals
// ---------------------------------------------------------------------------

fn replace_hoisted_in_block(
    engine: &mut Engine,
    block: BlockId,
    candidates: &[HoistCandidate],
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
) {
    for s in engine.body.blocks[block].stmts.clone() {
        replace_hoisted_in_stmt(engine, s, candidates, ref_bindings);
    }
}

fn replace_hoisted_in_stmt(
    engine: &mut Engine,
    s: StmtId,
    candidates: &[HoistCandidate],
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
) {
    for child in stmt_child_nodes(engine.body, s) {
        match child {
            Child::Expr(e) => replace_hoisted_in_expr(engine, e, candidates, ref_bindings),
            Child::Block(b) => replace_hoisted_in_block(engine, b, candidates, ref_bindings),
        }
    }
}

fn replace_hoisted_in_expr(
    engine: &mut Engine,
    e: ExprId,
    candidates: &[HoistCandidate],
    ref_bindings: &IndexMap<u32, LicmRefBinding>,
) {
    // First, check if this expression matches a hoist candidate.
    let matched = if let ExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &engine.body.exprs[e].kind
        && let Some(inner_e) = inner.as_expr()
        && let ExprKind::Local { index, .. } = &engine.body.exprs[inner_e].kind
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
        engine.replace_expr_kind(
            e,
            ExprKind::Local {
                index: new_local_index,
                name: format!("_licm_{field_name}_{new_local_index}"),
            },
        );
        return;
    }

    // Recurse into sub-expressions / sub-blocks.
    for child in expr_child_nodes(engine.body, e) {
        match child {
            Child::Expr(c) => replace_hoisted_in_expr(engine, c, candidates, ref_bindings),
            Child::Block(b) => replace_hoisted_in_block(engine, b, candidates, ref_bindings),
        }
    }
}
