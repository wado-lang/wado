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
//! This is what `inline` produces for `&self.tokens.len()`-style calls after
//! the body's `self.used` is rewritten in terms of the inlined receiver.
//!
//! The algorithm uses a two-pass approach that processes ALL ref bindings
//! simultaneously, avoiding the O(K × N) cost of processing each binding
//! separately (where K = number of bindings, N = body size).
//!
//! Pass 1 (analyze): Single traversal to collect all `let r = &v` bindings
//!   and classify every use of each `r` as field-access-only or not.
//! Pass 2 (transform): Single traversal to replace eliminable field accesses
//!   and remove dead let statements.

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirFunction, NirStmt, NirStmtKind, NirUnaryOp};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{NirOptVisitor, NirRefVisitor, opt_walk_expr, opt_walk_stmt};

/// Per-binding analysis state, keyed by the ref local index.
struct RefInfo {
    /// The expression `E` from `let r = &E` (or the resolved referent of a
    /// transitive `let r = s` shadow). Must be a chain of `FieldAccess`
    /// bottoming out at a `Local`, so it's safe to clone at each use site.
    referent: NirExpr,
    /// True until a non-field-access use is found
    eliminable: bool,
}

/// An expression is a valid referent if it's a pure read of a local — either
/// a bare `Local` or a chain of `FieldAccess` bottoming out at one. Restricting
/// to this shape keeps substitution cheap (duplicates only struct.get) and
/// observably equivalent (no side effects, no method calls).
fn is_valid_referent(expr: &NirExpr) -> bool {
    match &expr.kind {
        NirExprKind::Local { .. } => true,
        NirExprKind::FieldAccess { expr: inner, .. } => is_valid_referent(inner),
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. }
        | NirExprKind::VariantConstruct { .. }
        | NirExprKind::VariantTag { .. }
        | NirExprKind::VariantTest { .. }
        | NirExprKind::VariantPayload { .. }
        | NirExprKind::Binary { .. }
        | NirExprKind::Unary { .. }
        | NirExprKind::Cast { .. }
        | NirExprKind::Assign { .. }
        | NirExprKind::Index { .. }
        | NirExprKind::Call { .. }
        | NirExprKind::CmRawCall { .. }
        | NirExprKind::MethodCall { .. }
        | NirExprKind::IndirectCall { .. }
        | NirExprKind::ClosureToCanonical { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::GlobalVarSet { .. }
        | NirExprKind::StructLiteral { .. }
        | NirExprKind::TupleLiteral { .. }
        | NirExprKind::ArrayLiteral { .. }
        | NirExprKind::Block(_)
        | NirExprKind::LabeledBlock { .. }
        | NirExprKind::If { .. }
        | NirExprKind::Match { .. }
        | NirExprKind::Switch { .. } => false,
    }
}

/// Resolve any `Local(idx)` in `expr` whose binding is already tracked, by
/// splicing in the tracked binding's referent. Eliminable bindings are dropped
/// in pass 2; without this resolution, a chained pattern like
/// `let r1 = &v; let r2 = &r1.field; ... r2.x ...` would substitute `r2.x`
/// to `(Local(r1)).field.x` only to find `r1`'s `let` removed underneath it.
/// Pre-resolving at registration time keeps Pass 2 a single substitution.
fn resolve_referent(expr: &NirExpr, refs: &IndexMap<u32, RefInfo>) -> NirExpr {
    match &expr.kind {
        NirExprKind::Local { index, .. } => {
            if let Some(info) = refs.get(index) {
                info.referent.clone()
            } else {
                expr.clone()
            }
        }
        NirExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => {
            let resolved_inner = resolve_referent(inner, refs);
            NirExpr {
                kind: NirExprKind::FieldAccess {
                    expr: Box::new(resolved_inner),
                    field_index: *field_index,
                    field_name: field_name.clone(),
                },
                type_id: expr.type_id,
                span: expr.span,
            }
        }
        _ => expr.clone(),
    }
}

/// Walk the function body and return the set of local indices that are
/// bound by more than one `Let`. `analyze_refs_in_block` skips Pattern 1/2
/// registration for those locals — the inliner may reuse an index when
/// expanding mutually-exclusive branches, and the second `Let` would
/// otherwise overwrite the first binding's referent (see
/// `eliminate_refs_in_function`).
fn find_rebound_locals(block: &NirBlock) -> IndexSet<u32> {
    let mut collector = RebindCollector {
        seen: IndexSet::default(),
        rebound: IndexSet::default(),
    };
    collector.visit_block(block);
    collector.rebound
}

struct RebindCollector {
    seen: IndexSet<u32>,
    rebound: IndexSet<u32>,
}

impl NirRefVisitor for RebindCollector {
    fn visit_stmt(&mut self, stmt: &NirStmt) {
        if let NirStmtKind::Let { local_index, .. } = &stmt.kind
            && !self.seen.insert(*local_index)
        {
            self.rebound.insert(*local_index);
        }
        self.walk_stmt(stmt);
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Pass 1: collect ref bindings + classify every use of each binding.
// ──────────────────────────────────────────────────────────────────────────────

/// Collect all `let r = &v` / `let r = &v.f` / inlined shadow `let r = s`
/// bindings reachable from `block`, and classify every use of each tracked
/// local as field-access-only (eliminable) or other (non-eliminable).
///
/// The walk threads `refs` through the visitor as a single mutable map,
/// which gives both registration (Let stmts) and use-classification (every
/// other expression) a stable view of which locals are tracked at the
/// statement they're inspecting.
fn analyze_refs_in_block(
    block: &NirBlock,
    rebound: &IndexSet<u32>,
    refs: &mut IndexMap<u32, RefInfo>,
) {
    let mut analyzer = RefAnalyzer { rebound, refs };
    analyzer.visit_block(block);
}

struct RefAnalyzer<'a> {
    rebound: &'a IndexSet<u32>,
    refs: &'a mut IndexMap<u32, RefInfo>,
}

impl RefAnalyzer<'_> {
    fn register_let_binding(&mut self, local_index: u32, value: &NirExpr) {
        if self.rebound.contains(&local_index) {
            return;
        }
        // Pattern (1): `let r = &E` / `let r = &mut E` where E is a
        // pure-read referent (Local or FieldAccess chain). Resolve any
        // tracked Locals in `E` to their referents up front so chained
        // shadows survive Pass 2's drop of intermediate bindings.
        if let NirExprKind::Unary { op, expr } = &value.kind
            && matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
            && is_valid_referent(expr)
        {
            self.refs.insert(
                local_index,
                RefInfo {
                    referent: resolve_referent(expr, self.refs),
                    eliminable: true,
                },
            );
            return;
        }
        // Pattern (2): `let r = s` where s is itself a tracked ref local
        // (the inlined shadow). Resolve transitively to s's referent so
        // `r.field` can be replaced with `<root>.field` directly.
        if let NirExprKind::Local { index, .. } = &value.kind
            && let Some(info) = self.refs.get(index)
        {
            let info = RefInfo {
                referent: info.referent.clone(),
                eliminable: info.eliminable,
            };
            self.refs.insert(local_index, info);
        }
    }
}

impl NirRefVisitor for RefAnalyzer<'_> {
    fn visit_stmt(&mut self, stmt: &NirStmt) {
        if let NirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
        {
            self.register_let_binding(*local_index, value);
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            // Field access on a tracked ref local: this is the pattern
            // we want to optimize. The use is acceptable so we *do not*
            // mark it as non-eliminable, but we still need to recurse
            // into any non-Local inner expressions so nested ref uses
            // are still classified.
            NirExprKind::FieldAccess { expr: inner, .. } => {
                if let NirExprKind::Local { index, .. } = &inner.kind
                    && self.refs.contains_key(index)
                {
                    return;
                }
                self.visit_expr(inner);
            }
            // Direct use of a tracked ref local (not through field
            // access): non-eliminable.
            NirExprKind::Local { index, .. } => {
                if let Some(info) = self.refs.get_mut(index) {
                    info.eliminable = false;
                }
            }
            NirExprKind::Binary { .. }
            | NirExprKind::Unary { .. }
            | NirExprKind::Cast { .. }
            | NirExprKind::Assign { .. }
            | NirExprKind::Index { .. }
            | NirExprKind::Call { .. }
            | NirExprKind::CmRawCall { .. }
            | NirExprKind::MethodCall { .. }
            | NirExprKind::IndirectCall { .. }
            | NirExprKind::ClosureToCanonical { .. }
            | NirExprKind::Block(_)
            | NirExprKind::LabeledBlock { .. }
            | NirExprKind::If { .. }
            | NirExprKind::Match { .. }
            | NirExprKind::Switch { .. }
            | NirExprKind::StructLiteral { .. }
            | NirExprKind::TupleLiteral { .. }
            | NirExprKind::ArrayLiteral { .. }
            | NirExprKind::VariantConstruct { .. }
            | NirExprKind::VariantTag { .. }
            | NirExprKind::VariantTest { .. }
            | NirExprKind::VariantPayload { .. }
            | NirExprKind::GlobalVarGet { .. }
            | NirExprKind::GlobalVarSet { .. }
            | NirExprKind::IntLiteral { .. }
            | NirExprKind::FloatLiteral { .. }
            | NirExprKind::BoolLiteral(_)
            | NirExprKind::CharLiteral(_)
            | NirExprKind::StringLiteral(_)
            | NirExprKind::BytesLiteral(_)
            | NirExprKind::Null
            | NirExprKind::Unit
            | NirExprKind::EnumConstruct { .. } => self.walk_expr(expr),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Pass 2: replace eliminable `r.field` with the referent and drop the let.
// ──────────────────────────────────────────────────────────────────────────────

fn transform_block(block: &mut NirBlock, eliminable: &IndexMap<u32, RefInfo>) {
    // Remove dead let statements for eliminable bindings.
    block.stmts.retain(|stmt| {
        if let NirStmtKind::Let { local_index, .. } = &stmt.kind {
            !eliminable.contains_key(local_index)
        } else {
            true
        }
    });

    let mut transformer = RefTransformer { eliminable };
    for stmt in &mut block.stmts {
        transformer.visit_stmt(stmt);
    }
}

struct RefTransformer<'a> {
    eliminable: &'a IndexMap<u32, RefInfo>,
}

impl NirOptVisitor for RefTransformer<'_> {
    fn visit_expr(&mut self, expr: &mut NirExpr) -> bool {
        if let NirExprKind::FieldAccess { expr: inner, .. } = &mut expr.kind
            && let NirExprKind::Local { index, .. } = &inner.kind
            && let Some(info) = self.eliminable.get(index)
        {
            // Replace `r` with the referent expression. For `let r = &v`
            // the referent is `Local(v)`; for `let r = &v.f` it's
            // `FieldAccess(Local(v), "f")`. We swap only the `kind` and
            // keep `inner.type_id`/`inner.span` — the surrounding code
            // (the outer `FieldAccess.field_index` we're inside, plus
            // any downstream consumers) was sized to the ref-type tag
            // that `r` had at this position, and changing it here would
            // ripple incorrect types into codegen.
            inner.kind = info.referent.clone().kind;
            return true;
        }
        opt_walk_expr(self, expr)
    }

    fn visit_block(&mut self, block: &mut NirBlock) -> bool {
        let before = block.stmts.len();
        block.stmts.retain(|stmt| {
            if let NirStmtKind::Let { local_index, .. } = &stmt.kind {
                !self.eliminable.contains_key(local_index)
            } else {
                true
            }
        });
        let mut changed = block.stmts.len() < before;
        for stmt in &mut block.stmts {
            changed |= self.visit_stmt(stmt);
        }
        changed
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Deref-only elision: `let r = &StructLit; ... *r ...` → inline the literal.
// ──────────────────────────────────────────────────────────────────────────────

/// Tracking state for `let r = &struct_or_tuple_literal` where r is only
/// used as `*r`.
struct DerefOnlyRef {
    /// The source expression (struct or tuple literal) from
    /// `let r = &source_expr`.
    source_expr: NirExpr,
    /// True until a non-deref use is found.
    eliminable: bool,
    /// Number of times r is used as `*r`.
    use_count: u32,
}

fn collect_deref_only_refs_in_block(block: &NirBlock, refs: &mut IndexMap<u32, DerefOnlyRef>) {
    let mut collector = DerefOnlyCollector { refs };
    collector.visit_block(block);
}

struct DerefOnlyCollector<'a> {
    refs: &'a mut IndexMap<u32, DerefOnlyRef>,
}

impl NirRefVisitor for DerefOnlyCollector<'_> {
    fn visit_stmt(&mut self, stmt: &NirStmt) {
        if let NirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && let NirExprKind::Unary { op, expr } = &value.kind
            && matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
            && matches!(
                expr.kind,
                NirExprKind::StructLiteral { .. } | NirExprKind::TupleLiteral { .. }
            )
        {
            self.refs.insert(
                *local_index,
                DerefOnlyRef {
                    source_expr: *expr.clone(),
                    eliminable: true,
                    use_count: 0,
                },
            );
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            // `*r` where r is a deref-only candidate: acceptable use.
            NirExprKind::Unary {
                op: NirUnaryOp::Deref,
                expr: inner,
            } => {
                if let NirExprKind::Local { index, .. } = &inner.kind
                    && let Some(info) = self.refs.get_mut(index)
                {
                    info.use_count += 1;
                    return;
                }
                self.visit_expr(inner);
            }
            // Any other bare use of r (field access on the ref, passed
            // as call argument, returned, ...) disqualifies.
            NirExprKind::Local { index, .. } => {
                if let Some(info) = self.refs.get_mut(index) {
                    info.eliminable = false;
                }
            }
            NirExprKind::Unary { .. }
            | NirExprKind::Binary { .. }
            | NirExprKind::Cast { .. }
            | NirExprKind::Assign { .. }
            | NirExprKind::Index { .. }
            | NirExprKind::Call { .. }
            | NirExprKind::CmRawCall { .. }
            | NirExprKind::MethodCall { .. }
            | NirExprKind::IndirectCall { .. }
            | NirExprKind::ClosureToCanonical { .. }
            | NirExprKind::Block(_)
            | NirExprKind::LabeledBlock { .. }
            | NirExprKind::If { .. }
            | NirExprKind::Match { .. }
            | NirExprKind::Switch { .. }
            | NirExprKind::StructLiteral { .. }
            | NirExprKind::TupleLiteral { .. }
            | NirExprKind::ArrayLiteral { .. }
            | NirExprKind::VariantConstruct { .. }
            | NirExprKind::VariantTag { .. }
            | NirExprKind::VariantTest { .. }
            | NirExprKind::VariantPayload { .. }
            | NirExprKind::FieldAccess { .. }
            | NirExprKind::GlobalVarGet { .. }
            | NirExprKind::GlobalVarSet { .. }
            | NirExprKind::IntLiteral { .. }
            | NirExprKind::FloatLiteral { .. }
            | NirExprKind::BoolLiteral(_)
            | NirExprKind::CharLiteral(_)
            | NirExprKind::StringLiteral(_)
            | NirExprKind::BytesLiteral(_)
            | NirExprKind::Null
            | NirExprKind::Unit
            | NirExprKind::EnumConstruct { .. } => self.walk_expr(expr),
        }
    }
}

struct DerefOnlyRewriter<'a> {
    eliminable: &'a IndexMap<u32, NirExpr>,
}

impl NirOptVisitor for DerefOnlyRewriter<'_> {
    fn visit_expr(&mut self, expr: &mut NirExpr) -> bool {
        if let NirExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: inner,
        } = &expr.kind
            && let NirExprKind::Local { index, .. } = &inner.kind
            && let Some(source) = self.eliminable.get(index)
        {
            let span = expr.span;
            let type_id = expr.type_id;
            *expr = NirExpr {
                kind: source.kind.clone(),
                type_id,
                span,
            };
            return true;
        }
        opt_walk_expr(self, expr)
    }

    fn visit_block(&mut self, block: &mut NirBlock) -> bool {
        let before = block.stmts.len();
        block.stmts.retain(|stmt| {
            if let NirStmtKind::Let { local_index, .. } = &stmt.kind {
                !self.eliminable.contains_key(local_index)
            } else {
                true
            }
        });
        let mut changed = block.stmts.len() < before;
        for stmt in &mut block.stmts {
            changed |= self.visit_stmt(stmt);
        }
        changed
    }

    fn visit_stmt(&mut self, stmt: &mut NirStmt) -> bool {
        opt_walk_stmt(self, stmt)
    }
}

/// Eliminate `let r = &struct_literal` bindings where all uses of r are `*r`.
///
/// This handles the pattern produced by inlining `into_iter()` on struct iterators:
/// ```text
/// let self: &StrUtf8ByteIter = &StrUtf8ByteIter { repr, used, index: 0 };
/// break label: *self;
/// ```
/// After elimination:
/// ```text
/// break label: StrUtf8ByteIter { repr, used, index: 0 };
/// ```
fn eliminate_deref_ref_pairs_in_function(func: &mut NirFunction) -> bool {
    let Some(mut owned) = func.body_block() else {
        return false;
    };
    let body = &mut owned;

    let mut refs: IndexMap<u32, DerefOnlyRef> = IndexMap::default();
    collect_deref_only_refs_in_block(body, &mut refs);

    // Only eliminate refs that are still eliminable AND used exactly
    // once via `*r`. Multi-use elision would duplicate the source
    // literal at every site, which trades a GC alloc for repeated
    // recomputation of any field initialiser inside it.
    let eliminable: IndexMap<u32, NirExpr> = refs
        .into_iter()
        .filter(|(_, info)| info.eliminable && info.use_count == 1)
        .map(|(idx, info)| (idx, info.source_expr))
        .collect();

    if eliminable.is_empty() {
        return false;
    }

    let mut rewriter = DerefOnlyRewriter {
        eliminable: &eliminable,
    };
    let r = rewriter.visit_block(body);
    func.set_body_block(owned);
    r
}

/// Eliminate unnecessary reference bindings in a single function.
fn eliminate_refs_in_function(func: &mut NirFunction) -> bool {
    let Some(mut owned) = func.body_block() else {
        return false;
    };
    let body = &mut owned;

    // Pre-scan: locals bound by more than one `Let` (the inliner reuses
    // an index when expanding mutually-exclusive branches that each
    // rebind the same temporary, e.g. an inlined function whose body
    // has two `let g = &variant_payload(...)` shadows for different
    // match arms). Each `Let` can carry a different referent, but
    // `refs.insert` is keyed by `local_index` and would silently
    // overwrite — leaving every use of the local substituted with the
    // last-seen referent, even in branches that initialised it
    // differently. The cheapest correct response is to refuse
    // elimination on any rebound local.
    let rebound = find_rebound_locals(body);

    // Pass 1: Collect all ref bindings and analyze uses in a single traversal.
    let mut refs: IndexMap<u32, RefInfo> = IndexMap::default();
    analyze_refs_in_block(body, &rebound, &mut refs);

    // Filter to only eliminable bindings.
    let eliminable: IndexMap<u32, RefInfo> = refs
        .into_iter()
        .filter(|(_, info)| info.eliminable)
        .collect();

    if eliminable.is_empty() {
        return false;
    }

    // Pass 2: Replace field accesses and remove dead bindings in a single traversal.
    transform_block(body, &eliminable);
    func.set_body_block(owned);
    true
}

/// Eliminate unnecessary reference bindings in all functions.
///
/// Main entry point for reference elimination optimization.
pub fn eliminate_unnecessary_refs(project: &mut NirPackage) -> bool {
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= eliminate_refs_in_function(&mut func);
        changed |= eliminate_deref_ref_pairs_in_function(&mut func);
    }
    changed
}
