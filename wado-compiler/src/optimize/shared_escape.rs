//! Whether a constant aggregate stays safe to share once a callee stashes it
//! into the heap: nothing in the program writes through the object. See the WEP.

use std::cell::{Cell, RefCell};
use std::ops::ControlFlow;

use crate::compiler_trace;
use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, FunctionRef, NirFunction};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand, PatId, PatKind, StmtKind};
use crate::nir_package::NirPackage;
use crate::tir::TypeTable;

use super::arena_query::{bare_promoted_local, collect_pattern_bindings, holds_reference};

use cranelift_entity::EntityRef;

/// A place a tainted value can come to rest, and so a set of reads to taint.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Slot {
    /// Every `_.name` read in the program. A name is one key however the
    /// receiver is spelled, so no wrapper type hides a read from the scan.
    Field(String),
    /// One callee's parameter, tainted inside that callee's body alone.
    Param(FuncId, usize),
    /// One callee's result, tainted at every call of it.
    Ret(FuncId),
}

/// Memoized [`Slot`] verdicts over one `NirPackage`.
///
/// Every query reads bodies and writes none, so it shares its borrows with the
/// caller's: a pass asks while walking a body of its own, and both hold a
/// `Ref`. Asking one while a `RefMut` is out is the caller's bug — answering it
/// with a verdict would make the analysis depend on who else held a borrow.
pub(super) struct SharedEscape<'a> {
    project: &'a NirPackage,
    /// Settled verdicts, which no assumption stands behind.
    verdicts: RefCell<IndexMap<Slot, bool>>,
    /// The slots being computed. One asked for again reads `true`, the sound
    /// seed for a safety property: a cycle carrying no write really does hold.
    in_flight: RefCell<IndexSet<Slot>>,
    /// Whether the query in progress leaned on such an assumption.
    assumed: Cell<bool>,
    /// Per-body seed census, built on first ask. Every slot is asked of every
    /// body, so rediscovering one body's reads per slot is quadratic.
    census: RefCell<IndexMap<usize, BodyCensus>>,
}

impl<'a> SharedEscape<'a> {
    pub(super) fn new(project: &'a NirPackage) -> Self {
        Self {
            project,
            verdicts: RefCell::new(IndexMap::default()),
            in_flight: RefCell::new(IndexSet::default()),
            assumed: Cell::new(false),
            census: RefCell::new(IndexMap::default()),
        }
    }

    /// Whether `body` can read the slot at all. A function that cannot is left
    /// alone, so an unrelated one never refuses the query.
    fn can_read(
        &self,
        func_idx: usize,
        body: &Body,
        seed_field: Option<&str>,
        seed_call: Option<FuncId>,
    ) -> bool {
        let mut census = self.census.borrow_mut();
        census
            .entry(func_idx)
            .or_insert_with(|| BodyCensus::of(body, &self.project.type_table.borrow()))
            .can_read(seed_field, seed_call)
    }

    /// Whether a constant handed to `func_id`'s parameter at `pos` may be
    /// shared by every call: no write reaches it, through the callee or
    /// through anywhere the callee leaves it.
    pub(super) fn param_shareable(&self, func_id: FuncId, pos: usize) -> bool {
        self.slot_ok(&Slot::Param(func_id, pos))
    }

    fn slot_ok(&self, slot: &Slot) -> bool {
        if let Some(&cached) = self.verdicts.borrow().get(slot) {
            return cached;
        }
        if self.in_flight.borrow().contains(slot) {
            self.assumed.set(true);
            return true;
        }
        self.in_flight.borrow_mut().insert(slot.clone());
        let outer_assumed = self.assumed.replace(false);
        let verdict = self.compute_slot(slot);
        let assumed = self.assumed.get();
        self.in_flight.borrow_mut().swap_remove(slot);
        // A verdict resting on a cycle's assumption is only as good as the
        // query that made it: cache it and a later refutation of the cycle
        // would leave it stale.
        if !assumed {
            self.verdicts.borrow_mut().insert(slot.clone(), verdict);
        }
        self.assumed.set(outer_assumed || assumed);
        verdict
    }

    fn compute_slot(&self, slot: &Slot) -> bool {
        if let Some(declared) = self.declared_slot(slot) {
            return declared;
        }
        let mut obligations: IndexSet<Slot> = IndexSet::default();
        for (idx, func) in self.project.functions.iter().enumerate() {
            let func = func.borrow();
            if !self.scan_function(slot, idx, &func, &mut obligations) {
                compiler_trace!("shared_escape", "{slot:?} refused in {}", func.name);
                return false;
            }
        }
        compiler_trace!("shared_escape", "{slot:?} clear, owes {obligations:?}");
        obligations.iter().all(|next| self.slot_ok(next))
    }

    /// The verdict for a parameter of a bodyless owner, which the program walk
    /// would clear having looked at nothing. `None` leaves it to that walk.
    fn declared_slot(&self, slot: &Slot) -> Option<bool> {
        // A bodyless owner's result is no dead end, so `Slot::Ret` stays with
        // the walk: a pass-through hands the object to its caller, and the walk
        // is what reads it there.
        let Slot::Param(id, pos) = slot else {
            return None;
        };
        let owner = self.project.functions[id.index()].borrow();
        if owner.body.is_some() {
            return None;
        }
        let clauses = self.declared_arg(&owner, *pos);
        let verdict = clauses.reads && (!clauses.hands_back || self.slot_ok(&Slot::Ret(*id)));
        compiler_trace!(
            "shared_escape",
            "{slot:?} bodyless `{}` declares {verdict}",
            owner.name
        );
        Some(verdict)
    }

    /// Taint `func`'s body from `slot` and check every use, collecting the
    /// slots a stored tainted value raises. `false` refuses the query.
    fn scan_function(
        &self,
        slot: &Slot,
        func_idx: usize,
        func: &NirFunction,
        obligations: &mut IndexSet<Slot>,
    ) -> bool {
        let Some(body) = func.body.as_ref() else {
            return true;
        };
        let seed_field = match slot {
            Slot::Field(name) => Some(name.as_str()),
            Slot::Param(..) | Slot::Ret(_) => None,
        };
        let seed_call = match slot {
            Slot::Ret(id) => Some(*id),
            Slot::Field(_) | Slot::Param(..) => None,
        };
        let mut locals: IndexSet<u32> = IndexSet::default();
        if let Slot::Param(id, pos) = slot
            && id.index() == func_idx
        {
            // A position this body declares no parameter for is an argument
            // the walk cannot name, and an argument it cannot name is one
            // whose writes it cannot see.
            let Some(param) = func.params.get(*pos) else {
                return false;
            };
            locals.insert(param.local_index);
        }
        if locals.is_empty() {
            // A `Param` slot seeds its owner's body alone. Every other body is
            // left without looking, there being no name for a seed to match.
            if seed_field.is_none() && seed_call.is_none() {
                return true;
            }
            if !self.can_read(func_idx, body, seed_field, seed_call) {
                return true;
            }
        }
        let type_table = self.project.type_table.borrow();
        let mut taint = Taint {
            body,
            type_table: &type_table,
            seed_field,
            seed_call,
            locals,
            exprs: IndexSet::default(),
        };
        taint.saturate();
        let raised_from = obligations.len();
        let ok = self.check_uses(&taint, FuncId::new(func_idx), obligations);
        // Names the body each obligation came out of, which the `FuncId` in the
        // slot itself does not.
        if obligations.len() != raised_from {
            compiler_trace!(
                "shared_escape",
                "{slot:?} through `{}` raises {:?}",
                func.name,
                &obligations.as_slice()[raised_from..]
            );
        }
        ok
    }

    /// Every use of a tainted value, refused or turned into an obligation. A
    /// `Return` hands the object to every caller, which is a slot of its own.
    fn check_uses(
        &self,
        taint: &Taint<'_>,
        owner: FuncId,
        obligations: &mut IndexSet<Slot>,
    ) -> bool {
        let body = taint.body;
        let mut ok = true;
        let mut returns_tainted = false;
        body.for_each_reachable_node(|node| {
            if !ok {
                return;
            }
            ok = match node {
                NodeRef::Stmt(s) => {
                    if let StmtKind::Return { value } = &body.stmts[s].kind
                        && value.is_some_and(|v| taint.operand(v))
                    {
                        returns_tainted = true;
                    }
                    true
                }
                NodeRef::Expr(e) => self.check_expr_use(taint, e, obligations),
                NodeRef::Block(_) | NodeRef::Pat(_) => true,
            };
        });
        // A body whose last statement is its value returns without a `Return`.
        if let Some(&last) = body.blocks[body.root].stmts.last()
            && let StmtKind::Expr(op) = &body.stmts[last].kind
            && taint.operand(*op)
        {
            returns_tainted = true;
        }
        if returns_tainted {
            obligations.insert(Slot::Ret(owner));
        }
        ok
    }

    fn check_expr_use(
        &self,
        taint: &Taint<'_>,
        expr: ExprId,
        obligations: &mut IndexSet<Slot>,
    ) -> bool {
        let body = taint.body;
        match &body.exprs[expr].kind {
            // Writing through a tainted place writes the shared object. The
            // test is on the place's receiver chain, not on the target itself:
            // `v.grad = …` names an `f64`, which no taint ever reaches, while
            // the `v` it writes through may be the shared object.
            ExprKind::Assign { target, value } => {
                // Filling a bare local renames, it does not write: the local
                // the walk tracks keeps naming the same object.
                let bare_local = matches!(body.exprs[*target].kind, ExprKind::Local { .. });
                if !bare_local && place_writes_taint(taint, *target) {
                    return false;
                }
                if taint.operand(*value) {
                    return match &body.exprs[*target].kind {
                        ExprKind::Local { index, .. } => taint.locals.contains(index),
                        ExprKind::FieldAccess { field_name, .. } => {
                            obligations.insert(Slot::Field(field_name.clone()));
                            true
                        }
                        _ => false,
                    };
                }
                true
            }
            ExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    if taint.operand(f.value) {
                        obligations.insert(Slot::Field(f.name.clone()));
                    }
                }
                true
            }
            // A tainted value put into an array, a tuple, a variant payload or
            // a closure capture is reachable through reads this walk cannot
            // name.
            ExprKind::ArrayLiteral { elements } | ExprKind::TupleLiteral { elements } => {
                !elements.iter().any(|&e| taint.operand(e))
            }
            ExprKind::VariantConstruct { payload, .. } => {
                !payload.is_some_and(|p| taint.operand(p))
            }
            ExprKind::ClosureToCanonical { functor, .. } => !taint.operand(*functor),
            ExprKind::GlobalVarSet { value, .. } => !taint.operand(*value),
            ExprKind::IndirectCall { callee, args } => {
                !taint.operand(*callee) && !args.iter().any(|&a| taint.operand(a))
            }
            ExprKind::CmRawCall { args, .. } => !args.iter().any(|&a| taint.operand(a)),
            // Handing the object to a callee is a question about that callee's
            // parameter, which [`Self::declared_slot`] answers for a bodyless
            // one and the program walk for the rest.
            ExprKind::Call { func_id, args, .. } => {
                for (pos, _) in args
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| taint.operand(a.expr))
                {
                    obligations.insert(Slot::Param(*func_id, pos));
                }
                true
            }
            _ => true,
        }
    }

    /// What a bodyless callee's clauses say about the argument at `pos`.
    fn declared_arg(&self, callee: &NirFunction, pos: usize) -> ArgClauses {
        // Only `core:builtin` answers, where the value-copy plan already trusts
        // `#[retain]`. Elsewhere an absent clause is silence, not consent.
        if !callee.module_source.is_core_builtin() {
            return ArgClauses::REFUSED;
        }
        let reference = FunctionRef::from_resolved(callee, callee.module_source.clone());
        let declarations = &self.project.builtin_declarations;
        // `#[retain(elements_of = p)]` leaves `p` alone and re-homes what `p`
        // holds — into another parameter, or into an owned result. Either lands
        // the elements under a name this walk never sees, so a write through
        // that name would reach the shared object's contents unobserved.
        if declarations
            .retain_specs(&reference)
            .any(|r| r.source == pos && r.elements)
        {
            return ArgClauses::REFUSED;
        }
        // Only a result that can hold a reference is a way out of the call. Two
        // clauses answer for it: `#[result(owned)]` says the object handed back
        // is not the one given, and `#[result(part_of = p)]` says it is — which
        // makes the result the caller's to account for, under `Slot::Ret`.
        let escapes = holds_reference(&self.project.type_table.borrow(), callee.return_type);
        let hands_back = escapes && declarations.part_of(&reference) == Some(pos);
        if escapes && !hands_back && !declarations.returns_owned(&reference) {
            return ArgClauses::REFUSED;
        }
        ArgClauses {
            reads: declarations.reads_param(&reference, pos),
            hands_back,
        }
    }
}

/// Whether a callee leaves an argument and everything it holds alone, and
/// whether its result is that argument coming back.
struct ArgClauses {
    reads: bool,
    hands_back: bool,
}

impl ArgClauses {
    const REFUSED: Self = Self {
        reads: false,
        hands_back: false,
    };
}

/// The tainted expressions and locals of one body, saturated to a fixpoint.
struct Taint<'a> {
    body: &'a Body,
    type_table: &'a TypeTable,
    seed_field: Option<&'a str>,
    seed_call: Option<FuncId>,
    locals: IndexSet<u32>,
    exprs: IndexSet<ExprId>,
}

impl Taint<'_> {
    fn expr(&self, e: ExprId) -> bool {
        self.exprs.contains(&e)
    }

    /// A promoted operand carries no skeleton node, so it is read through the
    /// local it extracts back to, and counts as tainted where it extracts to none.
    fn operand(&self, op: Operand) -> bool {
        match op {
            Operand::Expr(e) => self.expr(e),
            Operand::Value(_) => match promoted_reference(self.body, self.type_table, op) {
                PromotedRef::None => false,
                PromotedRef::Local(l) => self.locals.contains(&l),
                PromotedRef::Unknown => true,
            },
        }
    }

    /// Re-walk the body until no new expression or local is tainted. The height
    /// of the projection chains bounds how many linear passes that takes.
    fn saturate(&mut self) {
        loop {
            let before = (self.exprs.len(), self.locals.len());
            self.pass();
            if (self.exprs.len(), self.locals.len()) == before {
                return;
            }
        }
    }

    fn pass(&mut self) {
        let body = self.body;
        let mut found_exprs: Vec<ExprId> = Vec::new();
        let mut found_locals: Vec<u32> = Vec::new();
        body.for_each_reachable_node(|node| match node {
            NodeRef::Expr(e) => {
                if !self.expr(e) && self.taints(e) {
                    found_exprs.push(e);
                }
                // A `match` arm binds from its scrutinee, so a tainted
                // scrutinee taints every name its arms bind.
                if let ExprKind::Match { expr, arms } = &body.exprs[e].kind
                    && self.operand(*expr)
                {
                    for arm in arms {
                        collect_pattern_bindings(body, arm.pattern, &mut found_locals);
                    }
                }
            }
            NodeRef::Pat(p) => {
                for sub in pattern_field_reads(body, p, self.seed_field) {
                    collect_pattern_bindings(body, sub, &mut found_locals);
                }
            }
            NodeRef::Stmt(s) => match &body.stmts[s].kind {
                StmtKind::Let {
                    local_index, value, ..
                } => {
                    if self.operand(*value) {
                        found_locals.push(*local_index);
                    }
                }
                StmtKind::LetDestructure { pattern, value, .. } => {
                    if self.operand(*value) {
                        collect_pattern_bindings(body, *pattern, &mut found_locals);
                    }
                }
                _ => {}
            },
            NodeRef::Block(_) => {}
        });
        self.exprs.extend(found_exprs);
        self.locals.extend(found_locals);
    }

    /// Whether `e` denotes the shared object or a projection of it. A scalar
    /// result never can: it holds no reference.
    fn taints(&self, e: ExprId) -> bool {
        let body = self.body;
        if !holds_reference(self.type_table, body.exprs[e].type_id) {
            return false;
        }
        match &body.exprs[e].kind {
            ExprKind::Local { index, .. } => self.locals.contains(index),
            ExprKind::FieldAccess {
                expr, field_name, ..
            } => self.seed_field == Some(field_name.as_str()) || self.operand(*expr),
            ExprKind::Index { expr, .. }
            | ExprKind::Cast { expr, .. }
            | ExprKind::Unary { expr, .. }
            | ExprKind::VariantPayload { expr, .. } => self.operand(*expr),
            // One branch is the value, so any tainted node under it is taken to
            // be it. `match_to_switch` makes a `Switch` of a dense `Match` here.
            ExprKind::LabeledBlock { .. }
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Switch { .. } => self.subtree_tainted(NodeRef::Expr(e)),
            // An accessor hands back a handle into its first argument. A bodied
            // callee is read the same way, its parameter being the real answer.
            ExprKind::Call { func_id, args, .. } => {
                self.seed_call == Some(*func_id)
                    || args.first().is_some_and(|a| self.operand(a.expr))
            }
            // Builds a container around the object rather than naming it, so
            // the taint stops here; `check_expr_use` is what answers for it.
            ExprKind::PackedArray(_)
            | ExprKind::StructLiteral { .. }
            | ExprKind::TupleLiteral { .. }
            | ExprKind::ArrayLiteral { .. }
            | ExprKind::VariantConstruct { .. }
            | ExprKind::EnumConstruct { .. } => false,
            // `check_expr_use` refuses the query before a tainted value can
            // reach any of these, so no result of one is ever the object.
            ExprKind::GlobalVarGet { .. }
            | ExprKind::GlobalVarSet { .. }
            | ExprKind::IndirectCall { .. }
            | ExprKind::CmRawCall { .. }
            | ExprKind::ClosureToCanonical { .. }
            | ExprKind::Assign { .. } => false,
            // None of these yields a reference: the first three are scalars,
            // and a `Dead` node is never evaluated.
            ExprKind::Binary { .. }
            | ExprKind::VariantTag { .. }
            | ExprKind::VariantTest { .. }
            | ExprKind::Dead => false,
        }
    }

    /// Whether anything under `node` is tainted. The walk visits skeleton nodes,
    /// which a promoted operand is not, so each node's operands are read too.
    fn subtree_tainted(&self, node: NodeRef) -> bool {
        self.body
            .walk_nodes_under::<()>(node, |c| {
                let mut hit = matches!(c, NodeRef::Expr(e) if self.expr(e));
                self.body.for_each_operand(c, |op| hit |= self.operand(op));
                if hit {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(true)
                }
            })
            .is_some()
    }
}

/// Whether assigning to `target` writes through a tainted value: any step of
/// its receiver chain is the shared object. The target's own type says nothing
/// — a scalar field of a tainted receiver is still a write of it.
fn place_writes_taint(taint: &Taint<'_>, target: ExprId) -> bool {
    if taint.expr(target) {
        return true;
    }
    let inner = match &taint.body.exprs[target].kind {
        ExprKind::FieldAccess { expr, .. }
        | ExprKind::Index { expr, .. }
        | ExprKind::Unary { expr, .. } => *expr,
        _ => return false,
    };
    if taint.operand(inner) {
        return true;
    }
    inner
        .as_expr()
        .is_some_and(|e| place_writes_taint(taint, e))
}

/// What a promoted operand can hold: nothing that references the heap, one
/// local's value, or something this walk cannot name.
enum PromotedRef {
    None,
    Local(u32),
    Unknown,
}

fn promoted_reference(body: &Body, type_table: &TypeTable, op: Operand) -> PromotedRef {
    let Some(v) = op.as_value() else {
        return PromotedRef::None;
    };
    if !holds_reference(type_table, body.operand_type(op)) {
        return PromotedRef::None;
    }
    if let Some(l) = bare_promoted_local(body, op) {
        return PromotedRef::Local(l);
    }
    if body.values.kind(v).is_constant() {
        return PromotedRef::None;
    }
    PromotedRef::Unknown
}

/// The sub-patterns `pat` binds out of the field named `field`. Destructuring
/// reads a field by naming it here, leaving no `ExprKind::FieldAccess` to match.
fn pattern_field_reads<'a>(
    body: &'a Body,
    pat: PatId,
    field: Option<&'a str>,
) -> impl Iterator<Item = PatId> + 'a {
    let fields = match (&body.pats[pat].kind, field) {
        (PatKind::Struct { fields, .. }, Some(_)) => Some(fields),
        _ => None,
    };
    fields
        .into_iter()
        .flatten()
        .filter(move |f| field == Some(f.field_name.as_str()))
        .map(|f| f.pattern)
}

/// What one body can read, as the sets every seed question is asked against.
#[derive(Default)]
struct BodyCensus {
    fields_read: IndexSet<String>,
    /// A field read promoted to a value carries no name to match, so it counts
    /// as a read of any field.
    reads_unnamed_field: bool,
    callees: IndexSet<FuncId>,
}

impl BodyCensus {
    /// One walk answering every seed question this body will be asked.
    fn of(body: &Body, type_table: &TypeTable) -> Self {
        let mut census = Self::default();
        body.for_each_reachable_node(|node| {
            match node {
                NodeRef::Expr(e) => match &body.exprs[e].kind {
                    ExprKind::FieldAccess { field_name, .. } => {
                        census.fields_read.insert(field_name.clone());
                    }
                    ExprKind::Call { func_id, .. } => {
                        census.callees.insert(*func_id);
                    }
                    _ => {}
                },
                NodeRef::Pat(p) => {
                    if let PatKind::Struct { fields, .. } = &body.pats[p].kind {
                        for f in fields {
                            census.fields_read.insert(f.field_name.clone());
                        }
                    }
                }
                NodeRef::Stmt(_) | NodeRef::Block(_) => {}
            }
            if !census.reads_unnamed_field {
                body.for_each_operand(node, |op| {
                    census.reads_unnamed_field |= matches!(
                        promoted_reference(body, type_table, op),
                        PromotedRef::Unknown
                    );
                });
            }
        });
        census
    }

    fn can_read(&self, seed_field: Option<&str>, seed_call: Option<FuncId>) -> bool {
        seed_field.is_some_and(|f| self.reads_unnamed_field || self.fields_read.contains(f))
            || seed_call.is_some_and(|id| self.callees.contains(&id))
    }
}
