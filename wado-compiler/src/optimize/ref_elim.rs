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
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::tir::TypeId;
use crate::token::Span;

/// Per-binding analysis state, keyed by the ref local index.
struct RefInfo {
    /// The *unresolved* source expression `E` from `let r = &E` (or the
    /// inherited referent of a transitive `let r = s` shadow). Resolved
    /// lazily during the transform.
    referent_e: ExprId,
    /// True until a non-field-access use is found.
    eliminable: bool,
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
                let inner = *inner;
                let ExprKind::Local { index, .. } = &engine.body.exprs[inner].kind else {
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
                // sized to the ref-type tag `r` had at this position.
                let kind = engine.body.exprs[resolved].kind.clone();
                engine.replace_expr_kind(inner, kind);
                true
            }
            // `*r` for a single-use deref-only ref `r` → inline the literal.
            ExprKind::Unary {
                op: NirUnaryOp::Deref,
                expr: inner,
            } => {
                let inner = *inner;
                let ExprKind::Local { index, .. } = &engine.body.exprs[inner].kind else {
                    return false;
                };
                let Some(&source_e) = self.deref_sources.get(index) else {
                    return false;
                };
                // Single-use: move the source literal into this `*r` site,
                // keeping the deref expr's type_id / span.
                let kind = std::mem::replace(&mut engine.body.exprs[source_e].kind, ExprKind::Unit);
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
        ExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => Step::Field(
            *inner,
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
                    expr: resolved_inner,
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

/// An expression is a valid referent if it's a pure read of a local — either
/// a bare `Local` or a chain of `FieldAccess` bottoming out at one.
fn is_valid_referent(body: &Body, id: ExprId) -> bool {
    match &body.exprs[id].kind {
        ExprKind::Local { .. } => true,
        ExprKind::FieldAccess { expr: inner, .. } => is_valid_referent(body, *inner),
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
    {
        register_let_binding(body, *local_index, *value, rebound, refs);
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
    // Pattern (1): `let r = &E` / `let r = &mut E` with E a pure-read referent.
    if let ExprKind::Unary { op, expr } = &body.exprs[value].kind
        && matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
        && is_valid_referent(body, *expr)
    {
        let referent_e = *expr;
        refs.insert(
            local_index,
            RefInfo {
                referent_e,
                eliminable: true,
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
            if let ExprKind::Local { index, .. } = &body.exprs[inner].kind
                && refs.contains_key(index)
            {
                return;
            }
            analyze_expr(body, inner, rebound, refs);
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
        && let ExprKind::Unary { op, expr } = &body.exprs[*value].kind
        && matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
        && matches!(
            body.exprs[*expr].kind,
            ExprKind::StructLiteral { .. } | ExprKind::TupleLiteral { .. }
        )
    {
        refs.insert(
            *local_index,
            DerefOnlyRef {
                source_e: *expr,
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

fn deref_collect_expr(body: &Body, id: ExprId, refs: &mut IndexMap<u32, DerefOnlyRef>) {
    match &body.exprs[id].kind {
        // `*r` where r is a deref-only candidate: an acceptable use.
        ExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: inner,
        } => {
            let inner = *inner;
            if let ExprKind::Local { index, .. } = &body.exprs[inner].kind {
                let index = *index;
                if let Some(info) = refs.get_mut(&index) {
                    info.use_count += 1;
                    return;
                }
            }
            deref_collect_expr(body, inner, refs);
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
