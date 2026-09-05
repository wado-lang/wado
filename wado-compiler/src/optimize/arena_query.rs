//! Arena-side structural queries shared by the rewrite-engine rules, reading the
//! [`Body`] directly so the ported passes need no `Body ↔ tree` bridge. A new
//! see-through node kind must be taught to `storage_root` / `strip_refs` here
//! *and* to `niri/place.rs`, which keeps its own transparent-wrapper set.
//!
//! After operand promotion a pure read lives in the value pool rather than the
//! skeleton, where a node walk cannot see it; the `promoted_*` queries supply
//! exactly that, scoped to the *reachable* operands — the pool is append-only, so
//! seeding from it wholesale would keep long-folded locals alive forever.

use crate::hashmap::IndexSet;
use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatId, PatKind, StmtId, StmtKind,
};
use crate::nir_engine::Engine;
use crate::nir_value_graph::{OpaqueSource, ValueId, ValueKind};
use crate::tir::TypeTable;

/// Every block reachable from the body root, in DFS pop order (a block precedes
/// the blocks nested under it). The NIR block graph is a tree, so no visited set
/// is needed.
pub(super) fn reachable_blocks(body: &Body) -> Vec<BlockId> {
    let mut out = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Block(b) = node {
            out.push(b);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    out
}

/// Every expression a statement evaluates and throws away. A block's tail is
/// excluded — it may be the block's value, and a wrong answer there is unsound.
pub(super) fn discarded_exprs(body: &Body) -> IndexSet<ExprId> {
    let mut out = IndexSet::default();
    for block in body.blocks.values() {
        let Some((_, dropped)) = block.stmts.split_last() else {
            continue;
        };
        for s in dropped {
            if let StmtKind::Expr(Operand::Expr(e)) = body.stmts[*s].kind {
                out.insert(e);
            }
        }
    }
    out
}

/// Every local a reachable promoted operand reads — the `Opaque(Local)` leaves
/// of the values the skeleton still carries. A pass deciding a local is unused
/// unions this into its skeleton census.
///
/// A rule inside an engine session asks [`crate::nir_engine::Engine`], which
/// memoizes it; this entry point is for the standalone passes.
pub(super) fn promoted_local_reads(body: &Body, out: &mut IndexSet<u32>) {
    body.promoted_local_reads(out);
}

/// How many of `node`'s operand slots read `idx` through a promoted value. A
/// skeleton use census adds this at each node it visits.
///
/// A buried read (`Binary(Opaque(Local x), 1)`) counts like a bare one: a gate
/// that pairs a total against a specific-shape tally reads the imbalance as
/// "a read this rewrite cannot reach", which both are.
pub(super) fn promoted_read_count_at(body: &Body, node: NodeRef, idx: u32) -> usize {
    let mut count = 0;
    body.for_each_operand(node, |op| {
        if let Some(v) = op.as_value()
            && body.values.value_reads_local(v, idx)
        {
            count += 1;
        }
    });
    count
}

/// The values of the reachable operands that are exactly `Opaque(Local idx)`.
/// A pass rewriting *every* read of `idx` — globalizing it, propagating it away
/// — can replace these, because the local fills the slot whole;
/// [`buried_promoted_reads`] names the ones it cannot.
pub(super) fn bare_promoted_reads(
    body: &Body,
    idx: u32,
) -> IndexSet<crate::nir_value_graph::ValueId> {
    let mut out = IndexSet::default();
    for node in reachable_nodes(body) {
        body.for_each_operand(node, |op| {
            if bare_promoted_local(body, op) == Some(idx)
                && let Some(v) = op.as_value()
            {
                out.insert(v);
            }
        });
    }
    out
}

/// Every local a reachable operand reads *without* filling its slot — a leaf of
/// a compound value (`Binary(Opaque(Local x), 1)`), which no operand rewrite can
/// reach. A pass that rewrites the reads of a local and then drops it must
/// refuse these. One walk answers for every local.
pub(super) fn buried_promoted_reads(body: &Body) -> IndexSet<u32> {
    let mut out = IndexSet::default();
    let mut seen = IndexSet::default();
    for node in reachable_nodes(body) {
        body.for_each_operand(node, |op| {
            if let Some(v) = op.as_value()
                && bare_promoted_local(body, op).is_none()
            {
                body.values
                    .collect_opaque_locals_seen(v, &mut seen, &mut out);
            }
        });
    }
    out
}

/// Every node reachable from the body root.
///
/// A body with no blocks has no root to walk from — a scratch arena a pass is
/// still filling — so every node it holds counts. A census may over-count, which
/// only keeps something alive; missing a read is what would miscompile.
pub(super) fn reachable_nodes(body: &Body) -> Vec<NodeRef> {
    let mut out = Vec::new();
    body.for_each_reachable_node(|n| out.push(n));
    out
}

/// The single optional payload-binding local of a variant arm's `bindings`,
/// as two distinct outcomes callers tell apart (so `?` propagates the reject):
/// `Some(None)` = no binding (`[]` or `[_]`); `Some(Some(idx))` = one `Binding`
/// slot; `None` = reject (multiple bindings, or a nested subpattern the
/// `labeled_block_fusion` payload substitution does not handle).
#[allow(clippy::option_option)]
pub(super) fn single_payload_binding(body: &Body, bindings: &[PatId]) -> Option<Option<u32>> {
    match bindings {
        [] => Some(None),
        [single] => match &body.pats[*single].kind {
            PatKind::Wildcard => Some(None),
            PatKind::Binding { local_index, .. } => Some(Some(*local_index)),
            _ => None,
        },
        _ => None,
    }
}

/// If `expr` is a place rooted at a local — `x`, `x.f`, `x[i]`, `*x`, and any
/// chain thereof — return that root local index; otherwise `None`.
///
/// Deliberately narrower than [`storage_root`]: stopping at `&x` lets
/// `copy_prop`'s mutation collector dispatch on the wrapper (a `&T` receiver is
/// not written through, so it is correctly not marked; `&mut x` is caught by
/// its own arm). Widening through references would over-mark and cost
/// propagations.
pub(super) fn place_root_local(body: &Body, expr: ExprId) -> Option<u32> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. } | ExprKind::Index { expr: inner, .. } => {
            inner.as_expr().and_then(|e| place_root_local(body, e))
        }
        ExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: inner,
        } => inner.as_expr().and_then(|e| place_root_local(body, e)),
        _ => None,
    }
}

/// A place as its root local plus the field-index chain leading off it.
pub(super) type Place = (u32, Vec<u32>);

/// The [`Place`] of an expression — its root local and field-access chain —
/// seeing through `&`/`&mut`/deref wrappers (so an inlined `self.f` whose
/// receiver became `&mut b` still roots at `b`). `None` at an `Index` or any
/// non-place step: a non-field place can never be a prefix of a pure
/// Local/field place, so it never overlaps one.
pub(super) fn place_path(body: &Body, expr: ExprId) -> Option<Place> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some((*index, Vec::new())),
        ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            let (root, mut fields) = place_path(body, inner.as_expr()?)?;
            fields.push(*field_index);
            Some((root, fields))
        }
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
            expr: inner,
        } => place_path(body, inner.as_expr()?),
        _ => None,
    }
}

/// Whether place `q` is a (non-strict) prefix of place `p`: same root and `q`'s
/// field chain leads `p`'s. Replacing the handle at `q` replaces the object a
/// reference to `p` observes.
pub(super) fn is_place_prefix(q: &Place, p: &Place) -> bool {
    q.0 == p.0 && q.1.len() <= p.1.len() && q.1 == p.1[..q.1.len()]
}

/// The local whose interior storage `expr` reaches, seeing through the sharing
/// projections — field access, indexing, variant payload, a transparent cast,
/// and `&`/`&mut`/`*` — but not an arithmetic unary. The root-only query for the
/// escape / aliasing / mutation analyses. `None` does not mean fresh:
/// `container.index_value(i)` still aliases, so pair it with a freshness gate.
pub(super) fn storage_root(body: &Body, expr: ExprId) -> Option<u32> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
            expr: inner,
        }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. } => storage_root(body, inner.as_expr()?),
        _ => None,
    }
}

/// The local `node` writes: the storage root of an `Assign` target, or of a
/// `&mut` handing the place to a callee. One definition of the write channels,
/// so a whole-body scan and a subtree one cannot disagree.
///
/// Not the `&mut self` receiver, which carries no `&mut` node and takes the
/// whole-program context `alias.rs` has to decide.
pub(super) fn local_written_by(body: &Body, node: NodeRef) -> Option<u32> {
    let NodeRef::Expr(e) = node else {
        return None;
    };
    match &body.exprs[e].kind {
        ExprKind::Assign { target, .. } => storage_root(body, *target),
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr,
        } => storage_root(body, expr.as_expr()?),
        _ => None,
    }
}

/// Whether the subtree at `node` contains a `Break` targeting `label`. A full
/// subtree search, so nested blocks that rebind the same label are still
/// searched — the conservative behaviour the sync-placement passes rely on.
pub(super) fn has_break_to(body: &Body, node: NodeRef, label: &str) -> bool {
    if let NodeRef::Stmt(s) = node
        && let StmtKind::Break { label: Some(l), .. } = &body.stmts[s].kind
        && l == label
    {
        return true;
    }
    let mut found = false;
    body.for_each_child(node, |c| {
        if !found {
            found = has_break_to(body, c, label);
        }
    });
    found
}

/// Strip outer auto-ref / deref wrappers (`&`, `&mut`, `*`) from an expression,
/// returning the inner value's id.
pub(super) fn strip_refs(body: &Body, id: ExprId) -> ExprId {
    match &body.exprs[id].kind {
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
            expr: inner,
            // A promoted `Operand::Value` inner cannot be stripped further; the
            // wrapper id is the leaf.
        } => inner.as_expr().map_or(id, |e| strip_refs(body, e)),
        _ => id,
    }
}

/// Strip a single `$value_copy$T(inner)` wrapper, returning its inner
/// expression, or `None` when `e` is not a one-argument value-copy call.
pub(super) fn strip_one_value_copy(
    body: &Body,
    e: ExprId,
    value_copy_ids: &IndexSet<crate::nir::FuncId>,
) -> Option<ExprId> {
    let ExprKind::Call { func_id, args, .. } = &body.exprs[e].kind else {
        return None;
    };
    if value_copy_ids.contains(func_id) && args.len() == 1 {
        args[0].expr.as_expr()
    } else {
        None
    }
}

/// See [`Body::collect_local_reads`].
pub(super) fn collect_reads(body: &Body, out: &mut IndexSet<u32>) {
    body.collect_local_reads(out);
}

/// Whether `id` is a bare `Local(idx)` reference.
pub(super) fn is_local(body: &Body, id: ExprId, idx: u32) -> bool {
    matches!(&body.exprs[id].kind, ExprKind::Local { index, .. } if *index == idx)
}

/// Whether `op` reads exactly `Local(idx)` — a bare skeleton `Local`, or the
/// promoted value that extracts back to one. Interchangeable in an operand slot,
/// so a rewrite substituting one substitutes the other.
pub(super) fn is_local_operand(body: &Body, op: Operand, idx: u32) -> bool {
    match op {
        Operand::Expr(e) => is_local(body, e, idx),
        Operand::Value(_) => bare_promoted_local(body, op) == Some(idx),
    }
}

/// The local a promoted operand reads whole: `Some(idx)` when the operand is
/// exactly `Opaque(Local idx)`. `None` for a skeleton operand, or a value that
/// is more than that leaf.
pub(super) fn bare_promoted_local(body: &Body, op: Operand) -> Option<u32> {
    let ValueKind::Opaque(oid) = body.values.kind(op.as_value()?) else {
        return None;
    };
    match body.values.opaque_source(*oid)? {
        OpaqueSource::Local(idx) => Some(idx),
        OpaqueSource::Expr(_) => None,
    }
}

/// Whether `idx` appears anywhere in the expression subtree at `id`. Matches
/// the coverage of the tree `expr_mentions_local` (every nested statement,
/// block, and `ConstantValue` pattern expression is walked).
pub(super) fn expr_mentions_local(body: &Body, id: ExprId, idx: u32) -> bool {
    node_mentions_local(body, NodeRef::Expr(id), idx)
}

/// Whether `idx` appears anywhere in the statement subtree at `id`.
pub(super) fn stmt_mentions_local(body: &Body, id: StmtId, idx: u32) -> bool {
    node_mentions_local(body, NodeRef::Stmt(id), idx)
}

fn node_mentions_local(body: &Body, node: NodeRef, idx: u32) -> bool {
    if let NodeRef::Expr(id) = node
        && is_local(body, id, idx)
    {
        return true;
    }
    let mut found = false;
    body.for_each_operand(node, |op| {
        if !found && let Some(v) = op.as_value() {
            found = body.values.value_reads_local(v, idx);
        }
    });
    if found {
        return true;
    }
    body.for_each_child(node, |c| {
        if !found {
            found = node_mentions_local(body, c, idx);
        }
    });
    found
}

/// Whether any `Loop` statement is nested anywhere under `block`. Exhaustive
/// via `for_each_child`, so it sees loops in `if`/`match`/`switch` arms, break
/// values, and every other position — not just direct `Block`/`LabeledBlock`
/// nesting. Shared by the inliner (cold-cost) and labeled-block fusion
/// (unlabeled-break capture guard).
pub(super) fn block_contains_loop(body: &Body, block: BlockId) -> bool {
    let mut stack = vec![NodeRef::Block(block)];
    while let Some(n) = stack.pop() {
        if let NodeRef::Stmt(s) = n
            && matches!(body.stmts[s].kind, StmtKind::Loop { .. })
        {
            return true;
        }
        body.for_each_child(n, |c| stack.push(c));
    }
    false
}

/// [`is_pure_expr`] for an operand: a promoted constant is pure.
pub(super) fn is_pure_operand(body: &Body, op: Operand) -> bool {
    op.as_expr().is_none_or(|e| is_pure_expr(body, e))
}

/// What `S { f: v, .. }.f` at `field_access` projects, when the receiver is the
/// literal itself and every other field is pure — so dropping the struct with it
/// discards no side effect.
pub(super) fn projected_const_field(body: &Body, field_access: ExprId) -> Option<Operand> {
    let ExprKind::FieldAccess {
        expr, field_name, ..
    } = &body.exprs[field_access].kind
    else {
        return None;
    };
    let ExprKind::StructLiteral { fields, .. } = &body.exprs[expr.as_expr()?].kind else {
        return None;
    };
    let mut projected = None;
    for f in fields {
        if f.name == *field_name {
            projected = Some(f.value);
        } else if !is_pure_operand(body, f.value) {
            return None;
        }
    }
    projected
}

/// True when the expression has no observable effect *and cannot trap* — the
/// predicate for a pass that *deletes* an expression, where dropping a `100 / x`
/// would erase a trap the program is entitled to. [`is_pure_expr`] stays
/// trap-agnostic for reordering and CSE. With a type table a `FieldAccess` on a
/// non-null receiver is non-trapping; pass `None` to stay conservative.
pub(super) fn is_pure_nontrapping_expr_typed(
    body: &Body,
    id: ExprId,
    types: Option<&crate::tir::TypeTable>,
) -> bool {
    is_pure_expr(body, id) && !super::mod_ref::ModRef::of_expr_typed(body, id, types).may_trap
}

/// [`is_pure_nontrapping_expr_typed`] for an operand. A promoted value is pure
/// by construction but not necessarily total — `let _ = 1 / 0` freezes a
/// trapping division into one — so its tree is walked in the pool.
pub(super) fn is_pure_nontrapping_operand_typed(
    body: &Body,
    op: Operand,
    types: Option<&crate::tir::TypeTable>,
) -> bool {
    match op {
        Operand::Expr(e) => is_pure_nontrapping_expr_typed(body, e, types),
        Operand::Value(v) => !value_may_trap(&body.values, v),
    }
}

/// True when the expression at `id` and every sub-expression has no observable
/// effect. The arena counterpart of `elide_local::is_pure_expr`; the two must
/// agree, since both gate the same rewrites.
pub(super) fn is_pure_expr(body: &Body, id: ExprId) -> bool {
    match &body.exprs[id].kind {
        ExprKind::PackedArray(_)
        | ExprKind::Local { .. }
        | ExprKind::GlobalVarGet { .. }
        | ExprKind::EnumConstruct { .. } => true,
        ExprKind::Binary { left, right, .. } => {
            is_pure_operand(body, *left) && is_pure_operand(body, *right)
        }
        ExprKind::Unary { expr: inner, .. } => is_pure_operand(body, *inner),
        ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. } => is_pure_operand(body, *inner),
        ExprKind::Index { expr: e, index: i } => {
            is_pure_operand(body, *e) && is_pure_operand(body, *i)
        }
        ExprKind::StructLiteral { fields, .. } => {
            fields.iter().all(|f| is_pure_operand(body, f.value))
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            elements.iter().all(|&e| is_pure_operand(body, e))
        }
        ExprKind::VariantConstruct { payload, .. } => {
            payload.is_none_or(|p| is_pure_operand(body, p))
        }
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            is_pure_block(body, *block)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            condition.as_expr().is_none_or(|e| is_pure_expr(body, e))
                && is_pure_block(body, *then_branch)
                && else_branch.is_none_or(|b| is_pure_block(body, b))
        }
        // Calls, mutations, closures, control-flow exits, and anything that
        // could suspend are conservatively impure.
        _ => false,
    }
}

fn is_pure_block(body: &Body, block: BlockId) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .all(|s| match &body.stmts[*s].kind {
            StmtKind::Expr(e) => is_pure_operand(body, *e),
            StmtKind::Let { value, .. } => is_pure_operand(body, *value),
            _ => false,
        })
}

// ---------------------------------------------------------------------------
// Per-node trap taxonomy
// ---------------------------------------------------------------------------
//
// The single listing of which operation traps, shared so no two passes reasoning
// about trap preservation can drift apart — an earlier trap-deletion P0 was
// exactly that. Mirrors per node what `mod_ref::ModRef::may_trap` accumulates
// recursively; `mod_ref` remains the recursive authority these reproduce.

/// Whether a [`NirBinaryOp`] may trap at runtime, independent of its operands.
/// Integer `Div` / `Mod` trap on a zero divisor (and `INT_MIN / -1`); every
/// other binary op is total.
pub(super) fn binary_op_may_trap(op: NirBinaryOp) -> bool {
    matches!(op, NirBinaryOp::Div | NirBinaryOp::Mod)
}

/// Whether a [`NirUnaryOp`] may trap, independent of its operand. `Deref`
/// traps on a null reference; `Ref` / `MutRef` / `Neg` / `Not` / `BitNot`
/// are total.
pub(super) fn unary_op_may_trap(op: NirUnaryOp) -> bool {
    matches!(op, NirUnaryOp::Deref)
}

/// Whether running the value tree at `v` can trap. The [`ValuePool`] counterpart
/// of [`expr_node_may_trap`], recursive because a value tree has no skeleton
/// nodes for a walker to descend through.
///
/// Two callers, for the two ways a promoted value can lose a trap: extraction
/// materialises it at a point that dominates the uses — above the guard each use
/// sits behind — and the deletion predicates drop the statement holding it.
///
/// Conservative on `Cast`: classifying one needs the operand's source type, and
/// nothing guarantees the type-erased tree recorded it.
pub(super) fn value_may_trap(pool: &crate::nir_value_graph::ValuePool, v: ValueId) -> bool {
    match pool.kind(v) {
        ValueKind::Binary { op, lhs, rhs, .. } => {
            binary_op_may_trap(*op) || value_may_trap(pool, *lhs) || value_may_trap(pool, *rhs)
        }
        ValueKind::Unary { op, operand, .. } => {
            unary_op_may_trap(*op) || value_may_trap(pool, *operand)
        }
        ValueKind::Cast { .. } => true,
        ValueKind::Select { cond, then, else_ } => {
            value_may_trap(pool, *cond)
                || value_may_trap(pool, *then)
                || value_may_trap(pool, *else_)
        }
        ValueKind::FieldAccess { .. } | ValueKind::LoopPhi { .. } => true,
        ValueKind::Int(..)
        | ValueKind::Float(..)
        | ValueKind::Bool(_)
        | ValueKind::Char(_)
        | ValueKind::Null
        | ValueKind::Unit
        | ValueKind::Const(..)
        | ValueKind::Opaque(_) => false,
    }
}

/// Whether the expression *node* at `id` — its own operation only, not its
/// children — may trap. The recursive trap of a whole subtree is
/// `mod_ref::ModRef::of_expr(..).may_trap`; this is the per-node contribution
/// a walker consults while it recurses itself.
///
/// `Cast` is conservatively trap-capable here (matching `mod_ref`); a consumer
/// needing the finer "only float→int truncation traps" refinement keeps its own
/// check (see `select_lowering::is_trapping_cast`).
pub(super) fn expr_node_may_trap(body: &Body, id: ExprId) -> bool {
    match &body.exprs[id].kind {
        ExprKind::Binary { op, .. } => binary_op_may_trap(*op),
        ExprKind::Unary { op, .. } => unary_op_may_trap(*op),
        // Numeric narrowing / `ref.cast`, and heap projections on a
        // possibly-null (or case-mismatched) receiver.
        ExprKind::Cast { .. }
        | ExprKind::FieldAccess { .. }
        | ExprKind::Index { .. }
        | ExprKind::VariantTag { .. }
        | ExprKind::VariantTest { .. }
        | ExprKind::VariantPayload { .. } => true,
        // A callee may trap (`panic`, OOB index, division, `unreachable`).
        ExprKind::Call { .. } | ExprKind::IndirectCall { .. } | ExprKind::CmRawCall { .. } => true,
        // A store through a projection traps on a null / OOB / mismatched
        // receiver; a bare-local rebind does not, but classify the node
        // conservatively — its sole consumer routes `Assign` through a
        // dedicated arm, so this value never decides an elision.
        ExprKind::Assign { .. } => true,
        // Pure value ops, constructors, constant leaves, global reads/writes,
        // and control-flow (whose sub-trees carry their own traps).
        ExprKind::GlobalVarGet { .. }
        | ExprKind::GlobalVarSet { .. }
        | ExprKind::Local { .. }
        | ExprKind::PackedArray(_)
        | ExprKind::EnumConstruct { .. }
        | ExprKind::StructLiteral { .. }
        | ExprKind::TupleLiteral { .. }
        | ExprKind::ArrayLiteral { .. }
        | ExprKind::VariantConstruct { .. }
        | ExprKind::ClosureToCanonical { .. }
        | ExprKind::Block(_)
        | ExprKind::LabeledBlock { .. }
        | ExprKind::If { .. }
        | ExprKind::Match { .. }
        | ExprKind::Switch { .. }
        | ExprKind::Dead => false,
    }
}

// ---------------------------------------------------------------------------
// Mutated-root queries — the canonical "which locals may a subtree mutate"
// facility. Consolidation target for the hand-rolled variants in
// `const_folding` and `condition_implication`.
// ---------------------------------------------------------------------------

/// Flow-insensitive `&mut`-alias map: per `&mut`-typed local, the function locals
/// its reference may point into. Built in one walk plus a fixpoint over
/// ref-to-ref copies; every other shape leaves the provenance unknown, so a write
/// may hit any `borrowed` local. A mut-ref parameter aliases nothing — a caller
/// cannot hold a reference into this frame's fresh locals.
#[derive(Debug, Default)]
pub(super) struct MutRefAliases {
    entries: crate::hashmap::IndexMap<u32, AliasEntry>,
    /// Locals whose storage some `&mut` may alias — the conservative target
    /// set for writes through an unknown-provenance reference.
    borrowed: IndexSet<u32>,
}

#[derive(Debug, Default)]
struct AliasEntry {
    roots: IndexSet<u32>,
    copies: IndexSet<u32>,
    unknown: bool,
    saw_def: bool,
}

/// Root of a written-through place chain (an `Assign` target's receiver).
enum WriteRoot {
    /// Chain bottoms out at a local (derefs of ref locals resolve through
    /// [`MutRefAliases`]).
    Local(u32),
    /// Chain passed a deref of a non-place (a call result, a ref stored in an
    /// aggregate): the written storage may belong to any borrowed local.
    Aliased,
    /// Fresh temporary storage (a literal receiver, no deref): mutating it
    /// cannot touch a named local.
    Temp,
}

fn write_root(body: &Body, e: ExprId, derefed: bool) -> WriteRoot {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => WriteRoot::Local(*index),
        ExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: inner,
        } => match inner.as_expr() {
            Some(ie) => write_root(body, ie, true),
            None => WriteRoot::Aliased,
        },
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. } => match inner.as_expr() {
            Some(ie) => write_root(body, ie, derefed),
            None if derefed => WriteRoot::Aliased,
            None => WriteRoot::Temp,
        },
        _ if derefed => WriteRoot::Aliased,
        _ => WriteRoot::Temp,
    }
}

impl MutRefAliases {
    /// Build the alias map for one function body. `locals` is the owning
    /// function's local table; locals `0..param_count` are its parameters
    /// (the layout `wir_build` also relies on).
    pub(super) fn of_body(
        body: &Body,
        locals: &[crate::nir::NirLocal],
        param_count: usize,
        type_table: &crate::tir::TypeTable,
    ) -> Self {
        use crate::tir::ResolvedType;
        let mut map = Self::default();
        for (i, l) in locals.iter().enumerate().skip(param_count) {
            if matches!(type_table.get(l.type_id), ResolvedType::MutRef(_)) {
                map.entries.entry(i as u32).or_default();
            }
        }
        let mut borrowed_refs: IndexSet<u32> = IndexSet::default();
        map.build_walk(body, NodeRef::Block(body.root), &mut borrowed_refs);
        // A ref-typed local with no recognized definition (a pattern binding,
        // an engine-synthesized slot) has unknown provenance.
        for e in map.entries.values_mut() {
            if !e.saw_def {
                e.unknown = true;
            }
        }
        // Fixpoint over ref-to-ref copies.
        loop {
            let mut changed = false;
            let keys: Vec<u32> = map.entries.keys().copied().collect();
            for k in &keys {
                let copies: Vec<u32> = map.entries[k].copies.iter().copied().collect();
                for c in copies {
                    let Some(src) = map.entries.get(&c) else {
                        continue;
                    };
                    let add_roots: Vec<u32> = src.roots.iter().copied().collect();
                    let add_unknown = src.unknown;
                    let e = map.entries.get_mut(k).expect("key from entries");
                    for r in add_roots {
                        changed |= e.roots.insert(r);
                    }
                    if add_unknown && !e.unknown {
                        e.unknown = true;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        // Storage reachable through a borrowed ref local is borrowed too.
        for r in borrowed_refs {
            if let Some(e) = map.entries.get(&r) {
                let roots: Vec<u32> = e.roots.iter().copied().collect();
                for root in roots {
                    map.borrowed.insert(root);
                }
            }
        }
        map
    }

    fn build_walk(&mut self, body: &Body, node: NodeRef, borrowed_refs: &mut IndexSet<u32>) {
        match node {
            NodeRef::Stmt(s) => {
                if let StmtKind::Let {
                    local_index, value, ..
                } = &body.stmts[s].kind
                {
                    self.classify_def(body, *local_index, *value);
                }
            }
            NodeRef::Expr(e) => match &body.exprs[e].kind {
                ExprKind::Assign { target, value } => {
                    if let ExprKind::Local { index, .. } = &body.exprs[*target].kind {
                        self.classify_def(body, *index, *value);
                    }
                }
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr: inner,
                } => {
                    if let Some(ie) = inner.as_expr() {
                        self.record_borrow_target(body, ie, borrowed_refs);
                    }
                }
                ExprKind::Call { args, .. } => {
                    for arg in args {
                        if arg.is_mut
                            && let Some(ae) = arg.expr.as_expr()
                        {
                            self.record_borrow_target(body, ae, borrowed_refs);
                        }
                    }
                }
                _ => {}
            },
            NodeRef::Block(_) | NodeRef::Pat(_) => {}
        }
        let mut kids = Vec::new();
        body.for_each_child(node, |c| kids.push(c));
        for c in kids {
            self.build_walk(body, c, borrowed_refs);
        }
    }

    /// Record the storage a `&mut place` (or `mut`-flagged argument) may
    /// alias: a plain local root goes into `borrowed`; a chain through a ref
    /// local defers to that local's resolved roots (`borrowed_refs`).
    fn record_borrow_target(&mut self, body: &Body, e: ExprId, borrowed_refs: &mut IndexSet<u32>) {
        if let WriteRoot::Local(root) = write_root(body, e, false) {
            if self.entries.contains_key(&root) {
                borrowed_refs.insert(root);
            } else {
                self.borrowed.insert(root);
            }
        }
    }

    fn classify_def(&mut self, body: &Body, local: u32, value: Operand) {
        if !self.entries.contains_key(&local) {
            return;
        }
        enum Def {
            Root(u32),
            Copy(u32),
            Unknown,
        }
        let def = match value.as_expr().map(|ve| &body.exprs[ve].kind) {
            Some(ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            }) => match inner.as_expr().map(|ie| write_root(body, ie, false)) {
                Some(WriteRoot::Local(root)) => {
                    if self.entries.contains_key(&root) {
                        Def::Copy(root)
                    } else {
                        Def::Root(root)
                    }
                }
                Some(WriteRoot::Temp | WriteRoot::Aliased) | None => Def::Unknown,
            },
            Some(ExprKind::Local { index, .. }) => Def::Copy(*index),
            Some(_) | None => Def::Unknown,
        };
        let e = self.entries.get_mut(&local).expect("checked above");
        e.saw_def = true;
        match def {
            Def::Root(r) => {
                e.roots.insert(r);
            }
            Def::Copy(r) => {
                e.copies.insert(r);
            }
            Def::Unknown => e.unknown = true,
        }
    }

    /// Invoke `sink` with `root` plus every local the stored `&mut` in `root`
    /// may point into (all of `borrowed` for unknown provenance).
    fn expand(&self, root: u32, sink: &mut impl FnMut(u32)) {
        sink(root);
        if let Some(e) = self.entries.get(&root) {
            for &r in &e.roots {
                sink(r);
            }
            if e.unknown {
                for &b in &self.borrowed {
                    sink(b);
                }
            }
        }
    }
}

/// One mutation of a local root: a whole-value rebind (`x = v`) or a write
/// into storage the root owns or aliases (field / index / payload / deref
/// store, `&mut` escape, mutating callee channel).
#[derive(Debug, Clone, Copy)]
pub(super) enum RootMutation {
    Rebind(u32),
    Through(u32),
}

impl RootMutation {
    pub(super) fn local(self) -> u32 {
        match self {
            RootMutation::Rebind(l) | RootMutation::Through(l) => l,
        }
    }
}

fn is_mut_ref_typed(body: &Body, e: ExprId, type_table: &crate::tir::TypeTable) -> bool {
    matches!(
        type_table.get(body.exprs[e].type_id),
        crate::tir::ResolvedType::MutRef(_)
    )
}

/// Report every local root the node `id` itself may mutate, the caller's walk
/// driving traversal into children. The one shared witness→root dispatch, with a
/// single bodyless-callee fallback. That fallback trusts the call site's declared
/// `mut` bit, since the `&mut`-type test misses a boxed argument: `&mut scalar`
/// arrives `Box`-typed with `is_mut` still set.
pub(super) fn for_each_mutated_root(
    body: &Body,
    id: ExprId,
    type_table: &crate::tir::TypeTable,
    oracle: &super::value_copy::mutation::MutationOracle<'_>,
    aliases: &MutRefAliases,
    sink: &mut impl FnMut(RootMutation),
) {
    use super::value_copy::mutation::{Witness, expr_witnesses};
    let through_storage = |sink: &mut dyn FnMut(RootMutation), e: ExprId| {
        // A rootless chain here is a fresh temporary (e.g. a `Box { … }`
        // literal receiver): mutating it cannot touch a named local.
        if let Some(root) = storage_root(body, e) {
            aliases.expand(root, &mut |r| sink(RootMutation::Through(r)));
        }
    };
    expr_witnesses(body, id, oracle, &mut |w| match w {
        Witness::Rebind(l) => sink(RootMutation::Rebind(l)),
        Witness::Write(inner) => {
            let Some(ie) = inner.as_expr() else {
                return;
            };
            match write_root(body, ie, false) {
                WriteRoot::Local(root) => {
                    aliases.expand(root, &mut |r| sink(RootMutation::Through(r)));
                }
                WriteRoot::Aliased => {
                    for &b in &aliases.borrowed {
                        sink(RootMutation::Through(b));
                    }
                }
                WriteRoot::Temp => {}
            }
        }
        Witness::MutBorrow(e) => through_storage(sink, e),
        Witness::CalleeArg {
            expr,
            verdict,
            is_mut,
        } => {
            if verdict.unwrap_or(is_mut) {
                through_storage(sink, expr);
            }
        }
        // The elaborator guarantees a `&mut self` receiver is `&mut`-typed or
        // an explicit `MutRef` at reification, but the boxing rewrite erases
        // the `&mut`/`&` wrapper distinction for boxed-scalar receivers (see
        // the `mutation.rs` module doc), so a mutating receiver can appear as
        // a shared `Ref` here. `through_storage` sees through either wrapper
        // to the storage root, which is what soundness requires.
        Witness::Receiver { expr, verdict } => {
            if verdict.unwrap_or_else(|| is_mut_ref_typed(body, expr, type_table)) {
                through_storage(sink, expr);
            }
        }
        Witness::IndirectArg(e) => {
            if is_mut_ref_typed(body, e, type_table) {
                through_storage(sink, e);
            }
        }
    });
}

/// Every local possibly mutated anywhere in the subtree at `node` — the
/// consolidation query for pass-local "loop write" / "modifies" scans.
// Canonical implementation ahead of its consumers: `const_folding`'s
// `record_loop_write` and `condition_implication`'s `node_modifies` migrate
// onto it next.
#[allow(dead_code)]
pub(super) fn locals_possibly_mutated(
    body: &Body,
    node: NodeRef,
    type_table: &crate::tir::TypeTable,
    oracle: &super::value_copy::mutation::MutationOracle<'_>,
    aliases: &MutRefAliases,
) -> IndexSet<u32> {
    let mut out = IndexSet::default();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if let NodeRef::Expr(e) = n {
            for_each_mutated_root(body, e, type_table, oracle, aliases, &mut |rm| {
                out.insert(rm.local());
            });
        }
        body.for_each_child(n, |c| stack.push(c));
    }
    out
}

/// Whether WIR will expect a value from `block`'s tail. Walks the parent map,
/// which is bounded by tree depth.
pub(super) fn block_yields_value(engine: &Engine, block: BlockId) -> bool {
    node_yields_value(engine, NodeRef::Block(block))
}

/// Whether `op` is the operand slot `node` sits in.
fn operand_is(op: Operand, node: NodeRef) -> bool {
    matches!((op.as_expr(), node), (Some(e), NodeRef::Expr(n)) if e == n)
}

fn node_yields_value(engine: &Engine, node: NodeRef) -> bool {
    let Some(parent) = engine.parent_of(node) else {
        return false;
    };
    // WIR sizes a value region from the owning expression's own type, so a
    // branch of a non-unit one yields whether or not anything reads it. A
    // unit-typed owner still yields where its own position recovers a value,
    // which is what the walk answers.
    let inherits = |pe| {
        engine.body.exprs[pe].type_id != TypeTable::UNIT
            || node_yields_value(engine, NodeRef::Expr(pe))
    };
    match parent {
        NodeRef::Expr(pe) => match &engine.body.exprs[pe].kind {
            ExprKind::Block(_) | ExprKind::LabeledBlock { .. } => inherits(pe),
            // What a branching construct tests is read whatever the construct
            // yields; only what it selects among inherits its position. A
            // scrutinee read as a branch is what strips an `if` condition of
            // its value and leaves the branch reading nothing.
            ExprKind::If { condition, .. } => operand_is(*condition, node) || inherits(pe),
            ExprKind::Switch { scrutinee, .. } => operand_is(*scrutinee, node) || inherits(pe),
            // A `Match` holds its arm bodies as operands too, so the tested
            // operand is whatever is not one of them — the scrutinee or a guard.
            ExprKind::Match { arms, .. } => {
                !arms.iter().any(|arm| operand_is(arm.body, node)) || inherits(pe)
            }
            _ => true,
        },
        NodeRef::Stmt(ps) => match &engine.body.stmts[ps].kind {
            StmtKind::Let { .. }
            | StmtKind::LetDestructure { .. }
            | StmtKind::Return { value: Some(_) }
            | StmtKind::Break { value: Some(_), .. } => true,
            StmtKind::Expr(_) => node_yields_value(engine, NodeRef::Stmt(ps)),
            StmtKind::If { .. } => match node {
                NodeRef::Expr(_) => true,
                NodeRef::Block(_) => node_yields_value(engine, NodeRef::Stmt(ps)),
                NodeRef::Stmt(_) | NodeRef::Pat(_) => false,
            },
            StmtKind::Loop { .. }
            | StmtKind::LabeledBlock { .. }
            | StmtKind::Break { value: None, .. }
            | StmtKind::Return { value: None }
            | StmtKind::Continue => false,
        },
        NodeRef::Block(pb) => {
            let NodeRef::Stmt(s) = node else {
                return false;
            };
            engine.body.blocks[pb].stmts.last() == Some(&s)
                && node_yields_value(engine, NodeRef::Block(pb))
        }
        NodeRef::Pat(_) => false,
    }
}
