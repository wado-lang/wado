//! Reference elimination optimization for Wado NIR.
//!
//! Eliminates unnecessary reference bindings introduced during function inlining.
//! After inlining, we often have patterns like:
//!
//! ```text
//! let self: &List<T> = &arr;
//! ... self.repr ...
//! ```
//!
//! This can be optimized to:
//!
//! ```text
//! ... arr.repr ...
//! ```
//!
//! The pass also handles bindings whose source is a field-access chain
//! (`let r: &T = &v.f1.f2`), substituting the chain at each `r.field` use.
//!
//! Runs as `RefElimRule` inside the unified post-inline peephole session
//! (combine migration; see `docs/wep-2026-06-05-worklist-rewrite-engine.md`).
//! `build_ref_elim` collects all `let r = &v` bindings and classifies every use
//! of each `r` (field-access-only or not), plus the disjoint deref-only set, in
//! one pristine-body analysis per function. The rule then drops eliminable
//! bindings and substitutes uses through the engine edit API. The referent is
//! stored as the *unresolved* source expression id and resolved during the
//! transform — the refs map is complete, so a transitive `let r2 = &r1.field`
//! resolves through `r1` even though `r1`'s `let` is dropped. A deref-only
//! source is single-use, so it is moved into its one `*r` site rather than
//! cloned.

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::NirUnaryOp;
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::tir::TypeId;
use crate::token::Span;

use super::arena_query::{Place, is_place_prefix, place_path};

/// Per-binding analysis state, keyed by the ref local index.
struct RefInfo {
    /// The *unresolved* source expression `E` from `let r = &E` (or the
    /// inherited referent of a transitive `let r = s` shadow). Resolved
    /// lazily during the transform.
    referent_e: ExprId,
    /// True until a non-field-access use is found.
    eliminable: bool,
    /// A `let r = s` shadow inheriting `s`'s referent. Its capture point is `s`'s
    /// binding, not its own, so the positional capture guard falls back to a
    /// whole-body check for it (the safe superset).
    inherited: bool,
}

/// Reference elimination as a rule on the unified post-inline peephole session
/// (combine migration; formerly the standalone `nir/ref_elim` pass). Both maps
/// are built once per function from the pristine post-inline body (`build_ref_elim`)
/// — the two sub-analyses target disjoint binding shapes (`&pure-read-referent`
/// vs `&literal`), so a single pristine snapshot of each matches the old
/// sequential pass. The transforms (drop the binding `Let`, substitute each
/// `r.field` with the resolved referent, inline each single-use `*r` literal)
/// run through the engine edit API so the worklist re-examines the result.
pub(super) struct RefElimRule {
    /// `let r = &E` bindings (eliminable when every use is `r.field`).
    refs: IndexMap<u32, RefInfo>,
    /// `let r = &literal` bindings used exactly once as `*r`; value is the
    /// source literal expression to inline.
    deref_sources: IndexMap<u32, ExprId>,
}

/// Build a [`RefElimRule`] for one function from its pristine body.
pub(super) fn build_ref_elim(body: &Body) -> RefElimRule {
    let rebound = find_rebound_locals(body);
    let mut refs: IndexMap<u32, RefInfo> = IndexMap::default();
    analyze_block(body, body.root, &rebound, &mut refs);

    // Capture-at-binding: a `let r = &<place>` reference captures the object the
    // place denotes at binding time; folding `r.field` back into a re-read of
    // `<place>` is unsound only if that place's handle is *replaced* while the
    // capture is live — in the interval `(binding, last_use]`. A replacement
    // before the binding predates the capture (`b.xs = …; let r = &b.xs`) and one
    // after the last use can't reach it, so both fold soundly; only a replacement
    // between does not. An inherited shadow (`let r = s`) captures at `s`'s
    // binding, not its own, so it falls back to a whole-body check.
    let facts = collect_capture_facts(body, &refs);
    for (&local, info) in &mut refs {
        let Some(referent) = place_path(body, info.referent_e) else {
            continue;
        };
        let replaced = facts.replacements.iter().any(|r| {
            replaces_capture(r, &referent)
                && (info.inherited || invalidates_capture(r, local, &facts))
        });
        if replaced {
            info.eliminable = false;
        }
    }

    let mut deref: IndexMap<u32, DerefOnlyRef> = IndexMap::default();
    deref_collect_block(body, body.root, &mut deref);
    // Only eliminate refs still eliminable AND used exactly once via `*r`;
    // multi-use elision would duplicate the source literal at every site.
    let deref_sources = deref
        .into_iter()
        .filter(|(_, info)| info.eliminable && info.use_count == 1)
        .map(|(idx, info)| (idx, info.source_e))
        .collect();

    RefElimRule {
        refs,
        deref_sources,
    }
}

impl Rule for RefElimRule {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        let stmts = engine.body.blocks[block].stmts.clone();
        let kept: Vec<StmtId> = stmts
            .iter()
            .copied()
            .filter(|s| match &engine.body.stmts[*s].kind {
                StmtKind::Let { local_index, .. } => {
                    let drop = self.refs.get(local_index).is_some_and(|i| i.eliminable)
                        || self.deref_sources.contains_key(local_index);
                    !drop
                }
                _ => true,
            })
            .collect();
        if kept.len() == stmts.len() {
            return false;
        }
        engine.set_block_stmts(block, kept);
        true
    }

    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        match &engine.body.exprs[id].kind {
            // `r.field` for an eliminable ref `r` → resolved referent.
            ExprKind::FieldAccess { expr: inner, .. } => {
                // A promoted `Operand::Value` receiver is no eliminable ref local.
                let Some(inner_e) = inner.as_expr() else {
                    return false;
                };
                let ExprKind::Local { index, .. } = &engine.body.exprs[inner_e].kind else {
                    return false;
                };
                let index = *index;
                if !self.refs.get(&index).is_some_and(|i| i.eliminable) {
                    return false;
                }
                let referent_e = self.refs[&index].referent_e;
                let resolved = resolve_via_engine(engine, referent_e, &self.refs);
                // Replace `r` (the inner Local) with the resolved referent,
                // keeping `inner`'s type_id / span — the surrounding code was
                // sized to the ref-type tag `r` had at this position. `resolved`
                // is a fresh subtree; *move* its kind out (leaving `Dead`) rather
                // than cloning it, so its child exprs are not shared between the
                // orphaned `resolved` node and `inner_e`. A shared child would let
                // a later single-parent rewrite (SROA scalarization, copy-prop
                // redirect) corrupt one parent while leaving the other dangling.
                let kind = std::mem::replace(&mut engine.body.exprs[resolved].kind, ExprKind::Dead);
                engine.replace_expr_kind(inner_e, kind);
                true
            }
            // `*r` for a single-use deref-only ref `r` → inline the literal.
            ExprKind::Unary {
                op: NirUnaryOp::Deref,
                expr: inner,
            } => {
                let inner = *inner;
                // A promoted `Operand::Value` inner names no local — no rewrite.
                let Some(inner_e) = inner.as_expr() else {
                    return false;
                };
                let ExprKind::Local { index, .. } = &engine.body.exprs[inner_e].kind else {
                    return false;
                };
                let Some(&source_e) = self.deref_sources.get(index) else {
                    return false;
                };
                // Single-use: move the source literal into this `*r` site,
                // keeping the deref expr's type_id / span.
                let kind = std::mem::replace(&mut engine.body.exprs[source_e].kind, ExprKind::Dead);
                engine.replace_expr_kind(id, kind);
                true
            }
            _ => false,
        }
    }
}

/// Resolve the unresolved referent `e` into a fresh engine-allocated subtree,
/// splicing in any tracked local's referent transitively. Mirrors the old
/// `resolve`, but builds through the engine so the parent map / use index stay
/// coherent.
fn resolve_via_engine(engine: &mut Engine, e: ExprId, refs: &IndexMap<u32, RefInfo>) -> ExprId {
    enum Step {
        Tracked(ExprId),
        Field(ExprId, u32, String, TypeId, Span),
        Leaf,
    }
    let step = match &engine.body.exprs[e].kind {
        ExprKind::Local { index, .. } => match refs.get(index) {
            Some(info) => Step::Tracked(info.referent_e),
            None => Step::Leaf,
        },
        // A promoted `Operand::Value` receiver has no skeleton place to splice
        // through; clone the field access as-is (`Step::Leaf`).
        ExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } if inner.as_expr().is_some() => Step::Field(
            inner.as_expr().expect("checked some"),
            *field_index,
            field_name.clone(),
            engine.body.exprs[e].type_id,
            engine.body.exprs[e].span,
        ),
        _ => Step::Leaf,
    };
    match step {
        Step::Tracked(re) => resolve_via_engine(engine, re, refs),
        Step::Field(inner, field_index, field_name, type_id, span) => {
            let resolved_inner = resolve_via_engine(engine, inner, refs);
            engine.alloc_expr(
                ExprKind::FieldAccess {
                    expr: resolved_inner.into(),
                    field_index,
                    field_name,
                },
                type_id,
                span,
            )
        }
        Step::Leaf => engine.clone_expr(e),
    }
}

/// A pure inline aggregate (`StructLiteral`) referent: substitutable at each
/// `r.field` use because it has no observable effect and projecting a field off
/// it (`project_struct_literal`) drops the rest. Restricted to `&` (shared)
/// borrows by the caller.
fn is_inline_pure_aggregate(body: &Body, id: ExprId) -> bool {
    matches!(&body.exprs[id].kind, ExprKind::StructLiteral { .. })
        && super::arena_query::is_pure_expr(body, id)
}

/// An expression is a valid referent if it's a pure read of a local — either
/// a bare `Local` or a chain of `FieldAccess` bottoming out at one.
fn is_valid_referent(body: &Body, id: ExprId) -> bool {
    match &body.exprs[id].kind {
        ExprKind::Local { .. } => true,
        // A promoted `Operand::Value` receiver is not a pure local-read chain.
        ExprKind::FieldAccess { expr: inner, .. } => inner
            .as_expr()
            .is_some_and(|ie| is_valid_referent(body, ie)),
        _ => false,
    }
}

/// Local indices bound by more than one `Let`. The inliner may reuse an index
/// when expanding mutually-exclusive branches; a second `Let` would otherwise
/// overwrite the first binding's referent, so such locals are skipped.
fn find_rebound_locals(body: &Body) -> IndexSet<u32> {
    let mut seen: IndexSet<u32> = IndexSet::default();
    let mut rebound: IndexSet<u32> = IndexSet::default();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node
            && let StmtKind::Let { local_index, .. } = &body.stmts[s].kind
            && !seen.insert(*local_index)
        {
            rebound.insert(*local_index);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    rebound
}

/// A place that may replace a captured handle, tagged with the pre-order
/// statement position it occurs at (for the capture-interval check) and whether
/// it reaches the handle directly (`Assign`, replaces `q ⊑ referent`) or through
/// a `&mut` alias (reassigns only a *strictly* enclosed field, `q ⊏ referent`).
struct Replacement {
    place: Place,
    pos: usize,
    /// `true` for a `&mut` borrow (strict-prefix), `false` for a direct `Assign`.
    via_borrow: bool,
}

/// Per-body positional facts for the capture-at-binding guard: each tracked ref's
/// binding position and last-use position, plus every handle replacement. Ref
/// captures are snapshots, so a replacement invalidates a fold only when it falls
/// in the open-closed interval `(binding, last_use]`; a replacement before the
/// binding predates the capture and one after the last use can't reach it.
struct CaptureFacts {
    binding_pos: IndexMap<u32, usize>,
    last_use: IndexMap<u32, usize>,
    replacements: Vec<Replacement>,
    /// Position range `[entry, exit]` of every `Loop` body (inclusive). Pre-order
    /// numbering makes a loop's statements a contiguous range, so a position `p`
    /// is inside the loop iff `entry <= p <= exit`. Used to extend a captured
    /// ref's live region across a loop's back-edge: a use inside a loop is
    /// re-evaluated every iteration, so a write later in the loop body (in
    /// pre-order) still precedes that use on the next iteration.
    loops: Vec<(usize, usize)>,
}

/// Walk the body in pre-order, numbering statements, and collect the
/// [`CaptureFacts`]. An in-place mutation (method call, index / element write)
/// leaves the handle and is recorded in neither replacement form; a borrow or
/// write of a place deeper than a referent is never a prefix and never counts.
fn collect_capture_facts(body: &Body, refs: &IndexMap<u32, RefInfo>) -> CaptureFacts {
    let mut facts = CaptureFacts {
        binding_pos: IndexMap::default(),
        last_use: IndexMap::default(),
        replacements: Vec::new(),
        loops: Vec::new(),
    };
    let mut pos = 0;
    capture_walk(
        body,
        NodeRef::Block(body.root),
        0,
        &mut pos,
        refs,
        &mut facts,
    );
    facts
}

fn capture_walk(
    body: &Body,
    node: NodeRef,
    enclosing: usize,
    pos: &mut usize,
    refs: &IndexMap<u32, RefInfo>,
    facts: &mut CaptureFacts,
) {
    let mut loop_start: Option<usize> = None;
    let here = match node {
        NodeRef::Stmt(s) => {
            let p = *pos;
            *pos += 1;
            match &body.stmts[s].kind {
                StmtKind::Let { local_index, .. } if refs.contains_key(local_index) => {
                    facts.binding_pos.insert(*local_index, p);
                }
                StmtKind::Loop { .. } => loop_start = Some(p),
                _ => {}
            }
            p
        }
        _ => enclosing,
    };
    if let NodeRef::Expr(id) = node {
        match &body.exprs[id].kind {
            ExprKind::Assign { target, .. } => {
                if let Some(place) = place_path(body, *target) {
                    facts.replacements.push(Replacement {
                        place,
                        pos: here,
                        via_borrow: false,
                    });
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => {
                if let Some(place) = inner.as_expr().and_then(|e| place_path(body, e)) {
                    facts.replacements.push(Replacement {
                        place,
                        pos: here,
                        via_borrow: true,
                    });
                }
            }
            // A use of a tracked ref local (its `let` is not a use — the value is
            // `&E`, never the ref itself). Extends the ref's capture interval.
            ExprKind::Local { index, .. } if refs.contains_key(index) => {
                let slot = facts.last_use.entry(*index).or_insert(here);
                *slot = (*slot).max(here);
            }
            _ => {}
        }
    }
    body.for_each_child(node, |c| capture_walk(body, c, here, pos, refs, facts));
    if let Some(start) = loop_start {
        facts.loops.push((start, pos.saturating_sub(1)));
    }
}

/// Whether `replacement` replaces the handle observed through a reference to
/// `referent`: an `Assign` to the referent or an enclosing field (`q ⊑ referent`),
/// or a `&mut` alias of a *strictly* enclosing field through which a later
/// `*m = …` / `m.f = …` can reassign it (`q ⊏ referent`). `&mut referent` itself
/// is in-place mutation (`arr[i] = …`), which the shared handle folds soundly.
fn replaces_capture(r: &Replacement, referent: &Place) -> bool {
    if r.via_borrow {
        &r.place != referent && is_place_prefix(&r.place, referent)
    } else {
        is_place_prefix(&r.place, referent)
    }
}

/// A captured ref's live region ends at its last use, extended across every
/// enclosing loop whose entry follows the binding: a use inside such a loop is
/// re-evaluated each iteration, so the ref is effectively live through the whole
/// loop body (a later-in-pre-order write reaches the use on the back-edge). A
/// loop that also contains the binding is *not* extended — the binding re-runs
/// each iteration, so a fresh capture observes any earlier write.
fn extended_live_end(binding: usize, last_use: usize, loops: &[(usize, usize)]) -> usize {
    let mut end = last_use;
    for &(entry, exit) in loops {
        if entry <= last_use && last_use <= exit && binding < entry {
            end = end.max(exit);
        }
    }
    end
}

/// Whether replacement `r` invalidates ref `local`'s capture (making the
/// `r.field → re-read referent` fold unsound). An unused ref has no live region
/// and cannot be invalidated; a missing binding position is treated
/// conservatively as bound at the body entry.
///
/// - A direct `Assign` write invalidates only when it falls strictly after the
///   binding and within the (loop-extended) live region: `binding < pos <= end`.
/// - A `&mut` alias (`via_borrow`) invalidates whenever it is created at or
///   before the end of the live region (`pos <= end`), regardless of the
///   binding: the alias stays live past its creation and can write through the
///   referent at any later point, including one the creation position predates.
fn invalidates_capture(r: &Replacement, local: u32, facts: &CaptureFacts) -> bool {
    let Some(&last_use) = facts.last_use.get(&local) else {
        return false;
    };
    let binding = facts.binding_pos.get(&local).copied().unwrap_or(0);
    let end = extended_live_end(binding, last_use, &facts.loops);
    if r.via_borrow {
        r.pos <= end
    } else {
        binding < r.pos && r.pos <= end
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Pass 1: collect ref bindings + classify every use of each binding.
// ──────────────────────────────────────────────────────────────────────────────

fn analyze_block(
    body: &Body,
    block: BlockId,
    rebound: &IndexSet<u32>,
    refs: &mut IndexMap<u32, RefInfo>,
) {
    for s in &body.blocks[block].stmts {
        analyze_stmt(body, *s, rebound, refs);
    }
}

fn analyze_stmt(
    body: &Body,
    stmt: StmtId,
    rebound: &IndexSet<u32>,
    refs: &mut IndexMap<u32, RefInfo>,
) {
    if let StmtKind::Let {
        local_index, value, ..
    } = &body.stmts[stmt].kind
        && let Some(ve) = value.as_expr()
    {
        register_let_binding(body, *local_index, ve, rebound, refs);
    }
    // Then classify uses in the statement's children (the value, nested blocks).
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Stmt(stmt), |c| kids.push(c));
    for c in kids {
        analyze_node(body, c, rebound, refs);
    }
}

fn register_let_binding(
    body: &Body,
    local_index: u32,
    value: ExprId,
    rebound: &IndexSet<u32>,
    refs: &mut IndexMap<u32, RefInfo>,
) {
    if rebound.contains(&local_index) {
        return;
    }
    // Pattern (1): `let r = &E` / `let r = &mut E` with E a pure-read referent,
    // or `let r = &E` (shared only) with E a pure inline aggregate (e.g. the
    // inlined `len(&self)` binds `self = &String { … }`). Substituting a pure
    // aggregate at each `r.field` use lets `project_struct_literal` fold it; a
    // `&mut` to a temporary aggregate is excluded since separate substituted
    // copies would not share a write.
    if let ExprKind::Unary { op, expr } = &body.exprs[value].kind
        && matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
        && let Some(referent_e) = expr.as_expr()
        && (is_valid_referent(body, referent_e)
            || (matches!(op, NirUnaryOp::Ref) && is_inline_pure_aggregate(body, referent_e)))
    {
        refs.insert(
            local_index,
            RefInfo {
                referent_e,
                eliminable: true,
                inherited: false,
            },
        );
        return;
    }
    // Pattern (2): `let r = s` where s is itself a tracked ref local.
    if let ExprKind::Local { index, .. } = &body.exprs[value].kind
        && let Some(info) = refs.get(index)
    {
        let inherited = RefInfo {
            referent_e: info.referent_e,
            eliminable: info.eliminable,
            inherited: true,
        };
        refs.insert(local_index, inherited);
    }
}

fn analyze_node(
    body: &Body,
    node: NodeRef,
    rebound: &IndexSet<u32>,
    refs: &mut IndexMap<u32, RefInfo>,
) {
    match node {
        NodeRef::Expr(id) => analyze_expr(body, id, rebound, refs),
        NodeRef::Block(b) => analyze_block(body, b, rebound, refs),
        NodeRef::Stmt(s) => analyze_stmt(body, s, rebound, refs),
        NodeRef::Pat(_) => {
            // Patterns may carry a `ConstantValue` sub-expression; classify it.
            let mut kids = Vec::new();
            body.for_each_child(node, |c| kids.push(c));
            for c in kids {
                analyze_node(body, c, rebound, refs);
            }
        }
    }
}

fn analyze_expr_operand(
    body: &Body,
    op: Operand,
    rebound: &IndexSet<u32>,
    refs: &mut IndexMap<u32, RefInfo>,
) {
    if let Some(e) = op.as_expr() {
        analyze_expr(body, e, rebound, refs);
    }
}

fn analyze_expr(
    body: &Body,
    id: ExprId,
    rebound: &IndexSet<u32>,
    refs: &mut IndexMap<u32, RefInfo>,
) {
    match &body.exprs[id].kind {
        // Field access on a tracked ref local: the pattern we optimize — an
        // acceptable use, so do not mark non-eliminable. Recurse into a
        // non-Local inner so nested ref uses are still classified.
        ExprKind::FieldAccess { expr: inner, .. } => {
            let inner = *inner;
            // A promoted `Operand::Value` inner is a constant, not a tracked ref.
            if let Some(ie) = inner.as_expr()
                && let ExprKind::Local { index, .. } = &body.exprs[ie].kind
                && refs.contains_key(index)
            {
                return;
            }
            analyze_expr_operand(body, inner, rebound, refs);
        }
        // Direct (non-field-access) use of a tracked ref local: non-eliminable.
        ExprKind::Local { index, .. } => {
            let index = *index;
            if let Some(info) = refs.get_mut(&index) {
                info.eliminable = false;
            }
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
            for c in kids {
                analyze_node(body, c, rebound, refs);
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Deref-only elision: `let r = &StructLit; ... *r ...` → inline the literal.
// ──────────────────────────────────────────────────────────────────────────────

/// Tracking state for `let r = &struct_or_tuple_literal` where r is only used
/// as `*r`.
struct DerefOnlyRef {
    /// The arena id of the source struct / tuple literal from `let r = &source`.
    source_e: ExprId,
    /// True until a non-deref use is found.
    eliminable: bool,
    /// Number of times r is used as `*r`.
    use_count: u32,
}

fn deref_collect_block(body: &Body, block: BlockId, refs: &mut IndexMap<u32, DerefOnlyRef>) {
    for s in &body.blocks[block].stmts {
        deref_collect_stmt(body, *s, refs);
    }
}

fn deref_collect_stmt(body: &Body, stmt: StmtId, refs: &mut IndexMap<u32, DerefOnlyRef>) {
    if let StmtKind::Let {
        local_index, value, ..
    } = &body.stmts[stmt].kind
        && let Some(ve) = value.as_expr()
        && let ExprKind::Unary { op, expr } = &body.exprs[ve].kind
        && matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
        && let Some(src_e) = expr.as_expr()
        && matches!(
            body.exprs[src_e].kind,
            ExprKind::StructLiteral { .. } | ExprKind::TupleLiteral { .. }
        )
    {
        refs.insert(
            *local_index,
            DerefOnlyRef {
                source_e: src_e,
                eliminable: true,
                use_count: 0,
            },
        );
    }
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Stmt(stmt), |c| kids.push(c));
    for c in kids {
        deref_collect_node(body, c, refs);
    }
}

fn deref_collect_node(body: &Body, node: NodeRef, refs: &mut IndexMap<u32, DerefOnlyRef>) {
    match node {
        NodeRef::Expr(id) => deref_collect_expr(body, id, refs),
        NodeRef::Block(b) => deref_collect_block(body, b, refs),
        NodeRef::Stmt(s) => deref_collect_stmt(body, s, refs),
        NodeRef::Pat(_) => {
            let mut kids = Vec::new();
            body.for_each_child(node, |c| kids.push(c));
            for c in kids {
                deref_collect_node(body, c, refs);
            }
        }
    }
}

fn deref_collect_expr_operand(body: &Body, op: Operand, refs: &mut IndexMap<u32, DerefOnlyRef>) {
    if let Some(e) = op.as_expr() {
        deref_collect_expr(body, e, refs);
    }
}

fn deref_collect_expr(body: &Body, id: ExprId, refs: &mut IndexMap<u32, DerefOnlyRef>) {
    match &body.exprs[id].kind {
        // `*r` where r is a deref-only candidate: an acceptable use.
        ExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: inner,
        } => {
            let inner = *inner;
            // A promoted `Operand::Value` inner names no local candidate.
            if let Some(ie) = inner.as_expr()
                && let ExprKind::Local { index, .. } = &body.exprs[ie].kind
            {
                let index = *index;
                if let Some(info) = refs.get_mut(&index) {
                    info.use_count += 1;
                    return;
                }
            }
            deref_collect_expr_operand(body, inner, refs);
        }
        // Any other bare use of r disqualifies it.
        ExprKind::Local { index, .. } => {
            let index = *index;
            if let Some(info) = refs.get_mut(&index) {
                info.eliminable = false;
            }
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
            for c in kids {
                deref_collect_node(body, c, refs);
            }
        }
    }
}
