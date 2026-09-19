//! Whether a constant aggregate stays safe to share once a callee stashes it
//! into the heap: nothing, anywhere in the program, writes through the object.
//! [`const_object_globalization`](super::const_object_globalization) asks this
//! where its own read-only gate stops. See the WEP for the taint model.

use std::cell::{Cell, RefCell};
use std::ops::ControlFlow;

use crate::compiler_trace;
use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, FunctionRef, NirFunction};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand, StmtKind};
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
pub(super) struct SharedEscape<'a> {
    project: &'a NirPackage,
    /// Settled verdicts, which no assumption stands behind.
    verdicts: RefCell<IndexMap<Slot, bool>>,
    /// The slots being computed. One asked for again reads `true`, the sound
    /// seed for a safety property: a cycle carrying no write really does hold.
    in_flight: RefCell<IndexSet<Slot>>,
    /// Whether the query in progress leaned on such an assumption.
    assumed: Cell<bool>,
}

impl<'a> SharedEscape<'a> {
    pub(super) fn new(project: &'a NirPackage) -> Self {
        Self {
            project,
            verdicts: RefCell::new(IndexMap::default()),
            in_flight: RefCell::new(IndexSet::default()),
            assumed: Cell::new(false),
        }
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
            let Ok(func) = func.try_borrow() else {
                // A body already borrowed elsewhere cannot be read, and an
                // unread body is one whose writes are unseen.
                return false;
            };
            if !self.scan_function(slot, idx, &func, &mut obligations) {
                compiler_trace!("shared_escape", "{slot:?} refused in {}", func.name);
                return false;
            }
        }
        compiler_trace!("shared_escape", "{slot:?} clear, owes {obligations:?}");
        obligations.iter().all(|next| self.slot_ok(next))
    }

    /// The verdict for a slot whose owner has no body, where the program walk
    /// would clear it having looked at nothing. `None` where the owner has one.
    fn declared_slot(&self, slot: &Slot) -> Option<bool> {
        let (Slot::Param(id, _) | Slot::Ret(id)) = slot else {
            return None;
        };
        let Some(owner) = self.project.functions.get(id.index()) else {
            return Some(false);
        };
        let Ok(owner) = owner.try_borrow() else {
            return Some(false);
        };
        if owner.body.is_some() {
            return None;
        }
        // Only a parameter has something declared about it. A result comes out
        // of a body nothing here can see.
        let Slot::Param(_, pos) = slot else {
            return Some(false);
        };
        let verdict = self.reads_arg(&owner, *pos);
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
            && let Some(param) = func.params.get(*pos)
        {
            locals.insert(param.local_index);
        }
        if locals.is_empty() {
            // A `Param` slot seeds its owner's body alone. Every other body is
            // left without looking, there being no name for a seed to match.
            if seed_field.is_none() && seed_call.is_none() {
                return true;
            }
            if !has_seed(
                body,
                &self.project.type_table.borrow(),
                seed_field,
                seed_call,
            ) {
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
        self.check_uses(&taint, FuncId::new(func_idx), obligations)
    }

    /// Every use of a tainted value, refused or turned into an obligation.
    /// A `Break` stays inside the body, so its value is left to the taint walk;
    /// a `Return` hands the object to every caller, which is a slot of its own.
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

    /// Whether a bodyless callee leaves the argument at `pos` where the caller
    /// put it: it neither writes through it nor keeps it past the call.
    ///
    /// Only `core:builtin` answers. `#[retain(...)]` is already what the
    /// value-copy plan trusts there, so a clause missing from that file is a
    /// bug rather than a silence to read as consent — which is exactly what it
    /// would be on a CM import or a `.wasm` asset export, so those refuse.
    /// A handle the result carries away needs no case of its own:
    /// [`Taint::taints`] already taints any call whose first argument is
    /// tainted.
    fn reads_arg(&self, callee: &NirFunction, pos: usize) -> bool {
        if !callee.module_source.is_core_builtin() {
            return false;
        }
        let reference = FunctionRef::from_resolved(callee, callee.module_source.clone());
        self.project
            .builtin_declarations
            .reads_param(&reference, pos)
    }
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
    /// local it extracts back to; one that extracts to no local, and could
    /// hold a reference, counts as tainted rather than as unseen.
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

    /// Re-walk the body until no new expression or local is tainted. Each pass
    /// is one linear walk, and the height of the projection chains bounds how
    /// many it takes.
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
            NodeRef::Block(_) | NodeRef::Pat(_) => {}
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
            // A block, an `if` or a `match` yields one of its branches; any
            // tainted node under it is taken to be that branch.
            ExprKind::LabeledBlock { .. } | ExprKind::If { .. } | ExprKind::Match { .. } => {
                self.subtree_tainted(NodeRef::Expr(e))
            }
            // A builtin accessor hands back a handle into its array, so the
            // result is a projection. A bodied callee is treated the same way
            // — conservatively, since checking its parameter is what says
            // whether the handle comes back out.
            ExprKind::Call { func_id, args, .. } => {
                self.seed_call == Some(*func_id)
                    || args.first().is_some_and(|a| self.operand(a.expr))
            }
            _ => false,
        }
    }

    fn subtree_tainted(&self, node: NodeRef) -> bool {
        self.body
            .walk_nodes_under::<()>(node, |c| match c {
                NodeRef::Expr(e) if self.expr(e) => ControlFlow::Break(()),
                _ => ControlFlow::Continue(true),
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

/// Whether `body` can read the slot at all. A function that cannot is left
/// alone, so an unrelated one never refuses the query.
fn has_seed(
    body: &Body,
    type_table: &TypeTable,
    seed_field: Option<&str>,
    seed_call: Option<FuncId>,
) -> bool {
    let mut found = false;
    body.for_each_reachable_node(|node| {
        if found {
            return;
        }
        if let NodeRef::Expr(e) = node {
            found = match &body.exprs[e].kind {
                ExprKind::FieldAccess { field_name, .. } => seed_field == Some(field_name.as_str()),
                ExprKind::Call { func_id, .. } => seed_call == Some(*func_id),
                _ => false,
            };
        }
        if !found {
            body.for_each_operand(node, |op| {
                // A field read promoted to a value carries no name to match, so
                // it counts as a possible read of any field.
                found |= seed_field.is_some()
                    && matches!(
                        promoted_reference(body, type_table, op),
                        PromotedRef::Unknown
                    );
            });
        }
    });
    found
}
