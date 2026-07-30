//! Which locals a walk may hold a value for.
//!
//! A walk keeps a local's value only while nothing it does not perform can
//! reach that local. What reaches a place decides it: a read changes nothing,
//! so it counts wherever it appears, while a write counts only where the walk
//! performs it — inside a compile-time frame, which either carries the write
//! out or abandons the evaluation. An ordinary walk performs nothing at all.

use indexmap::IndexSet;

use crate::nir::NirUnaryOp;
use crate::nir_arena::{Body, ExprId, ExprKind, LocalSet, NodeRef, Operand, StmtId, StmtKind};

use super::place::{borrowed_place_operand, lvalue_root_local, place_of};
use super::{CalleeMap, CtfeBuiltinMap, reachable_exprs};

/// A method's receiver is its first parameter.
const RECEIVER: usize = 0;

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
    pub(super) fn in_frame(
        body: &Body,
        ctfe_builtins: Option<&CtfeBuiltinMap>,
        callees: Option<&CalleeMap>,
    ) -> Self {
        let performed = performed_exprs(body);
        let mut reached = Self::collect(body, ctfe_builtins, callees, &performed);
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
    pub(super) fn outside_frame(
        body: &Body,
        ctfe_builtins: Option<&CtfeBuiltinMap>,
        callees: Option<&CalleeMap>,
    ) -> Self {
        Self {
            reads: Self::collect(body, ctfe_builtins, callees, &performed_exprs(body)).reads,
            ..Self::default()
        }
    }

    /// What the walk reaches, given the expressions it performs.
    fn collect(
        body: &Body,
        ctfe_builtins: Option<&CtfeBuiltinMap>,
        callees: Option<&CalleeMap>,
        performed: &IndexSet<ExprId>,
    ) -> Self {
        let mut reached = Self::default();
        reached.collect_builtin_borrows(body, ctfe_builtins, performed);
        reached.collect_call_borrows(body, callees, performed);
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
            let (func_id, receiver, args) = match &node.kind {
                ExprKind::Call { func_id, args, .. } => (func_id, None, args),
                ExprKind::MethodCall {
                    func_id,
                    receiver,
                    args,
                    ..
                } => (func_id, Some(*receiver), args),
                _ => continue,
            };
            let Some(callee) = callees.get(func_id) else {
                continue;
            };
            let first_arg = usize::from(receiver.is_some());
            if first_arg + args.len() != callee.arity() {
                continue;
            }
            let at_statement = performed.contains(&e);
            if let Some(receiver) = receiver {
                match (callee.writes_param(RECEIVER), at_statement) {
                    (false, _) if callee.reads_only(RECEIVER) => {
                        self.record(body, receiver, Reach::Read);
                    }
                    (true, true) => self.record(body, receiver, Reach::Write),
                    (false, _) | (true, false) => {}
                }
            }
            for (i, arg) in args.iter().enumerate() {
                let index = first_arg + i;
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

    /// Both directions of the mention check peel casts as well as borrows:
    /// an argument reaches a builtin as `&x.repr as &Array<u8>`, and a
    /// mention recorded at the cast would never match the borrow the
    /// disqualification walk asks about.
    fn covers(&self, body: &Body, op: Operand) -> bool {
        let mut op = op;
        loop {
            let Some(e) = op.as_expr() else {
                return false;
            };
            match &body.exprs[e].kind {
                ExprKind::Unary {
                    op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
                    expr,
                }
                | ExprKind::Cast { expr, .. } => op = *expr,
                _ => return self.reads.contains(&e) || self.writes.contains(&e),
            }
        }
    }

    /// Records the place `op` names, peeling the borrows and casts it may be
    /// wrapped in.
    fn record(&mut self, body: &Body, op: Operand, reach: Reach) {
        let Some(e) = op.as_expr() else {
            return;
        };
        match &body.exprs[e].kind {
            ExprKind::Unary {
                op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
                expr,
            }
            | ExprKind::Cast { expr, .. } => self.record(body, *expr, reach),
            _ => {
                match reach {
                    Reach::Read => self.reads.insert(e),
                    Reach::Write => self.writes.insert(e),
                };
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Reach {
    Read,
    Write,
}

/// Locals of `body` that may bind an aggregate constant: ones every mention
/// only reads.
///
/// The read positions are listed rather than inferred from the absence of the
/// others, so a node kind nobody taught this walk about costs a fold and never
/// a wrong one.
///
/// Only the reachable body is scanned: a mention an earlier rewrite orphaned
/// cannot run, so it must not disqualify anything.
pub(super) fn aggregate_safe_locals(body: &Body, reached: &Reached) -> LocalSet {
    fn disqualify_root(body: &Body, op: Operand, set: &mut LocalSet) {
        if let Some(index) = lvalue_root_local(body, op) {
            set.insert(index);
        }
    }
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
                }
            }
            ExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    read_value(field.value, &mut value_reads);
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
            ExprKind::MethodCall { receiver, args, .. } => {
                if !reached.covers(body, *receiver) {
                    disqualify_root(body, *receiver, &mut disqualified);
                }
                for arg in args {
                    if !arg.is_mut {
                        read_value(arg.expr, &mut value_reads);
                    } else if !reached.covers(body, arg.expr) {
                        disqualify_root(body, arg.expr, &mut disqualified);
                    }
                }
            }
            ExprKind::Call { args, .. } => {
                for arg in args {
                    if !arg.is_mut {
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
            StmtKind::Return { value: Some(op) } => read_value(*op, &mut value_reads),
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
pub(super) fn clobbered_locals(body: &Body, reached: &Reached) -> LocalSet {
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
            ExprKind::Unary {
                op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                expr,
            } => {
                if !reached.covers(body, *expr) {
                    disqualify(body, *expr, &mut set);
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                if !reached.covers(body, *receiver) {
                    disqualify(body, *receiver, &mut set);
                }
                for arg in args.iter().filter(|a| a.is_mut) {
                    if !reached.covers(body, arg.expr) {
                        disqualify(body, arg.expr, &mut set);
                    }
                }
            }
            ExprKind::Call { args, .. } => {
                for arg in args.iter().filter(|a| a.is_mut) {
                    if !reached.covers(body, arg.expr) {
                        disqualify(body, arg.expr, &mut set);
                    }
                }
            }
            _ => {}
        }
    }
    set
}
