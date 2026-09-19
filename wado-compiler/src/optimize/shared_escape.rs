//! Whether a constant aggregate stays safe to share once it escapes into the
//! heap: nothing, anywhere in the program, writes through the object a callee
//! stashes away. [`const_object_globalization`](super::const_object_globalization)
//! asks this where its own read-only gate stops, at the bare read that hands a
//! parameter to a struct literal.
//!
//! Taint names the shared object itself, never a container holding it: a
//! projection of a tainted value is tainted, a struct built *around* one is
//! not. Storing a tainted value away instead raises an obligation on the slot
//! it lands in — a field name, a callee parameter — and the slot's own reads
//! are then tainted in turn. A write of anything tainted refuses the query, and
//! so does every shape this walk does not model.

use std::cell::{Cell, RefCell};

use crate::compiler_trace;
use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, FunctionRef, NirFunction};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand, PatId, PatKind, StmtKind};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::arena_query::bare_promoted_local;

use cranelift_entity::EntityRef;

/// A place a tainted value can come to rest, and so a set of reads to taint.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Slot {
    /// Every `_.name` read in the program. Keyed by the field *name* rather
    /// than by the receiver's type: a name is one key however the receiver is
    /// spelled, so no newtype or reference wrapper can hide a read from the
    /// scan.
    Field(String),
    /// One callee's parameter, tainted inside that callee's body alone.
    Param(FuncId, usize),
    /// One callee's result, tainted at every call of it.
    Ret(FuncId),
}

/// What a bodyless callee does with one argument.
enum ArgRole {
    /// Reads it, and hands back no handle into it.
    Read,
    /// Reads it, and the result is a handle into it.
    Project,
    /// Writes it, or is not modelled here.
    Opaque,
}

/// Memoized [`Slot`] verdicts over one `NirPackage`.
pub(super) struct SharedEscape<'a> {
    project: &'a NirPackage,
    /// Settled verdicts, which no assumption stands behind.
    verdicts: RefCell<IndexMap<Slot, bool>>,
    /// The slots being computed. One asked for again reads `true`, which is
    /// the sound seed: "no write reaches this object" is a safety property, so
    /// a cycle carrying no write of its own really does hold.
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
        // A bodyless callee has nothing to walk, so its parameter would pass
        // vacuously: `array_set`'s third argument writes the constant into a
        // slot nothing here can name, and `v128_const`'s wants a literal.
        let Some(callee) = self.project.functions.get(func_id.index()) else {
            return false;
        };
        if callee.try_borrow().is_ok_and(|f| f.body.is_none()) {
            return false;
        }
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
        // Nothing to walk is not the same as nothing to find.
        if let Slot::Param(id, _) | Slot::Ret(id) = slot
            && self.project.functions[id.index()]
                .try_borrow()
                .is_ok_and(|f| f.body.is_none())
        {
            return false;
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
        let type_table = self.project.type_table.borrow();
        if locals.is_empty() && !has_seed(body, &type_table, seed_field, seed_call) {
            return true;
        }
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
            ExprKind::Call { func_id, args, .. } => {
                let tainted: Vec<usize> = args
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| taint.operand(a.expr))
                    .map(|(pos, _)| pos)
                    .collect();
                if tainted.is_empty() {
                    return true;
                }
                let Some(callee) = self.project.functions.get(func_id.index()) else {
                    return false;
                };
                let Ok(callee) = callee.try_borrow() else {
                    return false;
                };
                if callee.body.is_some() {
                    for pos in tainted {
                        obligations.insert(Slot::Param(*func_id, pos));
                    }
                    return true;
                }
                let reference = FunctionRef::from_resolved(&callee, callee.module_source.clone());
                tainted
                    .into_iter()
                    .all(|pos| !matches!(arg_role(&reference, pos), ArgRole::Opaque))
            }
            _ => true,
        }
    }
}

/// What a bodyless callee does with the argument at `pos`. A builtin absent
/// from the match writes every argument, which refuses the query.
fn arg_role(reference: &FunctionRef, pos: usize) -> ArgRole {
    let name = reference
        .builtin_name()
        .or_else(|| reference.monomorphized_builtin_name());
    match (name.as_deref(), pos) {
        // An element read hands back a handle into the array.
        (
            Some(
                "builtin::array_get_value"
                | "builtin::array_get_value_u8"
                | "builtin::array_get_ref",
            ),
            0,
        ) => ArgRole::Project,
        (Some("builtin::black_box" | "builtin::select"), _) => ArgRole::Project,
        // Reads the array and builds a fresh one, so no handle survives.
        (
            Some(
                "builtin::array_len"
                | "builtin::array_clone"
                | "builtin::array_clone_prefix"
                | "builtin::array_clone_shallow"
                | "builtin::copy_value"
                | "builtin::is_uninitialized",
            ),
            0,
        ) => ArgRole::Read,
        // `array_copy(dst, dst_start, src, src_start, len)` writes `dst`.
        (Some("builtin::array_copy"), 2) => ArgRole::Read,
        _ => ArgRole::Opaque,
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
                if !self.exprs.contains(&e) && self.taints(e) {
                    found_exprs.push(e);
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
                        collect_bindings(body, *pattern, &mut found_locals);
                    }
                }
                _ => {}
            },
            NodeRef::Block(_) | NodeRef::Pat(_) => {}
        });
        // A `match` arm binds from its scrutinee, so a tainted scrutinee taints
        // every name its arms bind.
        for e in body.exprs.keys() {
            if let ExprKind::Match { expr, arms } = &body.exprs[e].kind
                && self.operand(*expr)
            {
                for arm in arms {
                    collect_bindings(body, arm.pattern, &mut found_locals);
                }
            }
        }
        self.exprs.extend(found_exprs);
        self.locals.extend(found_locals);
    }

    /// Whether `e` denotes the shared object or a projection of it. A scalar
    /// result never can: it holds no reference.
    fn taints(&self, e: ExprId) -> bool {
        let body = self.body;
        if !is_reference_type(self.type_table, body.exprs[e].type_id) {
            return false;
        }
        match &body.exprs[e].kind {
            ExprKind::Local { index, .. } => self.locals.contains(index),
            ExprKind::FieldAccess {
                expr, field_name, ..
            } => self.seed_field == Some(field_name.as_str()) || self.operand(*expr),
            ExprKind::Index { expr, .. } | ExprKind::Cast { expr, .. } => self.operand(*expr),
            ExprKind::Unary { expr, .. } => self.operand(*expr),
            ExprKind::VariantPayload { expr, .. } => self.operand(*expr),
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
        let mut found = false;
        self.body.for_each_child(node, |c| {
            if found {
                return;
            }
            found = match c {
                NodeRef::Expr(e) if self.exprs.contains(&e) => true,
                _ => self.subtree_tainted(c),
            };
        });
        found
    }
}

fn collect_bindings(body: &Body, pat: PatId, out: &mut Vec<u32>) {
    if let PatKind::Binding { local_index, .. } = &body.pats[pat].kind {
        out.push(*local_index);
    }
    body.for_each_child(NodeRef::Pat(pat), |c| {
        if let NodeRef::Pat(p) = c {
            collect_bindings(body, p, out);
        }
    });
}

/// Whether assigning to `target` writes through a tainted value: any step of
/// its receiver chain is the shared object. The target's own type says nothing
/// — a scalar field of a tainted receiver is still a write of it.
fn place_writes_taint(taint: &Taint<'_>, target: ExprId) -> bool {
    if taint.expr(target) {
        return true;
    }
    let inner = match &taint.body.exprs[target].kind {
        ExprKind::FieldAccess { expr, .. } | ExprKind::Index { expr, .. } => *expr,
        ExprKind::Unary { expr, .. } => *expr,
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
    if !is_reference_type(type_table, body.operand_type(op)) {
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

fn is_reference_type(type_table: &TypeTable, ty: TypeId) -> bool {
    !matches!(
        type_table.get(ty),
        ResolvedType::Primitive(_) | ResolvedType::Unit | ResolvedType::Never
    )
}
