//! Which locals a walk may hold a value for.
//!
//! A walk keeps a local's value only while nothing it does not perform can
//! reach that local. What reaches a place decides it: a read changes nothing,
//! so it counts wherever it appears, while a write counts only where the walk
//! performs it — inside a compile-time frame, which either carries the write
//! out or abandons the evaluation. An ordinary walk performs nothing at all.

use cranelift_entity::EntitySet;

use crate::hashmap::IndexMap;
use crate::nir::NirUnaryOp;
use crate::nir_arena::{
    Body, ExprId, ExprKind, LocalSet, NodeRef, Operand, PatKind, StmtId, StmtKind,
};

use super::place::{
    is_array_place, lvalue_root_local, peel_wrappers, place_of, projected_borrows,
    referenced_place_operand, write_root_local,
};
use crate::nir_visitor::reachable_exprs;
use crate::type_table::TypeTable;

use super::ProgramFacts;
use crate::compiler_trace;

/// The expressions a walk carries out: the operand each reachable statement
/// performs. Reachable from the body root, because an orphaned statement never
/// runs — counting one as performed would credit the walk with a write it will
/// never carry out.
fn performed_exprs(body: &Body) -> EntitySet<ExprId> {
    let mut performed = EntitySet::with_capacity(body.exprs.len());
    for s in reachable_stmts(body) {
        let op = match &body.stmts[s].kind {
            StmtKind::Expr(op)
            | StmtKind::Let {
                value: Some(op), ..
            } => *op,
            StmtKind::Let { value: None, .. }
            | StmtKind::Return { .. }
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

/// Every reachable statement id, in walk order.
fn reachable_stmts(body: &Body) -> Vec<StmtId> {
    let mut stmts = Vec::new();
    body.for_each_reachable_node(|node| {
        if let NodeRef::Stmt(s) = node {
            stmts.push(s);
        }
    });
    stmts
}

/// What a walk reads wherever in the arena it appears, whatever the walk
/// performs: the arguments of a builtin that writes nothing, a parameter that
/// only reads, and every statement's operand. None of them depends on the
/// body's root, so the frames over one body's regions and the runs of one
/// callee share it while the body is unchanged.
pub(super) struct ArenaReads {
    /// Recorded through [`peel_wrappers`], as [`Reached::record`] records.
    places: EntitySet<ExprId>,
    /// As written: the operand a `Return`, `Break`, `Let` or `Expr` yields.
    stmt_operands: EntitySet<ExprId>,
}

impl ArenaReads {
    pub(super) fn of(body: &Body, facts: ProgramFacts<'_>) -> Self {
        let mut places = EntitySet::with_capacity(body.exprs.len());
        let mut record = |op: Operand| {
            if let Some(e) = peel_wrappers(body, op) {
                places.insert(e);
            }
        };
        for (_, node) in &body.exprs {
            let ExprKind::Call { func_id, args, .. } = &node.kind else {
                continue;
            };
            if let Some(builtin) = facts.ctfe_builtins.and_then(|map| map.get(func_id))
                && !builtin.is_write()
            {
                for arg in args {
                    record(arg.expr);
                }
            }
            if let Some(callee) = facts.callees.and_then(|map| map.get(func_id))
                && args.len() == callee.arity()
            {
                for (index, arg) in args.iter().enumerate() {
                    if !callee.writes_param(index) && callee.reads_only(index) {
                        record(arg.expr);
                    }
                }
            }
        }
        let mut stmt_operands = EntitySet::with_capacity(body.exprs.len());
        for (_, stmt) in &body.stmts {
            let (StmtKind::Return { value: Some(op) }
            | StmtKind::Break {
                value: Some(op), ..
            }
            | StmtKind::Let {
                value: Some(op), ..
            }
            | StmtKind::Expr(op)) = &stmt.kind
            else {
                continue;
            };
            if let Some(e) = op.as_expr() {
                stmt_operands.insert(e);
            }
        }
        Self {
            places,
            stmt_operands,
        }
    }
}

/// The places a walk reaches, and how. A mention it does not reach disqualifies
/// the local that mention roots at.
pub(super) struct Reached<'r> {
    arena: &'r ArenaReads,
    /// Read beyond [`ArenaReads::places`].
    reads: EntitySet<ExprId>,
    writes: EntitySet<ExprId>,
    /// Reached through an `Assign`'s target rather than through an operand.
    place_assigns: EntitySet<ExprId>,
}

impl<'r> Reached<'r> {
    pub(super) fn in_frame(
        body: &Body,
        facts: ProgramFacts<'_>,
        arena: &'r ArenaReads,
        reassigned: &LocalSet,
        type_table: &TypeTable,
    ) -> Self {
        let performed = performed_exprs(body);
        let mut reached = Self::collect(body, facts, arena, &performed);
        for e in performed.iter() {
            if let ExprKind::Assign { target, .. } = &body.exprs[e].kind
                && place_of(body, (*target).into()).is_some()
            {
                reached.place_assigns.insert(e);
            }
        }
        reached.record_alias_refs(body, reassigned, type_table);
        reached
    }

    /// A `let` binding a reference to a place records that place as reached: the
    /// frame resolves the binding to an alias, so a write through it is either
    /// performed against the place's current value or abandons the evaluation,
    /// and a read projects that value rather than a copy. Frame-only, like the
    /// write accounting — an ordinary walk resolves no aliases.
    fn record_alias_refs(&mut self, body: &Body, reassigned: &LocalSet, type_table: &TypeTable) {
        for s in reachable_stmts(body) {
            let StmtKind::Let {
                value: Some(value),
                local_index,
                ..
            } = &body.stmts[s].kind
            else {
                continue;
            };
            // The predicate the frame binds an alias under, and the two must
            // agree: a reference the frame aliases but this walk clobbers leaves
            // the frame holding no value for the place it just aliased.
            if reassigned.contains(*local_index) {
                compiler_trace!(
                    "region_seed",
                    "alias reference skipped: let {local_index} is reassigned"
                );
                continue;
            }
            // A `&mut` in a struct literal's field is the frame's place field,
            // and reaches its place the way the reference alone would.
            if let Some(e) = value.as_expr()
                && let ExprKind::StructLiteral { fields, .. } = &body.exprs[e].kind
            {
                for field in fields {
                    if let Some((true, inner)) = referenced_place_operand(body, field.value) {
                        self.record(body, inner, Reach::Write);
                    }
                }
                continue;
            }
            if is_array_place(body, *value, type_table) {
                compiler_trace!(
                    "region_seed",
                    "array alias recorded: let {local_index} over {value:?}"
                );
                self.record(body, *value, Reach::Write);
                continue;
            }
            let Some((is_mut, inner)) = referenced_place_operand(body, *value) else {
                continue;
            };
            compiler_trace!(
                "region_seed",
                "alias reference recorded: let {local_index} over {inner:?} (mut={is_mut})"
            );
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
        facts: ProgramFacts<'_>,
        arena: &'r ArenaReads,
    ) -> Self {
        let reached = Self::collect(body, facts, arena, &performed_exprs(body));
        Self {
            writes: EntitySet::new(),
            ..reached
        }
    }

    /// What the walk reaches, given the expressions it performs, beyond what
    /// `arena` reads wherever it appears. A call is performed only at
    /// statement position, so only those reach more.
    ///
    /// The engine models a builtin call exactly, so the destination it writes
    /// and the source it reads are equally current — where the frame runs it.
    /// A callee's `&mut` argument counts only at statement position, where a
    /// frame performs the write; elsewhere the projection merely reads the call.
    /// A shared receiver or by-value one counts wherever it appears, which
    /// `arena` holds: that is what carries a container through the `&self`
    /// reads `push` makes of its own capacity. A callee the map does not hold
    /// exempts nothing.
    fn collect(
        body: &Body,
        facts: ProgramFacts<'_>,
        arena: &'r ArenaReads,
        performed: &EntitySet<ExprId>,
    ) -> Self {
        let exprs = body.exprs.len();
        let mut reached = Self {
            arena,
            reads: EntitySet::with_capacity(exprs),
            writes: EntitySet::with_capacity(exprs),
            place_assigns: EntitySet::with_capacity(exprs),
        };
        for e in performed.iter() {
            let ExprKind::Call { func_id, args, .. } = &body.exprs[e].kind else {
                continue;
            };
            if let Some(builtin) = facts.ctfe_builtins.and_then(|map| map.get(func_id))
                && builtin.is_write()
            {
                for (i, arg) in args.iter().enumerate() {
                    let reach = if i == 0 { Reach::Write } else { Reach::Read };
                    reached.record(body, arg.expr, reach);
                }
            }
            if let Some(callee) = facts.callees.and_then(|map| map.get(func_id))
                && args.len() == callee.arity()
            {
                for (index, arg) in args.iter().enumerate() {
                    if callee.writes_param(index) {
                        reached.record(body, arg.expr, Reach::Write);
                    }
                }
            }
        }
        reached
    }

    /// Both directions of the mention check go through [`peel_wrappers`], so a
    /// mention recorded at a cast still matches the reference the disqualification
    /// walk asks about.
    fn covers(&self, body: &Body, op: Operand) -> bool {
        peel_wrappers(body, op).is_some_and(|e| self.reaches(e))
    }

    /// Whether `e`, as recorded, is reached.
    fn reaches(&self, e: ExprId) -> bool {
        self.arena.places.contains(e) || self.reads.contains(e) || self.writes.contains(e)
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
/// from the reachable body alone, an orphaned mention being unable to run. The
/// reads [`ArenaReads`] holds sweep the whole arena instead: an in-place rewrite
/// shares ids between a live node and the displaced parent that held it, so a
/// mention's only witness may sit in a node nothing live refers to.
pub(super) fn aggregate_safe_locals(
    body: &Body,
    reached: &Reached,
    type_table: &TypeTable,
    const_views: &LocalSet,
) -> LocalSet {
    fn disqualify_root(body: &Body, op: Operand, set: &mut LocalSet) {
        if let Some(index) = lvalue_root_local(body, op) {
            compiler_trace!(
                "region_seed",
                "not aggregate-safe: local {index} via {op:?}"
            );
            set.insert(index);
        }
    }
    let share_root = |body: &Body, op: Operand, set: &mut LocalSet| {
        if let Some(index) = shared_reference_root(body, op, type_table, const_views) {
            compiler_trace!(
                "region_seed",
                "not aggregate-safe: local {index} shared via {op:?}"
            );
            set.insert(index);
        }
    };
    fn read_value(op: Operand, reads: &mut EntitySet<ExprId>) {
        if let Some(e) = op.as_expr() {
            reads.insert(e);
        }
    }
    let mut value_reads = EntitySet::with_capacity(body.exprs.len());
    let mut local_mentions: Vec<(ExprId, u32)> = Vec::new();
    let mut disqualified = LocalSet::default();
    let projected = projected_borrows(body);
    for e in reachable_exprs(body) {
        let node = &body.exprs[e];
        match &node.kind {
            ExprKind::Local { index, .. } => local_mentions.push((e, *index)),
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr,
            } if projected.contains(&e) => read_value(*expr, &mut value_reads),
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
                // A store the frame performs replaces a value. Over a reference
                // it would repoint an alias instead, which stays a disqualifying
                // mention.
                if !reached.place_assigns.contains(e) {
                    disqualify_root(body, (*target).into(), &mut disqualified);
                } else if !type_table.is_reference(body.exprs[*target].type_id) {
                    read_value((*target).into(), &mut value_reads);
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
            ExprKind::Call { args, .. } => {
                for (arg, reaches_storage) in args.with_storage_reach(true) {
                    if !reaches_storage {
                        read_value(arg.expr, &mut value_reads);
                    } else if !reached.covers(body, arg.expr) {
                        disqualify_root(body, arg.expr, &mut disqualified);
                    }
                }
            }
            _ => {}
        }
    }
    for (e, index) in &local_mentions {
        if !value_reads.contains(*e)
            && !reached.arena.stmt_operands.contains(*e)
            && !reached.reaches(*e)
        {
            compiler_trace!(
                "region_seed",
                "not aggregate-safe: local {index} mentioned at {e:?} outside a read position"
            );
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
/// write it performs itself can reach them — a `&mut`, a mutable argument, a
/// method receiver, or an assignment buried inside a larger expression.
///
/// Reachable body only, as in [`aggregate_safe_locals`]. The arena keeps every
/// node an in-place rewrite displaced, and one nothing refers to cannot run.
pub(super) fn clobbered_locals(
    body: &Body,
    reached: &Reached,
    type_table: &TypeTable,
    const_views: &LocalSet,
) -> LocalSet {
    fn disqualify(body: &Body, op: Operand, set: &mut LocalSet) {
        if let Some(index) = lvalue_root_local(body, op) {
            compiler_trace!("region_seed", "clobbered: local {index} via {op:?}");
            set.insert(index);
        }
    }
    let projected = projected_borrows(body);
    let mut set = LocalSet::default();
    for e in reachable_exprs(body) {
        let node = &body.exprs[e];
        match &node.kind {
            ExprKind::Assign { target, .. } => {
                if !reached.place_assigns.contains(e) {
                    disqualify(body, (*target).into(), &mut set);
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                ..
            } if projected.contains(&e) => {}
            // Only a `&mut`. A `&` cannot be written through, so
            // it cannot make the referent stale — which is the same reading
            // [`aggregate_safe_locals`] gives the node, where `&x` counts as a
            // read of `x`. A frame that refused it could not run a body whose
            // own parameter is referenced, which every `&self` method's is.
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr,
            } => {
                if !reached.covers(body, *expr) {
                    disqualify(body, *expr, &mut set);
                }
            }
            ExprKind::Call { args, .. } => {
                for (arg, reaches_storage) in args.with_storage_reach(true) {
                    if reaches_storage && !reached.covers(body, arg.expr) {
                        disqualify(body, arg.expr, &mut set);
                    }
                }
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for element in elements {
                    if let Some(index) =
                        shared_reference_root(body, *element, type_table, const_views)
                    {
                        set.insert(index);
                    }
                }
            }
            ExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    if let Some(index) =
                        shared_reference_root(body, field.value, type_table, const_views)
                    {
                        set.insert(index);
                    }
                }
            }
            _ => {}
        }
    }
    set
}

/// Every local an `Assign` names as its whole target. A projection into one is a
/// write through the binding, not a rebinding of it, so only a bare local counts.
/// Nor does a reload, which assigns the local the place its one `let` bound:
/// the binding names that place again, which the frame checks as it runs.
fn reassigned_locals(body: &Body) -> LocalSet {
    let mut let_places: IndexMap<u32, Option<(u32, Vec<u32>)>> = IndexMap::default();
    for s in reachable_stmts(body) {
        if let StmtKind::Let {
            local_index, value, ..
        } = &body.stmts[s].kind
        {
            let place = value.and_then(|value| place_of(body, value));
            let_places
                .entry(*local_index)
                .and_modify(|bound| *bound = None)
                .or_insert(place);
        }
    }
    let mut set = LocalSet::default();
    for e in reachable_exprs(body) {
        if let ExprKind::Assign { target, value } = &body.exprs[e].kind
            && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
        {
            let bound = let_places.get(index).and_then(Option::as_ref);
            if bound.is_none() || bound != place_of(body, *value).as_ref() {
                set.insert(*index);
            }
        }
    }
    set
}

/// Locals that name settled storage: every binding of one is a `&` reference
/// rooted at a literal, at a global the program holds one value for, or at a
/// local nothing writes after its one binding, and nothing rebinds it. Nothing
/// can write what such a local names, so a value read off it stands as long as
/// the body does.
///
/// The local root is the shape inlining leaves: `let r = &s.repr` over the
/// `let s = String { repr: packed"…", used: 8 }` a constant argument bound.
pub(super) fn const_view_locals(
    body: &Body,
    facts: ProgramFacts<'_>,
    reassigned: &LocalSet,
) -> LocalSet {
    let storage = unwritten_locals(body, reassigned);
    let settled = |op: Operand| {
        body.shared_ref_root(op)
            .is_some_and(|root| match &body.exprs[root].kind {
                ExprKind::PackedArray(_) => true,
                ExprKind::GlobalVarGet {
                    module_source,
                    name,
                } => facts.global_is_const(&(module_source.clone(), name.clone())),
                _ => false,
            })
            || referenced_place_operand(body, op).is_some_and(|(is_mut, inner)| {
                !is_mut && place_of(body, inner).is_some_and(|(root, _)| storage.contains(root))
            })
    };
    let mut set = LocalSet::default();
    let mut rebound = LocalSet::default();
    body.for_each_reachable_node(|node| match node {
        NodeRef::Stmt(s) => {
            if let StmtKind::Let {
                local_index, value, ..
            } = &body.stmts[s].kind
            {
                if value.is_some_and(settled) {
                    set.insert(*local_index);
                } else {
                    rebound.insert(*local_index);
                }
            }
        }
        NodeRef::Pat(p) => {
            if let PatKind::Binding { local_index, .. } = &body.pats[p].kind {
                rebound.insert(*local_index);
            }
        }
        NodeRef::Expr(_) | NodeRef::Block(_) => {}
    });
    let mut out = LocalSet::default();
    for index in set.iter() {
        if !rebound.contains(index) && !reassigned.contains(index) {
            out.insert(index);
        }
    }
    out
}

/// Locals bound once, immutably, and reached by no write: no `Assign` into
/// them, no `&mut` over them, no call argument that reaches their storage.
fn unwritten_locals(body: &Body, reassigned: &LocalSet) -> LocalSet {
    let mut set = LocalSet::default();
    let mut unsettled = LocalSet::default();
    body.for_each_reachable_node(|node| match node {
        NodeRef::Stmt(s) => {
            if let StmtKind::Let {
                local_index,
                is_mut,
                ..
            } = &body.stmts[s].kind
            {
                if !is_mut && set.insert(*local_index) {
                    return;
                }
                unsettled.insert(*local_index);
            }
        }
        NodeRef::Pat(p) => {
            if let PatKind::Binding { local_index, .. } = &body.pats[p].kind {
                unsettled.insert(*local_index);
            }
        }
        NodeRef::Expr(e) => {
            let mut written = |op: Operand| {
                if let Some(index) = write_root_local(body, op) {
                    unsettled.insert(index);
                }
            };
            match &body.exprs[e].kind {
                ExprKind::Assign { target, .. } => written((*target).into()),
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr,
                } => written(*expr),
                ExprKind::Call { args, .. } => {
                    for (arg, reaches_storage) in args.with_storage_reach(true) {
                        if reaches_storage {
                            written(arg.expr);
                        }
                    }
                }
                _ => {}
            }
        }
        NodeRef::Block(_) => {}
    });
    let mut out = LocalSet::default();
    for index in set.iter() {
        if !unsettled.contains(index) && !reassigned.contains(index) {
            out.insert(index);
        }
    }
    out
}

/// The local a stored reference names. An aggregate holding one is a second
/// holder of its object — a closure environment over a boxed local is the shape
/// this reaches. A value element copies, so only a reference shape answers, and
/// a [`const_view_locals`] one answers nothing: nothing writes what it names.
fn shared_reference_root(
    body: &Body,
    op: Operand,
    type_table: &TypeTable,
    const_views: &LocalSet,
) -> Option<u32> {
    let e = op.as_expr()?;
    if !type_table.is_reference(body.exprs[e].type_id) {
        return None;
    }
    let ExprKind::Local { index, .. } = &body.exprs[body.strip_casts(e)].kind else {
        return None;
    };
    (!const_views.contains(*index)).then_some(*index)
}

/// Locals whose object no second expression reaches: read exactly once, and
/// never referenced. A constant one may be written over at that read, since
/// nothing is left to tell the fresh literal from what the binding held —
/// where two reads, or a `&p` beside one, leave the program two objects where
/// it had one, which `ref.eq` tells apart.
///
/// Whether the copy planner will later copy a value read is not knowable here,
/// so a second read counts as sharing whatever its type.
fn unshared_locals(body: &Body) -> LocalSet {
    let mut once = LocalSet::default();
    let mut shared = LocalSet::default();
    for e in reachable_exprs(body) {
        match &body.exprs[e].kind {
            ExprKind::Local { index, .. } => {
                if !once.insert(*index) {
                    shared.insert(*index);
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                expr,
            } => {
                if let Some(index) = write_root_local(body, *expr) {
                    shared.insert(index);
                }
            }
            _ => {}
        }
    }
    let mut set = LocalSet::default();
    for index in once.iter() {
        if !shared.contains(index) {
            set.insert(index);
        }
    }
    set
}

/// What a walk of a body may hold values for: which locals may bind an
/// aggregate constant, and which ones a compile-time frame cannot track.
pub(super) struct Trackability {
    pub(super) aggregate_locals: LocalSet,
    /// See [`unshared_locals`].
    pub(super) unshared: LocalSet,
    pub(super) clobbered: LocalSet,
    /// Locals some `Assign` names as its whole target. A binding one of these
    /// carries can be displaced, so it cannot stand for a place; one nothing
    /// reassigns can, whether or not it was spelled `let mut`.
    pub(super) reassigned: LocalSet,
    /// See [`const_view_locals`].
    pub(super) const_views: LocalSet,
}

impl Trackability {
    /// For a compile-time frame, which performs the writes it walks.
    /// `arena` is [`ArenaReads::of`] a body with `body`'s nodes.
    pub(super) fn in_frame(
        body: &Body,
        facts: ProgramFacts<'_>,
        arena: &ArenaReads,
        type_table: &TypeTable,
    ) -> Self {
        let reassigned = reassigned_locals(body);
        let reached = Reached::in_frame(body, facts, arena, &reassigned, type_table);
        let const_views = const_view_locals(body, facts, &reassigned);
        Self {
            aggregate_locals: aggregate_safe_locals(body, &reached, type_table, &const_views),
            unshared: unshared_locals(body),
            clobbered: clobbered_locals(body, &reached, type_table, &const_views),
            reassigned,
            const_views,
        }
    }

    /// For an ordinary walk, which performs nothing, so no write it reaches is
    /// one it carries out.
    pub(super) fn outside_frame(
        body: &Body,
        facts: ProgramFacts<'_>,
        type_table: &TypeTable,
    ) -> Self {
        let arena = ArenaReads::of(body, facts);
        let reached = Reached::outside_frame(body, facts, &arena);
        let reassigned = reassigned_locals(body);
        let const_views = const_view_locals(body, facts, &reassigned);
        Self {
            aggregate_locals: aggregate_safe_locals(body, &reached, type_table, &const_views),
            unshared: unshared_locals(body),
            clobbered: LocalSet::default(),
            reassigned,
            const_views,
        }
    }
}
