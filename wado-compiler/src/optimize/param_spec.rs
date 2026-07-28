//! Constant-field specialization of struct-reference parameters.
//!
//! Interprocedural constant propagation over struct fields, implemented by
//! cloning. A hot caller often threads a by-reference config struct whose
//! fields are compile-time constants — a template's `Formatter` (`width`,
//! `precision`, `sign_plus`, …) through `fmt_decimal` → `prepare_int_write` →
//! `write_char_n`. Those callees sit above the inline threshold, so without
//! this pass the constants never reach them and every `if f.sign_plus` /
//! `if f.width > 0` survives all the way to Wasm.
//!
//! For a call `g(…, x, …)` where `x` is a struct local (or an already
//! specialized parameter) with known constant fields, the pass clones `g`,
//! substitutes those fields' reads with the constants, and retargets the call
//! to the clone. It performs no folding itself: the clone's dead branches are
//! left to `const_fold` / `const_branch_prune` on the next fixed-point
//! iteration, and `dce` drops the original when every call site specialized.
//!
//! ## Legality
//!
//! A field may be substituted only if the callee never writes it — directly or
//! through any function it forwards the reference to. [`summarize_params`]
//! computes that write set as a fixed point over the call graph, alongside the
//! read set (which drives profitability: a chain like `fmt_decimal` reads no
//! interesting field itself, only the callees it forwards to do) and an
//! `opaque` flag for a reference whose use the summary cannot classify.
//!
//! On the caller side [`collect_roots`] keeps a field only when every write to
//! it stores the same constant and every call it is forwarded to leaves it
//! alone. The facts are flow-insensitive — valid at every program point, so no
//! dataflow is needed to place them.
//!
//! ## Propagation depth
//!
//! Each clone records what it knows about its own parameters
//! ([`ParamSpecState::param_consts`]), so the next iteration treats the clone's
//! parameter as a root and specializes one level deeper. The optimizer's
//! fixed-point loop therefore walks the chain one call deep per iteration; the
//! clone cache keys on the callee plus its bindings, so a recursive call
//! specializes to the same clone and terminates.

use std::cell::RefCell;
use std::rc::Rc;

use cranelift_entity::EntityRef;

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, FunctionRef, NirFunction, NirUnaryOp};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand, PatKind, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueKind;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::gate::FunctionGate;

/// Cap on the number of distinct clones minted for one original callee. A
/// config struct threaded from a handful of call sites specializes; a callee
/// genuinely polymorphic in its configuration does not multiply.
const MAX_SPECIALIZATIONS_PER_FUNCTION: usize = 2;

/// Whole-module cap on synthesized clones, so a pathological program cannot
/// trade unbounded code size for constant propagation.
const MAX_TOTAL_SPECIALIZATIONS: usize = 128;

/// Largest callee body (expression nodes) worth cloning. A body this large
/// costs more in duplicated code than its folded branches can return.
const MAX_CLONE_EXPRS: usize = 2000;

/// A scalar constant known for one struct field. Portable across functions
/// (unlike a `ValueId`, which is pool-local), and hashable so it can key the
/// clone cache. `Int` / `Float` carry the source `TypeId` so a substitution
/// into a read of a different width is rejected rather than silently widened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FieldConst {
    Int(u64, TypeId),
    Float(u64, TypeId),
    Bool(bool),
    Char(char),
}

impl FieldConst {
    /// The constant an operand holds, or `None` for a non-constant operand or a
    /// non-scalar value kind (the pool models pure scalars; aggregates never
    /// reach an operand slot).
    fn of_operand(body: &Body, op: Operand) -> Option<Self> {
        let value = op.as_value()?;
        match body.values.kind(value) {
            ValueKind::Int(n, ty) => Some(FieldConst::Int(*n, *ty)),
            ValueKind::Float(bits, ty) => Some(FieldConst::Float(*bits, *ty)),
            ValueKind::Bool(b) => Some(FieldConst::Bool(*b)),
            ValueKind::Char(c) => Some(FieldConst::Char(*c)),
            ValueKind::Null
            | ValueKind::Unit
            | ValueKind::Opaque(_)
            | ValueKind::Binary { .. }
            | ValueKind::Unary { .. }
            | ValueKind::Cast { .. }
            | ValueKind::Select { .. }
            | ValueKind::LoopPhi { .. }
            | ValueKind::FieldAccess { .. } => None,
        }
    }

    /// A total order over constants, so a binding list has one canonical form
    /// and two call sites agreeing on the constants hit the same clone.
    fn sort_key(self) -> (u8, u64, u32) {
        match self {
            FieldConst::Int(n, ty) => (0, n, ty.0),
            FieldConst::Float(bits, ty) => (1, bits, ty.0),
            FieldConst::Bool(b) => (2, u64::from(b), 0),
            FieldConst::Char(c) => (3, u64::from(c), 0),
        }
    }

    /// The pool kind to intern at a read of type `at`, or `None` when the read's
    /// type does not match the constant's (a width or representation mismatch
    /// the substitution must not paper over).
    fn value_kind_at(self, at: TypeId) -> Option<ValueKind> {
        match self {
            FieldConst::Int(n, ty) => (ty == at).then_some(ValueKind::Int(n, at)),
            FieldConst::Float(bits, ty) => (ty == at).then_some(ValueKind::Float(bits, at)),
            FieldConst::Bool(b) => Some(ValueKind::Bool(b)),
            FieldConst::Char(c) => Some(ValueKind::Char(c)),
        }
    }
}

/// Constant fields keyed by field index.
type FieldConsts = IndexMap<u32, FieldConst>;

/// Identity of a clone: the original callee plus the bindings substituted into
/// it, as `(parameter local index, field index, constant)` sorted for a stable
/// key. Two call sites agreeing on the bindings share one clone.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SpecKey {
    callee: FuncId,
    bindings: Vec<(u32, u32, FieldConst)>,
}

/// Cross-iteration state for [`specialize_const_params`]. Lives beside the
/// optimizer's fixed-point loop so a clone minted in one iteration is reused
/// (rather than re-minted) by the next, and so the constants a clone's
/// parameters carry stay available to specialize one call deeper.
#[derive(Default)]
pub(super) struct ParamSpecState {
    /// Minted clones by identity.
    clones: IndexMap<SpecKey, FuncId>,
    /// Clones minted per original callee.
    per_callee: IndexMap<FuncId, usize>,
    /// What each clone knows about its own parameters, keyed by parameter
    /// *name*: `dae` renumbers local indices when it drops a parameter, so a
    /// positional key would silently drift onto a different parameter. Also the
    /// set of functions that are themselves clones, never specialized again.
    param_consts: IndexMap<FuncId, IndexMap<String, ParamSeed>>,
}

/// What a clone knows about one of its own parameters.
#[derive(Debug, Clone)]
struct ParamSeed {
    /// The struct the parameter referenced when the clone was minted. A later
    /// pass that reshapes the parameter (`sroa_param`) invalidates the seed, so
    /// the pointee is re-checked before the constants are believed.
    pointee: TypeId,
    consts: FieldConsts,
}

impl ParamSpecState {
    fn budget_exhausted(&self) -> bool {
        self.clones.len() >= MAX_TOTAL_SPECIALIZATIONS
    }
}

// ---------------------------------------------------------------------------
// Reachable-node enumeration
// ---------------------------------------------------------------------------

/// Visit every node reachable from the body root. Orphaned nodes an earlier
/// rewrite left behind are skipped, so a stale read cannot make a live
/// parameter look opaque.
fn for_each_reachable(body: &Body, mut f: impl FnMut(NodeRef)) {
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        f(node);
        body.for_each_child(node, |child| stack.push(child));
    }
}

// ---------------------------------------------------------------------------
// Use classification
// ---------------------------------------------------------------------------

/// How one tracked local (a struct local, or a struct-reference parameter) is
/// used within a single body. Shared by the callee-side summary and the
/// caller-side invalidation walk, which ask different questions of the same
/// facts.
#[derive(Debug, Default, Clone)]
struct LocalUses {
    /// Field indices read through the local.
    reads: IndexSet<u32>,
    /// Every write reaching a field: `Some(c)` for an assignment of the
    /// constant `c`, `None` for a non-constant assignment or a `&` / `&mut`
    /// borrow of the field.
    writes: Vec<(u32, Option<FieldConst>)>,
    /// Calls the local is passed to, as `(callee, callee parameter local index)`.
    forwards: Vec<(FuncId, u32)>,
    /// Operands assigned to the local itself.
    rebinds: Vec<Operand>,
    /// The local is used in a way the summary cannot classify; assume the worst.
    opaque: bool,
}

impl LocalUses {
    /// Field indices any write reaches, constant or not.
    fn written_fields(&self) -> IndexSet<u32> {
        self.writes.iter().map(|(field, _)| *field).collect()
    }
}

/// Per-function signature facts the classifier needs about callees.
struct Signatures {
    /// Parameter local indices per function index; empty for a bodyless record.
    param_locals: Vec<Vec<u32>>,
    /// The struct type each parameter's reference points at, when it is one.
    param_struct: Vec<Vec<Option<TypeId>>>,
    /// Whether the function has a body the summary can walk.
    has_body: Vec<bool>,
    /// Whether the function may be cloned. A `#[compiler_item]` function is the
    /// unique anchor passes resolve that item by, so a second copy carrying the
    /// same marker would shadow it.
    specializable: Vec<bool>,
}

impl Signatures {
    fn build(project: &NirPackage, types: &TypeTable) -> Self {
        let mut param_locals = Vec::with_capacity(project.functions.len());
        let mut param_struct = Vec::with_capacity(project.functions.len());
        let mut has_body = Vec::with_capacity(project.functions.len());
        let mut specializable = Vec::with_capacity(project.functions.len());
        for func in &project.functions {
            let func = func.borrow();
            param_locals.push(func.params.iter().map(|p| p.local_index).collect());
            param_struct.push(
                func.params
                    .iter()
                    .map(|p| {
                        // A parameter the callee may store a reference to
                        // outlives the call, so its fields are not ours to
                        // freeze.
                        if func.stores.contains(&p.name) {
                            None
                        } else {
                            reference_struct(p.type_id, types)
                        }
                    })
                    .collect(),
            );
            let live_body = func.body.is_some() && !func.is_dead;
            has_body.push(live_body);
            specializable.push(live_body && func.compiler_item.is_none());
        }
        Signatures {
            param_locals,
            param_struct,
            has_body,
            specializable,
        }
    }

    /// The parameter local index a call's argument at `position` binds to, for a
    /// callee whose body the summary can see.
    fn param_local(&self, callee: FuncId, position: usize) -> Option<u32> {
        let index = callee.index();
        if !self.has_body.get(index).copied().unwrap_or(false) {
            return None;
        }
        self.param_locals[index].get(position).copied()
    }
}

/// The struct type a `&T` / `&mut T` points at, looking through newtypes.
/// `None` for a non-reference type or a reference to a non-struct.
fn reference_struct(type_id: TypeId, types: &TypeTable) -> Option<TypeId> {
    match types.get_pruned(type_id)? {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            let inner = *inner;
            struct_base(inner, types)
        }
        _ => None,
    }
}

/// The struct type behind a type, looking through references and newtypes.
fn struct_base(type_id: TypeId, types: &TypeTable) -> Option<TypeId> {
    match types.get_pruned(type_id)? {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => struct_base(*inner, types),
        ResolvedType::Newtype { base_type, .. } => struct_base(*base_type, types),
        ResolvedType::Struct { .. } => Some(type_id),
        _ => None,
    }
}

/// Classify every reachable use of each tracked local in `body`.
fn collect_local_uses(
    body: &Body,
    tracked: &IndexSet<u32>,
    signatures: &Signatures,
    out: &mut IndexMap<u32, LocalUses>,
) {
    // A `FieldAccess` node's role depends on its parent, so the assignment
    // targets and borrow referents are gathered before the classifying walk.
    let mut assigned: IndexMap<ExprId, Operand> = IndexMap::default();
    let mut borrowed: IndexSet<ExprId> = IndexSet::default();
    for_each_reachable(body, |node| {
        let NodeRef::Expr(id) = node else {
            return;
        };
        match &body.exprs[id].kind {
            ExprKind::Assign { target, value } => {
                assigned.insert(*target, *value);
            }
            ExprKind::Unary {
                op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                expr,
            } => {
                if let Some(referent) = expr.as_expr() {
                    borrowed.insert(referent);
                }
            }
            _ => {}
        }
    });

    // Local reads reached through a classified position; anything left over is
    // a use the summary does not model.
    let mut classified: IndexSet<ExprId> = IndexSet::default();
    for_each_reachable(body, |node| {
        let NodeRef::Expr(id) = node else {
            return;
        };
        match &body.exprs[id].kind {
            ExprKind::FieldAccess {
                expr,
                field_index,
                field_name: _,
            } => {
                let Some(local) = tracked_local(body, *expr, tracked) else {
                    return;
                };
                classified.insert(expr.as_expr().expect("tracked local is a skeleton node"));
                let uses = out.entry(local).or_default();
                if borrowed.contains(&id) {
                    uses.writes.push((*field_index, None));
                    uses.reads.insert(*field_index);
                } else if let Some(value) = assigned.get(&id) {
                    uses.writes
                        .push((*field_index, FieldConst::of_operand(body, *value)));
                } else {
                    uses.reads.insert(*field_index);
                }
            }
            ExprKind::Call { func_id, args, .. } => {
                for (position, arg) in args.iter().enumerate() {
                    classify_argument(
                        body,
                        tracked,
                        signatures,
                        *func_id,
                        position,
                        arg.expr,
                        &mut classified,
                        out,
                    );
                }
            }
            ExprKind::MethodCall {
                func_id,
                receiver,
                args,
                ..
            } => {
                classify_argument(
                    body,
                    tracked,
                    signatures,
                    *func_id,
                    0,
                    *receiver,
                    &mut classified,
                    out,
                );
                for (position, arg) in args.iter().enumerate() {
                    classify_argument(
                        body,
                        tracked,
                        signatures,
                        *func_id,
                        position + 1,
                        arg.expr,
                        &mut classified,
                        out,
                    );
                }
            }
            ExprKind::Assign { target, value } => {
                if let ExprKind::Local { index, .. } = &body.exprs[*target].kind
                    && tracked.contains(index)
                {
                    classified.insert(*target);
                    out.entry(*index).or_default().rebinds.push(*value);
                }
            }
            _ => {}
        }
    });

    // Anything unclassified — a return value, an aggregate element, an indirect
    // call, a deref — leaves the local's storage beyond this summary's reach.
    for_each_reachable(body, |node| {
        match node {
            NodeRef::Expr(id) => {
                if let ExprKind::Local { index, .. } = &body.exprs[id].kind
                    && tracked.contains(index)
                    && !classified.contains(&id)
                {
                    out.entry(*index).or_default().opaque = true;
                }
            }
            NodeRef::Pat(id) => {
                // A destructuring binding rebinds the local to something the
                // summary never saw.
                if let PatKind::Binding { local_index, .. } = &body.pats[id].kind
                    && tracked.contains(local_index)
                {
                    out.entry(*local_index).or_default().opaque = true;
                }
            }
            NodeRef::Stmt(_) | NodeRef::Block(_) => {}
        }
    });
}

/// The tracked local an operand reads directly, if any.
fn tracked_local(body: &Body, op: Operand, tracked: &IndexSet<u32>) -> Option<u32> {
    let expr = op.as_expr()?;
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } if tracked.contains(index) => Some(*index),
        _ => None,
    }
}

/// The local an argument operand names, looking through the `&` / `&mut` a
/// by-value place is borrowed through at the call. Returns the local index and
/// the `Local` node, which the caller marks classified so the catch-all walk
/// does not read the same occurrence as an unmodelled escape.
fn argument_local(body: &Body, arg: Operand) -> Option<(u32, ExprId)> {
    let mut expr = arg.as_expr()?;
    if let ExprKind::Unary {
        op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
        expr: inner,
    } = &body.exprs[expr].kind
    {
        expr = inner.as_expr()?;
    }
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some((*index, expr)),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn classify_argument(
    body: &Body,
    tracked: &IndexSet<u32>,
    signatures: &Signatures,
    callee: FuncId,
    position: usize,
    arg: Operand,
    classified: &mut IndexSet<ExprId>,
    out: &mut IndexMap<u32, LocalUses>,
) {
    let Some((local, node)) = argument_local(body, arg) else {
        return;
    };
    if !tracked.contains(&local) {
        return;
    }
    classified.insert(node);
    let uses = out.entry(local).or_default();
    match signatures.param_local(callee, position) {
        Some(param_local) => uses.forwards.push((callee, param_local)),
        None => uses.opaque = true,
    }
}

// ---------------------------------------------------------------------------
// Callee-side summary
// ---------------------------------------------------------------------------

/// What a callee does to one of its struct-reference parameters, closed over
/// the calls the parameter is forwarded to.
#[derive(Debug, Default, Clone)]
struct ParamFacts {
    reads: IndexSet<u32>,
    writes: IndexSet<u32>,
    opaque: bool,
    forwards: Vec<(FuncId, u32)>,
}

/// Summarize every struct-reference parameter of every function, then close the
/// read / write / opaque facts over forwarding edges. Monotone, so the fixed
/// point is reached even through recursive call cycles.
fn summarize_params(
    project: &NirPackage,
    signatures: &Signatures,
) -> IndexMap<(FuncId, u32), ParamFacts> {
    let mut facts: IndexMap<(FuncId, u32), ParamFacts> = IndexMap::default();
    for (index, func_rc) in project.functions.iter().enumerate() {
        let func = func_rc.borrow();
        let Some(body) = &func.body else {
            continue;
        };
        if func.is_dead {
            continue;
        }
        let id = FuncId::new(index);
        let tracked: IndexSet<u32> = signatures.param_struct[index]
            .iter()
            .enumerate()
            .filter_map(|(position, pointee)| {
                pointee.map(|_| signatures.param_locals[index][position])
            })
            .collect();
        if tracked.is_empty() {
            continue;
        }
        let mut uses: IndexMap<u32, LocalUses> = IndexMap::default();
        collect_local_uses(body, &tracked, signatures, &mut uses);
        for local in tracked {
            let use_facts = uses.swap_remove(&local).unwrap_or_default();
            facts.insert(
                (id, local),
                ParamFacts {
                    writes: use_facts.written_fields(),
                    reads: use_facts.reads,
                    opaque: use_facts.opaque || !use_facts.rebinds.is_empty(),
                    forwards: use_facts.forwards,
                },
            );
        }
    }

    let mut changed = true;
    while changed {
        changed = false;
        let keys: Vec<(FuncId, u32)> = facts.keys().copied().collect();
        for key in keys {
            let forwards = facts[&key].forwards.clone();
            for edge in forwards {
                let Some(target) = facts.get(&edge).cloned() else {
                    // A forwarded-to parameter with no summary is one whose type
                    // is not a struct reference (or that the callee stores).
                    let current = &mut facts[&key];
                    changed |= !current.opaque;
                    current.opaque = true;
                    continue;
                };
                let current = &mut facts[&key];
                let before = (current.reads.len(), current.writes.len(), current.opaque);
                current.reads.extend(target.reads.iter().copied());
                current.writes.extend(target.writes.iter().copied());
                current.opaque |= target.opaque;
                changed |= before != (current.reads.len(), current.writes.len(), current.opaque);
            }
        }
    }
    facts
}

// ---------------------------------------------------------------------------
// Caller-side roots
// ---------------------------------------------------------------------------

/// The constant fields of a struct-literal operand, or `None` when the operand
/// is not one. `expected` guards against reading field indices off a literal of
/// a different struct than the local's declared type — a local slot reused for
/// two shapes would otherwise mix their field numbering.
fn struct_literal_consts(
    body: &Body,
    types: &TypeTable,
    value: Operand,
    expected: Option<TypeId>,
) -> Option<FieldConsts> {
    let expr = value.as_expr()?;
    let ExprKind::StructLiteral {
        struct_type,
        fields,
        ..
    } = &body.exprs[expr].kind
    else {
        return None;
    };
    if struct_base(*struct_type, types) != expected.and_then(|ty| struct_base(ty, types)) {
        return None;
    }
    Some(
        fields
            .iter()
            .filter_map(|field| {
                FieldConst::of_operand(body, field.value).map(|c| (field.field_index, c))
            })
            .collect(),
    )
}

/// The constant fields of every struct local (and already specialized
/// parameter) of `func` that survive every write and every call the local is
/// passed to. Flow-insensitive: a field is kept only when it holds the same
/// constant at every program point.
fn collect_roots(
    func: &NirFunction,
    body: &Body,
    types: &TypeTable,
    signatures: &Signatures,
    facts: &IndexMap<(FuncId, u32), ParamFacts>,
    seed: Option<&IndexMap<String, ParamSeed>>,
) -> IndexMap<u32, FieldConsts> {
    let locals = &func.locals;
    // A local is a root candidate when its declared type is a struct (or a
    // reference to one) — parameters included, so a clone's specialized
    // parameter propagates one call deeper.
    let mut roots: IndexMap<u32, FieldConsts> = IndexMap::default();
    for param in &func.params {
        let Some(entry) = seed.and_then(|seed| seed.get(&param.name)) else {
            continue;
        };
        if reference_struct(param.type_id, types) == Some(entry.pointee) {
            roots.insert(param.local_index, entry.consts.clone());
        }
    }

    let mut defined: IndexSet<u32> = roots.keys().copied().collect();
    let mut invalid: IndexSet<u32> = IndexSet::default();
    let mut note_def = |local: u32, value: Operand, roots: &mut IndexMap<u32, FieldConsts>| {
        let expected = locals.get(local as usize).map(|l| l.type_id);
        let Some(consts) = struct_literal_consts(body, types, value, expected) else {
            invalid.insert(local);
            return;
        };
        if defined.insert(local) {
            roots.insert(local, consts);
        } else if let Some(existing) = roots.get_mut(&local) {
            existing.retain(|field, c| consts.get(field) == Some(c));
        }
    };

    for_each_reachable(body, |node| match node {
        NodeRef::Stmt(id) => {
            if let StmtKind::Let {
                local_index, value, ..
            } = &body.stmts[id].kind
                && locals
                    .get(*local_index as usize)
                    .is_some_and(|l| struct_base(l.type_id, types).is_some())
            {
                note_def(*local_index, *value, &mut roots);
            }
        }
        NodeRef::Expr(_) | NodeRef::Block(_) | NodeRef::Pat(_) => {}
    });

    for local in invalid {
        roots.swap_remove(&local);
    }
    roots.retain(|_, consts| !consts.is_empty());
    if roots.is_empty() {
        return roots;
    }

    let tracked: IndexSet<u32> = roots.keys().copied().collect();
    let mut uses: IndexMap<u32, LocalUses> = IndexMap::default();
    collect_local_uses(body, &tracked, signatures, &mut uses);

    for (local, use_facts) in &uses {
        let Some(consts) = roots.get_mut(local) else {
            continue;
        };
        if use_facts.opaque {
            consts.clear();
            continue;
        }
        for (field, written) in &use_facts.writes {
            if consts.get(field) != written.as_ref() {
                consts.swap_remove(field);
            }
        }
        for rebind in &use_facts.rebinds {
            let expected = locals.get(*local as usize).map(|l| l.type_id);
            let Some(assigned) = struct_literal_consts(body, types, *rebind, expected) else {
                consts.clear();
                break;
            };
            consts.retain(|field, c| assigned.get(field) == Some(c));
        }
        for edge in &use_facts.forwards {
            match facts.get(edge) {
                Some(param) if !param.opaque => {
                    consts.retain(|field, _| !param.writes.contains(field));
                }
                Some(_) | None => consts.clear(),
            }
        }
    }
    roots.retain(|_, consts| !consts.is_empty());
    roots
}

// ---------------------------------------------------------------------------
// Site selection
// ---------------------------------------------------------------------------

/// One call to retarget: the call node, the callee, and the bindings its clone
/// substitutes.
#[derive(Debug)]
struct Site {
    call: ExprId,
    callee: FuncId,
    bindings: Vec<(u32, u32, FieldConst)>,
}

fn select_sites(
    locals: &[crate::nir::NirLocal],
    body: &Body,
    types: &TypeTable,
    signatures: &Signatures,
    facts: &IndexMap<(FuncId, u32), ParamFacts>,
    roots: &IndexMap<u32, FieldConsts>,
    state: &ParamSpecState,
) -> Vec<Site> {
    let mut sites = Vec::new();
    for_each_reachable(body, |node| {
        let NodeRef::Expr(id) = node else {
            return;
        };
        let (callee, arguments): (FuncId, Vec<(usize, Operand)>) = match &body.exprs[id].kind {
            ExprKind::Call { func_id, args, .. } => (
                *func_id,
                args.iter().enumerate().map(|(i, a)| (i, a.expr)).collect(),
            ),
            ExprKind::MethodCall {
                func_id,
                receiver,
                args,
                ..
            } => {
                let mut all = vec![(0, *receiver)];
                all.extend(args.iter().enumerate().map(|(i, a)| (i + 1, a.expr)));
                (*func_id, all)
            }
            _ => return,
        };
        if state.param_consts.contains_key(&callee) {
            // Already a clone: its constants are substituted, and re-cloning it
            // would key on a moving identity.
            return;
        }
        let index = callee.index();
        if !signatures.specializable[index] {
            return;
        }
        let mut bindings: Vec<(u32, u32, FieldConst)> = Vec::new();
        for (position, arg) in arguments {
            let Some((local, _)) = argument_local(body, arg) else {
                continue;
            };
            let Some(consts) = roots.get(&local) else {
                continue;
            };
            let Some(pointee) = signatures.param_struct[index]
                .get(position)
                .copied()
                .flatten()
            else {
                continue;
            };
            let local_struct = locals
                .get(local as usize)
                .and_then(|l| struct_base(l.type_id, types));
            if local_struct != Some(pointee) {
                continue;
            }
            let param_local = signatures.param_locals[index][position];
            let Some(param) = facts.get(&(callee, param_local)) else {
                continue;
            };
            if param.opaque {
                continue;
            }
            for (field, value) in consts {
                if param.writes.contains(field) || !param.reads.contains(field) {
                    continue;
                }
                bindings.push((param_local, *field, *value));
            }
        }
        if bindings.is_empty() {
            return;
        }
        bindings.sort_unstable_by_key(|(param, field, value)| (*param, *field, value.sort_key()));
        bindings.dedup();
        sites.push(Site {
            call: id,
            callee,
            bindings,
        });
    });
    sites
}

// ---------------------------------------------------------------------------
// Pass entry
// ---------------------------------------------------------------------------

/// Specialize callees on the constant fields of the struct references their
/// callers pass. Returns whether anything changed.
pub(super) fn specialize_const_params(
    project: &mut NirPackage,
    state: &mut ParamSpecState,
    gate: &mut FunctionGate,
) -> bool {
    if state.budget_exhausted() {
        return false;
    }
    let signatures = {
        let types = project.type_table.borrow();
        Signatures::build(project, &types)
    };
    let facts = summarize_params(project, &signatures);
    if facts.is_empty() {
        return false;
    }

    // Phase 1: pick the call sites, holding only shared borrows.
    let mut per_caller: Vec<(usize, Vec<Site>)> = Vec::new();
    {
        let types = project.type_table.borrow();
        for (index, func_rc) in project.functions.iter().enumerate() {
            let func = func_rc.borrow();
            let Some(body) = &func.body else {
                continue;
            };
            if func.is_dead {
                continue;
            }
            let seed = state.param_consts.get(&FuncId::new(index));
            let roots = collect_roots(&func, body, &types, &signatures, &facts, seed);
            if roots.is_empty() {
                continue;
            }
            let sites = select_sites(
                &func.locals,
                body,
                &types,
                &signatures,
                &facts,
                &roots,
                state,
            );
            if !sites.is_empty() {
                per_caller.push((index, sites));
            }
        }
    }
    if per_caller.is_empty() {
        return false;
    }

    // Phase 2: mint the clones each distinct binding set needs.
    let mut retarget: Vec<(usize, ExprId, FuncId)> = Vec::new();
    let mut minted: Vec<Rc<RefCell<NirFunction>>> = Vec::new();
    let mut next_id = project.next_func_id().index();
    for (caller, sites) in &per_caller {
        for site in sites {
            let key = SpecKey {
                callee: site.callee,
                bindings: site.bindings.clone(),
            };
            if let Some(&existing) = state.clones.get(&key) {
                retarget.push((*caller, site.call, existing));
                continue;
            }
            if state.budget_exhausted() {
                continue;
            }
            let ordinal = state.per_callee.get(&site.callee).copied().unwrap_or(0);
            if ordinal >= MAX_SPECIALIZATIONS_PER_FUNCTION {
                continue;
            }
            let Some(clone) = build_clone(project, site, FuncId::new(next_id), ordinal) else {
                continue;
            };
            next_id += 1;
            state.clones.insert(key, clone.id);
            state.per_callee.insert(site.callee, ordinal + 1);
            state.param_consts.insert(clone.id, clone.param_consts);
            crate::compiler_trace!(
                "param_spec",
                "specialized {} on {} field(s)",
                project.functions[site.callee.index()].borrow().name,
                site.bindings.len()
            );
            retarget.push((*caller, site.call, clone.id));
            minted.push(clone.function);
        }
    }
    if retarget.is_empty() {
        return false;
    }

    // Phase 3: retarget the call sites — a mechanical `func_id` swap.
    for (caller, call, target) in &retarget {
        let mut func = project.functions[*caller].borrow_mut();
        let Some(body) = &mut func.body else {
            continue;
        };
        match &mut body.exprs[*call].kind {
            ExprKind::Call { func_id, .. } | ExprKind::MethodCall { func_id, .. } => {
                *func_id = *target;
            }
            _ => panic!("param_spec site is not a call"),
        }
    }
    for func in minted {
        project.functions.push(func);
    }
    for (caller, _, _) in retarget {
        gate.mark_changed(FuncId::new(caller));
    }
    true
}

/// A freshly minted clone and the facts recorded for it.
struct Clone {
    id: FuncId,
    function: Rc<RefCell<NirFunction>>,
    param_consts: IndexMap<String, ParamSeed>,
}

/// Clone `site.callee` with its bindings substituted, registering the clone in
/// the function index so its call sites are born resolved. `None` when the
/// callee is too large to duplicate or the substitution matched nothing.
fn build_clone(project: &mut NirPackage, site: &Site, id: FuncId, ordinal: usize) -> Option<Clone> {
    let mut clone = project.functions[site.callee.index()].borrow().clone();
    let body_size = clone.body.as_ref().map_or(0, |b| b.exprs.len());
    if body_size > MAX_CLONE_EXPRS {
        return None;
    }
    let origin = (clone.module_source.clone(), clone.name.clone());
    let name = crate::name::param_spec_name(&clone.name, ordinal);
    clone.name.clone_from(&name);
    // A method's call-graph identity comes from its `method_name`, not only its
    // function name: DCE marks a `MethodCall`'s target through `method_info`.
    // Leaving the original's name there would resolve every call to the clone
    // back to the original, and the clone would be pruned out from under its
    // live call site.
    if let Some(info) = &mut clone.method_info {
        info.method_name = crate::name::param_spec_name(&info.method_name, ordinal);
    }
    clone.id = Some(id);
    clone.visibility = crate::ast::Visibility::Private;
    clone.is_export = false;

    let mut bound: IndexMap<u32, FieldConsts> = IndexMap::default();
    for (param_local, field, value) in &site.bindings {
        bound
            .entry(*param_local)
            .or_default()
            .insert(*field, *value);
    }

    // The substitution may match nothing — a pass-through callee reads none of
    // the bound fields itself, only the callees it forwards to do. The clone is
    // still what carries the constants to them: its recorded `param_consts` make
    // the next iteration specialize one call deeper.
    let body = clone.body.as_mut().expect("specialized callee has a body");
    let mut buffers = EngineBuffers::default();
    substitute_fields(body, &mut clone.locals, &mut buffers, &bound);

    let types = project.type_table.borrow();
    let param_consts: IndexMap<String, ParamSeed> = clone
        .params
        .iter()
        .filter_map(|param| {
            let consts = bound.get(&param.local_index)?.clone();
            let pointee = reference_struct(param.type_id, &types)?;
            Some((param.name.clone(), ParamSeed { pointee, consts }))
        })
        .collect();
    drop(types);

    let key = FunctionRef::from_resolved(&clone, clone.module_source.clone()).function_id();
    project.func_index.insert(key, id);
    // `function_strings` / `function_method_info` are name-keyed, and DCE reads
    // them to decide which string literals survive; the clone needs its own
    // entries or its literals are pruned out from under it.
    if let Some(strings) = project.function_strings.get(&origin).cloned() {
        project
            .function_strings
            .insert((clone.module_source.clone(), name.clone()), strings);
    }
    if let Some(info) = project.function_method_info.get(&origin).cloned() {
        project
            .function_method_info
            .insert((clone.module_source.clone(), name), info);
    }

    Some(Clone {
        id,
        function: Rc::new(RefCell::new(clone)),
        param_consts,
    })
}

/// Replace every value-position read of a bound field with its constant.
/// Returns whether any read was substituted.
fn substitute_fields(
    body: &mut Body,
    locals: &mut Vec<crate::nir::NirLocal>,
    buffers: &mut EngineBuffers,
    bindings: &IndexMap<u32, FieldConsts>,
) -> bool {
    let mut engine = Engine::new(body, buffers, locals);
    let mut reads: Vec<(ExprId, FieldConst)> = Vec::new();
    for_each_reachable(engine.body, |node| {
        let NodeRef::Expr(id) = node else {
            return;
        };
        let ExprKind::FieldAccess {
            expr, field_index, ..
        } = &engine.body.exprs[id].kind
        else {
            return;
        };
        let Some(receiver) = expr.as_expr() else {
            return;
        };
        let ExprKind::Local { index, .. } = &engine.body.exprs[receiver].kind else {
            return;
        };
        if let Some(value) = bindings.get(index).and_then(|f| f.get(field_index)) {
            reads.push((id, *value));
        }
    });

    let mut changed = false;
    for (read, value) in reads {
        // A place-position read hands out storage, not a value; substituting
        // there would destroy the place.
        if super::extract::is_place_read(&engine, read) {
            continue;
        }
        let type_id = engine.body.exprs[read].type_id;
        let Some(kind) = value.value_kind_at(type_id) else {
            continue;
        };
        let interned = engine.body.values.alloc_unshared(kind, type_id);
        changed |= engine.redirect_expr(read, Operand::Value(interned));
    }
    changed
}
