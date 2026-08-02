//! Scalar Replacement of Aggregates (SROA) optimization for Wado NIR
//!
//! Eliminates struct and tuple allocations when the aggregate is only used for
//! field access. After inlining exposes:
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
//! Runs on the worklist rewrite engine
//! (`docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a [`Rule`]: a
//! per-function standalone engine session whose `apply_block` fires once at
//! the function root and performs the whole-function decomposition in one
//! shot. The analysis phases (candidate collection, escape / soft-escape) read
//! `engine.body` directly; the rewrite routes every mutation through the
//! engine edit API (`set_block_stmts`, `replace_expr_kind`, `alloc_stmt`,
//! `alloc_expr`, `alloc_local`) so the parent map and use index stay
//! coherent. Locals discovered to be `&local`-aliased by a decomposed field
//! flow back into `func.stores_aliased_locals` via a `RefCell` the driver
//! merges after `engine.run` returns.

use std::cell::{Cell, RefCell};

use cranelift_entity::EntityRef;

use super::arena_query::strip_one_value_copy;
use super::gate::{FunctionGate, GatedPass};
use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::NirFunction;
use crate::nir_arena::{
    ArenaStructField, BlockId, Body, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::tir::TypeId;
use crate::token::Span;

/// Maps a callee → the set of its parameter indices that have `stores` declared.
type StoresLookup = IndexMap<crate::nir::FuncId, IndexSet<usize>>;

/// Information about a struct/tuple local that may be decomposable.
struct SroaCandidate {
    local_index: u32,
    local_name: String,
    /// The `StructLiteral` / `TupleLiteral` the `Let` binds.
    literal: ExprId,
    /// Per-field info: (`field_name`, `field_type_id`).
    fields: Vec<(String, TypeId)>,
    is_mut: bool,
    aggregate_type_id: TypeId,
    /// The struct name (empty for tuples).
    struct_name: String,
}

fn build_stores_lookup(project: &NirPackage) -> StoresLookup {
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
        if !stored_indices.is_empty()
            && let Some(id) = func.id
        {
            lookup.insert(id, stored_indices);
        }
    }
    lookup
}

pub fn scalar_replace_aggregates(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let stores_lookup = build_stores_lookup(project);
    let value_copy_ids = project.value_copy_func_ids();
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::Sroa, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
        }
        let stores_aliased_snapshot = func.stores_aliased_locals.clone();
        let rule = SroaRule {
            stores_lookup: &stores_lookup,
            value_copy_ids: &value_copy_ids,
            stores_aliased: stores_aliased_snapshot,
            newly_aliased: RefCell::new(IndexSet::default()),
            applied: Cell::new(false),
        };
        let changed = {
            let NirFunction { body, locals, .. } = &mut *func;
            let body = body.as_mut().expect("checked above");
            let mut engine = Engine::new(body, &mut buffers, locals);
            engine.run(&[&rule])
        };
        let newly = rule.newly_aliased.into_inner();
        if !newly.is_empty() {
            func.stores_aliased_locals.extend(newly);
        }
        changed
    })
}

// -----------------------------------------------------------------------
// Rule
// -----------------------------------------------------------------------

/// Standalone-session rule whose single `apply_block` performs the whole-
/// function SROA at the body root.
pub(super) struct SroaRule<'a> {
    stores_lookup: &'a StoresLookup,
    /// The `$value_copy$T` helper ids. A candidate consumed inside a value copy
    /// (`g($value_copy$S(s))`, `return $value_copy$S(s)`) is reconstructible: the
    /// soft-escape walk peels the wrapper and treats the inner bare local as a
    /// soft position.
    value_copy_ids: &'a IndexSet<crate::nir::FuncId>,
    /// Snapshot of `func.stores_aliased_locals` at session start. Used as a
    /// blacklist when picking candidates so a local that the existing alias
    /// analysis already flagged is never decomposed.
    stores_aliased: IndexSet<u32>,
    /// Locals discovered to be aliased by a decomposed candidate's `&local`
    /// field value (step 3b). Merged into `func.stores_aliased_locals` by the
    /// driver after the engine session ends.
    newly_aliased: RefCell<IndexSet<u32>>,
    /// Whole-function rewrite: only run once per session. The engine's
    /// re-try-after-success loop and any block re-enqueue triggered by edits
    /// could otherwise call `apply_block` at the root again.
    applied: Cell<bool>,
}

impl Rule for SroaRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        if self.applied.replace(true) {
            return false;
        }
        sroa_at_root(engine, self)
    }
}

fn sroa_at_root(engine: &mut Engine, rule: &SroaRule) -> bool {
    // Step 1: identify candidate Let bindings (struct/tuple literals).
    let candidates = collect_candidates(engine.body);
    if candidates.is_empty() {
        return false;
    }

    // Step 2: escape analysis.
    let uses = scan_candidate_uses(engine.body, &candidates);
    let soft_escaped = find_soft_escaped_locals(
        engine.body,
        &candidates,
        &uses,
        rule.stores_lookup,
        rule.value_copy_ids,
    );

    let mut decomposed: IndexSet<u32> = IndexSet::default();
    let mut reconstruct_set: IndexSet<u32> = IndexSet::default();
    for c in &candidates {
        if rule.stores_aliased.contains(&c.local_index) {
            continue;
        }
        if !uses.escaped.contains(&c.local_index) {
            decomposed.insert(c.local_index);
        } else if soft_escaped.contains(&c.local_index) {
            decomposed.insert(c.local_index);
            reconstruct_set.insert(c.local_index);
        }
    }
    if decomposed.is_empty() {
        return false;
    }

    // Step 3: allocate scalar locals for each field of each SROA'd candidate,
    // through the engine so the locals list grows coherently.
    let mut field_map: IndexMap<(u32, u32), FieldSlot> = IndexMap::default();
    for candidate in &candidates {
        if !decomposed.contains(&candidate.local_index) {
            continue;
        }
        // `candidate.fields` is field-index-ordered (0..N, asserted at
        // collection), so the positional index `i` *is* the `field_index` every
        // lookup keys by.
        for (i, (field_name, field_type)) in candidate.fields.iter().enumerate() {
            let name = format!("__sroa_{}_{}", candidate.local_name, field_name);
            let local_index = engine.alloc_local(name.clone(), *field_type, candidate.is_mut);
            field_map.insert(
                (candidate.local_index, i as u32),
                FieldSlot {
                    local_index,
                    name,
                    type_id: *field_type,
                },
            );
        }
    }

    let mut candidate_mut: IndexMap<u32, bool> = IndexMap::default();
    let mut reconstruct_info: IndexMap<u32, ReconstructInfo> = IndexMap::default();
    for candidate in &candidates {
        if !decomposed.contains(&candidate.local_index) {
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

    // Step 3b: mark locals referenced via &local in decomposed struct fields.
    // The delta is collected here and merged into `func.stores_aliased_locals`
    // by the driver after the session closes (rules can't touch
    // function-scope fields directly).
    {
        let mut newly = rule.newly_aliased.borrow_mut();
        mark_ref_field_locals_as_aliased(engine.body, &candidates, &decomposed, &mut newly);
    }

    // Step 4: rewrite — expand candidate Lets and replace field accesses.
    let ctx = Rewrite {
        decomposed: &decomposed,
        field_map: &field_map,
        candidate_mut: &candidate_mut,
        reconstruct_info: &reconstruct_info,
    };
    let root = engine.body.root;
    rewrite_block(engine, root, &ctx);

    true
}

// -----------------------------------------------------------------------
// Step 3b: mark &local field values as stores-aliased
// -----------------------------------------------------------------------

fn mark_ref_field_locals_as_aliased(
    body: &Body,
    candidates: &[SroaCandidate],
    decomposed: &IndexSet<u32>,
    stores_aliased: &mut IndexSet<u32>,
) {
    for c in candidates {
        if decomposed.contains(&c.local_index) {
            collect_ref_locals_in_fields(body, c.literal, stores_aliased);
        }
    }
}

fn collect_ref_locals_in_fields(body: &Body, expr: ExprId, stores_aliased: &mut IndexSet<u32>) {
    match &body.exprs[expr].kind {
        ExprKind::StructLiteral { fields, .. } => {
            for v in fields.iter().filter_map(|f| f.value.as_expr()) {
                extract_ref_local(body, v, stores_aliased);
            }
        }
        ExprKind::TupleLiteral { elements, .. } => {
            for e in elements.iter().filter_map(|e| e.as_expr()) {
                extract_ref_local(body, e, stores_aliased);
            }
        }
        _ => {}
    }
}

fn extract_ref_local(body: &Body, expr: ExprId, stores_aliased: &mut IndexSet<u32>) {
    if let ExprKind::Unary { op, expr: inner } = &body.exprs[expr].kind
        && matches!(
            op,
            crate::nir::NirUnaryOp::Ref | crate::nir::NirUnaryOp::MutRef
        )
        && let Some(ExprKind::Local { index, .. }) = inner.as_expr().map(|e| &body.exprs[e].kind)
    {
        stores_aliased.insert(*index);
    }
}

/// The scalar local one decomposed field became.
struct FieldSlot {
    local_index: u32,
    name: String,
    type_id: TypeId,
}

struct ReconstructInfo {
    struct_name: String,
    aggregate_type_id: TypeId,
    fields: Vec<(String, TypeId)>,
}

// -----------------------------------------------------------------------
// Candidate collection
// -----------------------------------------------------------------------

fn collect_candidates(body: &Body) -> Vec<SroaCandidate> {
    let mut candidates = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node {
            candidate_from_stmt(body, s, &mut candidates);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    candidates
}

fn candidate_from_stmt(body: &Body, stmt: StmtId, candidates: &mut Vec<SroaCandidate>) {
    let StmtKind::Let {
        name,
        local_index,
        is_mut,
        value,
        ..
    } = &body.stmts[stmt].kind
    else {
        return;
    };
    let (name, local_index, is_mut, value) = (name.clone(), *local_index, *is_mut, *value);
    // A promoted constant binding is not an aggregate to scalarize.
    let Some(value_e) = value.as_expr() else {
        return;
    };
    let aggregate_type_id = body.exprs[value_e].type_id;
    match &body.exprs[value_e].kind {
        ExprKind::StructLiteral {
            struct_name,
            fields,
            ..
        } => {
            // Order by `field_index` so positional slot k maps to field k in
            // allocation, expansion, and reconstruction (which all key by
            // `field_index`). The indices must cover 0..N exactly once.
            let mut ordered: Vec<(u32, String, TypeId)> = fields
                .iter()
                .map(|f| (f.field_index, f.name.clone(), body.operand_type(f.value)))
                .collect();
            ordered.sort_by_key(|(fi, _, _)| *fi);
            assert!(
                ordered
                    .iter()
                    .enumerate()
                    .all(|(k, (fi, _, _))| *fi == k as u32),
                "SROA struct-literal fields must cover 0..N by field_index"
            );
            let field_info: Vec<(String, TypeId)> =
                ordered.into_iter().map(|(_, n, t)| (n, t)).collect();
            candidates.push(SroaCandidate {
                local_index,
                local_name: name,
                literal: value_e,
                fields: field_info,
                is_mut,
                aggregate_type_id,
                struct_name: struct_name.clone(),
            });
        }
        ExprKind::TupleLiteral { elements, .. } => {
            let field_info: Vec<(String, TypeId)> = elements
                .iter()
                .enumerate()
                .map(|(i, e)| (i.to_string(), body.operand_type(*e)))
                .collect();
            candidates.push(SroaCandidate {
                local_index,
                local_name: name,
                literal: value_e,
                fields: field_info,
                is_mut,
                aggregate_type_id,
                struct_name: String::new(),
            });
        }
        _ => {}
    }
}

// -----------------------------------------------------------------------
// Escape analysis
// -----------------------------------------------------------------------

fn is_candidate_local(body: &Body, expr: ExprId, candidates: &IndexSet<u32>) -> Option<u32> {
    if let ExprKind::Local { index, .. } = &body.exprs[expr].kind
        && candidates.contains(index)
    {
        return Some(*index);
    }
    None
}

/// `&candidate` — the shape a callee that does not store its parameter may hold
/// without forcing an escape.
fn is_immut_ref_to_candidate(body: &Body, expr: ExprId, candidates: &IndexSet<u32>) -> bool {
    matches!(
        ref_to_candidate_local(body, expr, candidates),
        Some((_, false))
    )
}

/// The shared place classification every walker keys on: if `expr` is a
/// `FieldAccess` whose base is a bare candidate local, return
/// `(candidate local index, field index)`. Used both for a field *read*
/// (`candidate.field`) and, applied to an `Assign` target, for a field *write*
/// (`candidate.field = …`), so the `Assign`-target special case lives once.
fn field_access_of_candidate(
    body: &Body,
    expr: ExprId,
    candidates: &IndexSet<u32>,
) -> Option<(u32, u32)> {
    if let ExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &body.exprs[expr].kind
        && let Some(idx) = inner
            .as_expr()
            .and_then(|e| is_candidate_local(body, e, candidates))
    {
        return Some((idx, *field_index));
    }
    None
}

/// If `expr` is `&candidate` / `&mut candidate`, return `(local index, is the
/// reference mutable)`. Either escapes the candidate; only the soft walk cares
/// which.
fn ref_to_candidate_local(
    body: &Body,
    expr: ExprId,
    candidates: &IndexSet<u32>,
) -> Option<(u32, bool)> {
    let ExprKind::Unary { op, expr: inner } = &body.exprs[expr].kind else {
        return None;
    };
    let is_mut = match op {
        crate::nir::NirUnaryOp::Ref => false,
        crate::nir::NirUnaryOp::MutRef => true,
        crate::nir::NirUnaryOp::Not
        | crate::nir::NirUnaryOp::Neg
        | crate::nir::NirUnaryOp::BitNot
        | crate::nir::NirUnaryOp::Deref => return None,
    };
    let ie = inner.as_expr()?;
    let ExprKind::Local { index, .. } = &body.exprs[ie].kind else {
        return None;
    };
    candidates.contains(index).then_some((*index, is_mut))
}

/// What one read-only walk records about each candidate.
#[derive(Default)]
struct CandidateUses {
    /// Appeared in a non-projected position — a bare use or a reference. The
    /// soft walk decides whether such a candidate can still be reconstructed.
    escaped: IndexSet<u32>,
    /// Appeared as the base of a field access, read or written. A reconstructed
    /// candidate is worth decomposing only when some access projects it.
    field_accessed: IndexSet<u32>,
}

fn scan_candidate_uses(body: &Body, candidates: &[SroaCandidate]) -> CandidateUses {
    let candidate_set: IndexSet<u32> = candidates.iter().map(|c| c.local_index).collect();
    let mut uses = CandidateUses::default();
    UseWalk {
        candidates: &candidate_set,
    }
    .node(body, NodeRef::Block(body.root), &mut uses);
    uses
}

<<<<<<< HEAD
/// Which fact the read-only walker records. Both modes share one dispatch
/// skeleton over `FieldAccess` / `Assign` / `Local` / `Unary(Ref)`; they differ
/// only in what they record at those leaves.
#[derive(Clone, Copy)]
enum ReadKind {
    /// Record a candidate that appears in any non-projected position — a bare
    /// use or a reference means it escapes decomposition.
    Escape,
    /// Record a candidate that appears as the base of a field access.
    FieldAccess,
}

/// The unified read-only escape/field-access walker. `escape` and the
/// field-access scan re-encoded the same traversal; this parameterizes the one
/// skeleton by [`ReadKind`]. The soft-escape walk ([`SoftCtx`]) stays separate:
/// it threads a soft-context flag, peels `$value_copy$T` wrappers, and treats
/// call reference arguments specially, none of which this
/// read-only pair needs.
struct ReadWalk<'a> {
    kind: ReadKind,
||||||| b07ac9e97
/// Which fact the read-only walker records. Both modes share one dispatch
/// skeleton over `FieldAccess` / `Assign` / `Local` / `Unary(Ref)`; they differ
/// only in what they record at those leaves.
#[derive(Clone, Copy)]
enum ReadKind {
    /// Record a candidate that appears in any non-projected position — a bare
    /// use or a reference means it escapes decomposition.
    Escape,
    /// Record a candidate that appears as the base of a field access.
    FieldAccess,
}

/// The unified read-only escape/field-access walker. `escape` and the
/// field-access scan re-encoded the same traversal; this parameterizes the one
/// skeleton by [`ReadKind`]. The soft-escape walk ([`SoftCtx`]) stays separate:
/// it threads a soft-context flag, peels `$value_copy$T` wrappers, and treats
/// `Call` / `MethodCall` reference arguments specially, none of which this
/// read-only pair needs.
struct ReadWalk<'a> {
    kind: ReadKind,
=======
/// The read-only walk behind [`CandidateUses`]. The soft-escape walk
/// ([`SoftCtx`]) stays separate: it carries a context flag and a wrapper peel
/// this one has no use for.
struct UseWalk<'a> {
>>>>>>> origin/main
    candidates: &'a IndexSet<u32>,
}

impl UseWalk<'_> {
    fn node(&self, body: &Body, node: NodeRef, out: &mut CandidateUses) {
        if let NodeRef::Expr(id) = node {
            self.expr(body, id, out);
        } else {
            body.for_each_child(node, |c| self.node(body, c, out));
        }
    }

    fn expr_operand(&self, body: &Body, op: Operand, out: &mut CandidateUses) {
        if let Some(e) = op.as_expr() {
            self.expr(body, e, out);
        }
    }

    fn expr(&self, body: &Body, id: ExprId, out: &mut CandidateUses) {
        match &body.exprs[id].kind {
            ExprKind::FieldAccess { expr: inner, .. } => {
                let inner = *inner;
                if let Some((idx, _)) = field_access_of_candidate(body, id, self.candidates) {
                    out.field_accessed.insert(idx);
                    return;
                }
                self.expr_operand(body, inner, out);
            }
            ExprKind::Assign { target, value } => {
                let (target, value) = (*target, *value);
                if let Some((idx, _)) = field_access_of_candidate(body, target, self.candidates) {
                    out.field_accessed.insert(idx);
                } else {
                    self.expr(body, target, out);
                }
                self.expr_operand(body, value, out);
            }
            ExprKind::Local { index, .. } => {
                if self.candidates.contains(index) {
                    out.escaped.insert(*index);
                }
            }
            ExprKind::Unary { expr: inner, .. } => {
                let inner = *inner;
                if let Some((idx, _)) = ref_to_candidate_local(body, id, self.candidates) {
                    out.escaped.insert(idx);
                    return;
                }
                self.expr_operand(body, inner, out);
            }
            _ => body.for_each_child(NodeRef::Expr(id), |c| self.node(body, c, out)),
        }
    }
}

fn find_soft_escaped_locals(
    body: &Body,
    candidates: &[SroaCandidate],
    uses: &CandidateUses,
    stores_lookup: &StoresLookup,
    value_copy_ids: &IndexSet<crate::nir::FuncId>,
) -> IndexSet<u32> {
    let escaped_candidates: IndexSet<u32> = candidates
        .iter()
        .map(|c| c.local_index)
        .filter(|idx| uses.escaped.contains(idx))
        .collect();
    if escaped_candidates.is_empty() {
        return IndexSet::default();
    }

    let soft = SoftCtx {
        candidates: &escaped_candidates,
        stores_lookup,
        value_copy_ids,
    };
    let mut hard_escaped = IndexSet::default();
    soft.walk(body, NodeRef::Block(body.root), &mut hard_escaped);

    escaped_candidates
        .into_iter()
        .filter(|idx| !hard_escaped.contains(idx) && uses.field_accessed.contains(idx))
        .collect()
}

/// The shared immutable inputs of the soft-escape walk, bundled so the walk
/// threads one borrow instead of the former seven positional params.
struct SoftCtx<'a> {
    candidates: &'a IndexSet<u32>,
    stores_lookup: &'a StoresLookup,
    value_copy_ids: &'a IndexSet<crate::nir::FuncId>,
}

impl SoftCtx<'_> {
    fn walk(&self, body: &Body, node: NodeRef, hard_escaped: &mut IndexSet<u32>) {
        match node {
            NodeRef::Stmt(s) => {
                // Return / Break value's top expression is a soft context.
                if let StmtKind::Return { value: Some(v) }
                | StmtKind::Break { value: Some(v), .. } = &body.stmts[s].kind
                {
                    if let Some(ve) = v.as_expr() {
                        self.expr(body, ve, true, hard_escaped);
                    }
                } else {
                    body.for_each_child(NodeRef::Stmt(s), |c| self.walk(body, c, hard_escaped));
                }
            }
            NodeRef::Expr(id) => self.expr(body, id, false, hard_escaped),
            _ => body.for_each_child(node, |c| self.walk(body, c, hard_escaped)),
        }
    }

    fn expr_operand(&self, body: &Body, op: Operand, soft: bool, hard_escaped: &mut IndexSet<u32>) {
        if let Some(e) = op.as_expr() {
            self.expr(body, e, soft, hard_escaped);
        }
    }

    fn expr(&self, body: &Body, id: ExprId, soft: bool, hard_escaped: &mut IndexSet<u32>) {
        // See through a `$value_copy$T(inner)` wrapper: the copy reconstructs a
        // fresh value, so its wrapped candidate use is a soft (reconstructible)
        // position regardless of the enclosing context (`g($value_copy$S(s))`,
        // `return $value_copy$S(s)`). The rewrite reconstructs the literal inside
        // the copy, which then copies a fresh literal (a redundant, sound no-op).
        if let Some(inner) = strip_one_value_copy(body, id, self.value_copy_ids) {
            self.expr(body, inner, true, hard_escaped);
            return;
        }
        match &body.exprs[id].kind {
            ExprKind::FieldAccess { expr: inner, .. } => {
                let inner = *inner;
                if field_access_of_candidate(body, id, self.candidates).is_some() {
                    return;
                }
                self.expr_operand(body, inner, false, hard_escaped);
            }
            ExprKind::Assign { target, value } => {
                let (target, value) = (*target, *value);
                if field_access_of_candidate(body, target, self.candidates).is_some() {
                    if let Some(ve) = value.as_expr() {
                        self.expr(body, ve, false, hard_escaped);
                    }
                    return;
                }
                self.expr(body, target, false, hard_escaped);
                if let Some(ve) = value.as_expr() {
                    self.expr(body, ve, false, hard_escaped);
                }
            }
            ExprKind::Local { index, .. } => {
                if self.candidates.contains(index) && !soft {
                    hard_escaped.insert(*index);
                }
            }
            ExprKind::Unary { expr: inner, .. } => {
                let inner = *inner;
                if let Some((idx, _)) = ref_to_candidate_local(body, id, self.candidates) {
                    hard_escaped.insert(idx);
                    return;
                }
                self.expr_operand(body, inner, false, hard_escaped);
            }
            ExprKind::Call { func_id, args, .. } => {
                let callee_id = *func_id;
                let arg_ops: Vec<Operand> = args.iter().map(|a| a.expr).collect();
                for (i, arg) in arg_ops.into_iter().enumerate() {
                    let Some(arg) = arg.as_expr() else { continue };
                    if is_immut_ref_to_candidate(body, arg, self.candidates)
                        && !callee_stores_param_at(callee_id, i, self.stores_lookup)
                    {
                        continue;
                    }
                    self.expr(body, arg, false, hard_escaped);
                }
            }
<<<<<<< HEAD
            _ => {
                let mut kids = Vec::new();
                body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
                for c in kids {
                    self.walk(body, c, hard_escaped);
                }
            }
||||||| b07ac9e97
            ExprKind::MethodCall {
                receiver,
                func_id,
                args,
                ..
            } => {
                let receiver = *receiver;
                let callee_id = *func_id;
                let arg_ops: Vec<Operand> = args.iter().map(|a| a.expr).collect();
                if let Some(re) = receiver.as_expr()
                    && (!is_immut_ref_to_candidate(body, re, self.candidates)
                        || callee_stores_param_at(callee_id, 0, self.stores_lookup))
                {
                    self.expr(body, re, false, hard_escaped);
                }
                for (i, arg) in arg_ops.into_iter().enumerate() {
                    let Some(arg) = arg.as_expr() else { continue };
                    if is_immut_ref_to_candidate(body, arg, self.candidates)
                        && !callee_stores_param_at(callee_id, i + 1, self.stores_lookup)
                    {
                        continue;
                    }
                    self.expr(body, arg, false, hard_escaped);
                }
            }
            _ => {
                let mut kids = Vec::new();
                body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
                for c in kids {
                    self.walk(body, c, hard_escaped);
                }
            }
=======
            ExprKind::MethodCall {
                receiver,
                func_id,
                args,
                ..
            } => {
                let receiver = *receiver;
                let callee_id = *func_id;
                let arg_ops: Vec<Operand> = args.iter().map(|a| a.expr).collect();
                if let Some(re) = receiver.as_expr()
                    && (!is_immut_ref_to_candidate(body, re, self.candidates)
                        || callee_stores_param_at(callee_id, 0, self.stores_lookup))
                {
                    self.expr(body, re, false, hard_escaped);
                }
                for (i, arg) in arg_ops.into_iter().enumerate() {
                    let Some(arg) = arg.as_expr() else { continue };
                    if is_immut_ref_to_candidate(body, arg, self.candidates)
                        && !callee_stores_param_at(callee_id, i + 1, self.stores_lookup)
                    {
                        continue;
                    }
                    self.expr(body, arg, false, hard_escaped);
                }
            }
            _ => body.for_each_child(NodeRef::Expr(id), |c| self.walk(body, c, hard_escaped)),
>>>>>>> origin/main
        }
    }
}

fn callee_stores_param_at(
    func_id: crate::nir::FuncId,
    param_index: usize,
    stores_lookup: &StoresLookup,
) -> bool {
    // Born-resolved `func_id` names the callee directly — no entry-point module
    // remap needed (it resolved the real target at `lower`).
    stores_lookup
        .get(&func_id)
        .is_some_and(|stored_indices| stored_indices.contains(&param_index))
}

// -----------------------------------------------------------------------
// Rewrite (engine-routed)
// -----------------------------------------------------------------------

struct Rewrite<'a> {
    /// Every decomposed candidate: the non-escaping ones plus the soft-escaped
    /// ones [`Rewrite::reconstruct_info`] re-materializes.
    decomposed: &'a IndexSet<u32>,
    field_map: &'a IndexMap<(u32, u32), FieldSlot>,
    candidate_mut: &'a IndexMap<u32, bool>,
    reconstruct_info: &'a IndexMap<u32, ReconstructInfo>,
}

impl Rewrite<'_> {
    /// A `Local` node reading the scalar that replaced field `key`.
    fn field_local(&self, key: (u32, u32)) -> ExprKind {
        let slot = &self.field_map[&key];
        ExprKind::Local {
            index: slot.local_index,
            name: slot.name.clone(),
        }
    }
}

fn rewrite_block(engine: &mut Engine, block: BlockId, ctx: &Rewrite) {
    let old_stmts = engine.body.blocks[block].stmts.clone();
    let mut new_stmts: Vec<StmtId> = Vec::with_capacity(old_stmts.len());
    for stmt in old_stmts {
        let candidate = match &engine.body.stmts[stmt].kind {
            StmtKind::Let { local_index, .. } if ctx.decomposed.contains(local_index) => {
                Some(*local_index)
            }
            _ => None,
        };
        if let Some(local_idx) = candidate {
            let span = engine.body.stmts[stmt].span;
            let is_mut = ctx.candidate_mut.get(&local_idx).copied().unwrap_or(false);
            let StmtKind::Let { value, .. } = &engine.body.stmts[stmt].kind else {
                unreachable!("candidate must be Let statement");
            };
            let value = *value;
            // `candidate_from_stmt` only accepts a skeleton `StructLiteral` value.
            let Some(value_e) = value.as_expr() else {
                unreachable!("SROA candidate requires a skeleton StructLiteral value");
            };
            expand_struct_let(
                engine,
                value_e,
                local_idx,
                is_mut,
                span,
                ctx,
                &mut new_stmts,
            );
            continue;
        }
        rewrite_node(engine, NodeRef::Stmt(stmt), ctx);
        new_stmts.push(stmt);
    }
    engine.set_block_stmts(block, new_stmts);
}

fn rewrite_node(engine: &mut Engine, node: NodeRef, ctx: &Rewrite) {
    match node {
        NodeRef::Expr(id) => rewrite_expr(engine, id, ctx),
        NodeRef::Block(b) => rewrite_block(engine, b, ctx),
        _ => {
            let mut kids = Vec::new();
            engine.body.for_each_child(node, |c| kids.push(c));
            for c in kids {
                rewrite_node(engine, c, ctx);
            }
        }
    }
}

fn rewrite_expr(engine: &mut Engine, id: ExprId, ctx: &Rewrite) {
    // Field read: candidate.field -> scalar local.
    if let Some(key) = field_access_of_candidate(engine.body, id, ctx.decomposed)
        && ctx.field_map.contains_key(&key)
    {
        engine.replace_expr_kind(id, ctx.field_local(key));
        return;
    }

    // Field write: candidate.field = value -> scalar_local = value.
    if let ExprKind::Assign { target, value } = &engine.body.exprs[id].kind {
        let (target, value) = (*target, *value);
        if let Some(key) = field_access_of_candidate(engine.body, target, ctx.decomposed)
            && ctx.field_map.contains_key(&key)
        {
            engine.replace_expr_kind(target, ctx.field_local(key));
            if let Some(ve) = value.as_expr() {
                rewrite_expr(engine, ve, ctx);
            }
            return;
        }
    }

    // Reconstruct: bare Local of a soft-escape candidate -> re-materialize.
    if let ExprKind::Local { index, .. } = &engine.body.exprs[id].kind {
        let index = *index;
        if ctx.reconstruct_info.contains_key(&index) {
            reconstruct_aggregate(engine, id, index, ctx);
            return;
        }
    }

    let mut kids = Vec::new();
    engine
        .body
        .for_each_child(NodeRef::Expr(id), |c| kids.push(c));
    for c in kids {
        rewrite_node(engine, c, ctx);
    }
}

/// Expand a candidate `Let` value into one per-field `Let`, rewriting each
/// field expression as it goes.
fn expand_struct_let(
    engine: &mut Engine,
    value: ExprId,
    local_idx: u32,
    is_mut: bool,
    span: Span,
    ctx: &Rewrite,
    new_stmts: &mut Vec<StmtId>,
) {
    // (field_index, operand) pairs in field-index order.
    let mut pairs: Vec<(u32, Operand)> = match &engine.body.exprs[value].kind {
        ExprKind::StructLiteral { fields, .. } => {
            fields.iter().map(|f| (f.field_index, f.value)).collect()
        }
        ExprKind::TupleLiteral { elements, .. } => elements
            .iter()
            .enumerate()
            .map(|(i, e)| (i as u32, *e))
            .collect(),
        _ => unreachable!("candidate must be struct or tuple literal"),
    };
    pairs.sort_by_key(|(fi, _)| *fi);
    for (field_index, field_op) in pairs {
        // A promoted-constant field flows straight into the scalar's `let` slot;
        // a skeleton field is rewritten in place to propagate nested decompositions.
        if let Some(e) = field_op.as_expr() {
            rewrite_expr(engine, e, ctx);
        }
        push_field_let(
            engine,
            (local_idx, field_index),
            is_mut,
            span,
            field_op,
            ctx,
            new_stmts,
        );
    }
}

fn push_field_let(
    engine: &mut Engine,
    key: (u32, u32),
    is_mut: bool,
    span: Span,
    value: Operand,
    ctx: &Rewrite,
    new_stmts: &mut Vec<StmtId>,
) {
    let slot = &ctx.field_map[&key];
    let stmt = engine.alloc_stmt(
        StmtKind::Let {
            name: slot.name.clone(),
            local_index: slot.local_index,
            is_mut,
            is_reactive: false,
            type_id: slot.type_id,
            value,
            // The original literal was a fresh value, so its fields don't need
            // value_copy — see the original pass comment.
            skip_value_copy: true,
        },
        span,
    );
    new_stmts.push(stmt);
}

/// Build a reconstructed struct or tuple literal from SROA'd scalar locals,
/// replacing the bare-`Local` node `id` in place (keeping its `type_id` / span).
fn reconstruct_aggregate(engine: &mut Engine, id: ExprId, local_idx: u32, ctx: &Rewrite) {
    let info = &ctx.reconstruct_info[&local_idx];
    let span = engine.body.exprs[id].span;
    let is_tuple = info.struct_name.is_empty();
    let field_names: Vec<String> = info.fields.iter().map(|(name, _)| name.clone()).collect();
    let struct_name = info.struct_name.clone();
    let struct_type = info.aggregate_type_id;

    let mut values: Vec<ExprId> = Vec::with_capacity(field_names.len());
    for i in 0..field_names.len() {
        let key = (local_idx, i as u32);
        let kind = ctx.field_local(key);
        let type_id = ctx.field_map[&key].type_id;
        values.push(engine.alloc_expr(kind, type_id, span));
    }

    let kind = if is_tuple {
        ExprKind::TupleLiteral {
            elements: values
                .into_iter()
                .map(crate::nir_arena::Operand::Expr)
                .collect(),
        }
    } else {
        ExprKind::StructLiteral {
            struct_type,
            struct_name,
            fields: values
                .into_iter()
                .zip(field_names)
                .enumerate()
                .map(|(i, (value, name))| ArenaStructField {
                    name,
                    value: value.into(),
                    field_index: i as u32,
                })
                .collect(),
        }
    };
    engine.replace_expr_kind(id, kind);
}
