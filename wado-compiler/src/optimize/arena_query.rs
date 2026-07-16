//! Arena-side structural queries shared by the rewrite-engine rules.
//!
//! `is_local`, `expr_mentions_local`, `stmt_mentions_local`, `is_pure_expr`,
//! `collect_reads`, … read the [`Body`] arena directly, so the ported passes
//! need no `Body ↔ tree` bridge.

use crate::hashmap::IndexSet;
use crate::nir::NirUnaryOp;
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatId, PatKind, StmtId, StmtKind,
};

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

/// Whether two places overlap — one is a prefix of the other — so a write to
/// either may change a read of the other (`a.b` overlaps `a`, `a.b`, and
/// `a.b.c`, but not the sibling `a.c`).
pub(super) fn place_overlaps(a: &Place, b: &Place) -> bool {
    is_place_prefix(a, b) || is_place_prefix(b, a)
}

/// The local whose interior storage `expr` reaches, seeing through the
/// projections that share it: field access, indexing, variant payload, a
/// transparent cast, and `&`/`&mut`/`*`. Arithmetic unaries produce fresh
/// scalars and do not descend. The root-only storage query for the escape /
/// aliasing / mutation-witness analyses; distinct from [`place_root_local`]
/// (narrower, paired with the caller's own wrapper dispatch) and the
/// path-sensitive [`place_path`].
///
/// `None` does *not* mean "fresh": `container.index_value(i)` also returns
/// `None` yet aliases the container, so callers pair this with a freshness
/// gate (`EscapeMap::rvalue_is_fresh`) or treat `None` conservatively.
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

/// Collect every local index that is *read* — every `Local` mention except the
/// bare-`Local` target of an `Assign` (a write). `&local` / `&mut local`,
/// `local.field = …`, and every value-position `Local` count as reads. The
/// arena counterpart of `elide_local`'s tree `ReadCollector` /
/// `collect_reads_in_block`.
pub(super) fn collect_reads(body: &Body, out: &mut IndexSet<u32>) {
    collect_reads_node(body, NodeRef::Block(body.root), out);
}

fn collect_reads_node(body: &Body, node: NodeRef, out: &mut IndexSet<u32>) {
    if let NodeRef::Expr(id) = node {
        match &body.exprs[id].kind {
            ExprKind::Local { index, .. } => {
                out.insert(*index);
                return;
            }
            ExprKind::Assign { target, value } => {
                let (target, value) = (*target, *value);
                // The bare-`Local` target is a write, not a read; nested write
                // places (`a.field`, `a[i]`) and the assigned value are reads.
                if !matches!(&body.exprs[target].kind, ExprKind::Local { .. }) {
                    collect_reads_node(body, NodeRef::Expr(target), out);
                }
                if let Some(ve) = value.as_expr() {
                    collect_reads_node(body, NodeRef::Expr(ve), out);
                }
                return;
            }
            _ => {}
        }
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_reads_node(body, c, out);
    }
}

/// Whether `id` is a bare `Local(idx)` reference.
pub(super) fn is_local(body: &Body, id: ExprId, idx: u32) -> bool {
    matches!(&body.exprs[id].kind, ExprKind::Local { index, .. } if *index == idx)
}

/// Whether `op` is a bare `Local(idx)` reference. A promoted constant
/// (`Operand::Value`) is never a local.
pub(super) fn is_local_operand(body: &Body, op: Operand, idx: u32) -> bool {
    op.as_expr().is_some_and(|e| is_local(body, e, idx))
}

/// Whether `idx` appears anywhere in the operand. A promoted constant mentions
/// no local.
pub(super) fn operand_mentions_local(body: &Body, op: Operand, idx: u32) -> bool {
    op.as_expr()
        .is_some_and(|e| expr_mentions_local(body, e, idx))
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

/// True when the expression has no observable effect *and cannot trap*. A trap
/// is an observable effect that must survive, so this — not [`is_pure_expr`] —
/// is the predicate for passes that *delete* an expression (dead-argument
/// elimination, dead-return-value elimination, write-only-local elision):
/// dropping a `100 / x` or `arr[i]` erases a runtime trap the program is
/// entitled to. `is_pure_expr` stays trap-agnostic for reordering/CSE, which
/// keep the expression (its trap still fires). The trap dimension comes from the
/// shared [`ModRef`] oracle so the taxonomy lives in one place.
pub(super) fn is_pure_nontrapping_expr(body: &Body, id: ExprId) -> bool {
    is_pure_expr(body, id) && !super::mod_ref::ModRef::of_expr(body, id).may_trap
}

/// [`is_pure_nontrapping_expr`] for an operand: a promoted constant is pure and
/// cannot trap.
pub(super) fn is_pure_nontrapping_operand(body: &Body, op: Operand) -> bool {
    op.as_expr().is_none_or(|e| is_pure_nontrapping_expr(body, e))
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
// Mutated-root queries
// ---------------------------------------------------------------------------
//
// The canonical "which locals may a subtree mutate" facility, shared so every
// consumer applies one witness taxonomy, one bodyless-callee fallback, and one
// `&mut`-alias resolution. Direct consumers today: `copy_prop` (scope-stability
// scan and usage marking). Consolidation target for the hand-rolled variants in
// `const_folding::record_loop_write` and `condition_implication::node_modifies`.
//
// Receiver-wrapper caveat: at reification a `&mut self` receiver is
// `&mut`-typed or an explicit `Unary(MutRef)` (`elaborator/method_lookup.rs`,
// `adjust_receiver_for_self_kind_static`), but the boxing rewrite erases the
// `&mut`/`&` wrapper distinction for boxed-scalar receivers, so at NIR a
// mutating receiver can arrive as a bare shared `&` borrow. Mutation is
// therefore recognized by the callee's *declared* pre-boxing bit (the oracle
// verdict / the call site's `is_mut` bit), never by the wrapper shape alone;
// with a mutating verdict the attribution sees through either wrapper to the
// storage root. This is also why the bodyless fallback trusts `is_mut` rather
// than the — boxing-erased — `&mut` type.

/// Flow-insensitive `&mut`-alias map: for every `&mut`-typed local, the set of
/// function locals its stored reference may point into.
///
/// Built in one walk + a fixpoint over ref-to-ref copies:
///
/// - `let r = &mut place` / `r = &mut place` contributes the place's root
///   (or, when the place derefs another ref local, that local's own roots).
/// - `let r2 = r` between ref locals copies `r`'s roots.
/// - Every other definition shape (a call returning `&mut`, a ref read back
///   out of an aggregate, a pattern binding, an `if` producing a ref) makes
///   the local's provenance *unknown*: a write through it may hit any local
///   whose `&mut` was ever taken (`borrowed`).
///
/// Parameters are external: a caller cannot hold a `&mut` into this frame's
/// fresh locals, so a mut-ref parameter aliases no function local.
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
                ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. } => {
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

/// Report every local root the expression node `id` itself may mutate (the
/// caller's walk drives traversal into children). The single shared
/// witness→root dispatch: one root resolution (a storage chain, expanded
/// through [`MutRefAliases`]) and one bodyless-callee fallback.
///
/// Bodyless-callee fallback (`verdict: None`): trust the call site's declared
/// `mut` bit for arguments. The `&mut`-type test used for receivers /
/// indirect arguments has false negatives for arguments the lowering boxed
/// (`&mut scalar` arrives `Box`-typed with `is_mut` still set, and the box
/// cell IS the caller-visible storage), so where the declared bit exists it
/// is the more faithful signal; where it does not (receivers, indirect call
/// operands), the `&mut` type is all there is.
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
