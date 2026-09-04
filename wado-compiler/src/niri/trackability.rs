//! Which locals a walk may hold a value for.
//!
//! A walk keeps a local's value only while nothing it does not perform can
//! reach that local. What reaches a place decides it: a read changes nothing,
//! so it counts wherever it appears, while a write counts only where the walk
//! performs it — inside a compile-time frame, which either carries the write
//! out or abandons the evaluation. An ordinary walk performs nothing at all.

use crate::hashmap::IndexSet;
use crate::nir::NirUnaryOp;
use crate::nir_arena::{Body, ExprId, ExprKind, LocalSet, NodeRef, Operand, StmtId, StmtKind};

use super::place::{borrowed_place_operand, lvalue_root_local, peel_wrappers, place_of};
use crate::nir_visitor::reachable_exprs;
use crate::tir::TypeTable;

use super::{CalleeMap, CtfeBuiltinMap, ProgramFacts};

/// The expressions a walk carries out: the operand each reachable statement
/// performs. Reachable from the body root, because an orphaned statement never
/// runs — counting one as performed would credit the walk with a write it will
/// never carry out.
fn performed_exprs(body: &Body) -> IndexSet<ExprId> {
    let mut performed = IndexSet::default();
    for s in reachable_stmts(body) {
        let op = match &body.stmts[s].kind {
            StmtKind::Expr(op) | StmtKind::Let { value: op, .. } => *op,
            StmtKind::Return { .. }
            | StmtKind::If { .. }
            | StmtKind::Loop { .. }
            | StmtKind::Break { .. }
            | StmtKind::Continue
            | StmtKind::LabeledBlock { .. }
            | StmtKind::LetDestructure { .. } => continue,
        };
        if let Some(e) = op.as_expr() {
            performed.insert(e);
        }
    }
    performed
}

/// Every statement id reachable from the body root, in walk order — or every
/// statement, for a bare-expression body with no block structure.
fn reachable_stmts(body: &Body) -> Vec<StmtId> {
    if body.blocks.is_empty() {
        return body.stmts.iter().map(|(s, _)| s).collect();
    }
    let mut stmts = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node {
            stmts.push(s);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    stmts
}

/// The places a walk reaches, and how. A mention it does not reach disqualifies
/// the local that mention roots at.
#[derive(Default)]
pub(super) struct Reached {
    reads: IndexSet<ExprId>,
    writes: IndexSet<ExprId>,
    /// Reached through an `Assign`'s target rather than through an operand.
    place_assigns: IndexSet<ExprId>,
}

impl Reached {
    pub(super) fn in_frame(body: &Body, facts: ProgramFacts<'_>) -> Self {
        let performed = performed_exprs(body);
        let mut reached = Self::collect(body, facts, &performed);
        for e in &performed {
            if let ExprKind::Assign { target, .. } = &body.exprs[*e].kind
                && place_of(body, (*target).into()).is_some()
            {
                reached.place_assigns.insert(*e);
            }
        }
        reached.record_alias_borrows(body);
        reached
    }

    /// A `let` binding a borrow of a place records that place as reached: the
    /// frame resolves the binding to an alias, so a write through it is either
    /// performed against the place's current value or abandons the evaluation,
    /// and a read projects that value rather than a copy. Frame-only, like the
    /// write accounting — an ordinary walk resolves no aliases.
    fn record_alias_borrows(&mut self, body: &Body) {
        for s in reachable_stmts(body) {
            let StmtKind::Let {
                value,
                is_mut: false,
                ..
            } = &body.stmts[s].kind
            else {
                continue;
            };
            let Some((is_mut, inner)) = borrowed_place_operand(body, *value) else {
                continue;
            };
            let reach = if is_mut { Reach::Write } else { Reach::Read };
            self.record(body, inner, reach);
        }
    }

    /// An ordinary walk performs nothing, so nothing it reaches is a write it
    /// carries out. The reads are collected as [`Self::in_frame`] collects them:
    /// what a statement-position write builtin reads is read wherever that
    /// mention appears, whoever performs the write.
    pub(super) fn outside_frame(body: &Body, facts: ProgramFacts<'_>) -> Self {
        Self {
            reads: Self::collect(body, facts, &performed_exprs(body)).reads,
            ..Self::default()
        }
    }

    /// What the walk reaches, given the expressions it performs. Collected
    /// over the whole arena, not the reachable tree — [`aggregate_safe_locals`]
    /// owns the rule.
    fn collect(body: &Body, facts: ProgramFacts<'_>, performed: &IndexSet<ExprId>) -> Self {
        let mut reached = Self::default();
        reached.collect_builtin_borrows(body, facts.ctfe_builtins, performed);
        reached.collect_call_borrows(body, facts.callees, performed);
        reached
    }

    /// The engine models a builtin call exactly, so the destination it writes
    /// and the source it reads are equally current — where the frame runs it.
    fn collect_builtin_borrows(
        &mut self,
        body: &Body,
        ctfe_builtins: Option<&CtfeBuiltinMap>,
        performed: &IndexSet<ExprId>,
    ) {
        let Some(map) = ctfe_builtins else {
            return;
        };
        for (e, node) in &body.exprs {
            let ExprKind::Call { func_id, args, .. } = &node.kind else {
                continue;
            };
            let Some(builtin) = map.get(func_id) else {
                continue;
            };
            if !builtin.is_write() {
                for arg in args {
                    self.record(body, arg.expr, Reach::Read);
                }
                continue;
            }
            if !performed.contains(&e) {
                continue;
            }
            for (i, arg) in args.iter().enumerate() {
                let reach = if i == 0 { Reach::Write } else { Reach::Read };
                self.record(body, arg.expr, reach);
            }
        }
    }

    /// A shared receiver or by-value argument counts wherever it appears —
    /// that is what carries a container through the `&self` reads `push` makes
    /// of its own capacity. A `&mut` one counts only at statement position,
    /// where a frame performs the write; elsewhere the projection merely reads
    /// the call. A callee the map does not hold exempts nothing.
    fn collect_call_borrows(
        &mut self,
        body: &Body,
        callees: Option<&CalleeMap>,
        performed: &IndexSet<ExprId>,
    ) {
        let Some(callees) = callees else {
            return;
        };
        for (e, node) in &body.exprs {
            let ExprKind::Call { func_id, args, .. } = &node.kind else {
                continue;
            };
            let Some(callee) = callees.get(func_id) else {
                continue;
            };
            if args.len() != callee.arity() {
                continue;
            }
            let at_statement = performed.contains(&e);
            for (index, arg) in args.iter().enumerate() {
                match (callee.writes_param(index), at_statement) {
                    (false, _) if callee.reads_only(index) => {
                        self.record(body, arg.expr, Reach::Read);
                    }
                    (true, true) => self.record(body, arg.expr, Reach::Write),
                    (false, _) | (true, false) => {}
                }
            }
        }
    }

    /// Both directions of the mention check go through [`peel_wrappers`], so a
    /// mention recorded at a cast still matches the borrow the disqualification
    /// walk asks about.
    fn covers(&self, body: &Body, op: Operand) -> bool {
        let Some(e) = peel_wrappers(body, op) else {
            return false;
        };
        self.reads.contains(&e) || self.writes.contains(&e)
    }

    /// Records the place `op` names.
    fn record(&mut self, body: &Body, op: Operand, reach: Reach) {
        let Some(e) = peel_wrappers(body, op) else {
            return;
        };
        match reach {
            Reach::Read => self.reads.insert(e),
            Reach::Write => self.writes.insert(e),
        };
    }
}

#[derive(Clone, Copy)]
enum Reach {
    Read,
    Write,
}

/// Locals of `body` that may bind an aggregate constant: ones every mention only
/// reads. The read positions are listed rather than inferred from the absence of
/// the others, so an untaught node kind costs a fold and never a wrong one.
///
/// The two sides scan different populations. Mentions and disqualifications come
/// from the reachable body alone, an orphaned mention being unable to run. Two
/// `value_reads` sources sweep the whole arena instead: an in-place rewrite
/// shares ids between a live node and the displaced parent that held it, so a
/// mention's only witness may sit where nothing live refers to.
pub(super) fn aggregate_safe_locals(
    body: &Body,
    reached: &Reached,
    type_table: &TypeTable,
) -> LocalSet {
    fn disqualify_root(body: &Body, op: Operand, set: &mut LocalSet) {
        if let Some(index) = lvalue_root_local(body, op) {
            set.insert(index);
        }
    }
    let share_root = |body: &Body, op: Operand, set: &mut LocalSet| {
        if let Some(index) = shared_reference_root(body, op, type_table) {
            set.insert(index);
        }
    };
    fn read_value(op: Operand, reads: &mut IndexSet<ExprId>) {
        if let Some(e) = op.as_expr() {
            reads.insert(e);
        }
    }
    let mut value_reads: IndexSet<ExprId> = IndexSet::default();
    let mut local_mentions: Vec<(ExprId, u32)> = Vec::new();
    let mut disqualified = LocalSet::default();
    for e in reachable_exprs(body) {
        let node = &body.exprs[e];
        match &node.kind {
            ExprKind::Local { index, .. } => local_mentions.push((e, *index)),
            ExprKind::FieldAccess { expr, .. }
            | ExprKind::Match { expr, .. }
            | ExprKind::Switch {
                scrutinee: expr, ..
            } => read_value(*expr, &mut value_reads),
            ExprKind::Index { expr, index } => {
                read_value(*expr, &mut value_reads);
                read_value(*index, &mut value_reads);
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for element in elements {
                    read_value(*element, &mut value_reads);
                    share_root(body, *element, &mut disqualified);
                }
            }
            ExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    read_value(field.value, &mut value_reads);
                    share_root(body, field.value, &mut disqualified);
                }
            }
            ExprKind::Assign { target, value } => {
                read_value(*value, &mut value_reads);
                if !reached.place_assigns.contains(&e) {
                    disqualify_root(body, (*target).into(), &mut disqualified);
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr,
            } => {
                if !reached.covers(body, *expr) {
                    disqualify_root(body, *expr, &mut disqualified);
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr,
            }
            | ExprKind::Cast { expr, .. } => read_value(*expr, &mut value_reads),
            ExprKind::Call {
                args, has_receiver, ..
            } => {
                for (i, arg) in args.iter().enumerate() {
                    // A receiver reaches the caller's storage whatever its
                    // declared `self` mode, so it is never a passing read.
                    if !(arg.is_mut || (*has_receiver && i == 0)) {
                        read_value(arg.expr, &mut value_reads);
                    } else if !reached.covers(body, arg.expr) {
                        disqualify_root(body, arg.expr, &mut disqualified);
                    }
                }
            }
            _ => {}
        }
    }
    for (_, stmt) in &body.stmts {
        match &stmt.kind {
            StmtKind::Return { value: Some(op) }
            | StmtKind::Break {
                value: Some(op), ..
            } => {
                read_value(*op, &mut value_reads);
            }
            StmtKind::Let { value: op, .. } | StmtKind::Expr(op) => {
                read_value(*op, &mut value_reads);
            }
            _ => {}
        }
    }
    value_reads.extend(reached.reads.iter().chain(&reached.writes).copied());
    for (e, index) in &local_mentions {
        if !value_reads.contains(e) {
            disqualified.insert(*index);
        }
    }
    let mut safe = LocalSet::default();
    for (_, index) in local_mentions {
        if !disqualified.contains(index) {
            safe.insert(index);
        }
    }
    safe
}

/// Locals of `body` a compile-time frame cannot track: something other than a
/// write it performs itself can reach them — a borrow, a mutable argument, a
/// method receiver, or an assignment buried inside a larger expression.
///
/// Reachable body only, as in [`aggregate_safe_locals`]. The arena keeps every
/// node an in-place rewrite displaced, and one nothing refers to cannot run.
/// Every local an `Assign` names as its whole target. A projection into one is
/// a write through the binding, not a rebinding of it, so only a bare local
/// counts.
fn reassigned_locals(body: &Body) -> LocalSet {
    let mut set = LocalSet::default();
    for e in reachable_exprs(body) {
        if let ExprKind::Assign { target, .. } = &body.exprs[e].kind
            && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
        {
            set.insert(*index);
        }
    }
    set
}

pub(super) fn clobbered_locals(body: &Body, reached: &Reached, type_table: &TypeTable) -> LocalSet {
    fn disqualify(body: &Body, op: Operand, set: &mut LocalSet) {
        if let Some(index) = lvalue_root_local(body, op) {
            set.insert(index);
        }
    }
    let mut set = LocalSet::default();
    for e in reachable_exprs(body) {
        let node = &body.exprs[e];
        match &node.kind {
            ExprKind::Assign { target, .. } => {
                if !reached.place_assigns.contains(&e) {
                    disqualify(body, (*target).into(), &mut set);
                }
            }
            // Only a mutable borrow. A shared one cannot be written through, so
            // it cannot make the referent stale — which is the same reading
            // [`aggregate_safe_locals`] gives the node, where `&x` counts as a
            // read of `x`. A frame that refused it could not run a body whose
            // own parameter is borrowed, which every `&self` method's is.
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr,
            } => {
                if !reached.covers(body, *expr) {
                    disqualify(body, *expr, &mut set);
                }
            }
            ExprKind::Call {
                args, has_receiver, ..
            } => {
                for (i, arg) in args.iter().enumerate() {
                    if !(arg.is_mut || (*has_receiver && i == 0)) {
                        continue;
                    }
                    if !reached.covers(body, arg.expr) {
                        disqualify(body, arg.expr, &mut set);
                    }
                }
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for element in elements {
                    if let Some(index) = shared_reference_root(body, *element, type_table) {
                        set.insert(index);
                    }
                }
            }
            ExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    if let Some(index) = shared_reference_root(body, field.value, type_table) {
                        set.insert(index);
                    }
                }
            }
            _ => {}
        }
    }
    set
}

/// The local a stored reference names. An aggregate holding one is a second
/// holder of its object — a closure environment over a boxed local is the shape
/// this reaches. A value element copies, so only a reference shape answers.
fn shared_reference_root(body: &Body, op: Operand, type_table: &TypeTable) -> Option<u32> {
    let e = op.as_expr()?;
    if !type_table.is_reference_shaped(body.exprs[e].type_id) {
        return None;
    }
    let mut e = e;
    // A cast names the same storage as its operand, so it hides no holder.
    while let ExprKind::Cast { expr: inner, .. } = &body.exprs[e].kind {
        e = inner.as_expr()?;
    }
    let ExprKind::Local { index, .. } = &body.exprs[e].kind else {
        return None;
    };
    Some(*index)
}

/// What a walk of a body may hold values for: which locals may bind an
/// aggregate constant, and which ones a compile-time frame cannot track.
pub(super) struct Trackability {
    pub(super) aggregate_locals: LocalSet,
    pub(super) clobbered: LocalSet,
    /// Locals some `Assign` names as its whole target. A binding one of these
    /// carries can be displaced, so it cannot stand for a place; one nothing
    /// reassigns can, whether or not it was spelled `let mut`.
    pub(super) reassigned: LocalSet,
}

impl Trackability {
    /// For a compile-time frame, which performs the writes it walks.
    pub(super) fn in_frame(body: &Body, facts: ProgramFacts<'_>, type_table: &TypeTable) -> Self {
        let reached = Reached::in_frame(body, facts);
        Self {
            aggregate_locals: aggregate_safe_locals(body, &reached, type_table),
            clobbered: clobbered_locals(body, &reached, type_table),
            reassigned: reassigned_locals(body),
        }
    }

    /// For an ordinary walk, which performs nothing, so no write it reaches is
    /// one it carries out.
    pub(super) fn outside_frame(
        body: &Body,
        facts: ProgramFacts<'_>,
        type_table: &TypeTable,
    ) -> Self {
        let reached = Reached::outside_frame(body, facts);
        Self {
            aggregate_locals: aggregate_safe_locals(body, &reached, type_table),
            clobbered: LocalSet::default(),
            reassigned: reassigned_locals(body),
        }
    }
}
