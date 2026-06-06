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
//! The algorithm uses a two-pass approach that processes ALL ref bindings
//! simultaneously, avoiding the O(K × N) cost of processing each binding
//! separately (where K = number of bindings, N = body size).
//!
//! Pass 1 (analyze): Single traversal to collect all `let r = &v` bindings
//!   and classify every use of each `r` as field-access-only or not.
//! Pass 2 (transform): Single traversal to replace eliminable field accesses
//!   and remove dead let statements.
//!
//! Ported off the `Body ↔ tree` bridge (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`): the analysis and
//! transform read and mutate the arena `Body` directly. The referent of a
//! binding is stored as the *unresolved* source expression id and resolved
//! lazily during the transform — the refs map is complete by then, so a
//! transitive `let r2 = &r1.field` resolves through `r1` even though `r1`'s
//! `let` is dropped. The deref-only source is single-use, so it is moved into
//! its one `*r` site rather than cloned.

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirFunction, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, ExprNode, NodeRef, StmtId, StmtKind};
use crate::nir_package::NirPackage;
use crate::tir::TypeId;
use crate::token::Span;

/// Per-binding analysis state, keyed by the ref local index.
struct RefInfo {
    /// The *unresolved* source expression `E` from `let r = &E` (or the
    /// inherited referent of a transitive `let r = s` shadow). Resolved
    /// lazily in pass 2.
    referent_e: ExprId,
    /// True until a non-field-access use is found.
    eliminable: bool,
}

pub fn eliminate_unnecessary_refs(project: &mut NirPackage) -> bool {
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= eliminate_refs_in_function(&mut func);
        changed |= eliminate_deref_ref_pairs_in_function(&mut func);
    }
    changed
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
// Pass 2: replace eliminable `r.field` with the referent and drop the let.
// ──────────────────────────────────────────────────────────────────────────────

fn eliminate_refs_in_function(func: &mut NirFunction) -> bool {
    let Some(body) = func.body.as_mut() else {
        return false;
    };
    let rebound = find_rebound_locals(body);
    let mut refs: IndexMap<u32, RefInfo> = IndexMap::default();
    analyze_block(body, body.root, &rebound, &mut refs);

    if !refs.values().any(|i| i.eliminable) {
        return false;
    }

    transform_block(body, body.root, &refs);
    true
}

/// Resolve the unresolved referent `e` into a fresh arena subtree, splicing in
/// any tracked local's referent transitively.
fn resolve(body: &mut Body, e: ExprId, refs: &IndexMap<u32, RefInfo>) -> ExprId {
    enum Action {
        Tracked(ExprId),
        Field(ExprId, u32, String, TypeId, Span),
        Leaf,
    }
    let action = match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => match refs.get(index) {
            Some(info) => Action::Tracked(info.referent_e),
            None => Action::Leaf,
        },
        ExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => Action::Field(
            *inner,
            *field_index,
            field_name.clone(),
            body.exprs[e].type_id,
            body.exprs[e].span,
        ),
        _ => Action::Leaf,
    };
    match action {
        Action::Tracked(re) => resolve(body, re, refs),
        Action::Field(inner, field_index, field_name, type_id, span) => {
            let resolved_inner = resolve(body, inner, refs);
            body.exprs.push(ExprNode {
                kind: ExprKind::FieldAccess {
                    expr: resolved_inner,
                    field_index,
                    field_name,
                },
                type_id,
                span,
            })
        }
        Action::Leaf => {
            let node = body.exprs[e].clone();
            body.exprs.push(node)
        }
    }
}

fn transform_block(body: &mut Body, block: BlockId, refs: &IndexMap<u32, RefInfo>) {
    // Remove dead let statements for eliminable bindings.
    let kept: Vec<StmtId> = body.blocks[block]
        .stmts
        .iter()
        .copied()
        .filter(|s| match &body.stmts[*s].kind {
            StmtKind::Let { local_index, .. } => {
                !refs.get(local_index).is_some_and(|i| i.eliminable)
            }
            _ => true,
        })
        .collect();
    body.blocks[block].stmts = kept;

    let stmts = body.blocks[block].stmts.clone();
    for s in stmts {
        transform_node(body, NodeRef::Stmt(s), refs);
    }
}

fn transform_node(body: &mut Body, node: NodeRef, refs: &IndexMap<u32, RefInfo>) {
    match node {
        NodeRef::Expr(id) => transform_expr(body, id, refs),
        NodeRef::Block(b) => transform_block(body, b, refs),
        NodeRef::Stmt(_) | NodeRef::Pat(_) => {
            let mut kids = Vec::new();
            body.for_each_child(node, |c| kids.push(c));
            for c in kids {
                transform_node(body, c, refs);
            }
        }
    }
}

fn transform_expr(body: &mut Body, id: ExprId, refs: &IndexMap<u32, RefInfo>) {
    if let ExprKind::FieldAccess { expr: inner, .. } = &body.exprs[id].kind {
        let inner = *inner;
        if let ExprKind::Local { index, .. } = &body.exprs[inner].kind {
            let index = *index;
            if let Some(info) = refs.get(&index)
                && info.eliminable
            {
                // Replace `r` (the inner Local) with the resolved referent,
                // keeping `inner`'s type_id / span — the surrounding code was
                // sized to the ref-type tag `r` had at this position.
                let resolved = resolve(body, info.referent_e, refs);
                let kind = body.exprs[resolved].kind.clone();
                body.exprs[inner].kind = kind;
                return;
            }
        }
    }
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
    for c in kids {
        transform_node(body, c, refs);
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

fn eliminate_deref_ref_pairs_in_function(func: &mut NirFunction) -> bool {
    let Some(body) = func.body.as_mut() else {
        return false;
    };

    let mut refs: IndexMap<u32, DerefOnlyRef> = IndexMap::default();
    deref_collect_block(body, body.root, &mut refs);

    // Only eliminate refs still eliminable AND used exactly once via `*r`.
    // Multi-use elision would duplicate the source literal at every site.
    let eliminable: IndexSet<u32> = refs
        .iter()
        .filter(|(_, info)| info.eliminable && info.use_count == 1)
        .map(|(idx, _)| *idx)
        .collect();
    if eliminable.is_empty() {
        return false;
    }
    let sources: IndexMap<u32, ExprId> = refs
        .into_iter()
        .filter(|(idx, _)| eliminable.contains(idx))
        .map(|(idx, info)| (idx, info.source_e))
        .collect();

    deref_transform_block(body, body.root, &sources);
    true
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

fn deref_transform_block(body: &mut Body, block: BlockId, sources: &IndexMap<u32, ExprId>) {
    let kept: Vec<StmtId> = body.blocks[block]
        .stmts
        .iter()
        .copied()
        .filter(|s| match &body.stmts[*s].kind {
            StmtKind::Let { local_index, .. } => !sources.contains_key(local_index),
            _ => true,
        })
        .collect();
    body.blocks[block].stmts = kept;

    let stmts = body.blocks[block].stmts.clone();
    for s in stmts {
        deref_transform_node(body, NodeRef::Stmt(s), sources);
    }
}

fn deref_transform_node(body: &mut Body, node: NodeRef, sources: &IndexMap<u32, ExprId>) {
    match node {
        NodeRef::Expr(id) => deref_transform_expr(body, id, sources),
        NodeRef::Block(b) => deref_transform_block(body, b, sources),
        NodeRef::Stmt(_) | NodeRef::Pat(_) => {
            let mut kids = Vec::new();
            body.for_each_child(node, |c| kids.push(c));
            for c in kids {
                deref_transform_node(body, c, sources);
            }
        }
    }
}

fn deref_transform_expr(body: &mut Body, id: ExprId, sources: &IndexMap<u32, ExprId>) {
    if let ExprKind::Unary {
        op: NirUnaryOp::Deref,
        expr: inner,
    } = &body.exprs[id].kind
    {
        let inner = *inner;
        if let ExprKind::Local { index, .. } = &body.exprs[inner].kind {
            let index = *index;
            if let Some(&source_e) = sources.get(&index) {
                // Single-use (`use_count == 1`): move the source literal into
                // this `*r` site, keeping the deref expr's type_id / span.
                let kind = std::mem::replace(&mut body.exprs[source_e].kind, ExprKind::Unit);
                body.exprs[id].kind = kind;
                return;
            }
        }
    }
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
    for c in kids {
        deref_transform_node(body, c, sources);
    }
}
