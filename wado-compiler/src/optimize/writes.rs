//! What a subtree may write, in the engine's alias sets: each node's [`Write`]s,
//! and the [`Summaries`] that answer whether a subtree may change what a local
//! holds for the code after it.

use std::rc::Rc;

use crate::hashmap;
use crate::nir::{FuncId, FunctionKind, NirLocal, NirParam, NirUnaryOp};
use crate::nir_arena::{
    self, BlockId, Body, ExprId, ExprKind, NodeRef, Operand, StmtKind, Tracked,
};
use crate::nir_engine::{Engine, EngineBuffers};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueKind;
use crate::optimize::alias::{
    CallImmutability, FirstParamTypes, builder_alias_sets, call_verdicts, first_param_types,
};
use crate::optimize::arena_query::{WriteRoot, operand_local, storage_root, write_root};
use crate::optimize::value_copy::mutation::{MutationOracle, Witness, expr_witnesses};
use crate::parallel::Executor;
use crate::tir::{BuiltinDeclaration, TypeId};
use crate::type_table::TypeTable;

/// What an engine reading alias sets and callee classes needs from the whole
/// program, built once per pass run.
pub(super) struct EngineFacts<'a> {
    type_table: &'a TypeTable,
    first_param_types: FirstParamTypes,
    call_immutability: CallImmutability<'a>,
    panic_ids: hashmap::IndexSet<FuncId>,
    quiet_callee_ids: hashmap::IndexSet<FuncId>,
}

impl<'a> EngineFacts<'a> {
    pub(super) fn new(project: &NirPackage, type_table: &'a TypeTable, exec: &Executor) -> Self {
        let panic_ids = resolve_panic_ids(project, type_table);
        let quiet_callee_ids = resolve_quiet_callee_ids(project, &panic_ids);
        let first_param_types = first_param_types(project);
        Self {
            type_table,
            call_immutability: CallImmutability::new(project, type_table, &first_param_types, exec),
            first_param_types,
            panic_ids,
            quiet_callee_ids,
        }
    }

    /// An engine over one function's body, its alias sets and callee classes
    /// set.
    pub(super) fn engine<'e>(
        &'e self,
        body: &'e mut Body,
        buffers: &'e mut EngineBuffers,
        locals: &'e mut Tracked<Vec<NirLocal>>,
        params: &'e [NirParam],
        address_taken_locals: &hashmap::IndexSet<u32>,
    ) -> Engine<'e> {
        let alias = builder_alias_sets(
            body,
            locals,
            address_taken_locals,
            self.type_table,
            &self.first_param_types,
            &self.call_immutability,
        );
        let verdicts = call_verdicts(
            body,
            self.type_table,
            &self.first_param_types,
            &self.call_immutability,
        );
        let mut engine = Engine::new(body, buffers, locals, params, self.type_table);
        engine.set_alias_sets(alias);
        engine.set_call_verdicts(verdicts.pure, verdicts.receiver_immutable);
        engine.set_panic_callee_ids(&self.panic_ids);
        engine.set_quiet_callee_ids(&self.quiet_callee_ids);
        engine.set_pure_builtin_callees(self.call_immutability.effect_free());
        engine
    }
}

/// The [`FuncId`]s of every function that cannot return, so the panic-block
/// matcher identifies a diverging callee by id.
fn resolve_panic_ids(project: &NirPackage, type_table: &TypeTable) -> hashmap::IndexSet<FuncId> {
    project
        .functions
        .iter()
        .filter_map(|f| {
            let f = f.borrow();
            // A `-> !` return type, not a name: the matcher needs the arm to be
            // unable to continue, which is what the type says. A name list would
            // miss `unwrap_failed` and the helper `cold_outline` moves a panic
            // guard into, and would claim a `panic_free_parse` that merely reads
            // like one. `dce` retires the type slots only a dead function names,
            // and a dead function is uncallable, so skipping one loses nothing.
            f.id.filter(|_| !f.is_dead && type_table.is_never(f.return_type))
        })
        .collect()
}

/// The callees that do nothing a trap would hide: an unobservable builtin, such
/// as `builtin::unreachable` (all a contract check calls under
/// `-f bare-asserts`), a value copy, or a trap that reports nothing but a
/// failed contract: a contract report, or a function doing nothing else but call
/// a quiet one, as `cold_outline` leaves.
fn resolve_quiet_callee_ids(
    project: &NirPackage,
    panic_ids: &hashmap::IndexSet<FuncId>,
) -> hashmap::IndexSet<FuncId> {
    let mut quiet: hashmap::IndexSet<FuncId> = project
        .functions
        .iter()
        .filter_map(|f| {
            let f = f.borrow();
            let id = f.id?;
            (f.is_value_copy()
                || project
                    .builtin_declarations
                    .get(&*f)
                    .is_some_and(BuiltinDeclaration::is_unobservable))
            .then_some(id)
        })
        .collect();
    loop {
        let grown: Vec<FuncId> = project
            .functions
            .iter()
            .filter_map(|f| {
                let f = f.borrow();
                let id =
                    f.id.filter(|id| panic_ids.contains(id) && !quiet.contains(id))?;
                let reports_nothing_else = matches!(f.kind, FunctionKind::ContractReport)
                    || f.body.as_ref().is_some_and(|body| {
                        subtree_quiet(body, NodeRef::Block(body.root()), |callee| {
                            quiet.contains(&callee)
                        })
                    });
                reports_nothing_else.then_some(id)
            })
            .collect();
        if grown.is_empty() {
            return quiet;
        }
        quiet.extend(grown);
    }
}

/// Whether running `node` does nothing a trap would hide, with `quiet_call`
/// saying which callees qualify.
pub(super) fn subtree_quiet(
    body: &Body,
    node: NodeRef,
    quiet_call: impl Fn(FuncId) -> bool,
) -> bool {
    let mut quiet = true;
    body.for_each_node_under(node, |n| quiet &= node_quiet(body, n, &quiet_call));
    quiet
}

/// One write an expression node may perform, in the engine's alias sets, so it
/// and a scoped walk cannot disagree over what a store reaches.
#[derive(Clone, Copy)]
pub(super) enum Write {
    /// The slot or storage of this local.
    Root(u32),
    /// Storage another handle may reach: every aliased local's, as far as
    /// what was stored can change it.
    Aliased(Stored),
    /// What a call may do unseen: write every local escaped by `&mut`.
    Call,
}

/// What a store through an aliased handle can change, by the kinds of object
/// it writes ([`TypeTable::object_kind`]). A `&T` is the handle of `T`'s
/// object, so a store changes a read only through an object of a kind the read
/// passes.
#[derive(Clone, Copy)]
pub(super) enum Stored {
    /// A field or element of a `container`, and, where `value` is of an
    /// in-place type, the leaves of the object the slot held.
    Slot {
        container: TypeId,
        value: TypeId,
    },
    /// Anything a value of this type reaches: a whole-value store, or a callee
    /// handed it.
    Reaching(TypeId),
    Unknown,
}

impl Stored {
    /// Whether this store may change a read through objects of `kinds`, which
    /// [`read_kinds`] named. `reached` remembers each type walk, which every
    /// read of a bind would otherwise repeat for each store since it.
    pub(super) fn may_change(
        self,
        engine: &Engine,
        kinds: &[TypeId],
        reached: &mut hashmap::IndexMap<(TypeId, TypeId), bool>,
    ) -> bool {
        let types = engine.type_table();
        let mut reaches = |ty: TypeId| {
            kinds.iter().any(|&k| {
                *reached
                    .entry((ty, k))
                    .or_insert_with(|| types.reaches_object(ty, k))
            })
        };
        match self {
            Stored::Slot { container, value } => {
                kinds.contains(&types.object_kind(container)) || reaches(value)
            }
            Stored::Reaching(ty) => reaches(ty),
            Stored::Unknown => true,
        }
    }

    /// What a store into `place` writes.
    fn into_place(body: &nir_arena::Body, place: ExprId) -> Stored {
        match &body.exprs[place].kind {
            ExprKind::FieldAccess {
                expr: container, ..
            }
            | ExprKind::Index {
                expr: container, ..
            } => Stored::Slot {
                container: body.operand_type(*container),
                value: body.exprs[place].type_id,
            },
            ExprKind::Cast { expr: inner, .. } => inner
                .as_expr()
                .map_or(Stored::Unknown, |e| Stored::into_place(body, e)),
            ExprKind::Local { .. }
            | ExprKind::Unary {
                op: NirUnaryOp::Deref,
                ..
            } => Stored::Reaching(body.exprs[place].type_id),
            _ => Stored::Unknown,
        }
    }
}

/// The kinds of object `op` reads a field, an element or a box of, by
/// [`TypeTable::object_kind`]; `None` where it reads what this cannot name.
pub(super) fn read_kinds(engine: &Engine, op: Operand) -> Option<Vec<TypeId>> {
    let mut kinds = Vec::new();
    collect_read_kinds(engine, engine.type_table(), op, &mut kinds).then_some(kinds)
}

fn collect_read_kinds(
    engine: &Engine,
    types: &TypeTable,
    op: Operand,
    kinds: &mut Vec<TypeId>,
) -> bool {
    let body = &*engine.body;
    let read_of = |container: Operand, kinds: &mut Vec<TypeId>| {
        kinds.push(types.object_kind(body.operand_type(container)));
        collect_read_kinds(engine, types, container, kinds)
    };
    let local = |l: u32, kinds: &mut Vec<TypeId>| {
        if engine.aliased().contains(&l) {
            kinds.push(types.object_kind(engine.locals()[l as usize].type_id));
        }
        true
    };
    match op {
        Operand::Value(v) => match body.values.kind(v) {
            k if k.is_constant() => true,
            ValueKind::Opaque(_) => operand_local(body, op).is_some_and(|l| local(l, kinds)),
            ValueKind::Binary { lhs, rhs, .. } => {
                collect_read_kinds(engine, types, Operand::Value(*lhs), kinds)
                    && collect_read_kinds(engine, types, Operand::Value(*rhs), kinds)
            }
            ValueKind::Unary {
                op: NirUnaryOp::Deref,
                operand,
                ..
            }
            | ValueKind::FieldAccess {
                receiver: operand, ..
            } => read_of(Operand::Value(*operand), kinds),
            ValueKind::Unary { operand, .. } | ValueKind::Cast { operand, .. } => {
                collect_read_kinds(engine, types, Operand::Value(*operand), kinds)
            }
            _ => false,
        },
        Operand::Expr(e) => match &body.exprs[e].kind {
            ExprKind::Local { index, .. } => local(*index, kinds),
            ExprKind::Unary {
                op: NirUnaryOp::Deref,
                expr: container,
            }
            | ExprKind::FieldAccess {
                expr: container, ..
            } => read_of(*container, kinds),
            ExprKind::Index {
                expr: container,
                index,
            } => read_of(*container, kinds) && collect_read_kinds(engine, types, *index, kinds),
            ExprKind::Binary { left, right, .. } => {
                collect_read_kinds(engine, types, *left, kinds)
                    && collect_read_kinds(engine, types, *right, kinds)
            }
            ExprKind::Unary { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
                collect_read_kinds(engine, types, *inner, kinds)
            }
            _ => false,
        },
    }
}

/// Every write the expression node `e` itself may perform. A receiver counts
/// unless the engine's call verdicts clear the callee, which go by what its
/// body writes: boxing can erase the `&mut self` a type test would read.
pub(super) fn for_each_write(engine: &Engine, e: ExprId, sink: &mut impl FnMut(Write)) {
    let body = &*engine.body;
    let no_signatures = hashmap::IndexMap::default();
    let oracle = MutationOracle::new(&no_signatures);
    let handed = |arg: ExprId| Stored::Reaching(body.exprs[arg].type_id);
    expr_witnesses(body, e, &oracle, &mut |w| match w {
        Witness::Rebind(l) => sink(Write::Root(l)),
        Witness::MutRef(place) => {
            if let Some(root) = storage_root(body, place) {
                sink(Write::Root(root));
            }
        }
        Witness::Write(inner) => {
            if let Some(place) = inner.as_expr() {
                write_through(engine, place, Stored::into_place(body, place), sink);
            }
        }
        Witness::CalleeArg {
            expr,
            verdict,
            is_mut,
        } => {
            if verdict.unwrap_or(is_mut) {
                write_through(engine, expr, handed(expr), sink);
            }
        }
        Witness::Receiver { expr, .. } => {
            if !engine.is_receiver_immutable_call(e) {
                write_through(engine, expr, handed(expr), sink);
            }
        }
        Witness::IndirectArg(expr) => write_through(engine, expr, handed(expr), sink),
    });
    match &body.exprs[e].kind {
        ExprKind::Call { func_id, .. } => {
            if !engine.is_panic_callee(*func_id) && !engine.is_pure_builtin_callee(*func_id) {
                sink(Write::Call);
            }
        }
        ExprKind::IndirectCall { .. } => sink(Write::Call),
        _ => {}
    }
}

/// A store into `place`: its root's storage, and every aliased local's when the
/// root is aliased or the chain crosses a reference to storage it does not own.
fn write_through(engine: &Engine, place: ExprId, stored: Stored, sink: &mut impl FnMut(Write)) {
    match write_root(engine.body, place, false) {
        WriteRoot::Local(root) => {
            sink(Write::Root(root));
            if engine.aliased().contains(&root)
                || engine
                    .body
                    .place_crosses_reference(place, engine.type_table())
            {
                sink(Write::Aliased(stored));
            }
        }
        WriteRoot::Aliased => sink(Write::Aliased(stored)),
        WriteRoot::Temp => {}
    }
}

/// Whether `w` may change what `root` holds. A call reaches a reference's
/// pointee through a global without taking the reference.
fn write_hits(engine: &Engine, w: Write, root: u32) -> bool {
    match w {
        Write::Root(r) => r == root,
        Write::Aliased(_) => reachable_elsewhere(engine, root),
        Write::Call => engine.mut_escaped().contains(&root) || is_reference(engine, root),
    }
}

/// Whether another handle may reach what `root` holds: an aliased local, or a
/// reference, whose pointee the frame does not own.
pub(super) fn reachable_elsewhere(engine: &Engine, root: u32) -> bool {
    engine.aliased().contains(&root) || is_reference(engine, root)
}

/// Whether `local` may name storage rather than hold a value. A local minted
/// past the type table's reach might.
fn is_reference(engine: &Engine, local: u32) -> bool {
    engine
        .locals()
        .get(local as usize)
        .is_none_or(|l| engine.type_table().is_reference(l.type_id))
}

/// Whether anything under `node` may change what one of `roots` holds for the
/// code after it.
pub(super) fn modifies_any_root(engine: &Engine, node: NodeRef, roots: &[u32]) -> bool {
    Summaries::default().modifies_any_root(engine, node, roots)
}

/// What a subtree may do for the code after it, as [`modifies_any_root`] and
/// [`node_quiet`] read it: the roots it writes, whether it stores through an
/// alias or calls a callee that may write, whether a jump sits in it, and
/// whether it stays quiet. A block that panics with no `return`, `break` or
/// `continue` before it never hands control on, so its writes do not count.
pub(super) struct Summary {
    roots: hashmap::IndexSet<u32>,
    aliased: bool,
    call: bool,
    jumps: bool,
    pub(super) quiet: bool,
}

/// Each node's [`Summary`], computed once for a walk that asks of every node
/// once per enclosing level. The walk's edits only drop nodes, so a summary
/// kept past one claims more than remains: a fact it drops, never one kept
/// wrongly.
#[derive(Default)]
pub(super) struct Summaries {
    of: hashmap::IndexMap<NodeRef, Rc<Summary>>,
}

impl Summaries {
    pub(super) fn of(&mut self, engine: &Engine, node: NodeRef) -> Rc<Summary> {
        if let Some(s) = self.of.get(&node) {
            return Rc::clone(s);
        }
        let body = &*engine.body;
        let mut children: Vec<Rc<Summary>> = Vec::new();
        body.for_each_child(node, |c| children.push(self.of(engine, c)));
        let mut own = Summary {
            roots: hashmap::IndexSet::default(),
            aliased: false,
            call: false,
            jumps: matches!(node, NodeRef::Stmt(s) if body.stmts[s].kind.is_jump()),
            quiet: node_quiet(body, node, |id| engine.is_quiet_callee(id)),
        };
        if let NodeRef::Expr(e) = node {
            for_each_write(engine, e, &mut |w| match w {
                Write::Root(r) => {
                    own.roots.insert(r);
                }
                Write::Aliased(_) => own.aliased = true,
                Write::Call => own.call = true,
            });
        }
        for child in &children {
            own.jumps |= child.jumps;
            own.quiet &= child.quiet;
        }
        // A block that panics before any exit hands control on to nothing
        // after it, so its writes do not count.
        let diverges = match node {
            NodeRef::Block(b) => self.panics_before_any_exit(engine, b),
            _ => false,
        };
        if !diverges {
            for child in &children {
                own.roots.extend(child.roots.iter().copied());
                own.aliased |= child.aliased;
                own.call |= child.call;
            }
        }
        let summary = Rc::new(own);
        self.of.insert(node, Rc::clone(&summary));
        summary
    }

    /// Whether `block` reaches a top-level panic with no `return`, `break` or
    /// `continue` before it, which could leave the block first.
    fn panics_before_any_exit(&mut self, engine: &Engine, block: BlockId) -> bool {
        for &s in &engine.body.blocks[block].stmts {
            if let StmtKind::Expr(op) = &engine.body.stmts[s].kind
                && op.as_expr().is_some_and(|e| is_panic_call(engine, e))
            {
                return true;
            }
            if self.of(engine, NodeRef::Stmt(s)).jumps {
                return false;
            }
        }
        false
    }

    /// [`modifies_any_root`] read off `node`'s summary.
    pub(super) fn modifies_any_root(
        &mut self,
        engine: &Engine,
        node: NodeRef,
        roots: &[u32],
    ) -> bool {
        let s = self.of(engine, node);
        roots.iter().any(|&root| s.modifies(engine, root))
    }
}

impl Summary {
    /// Whether the subtree may change what `root` holds, as [`write_hits`]
    /// answers for each write it summarizes.
    pub(super) fn modifies(&self, engine: &Engine, root: u32) -> bool {
        self.roots.contains(&root)
            || (self.aliased && write_hits(engine, Write::Aliased(Stored::Unknown), root))
            || (self.call && write_hits(engine, Write::Call, root))
    }
}

pub(super) fn is_panic_call(engine: &Engine, e: ExprId) -> bool {
    match &engine.body.exprs[e].kind {
        ExprKind::Call { func_id, .. } => engine.is_panic_callee(*func_id),
        _ => false,
    }
}

/// Whether running `node` alone, its children aside, does nothing a trap would
/// hide: no call that may report, print or exit, and no loop, which might not
/// end.
fn node_quiet(body: &Body, node: NodeRef, quiet_call: impl Fn(FuncId) -> bool) -> bool {
    match node {
        NodeRef::Expr(e) => match &body.exprs[e].kind {
            ExprKind::Call { func_id, .. } => quiet_call(*func_id),
            kind => !matches!(
                kind,
                ExprKind::IndirectCall { .. } | ExprKind::CmRawCall { .. }
            ),
        },
        NodeRef::Stmt(s) => !matches!(
            body.stmts[s].kind,
            StmtKind::Loop { .. }
                | StmtKind::Return { .. }
                | StmtKind::Break { .. }
                | StmtKind::Continue
        ),
        NodeRef::Block(_) | NodeRef::Pat(_) => true,
    }
}
