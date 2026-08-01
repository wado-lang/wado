//! Which locals a walk may hold a value for.
//!
//! A walk keeps a local's value only while nothing it does not perform can
//! reach that local. What reaches a place decides it: a read changes nothing,
//! so it counts wherever it appears, while a write counts only where the walk
//! performs it — inside a compile-time frame, which either carries the write
//! out or abandons the evaluation. An ordinary walk performs nothing at all.
//!
//! Two questions come out of that, and [`Trackability`] answers both from a
//! single walk: which locals may bind an aggregate constant, and which ones a
//! frame cannot track at all. They differ in one place only — a shared borrow
//! is a read for the first and an escape for the second — so they are scanned
//! together and separated at the end.

use indexmap::IndexSet;

use crate::nir::NirUnaryOp;
use crate::nir_arena::{Body, ExprId, ExprKind, LocalSet, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_visitor::{NirRefVisitor, reachable_exprs};

use super::callee::CallSite;
use super::place::{borrowed_place_operand, lvalue_root_local, place_of};
use super::ProgramFacts;

/// Every node id reachable from the body root, walked once and shared by every
/// question below — each of which is another scan over these same two lists.
///
/// Reachable only, because an orphaned node never runs: counting one as
/// performed would credit the walk with a write it will never carry out, and
/// counting one as a read would grant a local a value nothing keeps current.
/// A bare-expression body has no block structure, so everything it holds is
/// reachable by construction.
struct Reachable {
    exprs: Vec<ExprId>,
    stmts: Vec<StmtId>,
}

impl Reachable {
    fn of(body: &Body) -> Self {
        #[derive(Default)]
        struct Collect {
            exprs: Vec<ExprId>,
            stmts: Vec<StmtId>,
        }
        impl NirRefVisitor for Collect {
            fn visit_node(&mut self, body: &Body, node: NodeRef) {
                match node {
                    NodeRef::Expr(e) => self.exprs.push(e),
                    NodeRef::Stmt(s) => self.stmts.push(s),
                    NodeRef::Block(_) | NodeRef::Pat(_) => {}
                }
                self.walk_node(body, node);
            }
        }
        if body.blocks.is_empty() {
            return Self {
                exprs: reachable_exprs(body),
                stmts: body.stmts.iter().map(|(s, _)| s).collect(),
            };
        }
        let mut collect = Collect::default();
        collect.visit_node(body, NodeRef::Block(body.root));
        Self {
            exprs: collect.exprs,
            stmts: collect.stmts,
        }
    }
}

/// What a walk of a body may hold values for.
pub(super) struct Trackability {
    /// Locals that may bind an aggregate constant: ones every mention only
    /// reads.
    pub(super) aggregate_locals: LocalSet,
    /// Locals a compile-time frame cannot track: something other than a write
    /// it performs itself can reach them. Empty outside a frame, which
    /// performs nothing and so tracks nothing to begin with.
    pub(super) clobbered: LocalSet,
}

impl Trackability {
    /// For a compile-time frame, which performs the writes it walks.
    pub(super) fn in_frame(body: &Body, facts: ProgramFacts<'_>) -> Self {
        let reachable = Reachable::of(body);
        let reached = Reached::in_frame(body, &reachable, facts);
        let scan = MentionScan::of(body, &reachable, &reached);
        Self {
            aggregate_locals: scan.aggregate_safe_locals(&reached),
            clobbered: scan.clobbered_locals(),
        }
    }

    /// For an ordinary walk, which performs nothing, so no write it reaches is
    /// one it carries out.
    pub(super) fn outside_frame(body: &Body, facts: ProgramFacts<'_>) -> Self {
        let reachable = Reachable::of(body);
        let reached = Reached::outside_frame(body, &reachable, facts);
        let scan = MentionScan::of(body, &reachable, &reached);
        Self {
            aggregate_locals: scan.aggregate_safe_locals(&reached),
            clobbered: LocalSet::default(),
        }
    }
}

/// The expressions a walk carries out: the operand each reachable statement
/// performs.
fn performed_exprs(body: &Body, reachable: &Reachable) -> IndexSet<ExprId> {
    let mut performed = IndexSet::default();
    for s in &reachable.stmts {
        let op = match &body.stmts[*s].kind {
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

/// The places a walk reaches, and how. A mention it does not reach disqualifies
/// the local that mention roots at.
#[derive(Default)]
struct Reached {
    reads: IndexSet<ExprId>,
    writes: IndexSet<ExprId>,
    /// Reached through an `Assign`'s target rather than through an operand.
    place_assigns: IndexSet<ExprId>,
}

impl Reached {
    fn in_frame(body: &Body, reachable: &Reachable, facts: ProgramFacts<'_>) -> Self {
        let performed = performed_exprs(body, reachable);
        let mut reached = Self::collect(body, reachable, facts, &performed);
        for e in &performed {
            if let ExprKind::Assign { target, .. } = &body.exprs[*e].kind
                && place_of(body, (*target).into()).is_some()
            {
                reached.place_assigns.insert(*e);
            }
        }
        reached.record_alias_borrows(body, reachable);
        reached
    }

    /// A `let` binding a borrow of a place records that place as reached: the
    /// frame resolves the binding to an alias, so a write through it is either
    /// performed against the place's current value or abandons the evaluation,
    /// and a read projects that value rather than a copy. Frame-only, like the
    /// write accounting — an ordinary walk resolves no aliases.
    fn record_alias_borrows(&mut self, body: &Body, reachable: &Reachable) {
        for s in &reachable.stmts {
            let StmtKind::Let {
                value,
                is_mut: false,
                ..
            } = &body.stmts[*s].kind
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
    /// carries out. The reads are collected as [`Self::in_frame`] collects
    /// them: what a statement-position write builtin reads is read wherever
    /// that mention appears, whoever performs the write.
    fn outside_frame(body: &Body, reachable: &Reachable, facts: ProgramFacts<'_>) -> Self {
        let performed = performed_exprs(body, reachable);
        Self {
            reads: Self::collect(body, reachable, facts, &performed).reads,
            ..Self::default()
        }
    }

    /// What the walk reaches, given the expressions it performs.
    fn collect(
        body: &Body,
        reachable: &Reachable,
        facts: ProgramFacts<'_>,
        performed: &IndexSet<ExprId>,
    ) -> Self {
        let mut reached = Self::default();
        for e in &reachable.exprs {
            reached.collect_builtin_borrows(body, *e, facts, performed);
            reached.collect_call_borrows(body, *e, facts, performed);
        }
        reached
    }

    /// The engine models a builtin call exactly, so the destination it writes
    /// and the source it reads are equally current — where the frame runs it.
    fn collect_builtin_borrows(
        &mut self,
        body: &Body,
        e: ExprId,
        facts: ProgramFacts<'_>,
        performed: &IndexSet<ExprId>,
    ) {
        let ExprKind::Call { func_id, args, .. } = &body.exprs[e].kind else {
            return;
        };
        let Some(builtin) = facts.ctfe_builtins.and_then(|m| m.get(func_id)) else {
            return;
        };
        if !builtin.is_write() {
            for arg in args {
                self.record(body, arg.expr, Reach::Read);
            }
            return;
        }
        if !performed.contains(&e) {
            return;
        }
        for (i, arg) in args.iter().enumerate() {
            let reach = if i == 0 { Reach::Write } else { Reach::Read };
            self.record(body, arg.expr, reach);
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
        e: ExprId,
        facts: ProgramFacts<'_>,
        performed: &IndexSet<ExprId>,
    ) {
        let Some(site) = CallSite::of(body, e) else {
            return;
        };
        let Some(callee) = facts.callees.and_then(|m| m.get(&site.func_id)) else {
            return;
        };
        let Some(operands) = site.matching_operands(callee) else {
            return;
        };
        let at_statement = performed.contains(&e);
        for (index, op) in operands {
            match (callee.writes_param(index), at_statement) {
                (false, _) if callee.reads_only(index) => self.record(body, op, Reach::Read),
                (true, true) => self.record(body, op, Reach::Write),
                (false, _) | (true, false) => {}
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

/// One walk of the reachable nodes, answering what every mention of a local
/// does with it.
///
/// The read positions are listed rather than inferred from the absence of the
/// others, so a node kind nobody taught this walk about costs a fold and never
/// a wrong one. A shared borrow and a cast are read positions that pass the
/// read on to their operand — Wado has no interior mutability, and a cast names
/// the same storage — which is what lets `push_str(&b)` count as a read of `b`
/// rather than an unknown mention.
#[derive(Default)]
struct MentionScan {
    /// Locals something other than a write the walk performs can reach.
    escaped: LocalSet,
    /// Locals reached by a shared borrow the walk does not account for. An
    /// escape for a frame, which tracks a local's value; a plain read for the
    /// aggregate question, which only asks whether anything writes it.
    shared_borrow_escaped: LocalSet,
    /// Operands read for their value.
    value_reads: IndexSet<ExprId>,
    /// Every `Local` node, with the local it names.
    local_mentions: Vec<(ExprId, u32)>,
}

impl MentionScan {
    fn of(body: &Body, reachable: &Reachable, reached: &Reached) -> Self {
        let mut scan = Self::default();
        for e in &reachable.exprs {
            scan.visit_expr(body, *e, reached);
        }
        for s in &reachable.stmts {
            match &body.stmts[*s].kind {
                StmtKind::Return { value: Some(op) }
                | StmtKind::Break {
                    value: Some(op), ..
                }
                | StmtKind::Let { value: op, .. }
                | StmtKind::Expr(op) => scan.read_value(*op),
                StmtKind::Return { value: None }
                | StmtKind::Break { value: None, .. }
                | StmtKind::If { .. }
                | StmtKind::Loop { .. }
                | StmtKind::Continue
                | StmtKind::LabeledBlock { .. }
                | StmtKind::LetDestructure { .. } => {}
            }
        }
        scan
    }

    fn visit_expr(&mut self, body: &Body, e: ExprId, reached: &Reached) {
        match &body.exprs[e].kind {
            ExprKind::Local { index, .. } => self.local_mentions.push((e, *index)),
            ExprKind::FieldAccess { expr, .. }
            | ExprKind::Match { expr, .. }
            | ExprKind::Switch {
                scrutinee: expr, ..
            } => self.read_value(*expr),
            ExprKind::Index { expr, index } => {
                self.read_value(*expr);
                self.read_value(*index);
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for element in elements {
                    self.read_value(*element);
                }
            }
            ExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.read_value(field.value);
                }
            }
            ExprKind::Assign { target, value } => {
                self.read_value(*value);
                if !reached.place_assigns.contains(&e) {
                    escape(body, (*target).into(), &mut self.escaped);
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr,
            } => {
                if !reached.covers(body, *expr) {
                    escape(body, *expr, &mut self.escaped);
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr,
            } => {
                self.read_value(*expr);
                if !reached.covers(body, *expr) {
                    escape(body, *expr, &mut self.shared_borrow_escaped);
                }
            }
            ExprKind::Cast { expr, .. } => self.read_value(*expr),
            ExprKind::MethodCall { receiver, args, .. } => {
                if !reached.covers(body, *receiver) {
                    escape(body, *receiver, &mut self.escaped);
                }
                self.visit_args(body, args, reached);
            }
            ExprKind::Call { args, .. } => self.visit_args(body, args, reached),
            _ => {}
        }
    }

    fn visit_args(
        &mut self,
        body: &Body,
        args: &[crate::nir_arena::ArenaCallArg],
        reached: &Reached,
    ) {
        for arg in args {
            if !arg.is_mut {
                self.read_value(arg.expr);
            } else if !reached.covers(body, arg.expr) {
                escape(body, arg.expr, &mut self.escaped);
            }
        }
    }

    fn read_value(&mut self, op: Operand) {
        if let Some(e) = op.as_expr() {
            self.value_reads.insert(e);
        }
    }

    /// Locals that may bind an aggregate constant: ones every mention only
    /// reads. A shared borrow is one of those reads.
    fn aggregate_safe_locals(&self, reached: &Reached) -> LocalSet {
        let mut disqualified = self.escaped.clone();
        for (e, index) in &self.local_mentions {
            let read = self.value_reads.contains(e)
                || reached.reads.contains(e)
                || reached.writes.contains(e);
            if !read {
                disqualified.insert(*index);
            }
        }
        let mut safe = LocalSet::default();
        for (_, index) in &self.local_mentions {
            if !disqualified.contains(*index) {
                safe.insert(*index);
            }
        }
        safe
    }

    /// Locals a compile-time frame cannot track: a borrow — shared or mutable
    /// — a mutable argument, a method receiver, or an assignment buried inside
    /// a larger expression can reach them.
    fn clobbered_locals(&self) -> LocalSet {
        let mut set = self.escaped.clone();
        for index in self.shared_borrow_escaped.iter() {
            set.insert(index);
        }
        set
    }
}

/// Record the local an unaccounted-for mention roots at.
fn escape(body: &Body, op: Operand, set: &mut LocalSet) {
    if let Some(index) = lvalue_root_local(body, op) {
        set.insert(index);
    }
}
