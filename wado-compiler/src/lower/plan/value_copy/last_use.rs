//! TIR-level move-eligibility for the value-copy fold (WEP 2026-05-21).
//!
//! A local qualifies to be *moved* rather than copied when moving its storage
//! is provably unobservable: every read of it is a final use, and its storage
//! traces back — through other locals that are themselves dead at the hand-off
//! — to a fresh allocation (an owned call, a construction, a literal) that
//! nothing else still references. This reaches *synthesized* function bodies —
//! serde `deserialize`, `Default` / `Clone` derives — which have no AST and are
//! invisible to the source-level last-use pass (`elaborator::liveness`) that
//! feeds `moved_local_spans`. Their temporaries (a field parsed into a local,
//! carried through an `Ok`-binding into the returned struct) are the fold's
//! remaining hot copies. The two are unioned at the fold.
//!
//! # Two facts, one backward liveness pass
//!
//! - *final read* — a read after which the local is dead on every live path. A
//!   local all of whose reads are final can be moved at each without a later
//!   observation. Backward liveness computes this precisely: divergent `match`
//!   arms (`… => return Err(e)`) contribute to a local's live-*in* but not its
//!   live-*out*, and a loop body reaches a fixpoint, so a value produced and
//!   consumed within one iteration is dead across the back-edge.
//! - *no live alias* — at the point a local is bound, none of the locals its
//!   value derives from are still live. A match-arm binding
//!   (`if let Some(s) = opt`) aliases its scrutinee's interior; if `opt` is
//!   read again, moving `s` would corrupt it, so the de-aliasing copy must
//!   stay. Checked against the live set *after* the binding (live-out), which
//!   excludes reads that only happen on divergent arms — so a once-consumed
//!   deserialize temporary passes while a re-read `opt` does not.
//!
//! Owned storage is then a least fixpoint: a local exclusively owns fresh
//! storage when it has no live alias and every source is owned given the locals
//! proven so far (`is_owned_value` resolves a `Local` reference through that
//! set). A function containing a closure, effect handler, or `resume` is
//! skipped wholesale — a captured local or a resumed continuation can
//! re-observe a local this pass does not model.
//!
//! Soundness rests on `live` being an over-approximation of the true live set
//! everywhere: an unknown control target keeps every local live (never a spurious
//! final read), so at worst a copy is kept.

use super::analyze::is_owned_value;
use super::funcset::FuncKeySet;
use super::ownership::OwnedCalls;
use super::stores::StoredParams;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{
    FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirMatchArm,
    TirPattern, TirStmt, TirStmtKind, TirUnaryOp, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// May-alias union-find over a function's locals: a `let` / assign / match-arm
/// binding from a projection connects the binding to its source root. Consumed
/// by the confinement check's `no_mut_alias`.
pub struct AliasComponents {
    parent: IndexMap<u32, u32>,
}

impl AliasComponents {
    pub fn empty() -> Self {
        AliasComponents {
            parent: IndexMap::default(),
        }
    }

    pub fn build(func: &TirFunction) -> Self {
        let mut ac = AliasComponents {
            parent: IndexMap::default(),
        };
        if let Some(body) = &func.body {
            let mut collector = AliasEdgeCollector { edges: Vec::new() };
            collector.visit_block(body);
            for (a, b) in collector.edges {
                ac.union(a, b);
            }
        }
        ac
    }

    /// Whether locals `a` and `b` may share storage.
    pub fn may_alias(&self, a: u32, b: u32) -> bool {
        a == b || self.find(a) == self.find(b)
    }

    fn find(&self, mut x: u32) -> u32 {
        while let Some(&p) = self.parent.get(&x) {
            if p == x {
                return x;
            }
            x = p;
        }
        x
    }

    fn union(&mut self, a: u32, b: u32) {
        let ra = self.find(a);
        let rb = self.find(b);
        self.parent.entry(ra).or_insert(ra);
        self.parent.entry(rb).or_insert(rb);
        if ra != rb {
            self.parent.insert(ra, rb);
        }
    }
}

/// Collects `(local, alias-root)` edges: a `let` / whole-local assign bound from
/// a projection, and a match-arm binding rooted at its scrutinee.
struct AliasEdgeCollector {
    edges: Vec<(u32, u32)>,
}

impl TirRefVisitor for AliasEdgeCollector {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && let Some(root) = alias_root(value)
        {
            self.edges.push((*local_index, root));
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Assign { target, value } => {
                if let TirExprKind::Local { index, .. } = &target.kind
                    && let Some(root) = alias_root(value)
                {
                    self.edges.push((*index, root));
                }
            }
            TirExprKind::Match { expr: scrut, arms } => {
                if let Some(root) = alias_root(scrut) {
                    for arm in arms {
                        let mut binds: IndexSet<u32> = IndexSet::default();
                        super::analyze::collect_pattern_bindings(&arm.pattern, &mut binds);
                        for b in binds {
                            self.edges.push((b, root));
                        }
                    }
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

/// Move-eligibility facts for the value-copy fold. Empty for a bodyless function
/// or one whose control forms (closure / handler / resume) defeat the
/// single-observation argument. `locals` are whole-local final-use moves;
/// `place_spans` are the spans of individual field / whole-value materializations
/// that alias out of a *dead* aggregate at a struct / tuple literal (place-level
/// move, keyed by the materialized expression's span).
#[derive(Default)]
pub struct MoveEligible {
    pub locals: IndexSet<u32>,
    pub place_spans: IndexSet<crate::token::Span>,
}

pub fn compute_move_eligible(
    func: &TirFunction,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
    stored_params: &StoredParams,
    mut_receiver_methods: &FuncKeySet,
) -> MoveEligible {
    let Some(body) = &func.body else {
        return MoveEligible::default();
    };
    if has_unsupported_form(body) {
        return MoveEligible::default();
    }

    let mut all_locals: IndexSet<u32> = (0..func.local_count).collect();
    // Guard against a local_count that lags a grown local set.
    let mut scan = MaxLocal { max: 0 };
    scan.visit_block(body);
    for i in 0..=scan.max {
        all_locals.insert(i);
    }

    let param_locals: IndexSet<u32> = func.params.iter().map(|p| p.local_index).collect();
    let mut a = Analyzer {
        stored_params,
        mut_receiver_methods,
        type_table,
        param_locals,
        non_final: IndexSet::default(),
        aliases_live: IndexSet::default(),
        borrow_escaped: IndexMap::default(),
        let_sources: IndexMap::default(),
        match_sources: Vec::new(),
        pending_mut_alias: Vec::new(),
        exits: Vec::new(),
        all_locals,
        place_cands: Vec::new(),
    };
    let mut live = IndexSet::default();
    a.walk_block(body, &mut live, true);
    a.resolve_pending_mut_aliases();

    // Structural freshness: the locals that *hold* an owned (unaliased) value,
    // regardless of how many times they are read. A least fixpoint — start with
    // every sourced local and drop those whose source is not owned given the
    // rest. `is_owned_value` resolves a `Local` reference through this set, so a
    // deserialize temporary read twice (the `?` tag-test + payload-extract, or a
    // re-wrapped `Err` arm) still counts, propagating freshness up the chain to
    // the field that is finally moved.
    // By-value (non-reference) parameters are owned storage the function holds
    // exclusively: the caller either deep-copied the argument in, or move-elided
    // a fresh-and-dead one (whose source is then dead), so nothing live aliases
    // the parameter. Consuming it at its final use is therefore a move — a
    // `build(self) -> Self { return self }` returns its receiver without a copy.
    // Multi-use or borrow-escaping parameters are still held back by `non_final`
    // / `borrow_escaped`, so this only frees genuinely final consumptions.
    // Reference parameters (`&self`) borrow the caller's storage and are never
    // seeded. The interprocedural return convention (`ownership.rs`) seeds
    // by-value params the same way and for the same reason: a returned parameter
    // is never confined, so the caller always deep-copies it in, and returning it
    // (a stored-then-returned parameter's store copied it too, since it is live at
    // the return) yields a value that aliases only the callee's own copy.
    let mut fresh: IndexSet<u32> = a
        .let_sources
        .keys()
        .copied()
        .chain(a.match_sources.iter().map(|(l, _)| *l))
        .chain(
            func.params
                .iter()
                .filter(|p| !is_reference_type(p.type_id, type_table))
                .map(|p| p.local_index),
        )
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        let snapshot: Vec<u32> = fresh.iter().copied().collect();
        for local in snapshot {
            if !fresh.contains(&local) {
                continue;
            }
            let ok = a
                .let_sources
                .get(&local)
                .into_iter()
                .flatten()
                .all(|s| is_owned_value(s, &fresh, oracle, type_table))
                && a.match_sources
                    .iter()
                    .filter(|(l, _)| *l == local)
                    .all(|(_, scrut)| is_owned_value(scrut, &fresh, oracle, type_table));
            if !ok {
                fresh.swap_remove(&local);
                changed = true;
            }
        }
    }

    // Move-eligible: a fresh local whose every value-read is final, which does
    // not alias a still-live local at its binding (`aliases_live`), and which is
    // not borrow-escaped — no reference to it persists past its move. A
    // transient `&`/`&mut` (a call argument to a callee that does not store it)
    // is a use that keeps the local live but never blocks a later move, so the
    // "build a fresh buffer, mutate it in place, hand it off" pattern
    // (`List::filled`, builders) moves instead of copying.
    let owned: IndexSet<u32> = fresh
        .iter()
        .copied()
        .filter(|idx| {
            !a.non_final.contains(idx)
                && !a.aliases_live.contains(idx)
                && !a.borrow_escaped.contains_key(idx)
        })
        .collect();

    let place_spans: IndexSet<crate::token::Span> = a
        .place_cands
        .iter()
        .filter(|(base, top, _)| {
            fresh.contains(base) && !a.aliases_live.contains(base) && !a.place_escaped(*base, *top)
        })
        .map(|(_, _, span)| *span)
        .collect();

    MoveEligible {
        locals: owned,
        place_spans,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Read-only-share refinement (WEP 2026-05-21)
// ──────────────────────────────────────────────────────────────────────────────

/// A place selector — one projection step, root-first.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Selector {
    Field(u32),
    Variant(u32),
    Index,
}

/// A storage location: a root local plus a root-first chain of projections.
/// `self.rows[0]` is `{ root: self, selectors: [Field(rows), Index] }`.
#[derive(Clone)]
struct AccessPath {
    root: u32,
    selectors: Vec<Selector>,
}

/// The access path of a pure place expression, or `None` for a non-place: a
/// deref (the pointee has no local identity), a call/construction result, or a
/// projection off one. Casts and non-deref unaries are transparent.
fn place_path(expr: &TirExpr) -> Option<AccessPath> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => Some(AccessPath {
            root: *index,
            selectors: Vec::new(),
        }),
        TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            let mut p = place_path(inner)?;
            p.selectors.push(Selector::Field(*field_index));
            Some(p)
        }
        TirExprKind::VariantPayload {
            expr: inner,
            case_index,
            ..
        } => {
            let mut p = place_path(inner)?;
            p.selectors.push(Selector::Variant(*case_index));
            Some(p)
        }
        TirExprKind::Index { expr: inner, .. } => {
            let mut p = place_path(inner)?;
            p.selectors.push(Selector::Index);
            Some(p)
        }
        TirExprKind::Cast { expr: inner, .. } => place_path(inner),
        TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: inner,
        } => place_path(inner),
        _ => None,
    }
}

/// Whether a write to `mutated` can never change the value read at `read` (both
/// rooted at the same local): they agree on a shared prefix and then diverge at
/// two distinct struct fields or variant cases. A dynamic `Index`, a mixed
/// selector kind, or one being a prefix of the other is conservatively
/// overlapping.
fn disjoint(mutated: &AccessPath, read: &AccessPath) -> bool {
    for (a, b) in mutated.selectors.iter().zip(read.selectors.iter()) {
        match (a, b) {
            (Selector::Field(x), Selector::Field(y)) if x != y => return true,
            (Selector::Variant(x), Selector::Variant(y)) if x != y => return true,
            (Selector::Field(_), Selector::Field(_))
            | (Selector::Variant(_), Selector::Variant(_))
            | (Selector::Index, Selector::Index) => {}
            _ => return false,
        }
    }
    false
}

/// Locals whose binding copy can be elided by sharing the source storage
/// (WEP 2026-05-21): a read-only local bound from a projection whose storage is
/// never mutated while live (`row = self.rows[0]; self.tick += 1; row.len()`).
///
/// Soundness rests on the source root being unconsumed: a mutation reaching the
/// shared storage through an alias or a callee must first consume the root, so
/// with the root unconsumed every such mutation is a direct write rooted at it,
/// which the disjointness check covers.
pub fn compute_share_eligible(
    func: &TirFunction,
    move_eligible: &IndexSet<u32>,
    mut_receiver_methods: &FuncKeySet,
    ref_receiver_methods: &FuncKeySet,
    returns_receiver_alias: &FuncKeySet,
) -> IndexSet<u32> {
    let Some(body) = &func.body else {
        return IndexSet::default();
    };
    if has_unsupported_form(body) {
        return IndexSet::default();
    }
    let mut collector = ShareCollector {
        mut_receiver_methods,
        ref_receiver_methods,
        returns_receiver_alias,
        sources: IndexMap::default(),
        mutated: Vec::new(),
        consumed: IndexSet::default(),
    };
    collector.walk_block(body);

    collector
        .sources
        .iter()
        .filter_map(|(&local, path)| {
            if path.root == local {
                return None;
            }
            if move_eligible.contains(&local)
                || collector.consumed.contains(&local)
                || collector.is_mutated_root(local)
                || collector.consumed.contains(&path.root)
            {
                return None;
            }
            let safe = collector
                .mutated
                .iter()
                .all(|m| m.root != path.root || disjoint(m, path));
            safe.then_some(local)
        })
        .collect()
}

struct ShareCollector<'a> {
    mut_receiver_methods: &'a FuncKeySet,
    ref_receiver_methods: &'a FuncKeySet,
    returns_receiver_alias: &'a FuncKeySet,
    /// Binding local → the access path of its source projection.
    sources: IndexMap<u32, AccessPath>,
    /// Every access path a mutation writes through.
    mutated: Vec<AccessPath>,
    /// Locals read in a value position (consumed), so not safe to share.
    consumed: IndexSet<u32>,
}

impl ShareCollector<'_> {
    fn is_mutated_root(&self, local: u32) -> bool {
        self.mutated.iter().any(|m| m.root == local)
    }

    /// Record a write through `place`. When the exact path is unknown, mark
    /// every local `place` mentions as fully mutated (a bare-root path) — the
    /// conservative default.
    fn record_mutation(&mut self, place: &TirExpr) {
        if let Some(p) = place_path(place) {
            self.mutated.push(p);
        } else {
            let mut roots: IndexSet<u32> = IndexSet::default();
            collect_local_roots(place, &mut roots);
            for r in roots {
                self.mutated.push(AccessPath {
                    root: r,
                    selectors: Vec::new(),
                });
            }
        }
    }

    fn mark_local_mutated(&mut self, index: u32) {
        self.mutated.push(AccessPath {
            root: index,
            selectors: Vec::new(),
        });
    }

    /// The access path a binding's value projects: a direct place, or a
    /// receiver-aliasing accessor call whose receiver / first arg is a place.
    fn source_path(&self, value: &TirExpr) -> Option<AccessPath> {
        if let Some(p) = place_path(value) {
            return Some(p);
        }
        match &value.kind {
            TirExprKind::MethodCall { func, receiver, .. }
                if self
                    .returns_receiver_alias
                    .contains(&func.module_source, &func.name) =>
            {
                place_path(receiver)
            }
            TirExprKind::Call { func, args, .. }
                if self
                    .returns_receiver_alias
                    .contains(&func.module_source, &func.name) =>
            {
                place_path(&args.first()?.expr)
            }
            _ => None,
        }
    }

    fn walk_block(&mut self, block: &TirBlock) {
        for stmt in &block.stmts {
            self.walk_stmt(stmt);
        }
    }

    fn walk_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                if let Some(path) = self.source_path(value) {
                    self.sources.insert(*local_index, path);
                }
                self.walk_value(value);
            }
            TirStmtKind::LetDestructure { value, .. } => self.walk_value(value),
            TirStmtKind::Expr(e) => self.walk_value(e),
            TirStmtKind::Return { value } => {
                if let Some(v) = value {
                    self.walk_value(v);
                }
            }
            TirStmtKind::TaskReturn { value } => self.walk_value(value),
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.walk_value(v);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.walk_value(condition);
                self.walk_block(then_block);
                if let Some(eb) = else_block {
                    self.walk_block(eb);
                }
            }
            TirStmtKind::Loop { body } => self.walk_block(body),
            TirStmtKind::LabeledBlock { block, .. } => self.walk_block(block),
            TirStmtKind::VariadicForOf { .. } => {}
        }
    }

    /// Walk `expr` in a value position: a whole-local read is a consumption,
    /// projection bases and borrow referents are only observed, a `&mut`
    /// referent is recorded mutated, and an unrecognized form conservatively
    /// consumes every local it mentions.
    fn walk_value(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                self.consumed.insert(*index);
            }
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. } => self.walk_place_base(inner),
            TirExprKind::Index { expr: inner, index } => {
                self.walk_place_base(inner);
                self.walk_value(index);
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Ref,
                expr: place,
            } => self.walk_place_base(place),
            TirExprKind::Unary {
                op: TirUnaryOp::MutRef,
                expr: place,
            } => {
                self.record_mutation(place);
                self.walk_place_base(place);
            }
            TirExprKind::Unary { expr: inner, .. } => self.walk_value(inner),
            TirExprKind::Binary { left, right, .. } => {
                self.walk_value(left);
                self.walk_value(right);
            }
            TirExprKind::Assign { target, value } => {
                match &target.kind {
                    TirExprKind::Local { index, .. } => self.mark_local_mutated(*index),
                    _ => self.record_mutation(target),
                }
                self.walk_value(value);
            }
            TirExprKind::MethodCall {
                func,
                receiver,
                args,
                ..
            } => {
                if self
                    .mut_receiver_methods
                    .contains(&func.module_source, &func.name)
                {
                    self.record_mutation(receiver);
                }
                if self
                    .ref_receiver_methods
                    .contains(&func.module_source, &func.name)
                {
                    self.walk_place_base(receiver);
                } else {
                    self.walk_value(receiver);
                }
                for a in args {
                    self.walk_value(&a.expr);
                }
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.walk_value(condition);
                self.walk_block(then_branch);
                if let Some(eb) = else_branch {
                    self.walk_block(eb);
                }
            }
            TirExprKind::Match { expr: scrut, arms } => {
                self.walk_value(scrut);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.walk_value(g);
                    }
                    self.walk_value(&arm.body);
                }
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                self.walk_block(block);
            }
            TirExprKind::GlobalVarSet { value, .. } => self.walk_value(value),
            _ => {
                let mut children: Vec<&TirExpr> = Vec::new();
                collect_child_exprs(expr, &mut children);
                for c in children {
                    self.walk_value(c);
                }
            }
        }
    }

    /// A place base: a whole `Local` is observed, not consumed; a non-place
    /// recurses as a value.
    fn walk_place_base(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Local { .. } => {}
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. } => self.walk_place_base(inner),
            TirExprKind::Index { expr: inner, index } => {
                self.walk_place_base(inner);
                self.walk_value(index);
            }
            _ => self.walk_value(expr),
        }
    }
}

/// Collect every local mentioned anywhere in `expr`.
fn collect_local_roots(expr: &TirExpr, out: &mut IndexSet<u32>) {
    struct W<'a>(&'a mut IndexSet<u32>);
    impl TirRefVisitor for W<'_> {
        fn visit_expr(&mut self, expr: &TirExpr) {
            if let TirExprKind::Local { index, .. } = &expr.kind {
                self.0.insert(*index);
            }
            self.walk_expr(expr);
        }
    }
    W(out).visit_expr(expr);
}

/// Closures / effect handlers / `resume` / an unexpanded variadic for-of defeat
/// the single-observation model. Detected up front so the whole function falls
/// back to copies.
fn has_unsupported_form(body: &TirBlock) -> bool {
    struct Scan {
        found: bool,
    }
    impl TirRefVisitor for Scan {
        fn visit_stmt(&mut self, stmt: &TirStmt) {
            if matches!(stmt.kind, TirStmtKind::VariadicForOf { .. }) {
                self.found = true;
            }
            self.walk_stmt(stmt);
        }
        fn visit_expr(&mut self, expr: &TirExpr) {
            if matches!(
                expr.kind,
                TirExprKind::Closure { .. }
                    | TirExprKind::WithHandler { .. }
                    | TirExprKind::Resume { .. }
            ) {
                self.found = true;
            }
            self.walk_expr(expr);
        }
    }
    let mut s = Scan { found: false };
    s.visit_block(body);
    s.found
}

struct MaxLocal {
    max: u32,
}
impl TirRefVisitor for MaxLocal {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Let { local_index, .. } = &stmt.kind {
            self.max = self.max.max(*local_index);
        }
        self.walk_stmt(stmt);
    }
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::Local { index, .. } = &expr.kind {
            self.max = self.max.max(*index);
        }
        self.walk_expr(expr);
    }
}

/// A break/continue target on the exit stack: the live set at the point control
/// resumes. Loops carry `label: None` (also the target of an unlabeled break /
/// continue); labeled blocks carry their label.
struct Exit {
    label: Option<String>,
    live: IndexSet<u32>,
}

struct Analyzer<'a> {
    /// Per-callee reference-storage: `stored_params(callee)` is the set of
    /// parameter positions the callee may persist a reference to (position 0 is
    /// the receiver). A `&`/`&mut` argument at a stored position escapes; at a
    /// non-stored position it is a transient borrow.
    stored_params: &'a StoredParams,
    mut_receiver_methods: &'a FuncKeySet,
    type_table: &'a TypeTable,
    /// Local indices that are the enclosing function's parameters. A functor
    /// *parameter*'s declared `stores` is a sound upper bound of every value
    /// bound to it — the frontend checks each call argument against the param's
    /// `stores` — so an indirect call through a parameter may trust its type's
    /// `stores`. A functor value from any other source (a `let`-bound closure, a
    /// returned functor) is not so guaranteed and stays conservative.
    param_locals: IndexSet<u32>,
    non_final: IndexSet<u32>,
    aliases_live: IndexSet<u32>,
    /// Locals a persisting reference is taken of — a `&`/`&mut` that is not a
    /// transient call argument, or is passed to a callee that may store it. Such
    /// a local may be observed through the reference after its move, so it stays
    /// copied. Field-sensitive: a borrow of `p.f` escapes only field `f`, so a
    /// disjoint field of `p` still moves (a borrow of `p.f` provably cannot alias
    /// `p.g`). A whole-local / imprecise borrow (`&p`, `&p[i]`, `&*p`) escapes
    /// every field.
    borrow_escaped: IndexMap<u32, FieldEscape>,
    let_sources: IndexMap<u32, Vec<TirExpr>>,
    match_sources: Vec<(u32, TirExpr)>,
    /// `(by-value arg root, storage the call mutates)` pairs, resolved after
    /// the walk when the alias chains are complete (see
    /// [`Analyzer::mark_sibling_mut_aliases`]).
    pending_mut_alias: Vec<(u32, Vec<u32>)>,
    exits: Vec<Exit>,
    all_locals: IndexSet<u32>,
    /// Provisional place-level move sites `(aggregate root, moved top-level field,
    /// materialized span)` found at literals, filtered by the owned-storage
    /// guards after the walk. `None` field = a whole-value materialization.
    place_cands: Vec<(u32, Option<u32>, crate::token::Span)>,
}

/// Which fields of a local an escaped borrow reaches. `Whole` (a `&local` or an
/// imprecise projection) covers every field; `Fields` names the exact top-level
/// fields borrowed via clean projections.
enum FieldEscape {
    Whole,
    Fields(IndexSet<u32>),
}

impl Analyzer<'_> {
    fn read(&mut self, index: u32, live: &mut IndexSet<u32>, record: bool) {
        if record && live.contains(&index) {
            self.non_final.insert(index);
        }
        live.insert(index);
    }

    /// A `&place` / `&mut place`: the referent local's storage is used here (keep
    /// it live so an earlier value-read is not mistaken for a final use), but a
    /// borrow is not a value consumption, so it never marks the local `non_final`
    /// — a transient borrow before a later move is fine. Projection indices in
    /// `place` (`&arr[i]`) are ordinary value-reads. Returns the referent.
    /// Record that a persisting borrow reaches `field` of `root` (`None` = the
    /// whole local / an imprecise projection, escaping every field).
    fn mark_escaped(&mut self, root: u32, field: Option<u32>) {
        let entry = self
            .borrow_escaped
            .entry(root)
            .or_insert_with(|| FieldEscape::Fields(IndexSet::default()));
        match field {
            None => *entry = FieldEscape::Whole,
            Some(f) => {
                if let FieldEscape::Fields(fs) = entry {
                    fs.insert(f);
                }
            }
        }
    }

    /// Whether a persisting borrow reaches the place `root.field` (`None` field =
    /// a whole-value move, blocked by any escaped field).
    fn place_escaped(&self, root: u32, field: Option<u32>) -> bool {
        match self.borrow_escaped.get(&root) {
            None => false,
            Some(FieldEscape::Whole) => true,
            Some(FieldEscape::Fields(fs)) => match field {
                None => !fs.is_empty(),
                Some(f) => fs.contains(&f),
            },
        }
    }

    fn borrow_read(
        &mut self,
        place: &TirExpr,
        live: &mut IndexSet<u32>,
        record: bool,
    ) -> Option<u32> {
        match &place.kind {
            TirExprKind::Local { index, .. } => {
                live.insert(*index);
                Some(*index)
            }
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Unary { expr: inner, .. } => self.borrow_read(inner, live, record),
            TirExprKind::Index { expr: inner, index } => {
                self.walk_expr(index, live, record);
                self.borrow_read(inner, live, record)
            }
            // A borrow of a non-place (a fresh temporary) escapes nothing.
            _ => {
                self.walk_expr(place, live, record);
                None
            }
        }
    }

    /// Whether the callee may persist a reference passed at position `pos`. An
    /// absent callee (bodyless / not a project function) or one with no stored
    /// positions stores nothing at `pos`.
    fn callee_stores(&self, callee: &FunctionRef, pos: usize) -> bool {
        self.stored_params
            .get(&callee.module_source, &callee.name)
            .is_some_and(|s| s.contains(&u32::try_from(pos).unwrap()))
    }

    /// The positions an indirect (functor) callee may store, from its functor
    /// type's declared `stores` clause. `None` is conservative: every position
    /// may be stored.
    ///
    /// Only a call through a functor *parameter* is trusted. A parameter's
    /// declared `stores` is a sound upper bound of every value bound to it: the
    /// frontend checks each call argument (closure / function) against the
    /// parameter's `stores`, rejecting one that would store more. A functor
    /// value from any other source (a `let`-bound closure whose synthesized type
    /// records no `stores`, a returned functor) carries no such guarantee, so it
    /// stays conservative and its `&`/`&mut` arguments keep their copies.
    fn functor_stores(&self, callee: &TirExpr) -> Option<IndexSet<u32>> {
        let TirExprKind::Local { index, .. } = &callee.kind else {
            return None;
        };
        if !self.param_locals.contains(index) {
            return None;
        }
        match self.type_table.get(callee.type_id) {
            ResolvedType::Function {
                stores,
                return_type,
                ..
            } => {
                if matches!(
                    self.type_table.get(*return_type),
                    ResolvedType::Ref(_) | ResolvedType::MutRef(_)
                ) {
                    return None;
                }
                Some(stores.iter().copied().collect())
            }
            _ => None,
        }
    }

    /// An indirect-call argument at position `pos`: a `&`/`&mut` argument is a
    /// transient borrow unless the functor may store that position (`None`
    /// stores = conservative, every position escapes).
    fn walk_indirect_arg(
        &mut self,
        arg: &TirExpr,
        pos: usize,
        stores: &Option<IndexSet<u32>>,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        if let TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: place,
        } = &arg.kind
        {
            let referent = self.borrow_read(place, live, record);
            let escapes = match stores {
                Some(s) => s.contains(&u32::try_from(pos).unwrap()),
                None => true,
            };
            if record
                && escapes
                && let Some(r) = referent
            {
                self.mark_escaped(r, borrow_top_field(place));
            }
        } else {
            self.walk_expr(arg, live, record);
        }
    }

    /// A method-call receiver at position 0. The auto-ref'd `&self` / `&mut self`
    /// receiver is a transient borrow that escapes only if the callee stores its
    /// receiver position — a method that stores only a value parameter (e.g.
    /// `List::push`) does not retain `self`, so the receiver local stays
    /// move-eligible. A by-value (consuming) receiver is an ordinary value read.
    fn walk_method_receiver(
        &mut self,
        receiver: &TirExpr,
        func: &FunctionRef,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        match &receiver.kind {
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } => {
                let referent = self.borrow_read(place, live, record);
                if record
                    && let Some(r) = referent
                    && self.callee_stores(func, 0)
                {
                    self.mark_escaped(r, borrow_top_field(place));
                }
            }
            _ => self.walk_expr(receiver, live, record),
        }
    }

    /// Process a call's argument at position `pos`. An explicit `&`/`&mut`
    /// argument is a transient borrow unless the callee may store that position,
    /// in which case the referent escapes; every other argument is an ordinary
    /// value.
    fn walk_call_arg(
        &mut self,
        arg: &TirExpr,
        callee: Option<&FunctionRef>,
        pos: usize,
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        if let TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: place,
        } = &arg.kind
        {
            let referent = self.borrow_read(place, live, record);
            if record
                && let Some(r) = referent
                && callee.is_some_and(|c| self.callee_stores(c, pos))
            {
                self.mark_escaped(r, borrow_top_field(place));
            }
        } else {
            self.walk_expr(arg, live, record);
        }
    }

    /// Record that binding `local` derives from `source`: if the local whose
    /// storage `source` aliases (a projection / whole-value move — `None` for a
    /// fresh allocation, whose result aliases nothing observable) is live
    /// *after* the binding, `local` shares still-live storage and must be copied.
    fn record_alias(&mut self, local: u32, source: &TirExpr, live: &IndexSet<u32>) {
        if let Some(root) = alias_root(source)
            && live.contains(&root)
        {
            self.aliases_live.insert(local);
        }
    }

    /// Resolve the deferred sibling-alias checks now the alias chains are
    /// complete: a by-value argument that transitively aliases storage its call
    /// mutates keeps its copy.
    fn resolve_pending_mut_aliases(&mut self) {
        let mut scrut_roots: IndexMap<u32, Vec<u32>> = IndexMap::default();
        for (binding, scrut) in &self.match_sources {
            if let Some(r) = alias_root(scrut) {
                scrut_roots.entry(*binding).or_default().push(r);
            }
        }
        let pending = std::mem::take(&mut self.pending_mut_alias);
        for (arg, mut_roots) in pending {
            let mut seen = IndexSet::default();
            if self.local_aliases(arg, &mut_roots, &scrut_roots, &mut seen) {
                self.aliases_live.insert(arg);
            }
        }
    }

    /// Whether `local` transitively shares storage with one of `targets`,
    /// following the recorded alias edges: a match binding shares its
    /// scrutinee's interior (`scrut_roots`), a `let x = <projection>` shares its
    /// source root (`let_sources`; `alias_root` is `None` for a fresh rvalue, so
    /// a copied source is not followed). Resolves the match-scrutinee hoist
    /// (`let __m = w.payload; match __m { Some(t) => … }`, so `t` reaches `w`).
    fn local_aliases(
        &self,
        local: u32,
        targets: &[u32],
        scrut_roots: &IndexMap<u32, Vec<u32>>,
        seen: &mut IndexSet<u32>,
    ) -> bool {
        if targets.contains(&local) {
            return true;
        }
        if !seen.insert(local) {
            return false;
        }
        let via_match = scrut_roots.get(&local).into_iter().flatten().copied();
        let via_let = self
            .let_sources
            .get(&local)
            .into_iter()
            .flatten()
            .filter_map(alias_root);
        via_match
            .chain(via_let)
            .collect::<Vec<_>>()
            .into_iter()
            .any(|r| self.local_aliases(r, targets, scrut_roots, seen))
    }

    /// Keep the copy of any by-value argument that aliases storage the same call
    /// mutates — an explicit sibling `&mut` argument, or (via `extra_mut_root`)
    /// the auto-ref'd `&mut self` receiver — since a move would let the callee's
    /// mutation leak into the moved value (wado-lang/wado#1544). The live-out
    /// alias check misses this: the scrutinee is mutated within the arm, not
    /// read after the match. Deferred to [`resolve_pending_mut_aliases`] because
    /// the backward walk reaches a call before the `let`s that root its
    /// scrutinee.
    fn mark_sibling_mut_aliases(&mut self, args: &[&TirExpr], extra_mut_root: Option<u32>) {
        let mut mut_roots: Vec<u32> = args
            .iter()
            .filter_map(|a| match &a.kind {
                TirExprKind::Unary {
                    op: TirUnaryOp::MutRef,
                    expr: place,
                } => alias_root(place),
                _ => None,
            })
            .collect();
        mut_roots.extend(extra_mut_root);
        if mut_roots.is_empty() {
            return;
        }
        for a in args {
            if matches!(
                &a.kind,
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                    ..
                }
            ) {
                continue;
            }
            if let Some(t) = alias_root(a) {
                self.pending_mut_alias.push((t, mut_roots.clone()));
            }
        }
    }

    fn kill_pattern(&self, pat: &TirPattern, live: &mut IndexSet<u32>) {
        let mut binds: IndexSet<u32> = IndexSet::default();
        super::analyze::collect_pattern_bindings(pat, &mut binds);
        for b in binds {
            live.swap_remove(&b);
        }
    }

    /// Record place-level move candidates at a struct/tuple literal. A direct
    /// child that materializes a whole value / clean field of an aggregate root
    /// `base` aliases it out of the aggregate instead of deep-copying, when:
    ///
    /// - `base` is dead after the literal (`!live_out.contains(base)`);
    /// - the materialization sites of `base` are non-overlapping owners (one
    ///   whole move, or field moves with distinct top-level fields); and
    /// - no *other* use of `base` in the literal mutates or moves its storage
    ///   (`conflict`). Read-only borrows (`&self` method, field read) and
    ///   independent deep copies are harmless because `base` dies right after.
    ///
    /// The owned-storage guards (fresh / no live alias / no escaped borrow) are
    /// applied later, once the fixpoint is known.
    fn collect_place_moves(&mut self, children: &[&TirExpr], live_out: &IndexSet<u32>) {
        let mut mats: IndexMap<u32, Vec<(Option<u32>, crate::token::Span)>> = IndexMap::default();
        let mut conflict: IndexSet<u32> = IndexSet::default();
        for child in children {
            if let Some((base, top)) = as_materialize(child) {
                mats.entry(base).or_default().push((top, child.span));
            } else {
                self.scan_place_uses(child, &mut conflict);
            }
        }
        for (base, sites) in &mats {
            if live_out.contains(base) || conflict.contains(base) {
                continue;
            }
            let has_whole = sites.iter().any(|(top, _)| top.is_none());
            if has_whole && sites.len() > 1 {
                continue;
            }
            let mut tops: Vec<u32> = sites.iter().filter_map(|(t, _)| *t).collect();
            tops.sort_unstable();
            if tops.windows(2).any(|w| w[0] == w[1]) {
                continue;
            }
            for (top, span) in sites {
                self.place_cands.push((*base, *top, *span));
            }
        }
    }

    /// Classify occurrences of an aggregate root inside a non-materialized literal
    /// child, marking `conflict` for any use that mutates or moves the aggregate's
    /// storage. Read-only borrows and deep copies leave no live alias.
    fn scan_place_uses(&self, expr: &TirExpr, conflict: &mut IndexSet<u32>) {
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                conflict.insert(*index);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                if clean_root(expr).is_none() {
                    self.scan_place_uses(inner, conflict);
                }
            }
            TirExprKind::MethodCall {
                func,
                receiver,
                args,
                ..
            } => {
                let (recv_place, recv_ref) = match &receiver.kind {
                    TirExprKind::Unary {
                        op: op @ (TirUnaryOp::Ref | TirUnaryOp::MutRef),
                        expr,
                    } => (expr.as_ref(), Some(*op)),
                    _ => (receiver.as_ref(), None),
                };
                match clean_root(recv_place) {
                    Some(base) => {
                        let read_only = matches!(recv_ref, Some(TirUnaryOp::Ref))
                            && !self
                                .mut_receiver_methods
                                .contains(&func.module_source, &func.name)
                            && !self.callee_stores(func, 0);
                        if !read_only {
                            conflict.insert(base);
                        }
                    }
                    None => self.scan_place_uses(recv_place, conflict),
                }
                for (pos, a) in args.iter().enumerate() {
                    self.scan_call_arg_place_use(&a.expr, Some(func), pos + 1, conflict);
                }
            }
            TirExprKind::Call { func, args, .. } => {
                for (pos, a) in args.iter().enumerate() {
                    self.scan_call_arg_place_use(&a.expr, Some(func), pos, conflict);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for (pos, a) in args.iter().enumerate() {
                    self.scan_call_arg_place_use(a, None, pos, conflict);
                }
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } => match clean_root(place) {
                Some(base) => {
                    conflict.insert(base);
                }
                None => self.scan_place_uses(place, conflict),
            },
            _ => {
                let mut kids: Vec<&TirExpr> = Vec::new();
                collect_child_exprs(expr, &mut kids);
                for k in kids {
                    self.scan_place_uses(k, conflict);
                }
            }
        }
    }

    /// A call/method argument at position `pos`: an explicit `&mut base` mutates
    /// the aggregate; a `&base` to a callee that may store that position escapes;
    /// both conflict. A transient `&base` to a non-storing position is a harmless
    /// read-only borrow.
    fn scan_call_arg_place_use(
        &self,
        arg: &TirExpr,
        callee: Option<&FunctionRef>,
        pos: usize,
        conflict: &mut IndexSet<u32>,
    ) {
        match &arg.kind {
            TirExprKind::Unary {
                op: TirUnaryOp::MutRef,
                expr: place,
            } => match clean_root(place) {
                Some(base) => {
                    conflict.insert(base);
                }
                None => self.scan_place_uses(place, conflict),
            },
            TirExprKind::Unary {
                op: TirUnaryOp::Ref,
                expr: place,
            } => match clean_root(place) {
                Some(base) => {
                    if callee.is_some_and(|c| self.callee_stores(c, pos)) {
                        conflict.insert(base);
                    }
                }
                None => self.scan_place_uses(place, conflict),
            },
            _ => self.scan_place_uses(arg, conflict),
        }
    }

    /// Live set at the resume point of a break/continue to `label`. Unknown
    /// target → every local (the sound over-approximation).
    fn exit_live(&self, label: &Option<String>) -> IndexSet<u32> {
        let found = match label {
            Some(l) => self
                .exits
                .iter()
                .rev()
                .find(|e| e.label.as_ref() == Some(l)),
            None => self.exits.iter().rev().find(|e| e.label.is_none()),
        };
        found.map_or_else(|| self.all_locals.clone(), |e| e.live.clone())
    }

    fn walk_block(&mut self, block: &TirBlock, live: &mut IndexSet<u32>, record: bool) {
        for stmt in block.stmts.iter().rev() {
            self.walk_stmt(stmt, live, record);
        }
    }

    fn walk_stmt(&mut self, stmt: &TirStmt, live: &mut IndexSet<u32>, record: bool) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                if record {
                    self.record_alias(*local_index, value, live);
                    self.let_sources
                        .entry(*local_index)
                        .or_default()
                        .push(value.clone());
                }
                live.swap_remove(local_index);
                self.walk_expr(value, live, record);
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                self.kill_pattern(pattern, live);
                self.walk_expr(value, live, record);
            }
            TirStmtKind::Expr(e) => self.walk_expr(e, live, record),
            TirStmtKind::Return { value } => {
                live.clear();
                if let Some(v) = value {
                    self.walk_expr(v, live, record);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let mut then_live = live.clone();
                self.walk_block(then_block, &mut then_live, record);
                let mut else_live = live.clone();
                if let Some(eb) = else_block {
                    self.walk_block(eb, &mut else_live, record);
                }
                *live = union(&then_live, &else_live);
                self.walk_expr(condition, live, record);
            }
            TirStmtKind::Loop { body } => self.walk_loop(body, live, record),
            TirStmtKind::Break { label, value } => {
                *live = self.exit_live(label);
                if let Some(v) = value {
                    self.walk_expr(v, live, record);
                }
            }
            TirStmtKind::Continue => {
                *live = self.exit_live(&None);
            }
            TirStmtKind::LabeledBlock { label, block } => {
                self.exits.push(Exit {
                    label: Some(label.clone()),
                    live: live.clone(),
                });
                self.walk_block(block, live, record);
                self.exits.pop();
            }
            TirStmtKind::TaskReturn { value } => self.walk_expr(value, live, record),
            TirStmtKind::VariadicForOf { .. } => unreachable!("filtered by has_unsupported_form"),
        }
    }

    /// A loop's live-in is the least fixpoint of its body over the back-edge.
    /// The fixpoint iterations run with `record = false` (liveness only); once
    /// the loop-head set is stable, one `record` pass takes the facts.
    fn walk_loop(&mut self, body: &TirBlock, live: &mut IndexSet<u32>, record: bool) {
        let exit_live = live.clone();
        let mut head = exit_live.clone();
        loop {
            self.exits.push(Exit {
                label: None,
                live: exit_live.clone(),
            });
            let mut work = head.clone();
            self.walk_block(body, &mut work, false);
            self.exits.pop();
            let candidate = union(&work, &exit_live);
            if candidate == head {
                break;
            }
            head = candidate;
        }
        if record {
            self.exits.push(Exit {
                label: None,
                live: exit_live.clone(),
            });
            let mut work = head.clone();
            self.walk_block(body, &mut work, true);
            self.exits.pop();
            head = union(&work, &exit_live);
        }
        *live = head;
    }

    fn walk_match(
        &mut self,
        scrut: &TirExpr,
        arms: &[TirMatchArm],
        live: &mut IndexSet<u32>,
        record: bool,
    ) {
        // `live` is the match's live-*out*: a pattern binding aliases live
        // storage iff a scrutinee local is live here (excludes divergent-arm
        // reads).
        let after = live.clone();
        if record {
            let scrut_aliases_live = alias_root(scrut).is_some_and(|r| after.contains(&r));
            for arm in arms {
                let mut binds: IndexSet<u32> = IndexSet::default();
                super::analyze::collect_pattern_bindings(&arm.pattern, &mut binds);
                for b in &binds {
                    self.match_sources.push((*b, scrut.clone()));
                    if scrut_aliases_live {
                        self.aliases_live.insert(*b);
                    }
                }
            }
        }
        let mut merged: IndexSet<u32> = IndexSet::default();
        for arm in arms {
            let mut arm_live = after.clone();
            self.walk_expr(&arm.body, &mut arm_live, record);
            if let Some(guard) = &arm.guard {
                self.walk_expr(guard, &mut arm_live, record);
            }
            self.kill_pattern(&arm.pattern, &mut arm_live);
            merged = union(&merged, &arm_live);
        }
        *live = merged;
        self.walk_expr(scrut, live, record);
    }

    fn walk_expr(&mut self, expr: &TirExpr, live: &mut IndexSet<u32>, record: bool) {
        match &expr.kind {
            TirExprKind::Local { index, .. } => self.read(*index, live, record),
            TirExprKind::Assign { target, value } => {
                if let TirExprKind::Local { index, .. } = &target.kind {
                    if record {
                        self.record_alias(*index, value, live);
                        self.let_sources
                            .entry(*index)
                            .or_default()
                            .push((**value).clone());
                    }
                    live.swap_remove(index);
                    self.walk_expr(value, live, record);
                } else {
                    self.walk_expr(value, live, record);
                    self.walk_expr(target, live, record);
                }
            }
            TirExprKind::Match { expr: scrut, arms } => {
                self.walk_match(scrut, arms, live, record);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let mut then_live = live.clone();
                self.walk_block(then_branch, &mut then_live, record);
                let mut else_live = live.clone();
                if let Some(eb) = else_branch {
                    self.walk_block(eb, &mut else_live, record);
                }
                *live = union(&then_live, &else_live);
                self.walk_expr(condition, live, record);
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                self.walk_block(block, live, record);
            }
            // Calls classify each `&`/`&mut` argument as a transient borrow (see
            // `walk_call_arg`); the callee / receiver is an ordinary read.
            TirExprKind::Call { func, args, .. } => {
                if record {
                    let exprs: Vec<&TirExpr> = args.iter().map(|a| &a.expr).collect();
                    self.mark_sibling_mut_aliases(&exprs, None);
                }
                for (pos, arg) in args.iter().enumerate().rev() {
                    self.walk_call_arg(&arg.expr, Some(func), pos, live, record);
                }
            }
            TirExprKind::MethodCall {
                func,
                receiver,
                args,
                ..
            } => {
                if record {
                    let exprs: Vec<&TirExpr> = args.iter().map(|a| &a.expr).collect();
                    let recv_mut_root = self
                        .mut_receiver_methods
                        .contains(&func.module_source, &func.name)
                        .then(|| alias_root(receiver))
                        .flatten();
                    self.mark_sibling_mut_aliases(&exprs, recv_mut_root);
                }
                for (pos, arg) in args.iter().enumerate().rev() {
                    self.walk_call_arg(&arg.expr, Some(func), pos + 1, live, record);
                }
                self.walk_method_receiver(receiver, func, live, record);
            }
            TirExprKind::CmRawCall { args, .. } => {
                for (pos, arg) in args.iter().enumerate().rev() {
                    self.walk_call_arg(arg, None, pos, live, record);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                let stores = self.functor_stores(callee);
                if record {
                    let exprs: Vec<&TirExpr> = args.iter().collect();
                    self.mark_sibling_mut_aliases(&exprs, None);
                }
                for (pos, arg) in args.iter().enumerate().rev() {
                    self.walk_indirect_arg(arg, pos, &stores, live, record);
                }
                self.walk_expr(callee, live, record);
            }
            // A `&`/`&mut` reached outside a call argument (a `let r = &x`, a
            // stored / returned reference) persists past the borrow, so the
            // referent escapes and stays copied.
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } => {
                if let Some(r) = self.borrow_read(place, live, record)
                    && record
                {
                    self.mark_escaped(r, borrow_top_field(place));
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                if record {
                    let children: Vec<&TirExpr> = fields.iter().map(|f| &f.value).collect();
                    self.collect_place_moves(&children, live);
                }
                for f in fields.iter().rev() {
                    self.walk_expr(&f.value, live, record);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                if record {
                    let children: Vec<&TirExpr> = elements.iter().collect();
                    self.collect_place_moves(&children, live);
                }
                for e in elements.iter().rev() {
                    self.walk_expr(e, live, record);
                }
            }
            _ => {
                let mut children: Vec<&TirExpr> = Vec::new();
                collect_child_exprs(expr, &mut children);
                for child in children.into_iter().rev() {
                    self.walk_expr(child, live, record);
                }
            }
        }
    }
}

/// The local whose storage this expression's *result* shares, if any. A
/// whole-value `Local` or a projection of one (`.field`, `[i]`, `*ref`, a
/// transparent cast) aliases that root local; a fresh allocation — a call, a
/// construction, a literal, an arithmetic result — aliases nothing observable,
/// so returns `None`. Errs toward `Some` for unmodelled projection-like nodes
/// (over-flagging only keeps a copy).
/// The top-level field a borrow place reaches off its root local: `Some(f)` for a
/// clean projection `local.f…`, `None` for a whole-local (`local`) or an
/// imprecise place (through an index / deref / cast / variant payload), which
/// conservatively escapes every field.
fn borrow_top_field(place: &TirExpr) -> Option<u32> {
    match &place.kind {
        TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            if matches!(inner.kind, TirExprKind::Local { .. }) {
                Some(*field_index)
            } else {
                borrow_top_field(inner)
            }
        }
        _ => None,
    }
}

pub(crate) fn alias_root(expr: &TirExpr) -> Option<u32> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::Unary { expr: inner, .. } => alias_root(inner),
        TirExprKind::Index { expr: inner, .. } => alias_root(inner),
        _ => None,
    }
}

/// The root local of a pure struct-field projection chain (`base`, `base.f`,
/// `base.f.g`), or `None` if the place goes through a deref, index, cast, or
/// variant payload — those may alias storage other than a plain field, so they
/// are never treated as a clean whole-field read/move.
fn clean_root(expr: &TirExpr) -> Option<u32> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        TirExprKind::FieldAccess { expr: inner, .. } => clean_root(inner),
        _ => None,
    }
}

/// The field index applied directly to the root local of a clean projection
/// chain (`base.top.f.g` → `top`); `None` if not such a chain.
fn top_field_of(expr: &TirExpr) -> Option<u32> {
    match &expr.kind {
        TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            if matches!(inner.kind, TirExprKind::Local { .. }) {
                Some(*field_index)
            } else {
                top_field_of(inner)
            }
        }
        _ => None,
    }
}

/// Classify a literal child as a whole-value / clean-field materialization of an
/// aggregate root: `Some((base, None))` for a bare `Local`, `Some((base,
/// Some(top)))` for a clean field projection off `top`.
fn as_materialize(expr: &TirExpr) -> Option<(u32, Option<u32>)> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => Some((*index, None)),
        TirExprKind::FieldAccess { .. } => Some((clean_root(expr)?, Some(top_field_of(expr)?))),
        _ => None,
    }
}

/// The immediate operand sub-expressions of `expr`, in evaluation order. The
/// control forms (`Match` / `If` / blocks / `Assign`) are handled by the walker
/// and never routed here.
fn collect_child_exprs<'e>(expr: &'e TirExpr, out: &mut Vec<&'e TirExpr>) {
    use TirExprKind as K;
    match &expr.kind {
        K::Binary { left, right, .. } => {
            out.push(left);
            out.push(right);
        }
        K::Unary { expr: inner, .. }
        | K::Cast { expr: inner, .. }
        | K::FieldAccess { expr: inner, .. }
        | K::VariantTag { expr: inner, .. }
        | K::VariantTest { expr: inner, .. }
        | K::VariantPayload { expr: inner, .. }
        | K::TupleSpread { expr: inner }
        | K::TupleZip { expr: inner }
        | K::TupleLen { expr: inner } => out.push(inner),
        K::GlobalVarSet { value, .. } => out.push(value),
        K::Index { expr: base, index } => {
            out.push(base);
            out.push(index);
        }
        K::Call { args, .. } => {
            for a in args {
                out.push(&a.expr);
            }
        }
        K::CmRawCall { args, .. } => {
            for a in args {
                out.push(a);
            }
        }
        K::MethodCall { receiver, args, .. } => {
            out.push(receiver);
            for a in args {
                out.push(&a.expr);
            }
        }
        K::IndirectCall { callee, args } => {
            out.push(callee);
            for a in args {
                out.push(a);
            }
        }
        K::StructLiteral { fields, .. } => {
            for f in fields {
                out.push(&f.value);
            }
        }
        K::TupleLiteral { elements } => {
            for e in elements {
                out.push(e);
            }
        }
        K::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                out.push(p);
            }
        }
        K::TypePackExpansion { call_expr, .. } => out.push(call_expr),
        K::TemplateString { parts } => {
            for part in parts {
                if let crate::tir::TirTemplatePart::Interpolation { expr, .. } = part {
                    out.push(expr);
                }
            }
        }
        _ => {}
    }
}

/// A `&T` / `&mut T` parameter borrows the caller's storage, so it is never a
/// movable owned value. Everything else a function takes by value it owns.
fn is_reference_type(type_id: crate::tir::TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
    )
}

fn union(a: &IndexSet<u32>, b: &IndexSet<u32>) -> IndexSet<u32> {
    let mut out = a.clone();
    for &id in b {
        out.insert(id);
    }
    out
}
