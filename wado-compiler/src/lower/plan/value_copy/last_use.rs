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
use super::ownership::{OwnedCalls, func_key};
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::FunctionId;
use crate::tir::{
    FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirMatchArm,
    TirPattern, TirStmt, TirStmtKind, TirUnaryOp, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// Local indices the fold moves rather than copies. Empty for a bodyless
/// function or one whose control forms (closure / handler / resume) defeat the
/// single-observation argument.
pub fn compute_move_eligible(
    func: &TirFunction,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
    functions_with_stores: &IndexSet<FunctionId>,
) -> IndexSet<u32> {
    let Some(body) = &func.body else {
        return IndexSet::default();
    };
    if has_unsupported_form(body) {
        return IndexSet::default();
    }

    let mut all_locals: IndexSet<u32> = (0..func.local_count).collect();
    // Guard against a local_count that lags a grown local set.
    let mut scan = MaxLocal { max: 0 };
    scan.visit_block(body);
    for i in 0..=scan.max {
        all_locals.insert(i);
    }

    let mut a = Analyzer {
        functions_with_stores,
        non_final: IndexSet::default(),
        aliases_live: IndexSet::default(),
        borrow_escaped: IndexSet::default(),
        let_sources: IndexMap::default(),
        match_sources: Vec::new(),
        exits: Vec::new(),
        all_locals,
    };
    let mut live = IndexSet::default();
    a.walk_block(body, &mut live, true);

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
    // seeded. (This intraprocedural move set is distinct from the interprocedural
    // return convention, which must stay conservative — a stored-then-returned
    // parameter aliases the store, so params are not owned there.)
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
                && !a.borrow_escaped.contains(idx)
        })
        .collect();
    owned
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
    functions_with_stores: &'a IndexSet<FunctionId>,
    non_final: IndexSet<u32>,
    aliases_live: IndexSet<u32>,
    /// Locals a persisting reference is taken of — a `&`/`&mut` that is not a
    /// transient call argument, or is passed to a callee that may store it. Such
    /// a local may be observed through the reference after its move, so it stays
    /// copied.
    borrow_escaped: IndexSet<u32>,
    let_sources: IndexMap<u32, Vec<TirExpr>>,
    match_sources: Vec<(u32, TirExpr)>,
    exits: Vec<Exit>,
    all_locals: IndexSet<u32>,
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

    /// Process a call's argument. An explicit `&`/`&mut` argument is a transient
    /// borrow unless the callee may store it (`functions_with_stores`), in which
    /// case the referent escapes; every other argument is an ordinary value.
    fn walk_call_arg(
        &mut self,
        arg: &TirExpr,
        callee: Option<&FunctionRef>,
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
                && callee.is_some_and(|c| {
                    self.functions_with_stores
                        .contains(&func_key(&c.module_source, &c.name))
                })
            {
                self.borrow_escaped.insert(r);
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

    fn kill_pattern(&self, pat: &TirPattern, live: &mut IndexSet<u32>) {
        let mut binds: IndexSet<u32> = IndexSet::default();
        super::analyze::collect_pattern_bindings(pat, &mut binds);
        for b in binds {
            live.swap_remove(&b);
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
                for arg in args.iter().rev() {
                    self.walk_call_arg(&arg.expr, Some(func), live, record);
                }
            }
            TirExprKind::MethodCall {
                func,
                receiver,
                args,
                ..
            } => {
                for arg in args.iter().rev() {
                    self.walk_call_arg(&arg.expr, Some(func), live, record);
                }
                self.walk_expr(receiver, live, record);
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args.iter().rev() {
                    self.walk_call_arg(arg, None, live, record);
                }
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
                    self.borrow_escaped.insert(r);
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
fn alias_root(expr: &TirExpr) -> Option<u32> {
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
