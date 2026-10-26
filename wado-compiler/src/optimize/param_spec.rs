//! Constant-argument propagation, and specialization on a constant argument or
//! on a constant field of one passed by reference.
//! Compiler items and cached clones retain their contracts for synthesized calls.

use std::sync::Arc;

use cranelift_entity::EntityRef;

use crate::const_eval::MAX_SEQ_ELEMENTS;
use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{
    FuncCell, FuncId, FuncParts, FuncRef, FunctionRef, NirFunction, NirParam, NirUnaryOp,
};
use crate::nir_arena::{
    ArenaStructField, Body, ExprId, ExprKind, NodeRef, Operand, PackedData, PatKind, StmtKind,
    Tracked,
};
use crate::nir_engine::{Engine, EngineBuffers};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueKind;
use crate::niri::{
    Callee, CalleeMap, CtfeBuiltinMap, GlobalKey, build_callee_map, build_ctfe_builtin_map,
};
use crate::tir::{ResolvedType, TypeId};
use crate::token::Span;
use crate::type_table::{TypeKey, TypeTable};

use super::arena_query::ColdRuns;
use super::body_memo::BodyMemo;
use super::const_branch_prune::BranchPruneRule;
use super::const_folding::ConstFoldRule;
use super::dae::is_dae_sroa_eligible;
use super::dce::{DescriptorCache, ReachabilityCache, reachable_function_positions};
use super::extract::is_place_read;
use super::gate::{FunctionGate, GatedPass};
use super::inline::{body_callees, recursive_functions};
use crate::ast::Visibility;
use crate::compiler_trace;
use crate::module_source::ModuleSource;
use crate::name::{COLD_PATH_INTRINSIC, param_spec_name};
use crate::nir::NirLocal;
use crate::parallel::Executor;

/// Whole-module cap on synthesized clones, so a pathological program cannot
/// trade unbounded code size for constant propagation.
const MAX_TOTAL_SPECIALIZATIONS: usize = 128;

/// Largest callee body (expression nodes) worth cloning. A body this large
/// costs more in duplicated code than its folded branches can return.
const MAX_CLONE_EXPRS: usize = 2000;

/// A constant known for a parameter or field. Portable across functions,
/// unlike a pool-local `ValueId`, and hashable so it can key the clone cache.
/// `Int` / `Float` carry their type so a width mismatch is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FieldConst {
    Int(u64, ConstType),
    Float(u64, ConstType),
    Bool(bool),
    Char(char),
    /// A literal built of constants alone, such as a `String` literal. The
    /// clone builds it once and its reads name that one object, as the
    /// caller's reads named the one it passed.
    Literal(Arc<ConstLiteral>),
    /// `&GLOBAL` for an immutable global: every read of it names what the
    /// caller's reference named.
    GlobalRef {
        module_source: ModuleSource,
        name: String,
        global_type: ConstType,
        ref_type: ConstType,
    },
}

/// The type of a constant: its key says which constants are one, and its id
/// types a node rebuilt from it.
#[derive(Debug, Clone, Copy)]
struct ConstType {
    key: TypeKey,
    id: TypeId,
}

impl ConstType {
    fn of(types: &TypeTable, id: TypeId) -> Self {
        ConstType {
            key: types.type_key(id),
            id,
        }
    }

    fn admits(&self, types: &TypeTable, at: TypeId) -> bool {
        self.key == types.type_key(at)
    }
}

impl PartialEq for ConstType {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for ConstType {}

impl std::hash::Hash for ConstType {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

/// A closed literal tree, held outside any body so a clone can rebuild it.
#[derive(Debug, PartialEq, Eq, Hash)]
enum ConstLiteral {
    Packed(PackedData, ConstType),
    Struct {
        struct_type: ConstType,
        struct_name: String,
        /// Index, name, value and type of each field.
        fields: Vec<(u32, String, FieldConst, ConstType)>,
    },
}

impl FieldConst {
    /// The constant an operand holds, or `None` otherwise.
    fn of_operand(
        body: &Body,
        types: &TypeTable,
        immutable: &IndexSet<GlobalKey>,
        op: Operand,
    ) -> Option<Self> {
        match op {
            Operand::Value(_) => Self::of_scalar(body, types, op),
            Operand::Expr(expr) => Self::of_literal(body, types, immutable, expr),
        }
    }

    /// The scalar constant an operand holds. Only a scalar may stand for a
    /// whole parameter: a literal there would name a new object, and the
    /// body's writes to the parameter's fields would reach none of its reads.
    fn of_scalar(body: &Body, types: &TypeTable, op: Operand) -> Option<Self> {
        let value = op.as_value()?;
        match body.values.kind(value) {
            ValueKind::Int(n, ty) => Some(FieldConst::Int(*n, ConstType::of(types, *ty))),
            ValueKind::Float(bits, ty) => Some(FieldConst::Float(*bits, ConstType::of(types, *ty))),
            ValueKind::Bool(b) => Some(FieldConst::Bool(*b)),
            ValueKind::Char(c) => Some(FieldConst::Char(*c)),
            ValueKind::Null
            | ValueKind::Unit
            | ValueKind::Const(..)
            | ValueKind::Opaque(_)
            | ValueKind::Binary { .. }
            | ValueKind::Unary { .. }
            | ValueKind::Cast { .. }
            | ValueKind::Select { .. }
            | ValueKind::LoopPhi { .. }
            | ValueKind::FieldAccess { .. } => None,
        }
    }

    fn of_literal(
        body: &Body,
        types: &TypeTable,
        immutable: &IndexSet<GlobalKey>,
        expr: ExprId,
    ) -> Option<Self> {
        let node = &body.exprs[expr];
        let literal = match &node.kind {
            ExprKind::PackedArray(data) if data.len() <= MAX_SEQ_ELEMENTS => {
                ConstLiteral::Packed(data.clone(), ConstType::of(types, node.type_id))
            }
            ExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => ConstLiteral::Struct {
                struct_type: ConstType::of(types, *struct_type),
                struct_name: struct_name.clone(),
                fields: fields
                    .iter()
                    .map(|f| {
                        let value = Self::of_operand(body, types, immutable, f.value)?;
                        let ty = ConstType::of(types, body.operand_type(f.value));
                        Some((f.field_index, f.name.clone(), value, ty))
                    })
                    .collect::<Option<_>>()?,
            },
            ExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr: Operand::Expr(inner),
            } if let ExprKind::GlobalVarGet {
                module_source,
                name,
            } = &body.exprs[*inner].kind
                && immutable.contains(&(module_source.clone(), name.clone())) =>
            {
                return Some(FieldConst::GlobalRef {
                    module_source: module_source.clone(),
                    name: name.clone(),
                    global_type: ConstType::of(types, body.exprs[*inner].type_id),
                    ref_type: ConstType::of(types, node.type_id),
                });
            }
            _ => return None,
        };
        Some(FieldConst::Literal(Arc::new(literal)))
    }

    /// The operand to put at a read of type `at`, or `None` when the read's
    /// type does not match the constant's (a width or representation mismatch
    /// the substitution must not paper over).
    fn operand_at(
        &self,
        engine: &mut Engine,
        types: &TypeTable,
        at: TypeId,
        span: Span,
    ) -> Option<Operand> {
        let kind = match self {
            FieldConst::Int(n, ty) => ty.admits(types, at).then_some(ValueKind::Int(*n, at))?,
            FieldConst::Float(bits, ty) => ty
                .admits(types, at)
                .then_some(ValueKind::Float(*bits, at))?,
            FieldConst::Bool(b) => ValueKind::Bool(*b),
            FieldConst::Char(c) => ValueKind::Char(*c),
            FieldConst::Literal(literal) => {
                return literal.build(engine, types, at, span).map(Operand::Expr);
            }
            FieldConst::GlobalRef {
                module_source,
                name,
                global_type,
                ref_type,
            } => {
                if !ref_type.admits(types, at) {
                    return None;
                }
                let global = engine.alloc_expr(
                    ExprKind::GlobalVarGet {
                        module_source: module_source.clone(),
                        name: name.clone(),
                    },
                    global_type.id,
                    span,
                );
                let reference = ExprKind::Unary {
                    op: NirUnaryOp::Ref,
                    expr: Operand::Expr(global),
                };
                return Some(Operand::Expr(engine.alloc_expr(reference, at, span)));
            }
        };
        Some(Operand::Value(engine.body.values.alloc_unshared(kind, at)))
    }
}

impl ConstLiteral {
    fn ty(&self) -> ConstType {
        match self {
            ConstLiteral::Packed(_, ty)
            | ConstLiteral::Struct {
                struct_type: ty, ..
            } => *ty,
        }
    }

    fn build(
        &self,
        engine: &mut Engine,
        types: &TypeTable,
        at: TypeId,
        span: Span,
    ) -> Option<ExprId> {
        if !self.ty().admits(types, at) {
            return None;
        }
        let kind = match self {
            ConstLiteral::Packed(data, _) => ExprKind::PackedArray(data.clone()),
            ConstLiteral::Struct {
                struct_type,
                struct_name,
                fields,
            } => {
                let fields = fields
                    .iter()
                    .map(|(field_index, name, value, ty)| {
                        Some(ArenaStructField {
                            name: name.clone(),
                            value: value.operand_at(engine, types, ty.id, span)?,
                            field_index: *field_index,
                        })
                    })
                    .collect::<Option<_>>()?;
                ExprKind::StructLiteral {
                    struct_type: struct_type.id,
                    struct_name: struct_name.clone(),
                    fields,
                }
            }
        };
        Some(engine.alloc_expr(kind, at, span))
    }
}

/// Constant fields keyed by field index.
type FieldConsts = IndexMap<u32, FieldConst>;

/// What a binding replaces: a by-value scalar parameter itself, or one field
/// reached through a struct-reference parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Slot {
    Whole,
    Field(u32),
}

/// One substitution a clone carries: the callee parameter's local index, the
/// slot it replaces, and the constant.
type Binding = (u32, Slot, FieldConst);

/// One substitution by the parameter's name, which `dae` leaves alone while it
/// renumbers locals.
type NamedBinding = (String, Slot, FieldConst);

/// Identity of a clone: the function no clone came from, and every binding
/// substituted on the way down, sorted for a stable key. Two call sites
/// agreeing on them share one clone, by however many steps each got there.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SpecKey {
    root: FuncId,
    bindings: Vec<NamedBinding>,
}

/// Cross-iteration state, so a clone minted in one iteration is reused by the
/// next rather than re-minted, and its parameters' constants stay available to
/// specialize one call deeper.
#[derive(Default)]
pub(super) struct ParamSpecState {
    /// Minted clones by identity.
    clones: IndexMap<SpecKey, FuncId>,
    /// Each clone's identity, which a clone of it extends.
    specs: IndexMap<FuncId, SpecKey>,
    /// Clones minted per original callee, which numbers the next one's name.
    per_callee: IndexMap<FuncId, usize>,
    /// What each clone knows about its own parameters. Keyed by *name*, since
    /// `dae` renumbers local indices when it drops a parameter. Doubles as the
    /// set of clones.
    param_consts: IndexMap<FuncId, IndexMap<String, ParamSeed>>,
    /// The call graph whose cycles a clone must not split.
    callees: BodyMemo<Vec<usize>>,
}

/// What a clone knows about one of its own parameters.
#[derive(Debug, Clone)]
struct ParamSeed {
    /// The struct the parameter referenced when the clone was minted,
    /// re-checked before the constants are believed — `sroa_param` reshapes
    /// parameters.
    pointee: TypeKey,
    consts: FieldConsts,
}

impl ParamSpecState {
    fn budget_exhausted(&self) -> bool {
        self.clones.len() >= MAX_TOTAL_SPECIALIZATIONS
    }

    /// Whether `func` is itself a clone this pass minted.
    fn is_clone(&self, func: FuncId) -> bool {
        self.param_consts.contains_key(&func)
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
    /// of the field.
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

/// The signature facts the classifier needs about callees, by function index.
struct Signatures {
    of: Vec<Signature>,
    /// The globals no write reaches, whose `&G` a clone may name again.
    immutable_globals: IndexSet<GlobalKey>,
}

struct Signature {
    /// Parameter local indices; empty for a bodyless record.
    param_locals: Vec<u32>,
    /// Parameter names, beside `param_locals`.
    param_names: Vec<String>,
    /// The struct type each parameter's reference points at, when it is one.
    /// `None` for a parameter the callee may store a reference to: it outlives
    /// the call, so its fields are not ours to freeze.
    param_struct: Vec<Option<TypeKey>>,
    /// Whether the function has a body the summary can walk and a clone can copy.
    has_body: bool,
    /// Whether each parameter's own slot may be replaced by a constant. A
    /// slot the body reassigns, references or aliases holds more than the argument.
    param_fixed: Vec<bool>,
    /// Whether the function's scalar arguments may move into a clone. A callee
    /// writing through a reference is one a compile-time frame runs for those
    /// writes, and a constant it no longer receives is one that frame cannot
    /// see — which costs a whole folded region to save a parameter. A callee on
    /// a call cycle is one whose recursion the constant decides nothing about,
    /// and cloning one member splits a cycle the return-ABI analysis reasons
    /// about whole.
    scalar_specializable: bool,
}

impl Signatures {
    fn build(
        project: &NirPackage,
        types: &TypeTable,
        recursive: &IndexSet<FuncId>,
        exec: &Executor,
    ) -> Self {
        let functions = &project.functions;
        let of = exec.map_indices(functions.len(), |index| {
            let func = functions[index].borrow();
            Signature {
                param_locals: func.params.iter().map(|p| p.local_index).collect(),
                param_names: func.params.iter().map(|p| p.name.clone()).collect(),
                param_struct: func
                    .params
                    .iter()
                    .map(|p| {
                        if func.retains.contains(&p.name) {
                            None
                        } else {
                            is_param_fixed(&func, p)
                                .then(|| held_struct(p.type_id, types))
                                .flatten()
                                .or_else(|| reference_struct(p.type_id, types))
                        }
                    })
                    .collect(),
                has_body: func.body.is_some() && !func.is_dead,
                param_fixed: func
                    .params
                    .iter()
                    .map(|p| is_param_fixed(&func, p))
                    .collect(),
                scalar_specializable: !func.params.iter().any(|p| p.is_mut_ref)
                    && !recursive.contains(&FuncId::new(index)),
            }
        });
        Signatures {
            of,
            immutable_globals: project
                .globals
                .iter()
                .filter(|g| !g.wado_mutable)
                .map(|g| (g.module_source.clone(), g.name.clone()))
                .collect(),
        }
    }

    /// The parameter local index a call's argument at `position` binds to, for a
    /// callee whose body the summary can see.
    fn param_local(&self, callee: FuncId, position: usize) -> Option<u32> {
        let signature = self.of.get(callee.index()).filter(|s| s.has_body)?;
        signature.param_locals.get(position).copied()
    }
}

/// Whether the parameter's slot holds exactly what the argument gave it for the
/// whole body, so every read of it is that constant. A reference to an
/// immutable slot cannot write it, so taking one leaves it fixed.
fn is_param_fixed(func: &NirFunction, param: &NirParam) -> bool {
    !param.is_mut && !param.is_mut_ref && !func.stores_aliased_locals.contains(&param.local_index)
}

/// The struct type a `&T` / `&mut T` points at, looking through newtypes.
/// `None` for a non-reference type or a reference to a non-struct.
fn reference_struct(type_id: TypeId, types: &TypeTable) -> Option<TypeKey> {
    match types.get_pruned(type_id)? {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            let inner = *inner;
            struct_base(inner, types)
        }
        _ => None,
    }
}

/// The struct a parameter holds, by value or through one reference, looking
/// through newtypes. A by-value one is the callee's own copy, so only a slot
/// the body never rebinds holds the caller's fields throughout.
fn held_struct(type_id: TypeId, types: &TypeTable) -> Option<TypeKey> {
    match types.get_pruned(type_id)? {
        ResolvedType::Ref(_) | ResolvedType::MutRef(_) => reference_struct(type_id, types),
        _ => struct_base(type_id, types),
    }
}

/// The struct type behind a type, looking through references and newtypes.
fn struct_base(type_id: TypeId, types: &TypeTable) -> Option<TypeKey> {
    match types.get_pruned(type_id)? {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => struct_base(*inner, types),
        ResolvedType::Newtype { base_type, .. } => struct_base(*base_type, types),
        ResolvedType::Struct { .. } => Some(types.type_key(type_id)),
        _ => None,
    }
}

/// Classify every reachable use of each tracked local in `body`. `alias_reads`
/// names the reads that bind a reference tracked alongside its referent, which are
/// accounted by that pairing rather than by this walk.
fn collect_local_uses(
    body: &Body,
    types: &TypeTable,
    tracked: &IndexSet<u32>,
    alias_reads: &IndexSet<ExprId>,
    signatures: &Signatures,
    out: &mut IndexMap<u32, LocalUses>,
) {
    let (assigned, referenced) = collect_place_contexts(body);
    let mut classified: IndexSet<ExprId> = alias_reads.clone();
    body.for_each_reachable_node(|node| {
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
                if referenced.contains(&id) {
                    uses.writes.push((*field_index, None));
                    uses.reads.insert(*field_index);
                } else if let Some(value) = assigned.get(&id) {
                    uses.writes.push((
                        *field_index,
                        FieldConst::of_operand(body, types, &signatures.immutable_globals, *value),
                    ));
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

    mark_unclassified_opaque(body, tracked, &classified, out);
}

/// The value each assignment target receives, and the referents of `&` /
/// `&mut`. Gathered up front because a `FieldAccess`'s role depends on its
/// parent, which the classifying walk may not have visited yet.
fn collect_place_contexts(body: &Body) -> (IndexMap<ExprId, Operand>, IndexSet<ExprId>) {
    let mut assigned: IndexMap<ExprId, Operand> = IndexMap::default();
    let mut referenced: IndexSet<ExprId> = IndexSet::default();
    let mut mut_referenced: Vec<ExprId> = Vec::new();
    body.for_each_reachable_node(|node| {
        let NodeRef::Expr(id) = node else {
            return;
        };
        match &body.exprs[id].kind {
            ExprKind::Assign { target, value } => {
                assigned.insert(*target, *value);
            }
            ExprKind::Unary {
                op: op @ (NirUnaryOp::Ref | NirUnaryOp::MutRef),
                expr,
            } => {
                if let Some(referent) = expr.as_expr() {
                    referenced.insert(referent);
                    if *op == NirUnaryOp::MutRef {
                        mut_referenced.push(referent);
                    }
                }
            }
            ExprKind::Call { args, .. } => {
                for arg in args.iter().filter(|a| a.is_mut) {
                    if let Some(place) = arg.expr.as_expr() {
                        referenced.insert(place);
                        mut_referenced.push(place);
                    }
                }
            }
            _ => {}
        }
    });
    // A write through `p.s.used`, or a `&mut` to it, writes the field `s` too,
    // which matters once a field's constant is an aggregate.
    for place in assigned.keys().copied().chain(mut_referenced) {
        let mut place = place;
        while let ExprKind::FieldAccess { expr, .. } | ExprKind::Index { expr, .. } =
            &body.exprs[place].kind
            && let Some(receiver) = expr.as_expr()
        {
            referenced.insert(receiver);
            place = receiver;
        }
    }
    (assigned, referenced)
}

/// Mark every tracked local the classifying walk did not reach — a return
/// value, an aggregate element, an indirect call, a deref, a destructuring
/// binding. Its storage is beyond this summary.
fn mark_unclassified_opaque(
    body: &Body,
    tracked: &IndexSet<u32>,
    classified: &IndexSet<ExprId>,
    out: &mut IndexMap<u32, LocalUses>,
) {
    body.for_each_reachable_node(|node| match node {
        NodeRef::Expr(id) => {
            if let ExprKind::Local { index, .. } = &body.exprs[id].kind
                && tracked.contains(index)
                && !classified.contains(&id)
            {
                out.entry(*index).or_default().opaque = true;
            }
        }
        NodeRef::Pat(id) => {
            if let PatKind::Binding { local_index, .. } = &body.pats[id].kind
                && tracked.contains(local_index)
            {
                out.entry(*local_index).or_default().opaque = true;
            }
        }
        NodeRef::Stmt(_) | NodeRef::Block(_) => {}
    });
}

/// The tracked local an operand reads directly, if any.
fn tracked_local(body: &Body, op: Operand, tracked: &IndexSet<u32>) -> Option<u32> {
    body.local_read(op)
        .map(|(local, _)| local)
        .filter(|local| tracked.contains(local))
}

/// The local an argument names, looking through the `&` / `&mut` a by-value
/// place is referenced through. Returns the `Local` node too, which the caller
/// marks classified so the catch-all does not re-read it as an escape.
fn argument_local(body: &Body, arg: Operand) -> Option<(u32, ExprId)> {
    body.referenced_local(arg).or_else(|| body.local_read(arg))
}

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
    exec: &Executor,
) -> IndexMap<(FuncId, u32), ParamFacts> {
    let functions = &project.functions;
    let types = project.type_table.borrow();
    let types: &TypeTable = &types;
    let per_function = exec.map_indices(functions.len(), |index| {
        let func = functions[index].borrow();
        let Some(body) = func.body.as_ref().filter(|_| !func.is_dead) else {
            return Vec::new();
        };
        let id = FuncId::new(index);
        let signature = &signatures.of[index];
        let tracked: IndexSet<u32> = signature
            .param_struct
            .iter()
            .zip(&signature.param_locals)
            .filter_map(|(pointee, &local)| pointee.map(|_| local))
            .collect();
        if tracked.is_empty() {
            return Vec::new();
        }
        let mut uses: IndexMap<u32, LocalUses> = IndexMap::default();
        let (alias_of, alias_reads) = param_aliases(body, &tracked);
        let with_aliases = tracked.iter().chain(alias_of.keys()).copied().collect();
        collect_local_uses(
            body,
            types,
            &with_aliases,
            &alias_reads,
            signatures,
            &mut uses,
        );
        tracked
            .into_iter()
            .map(|local| {
                let mut facts = ParamFacts::default();
                let names = alias_of
                    .iter()
                    .filter(|(_, base)| **base == local)
                    .map(|(alias, _)| *alias);
                for name in std::iter::once(local).chain(names) {
                    let use_facts = uses.swap_remove(&name).unwrap_or_default();
                    facts.writes.extend(use_facts.written_fields());
                    facts.reads.extend(use_facts.reads);
                    facts.opaque |= use_facts.opaque || !use_facts.rebinds.is_empty();
                    facts.forwards.extend(use_facts.forwards);
                }
                ((id, local), facts)
            })
            .collect()
    });
    let mut facts: IndexMap<(FuncId, u32), ParamFacts> =
        per_function.into_iter().flatten().collect();

    let mut changed = true;
    while changed {
        changed = false;
        let keys: Vec<(FuncId, u32)> = facts.keys().copied().collect();
        for key in keys {
            let forwards = facts[&key].forwards.clone();
            for edge in forwards {
                // No summary means the callee's parameter is not a tracked
                // struct reference, so nothing is known about it.
                let Some(target) = facts.get(&edge).cloned() else {
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

/// The constant fields of a struct-literal operand, or of a reference to one,
/// or `None` when the operand is neither. A cast names the storage its operand
/// does, so either side of the reference may carry one. A reference to a
/// literal is the only name its object has, so a reference local bound to one
/// roots it as a struct local roots its own. `expected` guards against reading
/// field indices off a literal of a different struct than the local's declared
/// type — a local slot reused for two shapes would otherwise mix their field
/// numbering.
fn struct_literal_consts(
    body: &Body,
    types: &TypeTable,
    immutable: &IndexSet<GlobalKey>,
    value: Operand,
    expected: Option<TypeKey>,
) -> Option<FieldConsts> {
    let mut expr = body.strip_casts(value.as_expr()?);
    if let ExprKind::Unary {
        op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
        expr: inner,
    } = &body.exprs[expr].kind
    {
        expr = body.strip_casts(inner.as_expr()?);
    }
    let ExprKind::StructLiteral {
        struct_type,
        fields,
        ..
    } = &body.exprs[expr].kind
    else {
        return None;
    };
    if struct_base(*struct_type, types) != expected {
        return None;
    }
    Some(
        fields
            .iter()
            .filter_map(|field| {
                FieldConst::of_operand(body, types, immutable, field.value)
                    .map(|c| (field.field_index, c))
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
    let mut roots: IndexMap<u32, FieldConsts> = IndexMap::default();
    for param in &func.params {
        let Some(entry) = seed.and_then(|seed| seed.get(&param.name)) else {
            continue;
        };
        if held_struct(param.type_id, types) == Some(entry.pointee) {
            roots.insert(param.local_index, entry.consts.clone());
        }
    }

    // A parameter arrives defined, by the argument. A later assignment is a
    // second definition to narrow against, never the local's only one, and a
    // reference assigned to it does not make it a second name for the referent.
    let mut defined: IndexSet<u32> = func.params.iter().map(|p| p.local_index).collect();
    let mut invalid: IndexSet<u32> = IndexSet::default();
    let mut note_def = |local: u32, value: Operand, roots: &mut IndexMap<u32, FieldConsts>| {
        let expected = locals
            .get(local as usize)
            .and_then(|l| struct_base(l.type_id, types));
        let Some(consts) =
            struct_literal_consts(body, types, &signatures.immutable_globals, value, expected)
        else {
            invalid.insert(local);
            return;
        };
        if defined.insert(local) {
            roots.insert(local, consts);
        } else if let Some(existing) = roots.get_mut(&local) {
            existing.retain(|field, c| consts.get(field) == Some(c));
        }
    };

    let is_struct_local = |local: u32| {
        locals
            .get(local as usize)
            .is_some_and(|l| struct_base(l.type_id, types).is_some())
    };
    // A reference is a second name for one object, so the two locals share a
    // root rather than the reference ending the chain. Inlining a function that passes
    // its `&mut` parameter on mints exactly this binding.
    let mut alias_defs: Vec<AliasDef> = Vec::new();
    let mut define = |local: u32, value: Operand, roots: &mut IndexMap<u32, FieldConsts>| {
        if !is_struct_local(local) {
            return;
        }
        match body.referenced_local(value) {
            Some((base, read)) if base != local => alias_defs.push(AliasDef { local, base, read }),
            _ => note_def(local, value, roots),
        }
    };
    body.for_each_reachable_node(|node| match node {
        NodeRef::Stmt(id) => {
            if let StmtKind::Let {
                local_index, value, ..
            } = &body.stmts[id].kind
            {
                define(*local_index, *value, &mut roots);
            }
        }
        NodeRef::Expr(id) => {
            if let ExprKind::Assign { target, value } = &body.exprs[id].kind
                && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
            {
                define(*index, *value, &mut roots);
            }
        }
        NodeRef::Block(_) | NodeRef::Pat(_) => {}
    });

    for local in &invalid {
        roots.swap_remove(local);
    }
    let (mut alias_of, alias_reads) = settle_aliases(&alias_defs, &defined, &invalid);
    // A binding settled as no alias still binds the local to another object, so
    // a literal it is bound to elsewhere is not its only one.
    for def in &alias_defs {
        if !alias_of.contains_key(&def.local) {
            roots.swap_remove(&def.local);
        }
    }
    flatten_aliases(&mut alias_of);
    for (alias, base) in &alias_of {
        if let Some(consts) = roots.get(base).cloned() {
            roots.insert(*alias, consts);
        }
    }
    roots.retain(|_, consts| !consts.is_empty());
    if roots.is_empty() {
        return roots;
    }

    let tracked: IndexSet<u32> = roots.keys().copied().collect();
    let mut uses: IndexMap<u32, LocalUses> = IndexMap::default();
    collect_local_uses(body, types, &tracked, &alias_reads, signatures, &mut uses);

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
            if body
                .referenced_local(*rebind)
                .is_some_and(|(_, read)| alias_reads.contains(&read))
            {
                continue;
            }
            let expected = locals
                .get(*local as usize)
                .and_then(|l| struct_base(l.type_id, types));
            let Some(assigned) = struct_literal_consts(
                body,
                types,
                &signatures.immutable_globals,
                *rebind,
                expected,
            ) else {
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
    merge_aliases(&mut roots, &alias_of);
    roots.retain(|_, consts| !consts.is_empty());
    roots
}

/// The locals bound once as `let r = &p;` to one of `params`, which inlining a
/// callee taking `&self` mints, mapped to the parameter, and the reads of `p`
/// those bindings make. A use of `r` is a use of `p`.
fn param_aliases(body: &Body, params: &IndexSet<u32>) -> (IndexMap<u32, u32>, IndexSet<ExprId>) {
    let mut defs: Vec<AliasDef> = Vec::new();
    let mut defined: IndexSet<u32> = params.clone();
    body.for_each_reachable_node(|node| match node {
        NodeRef::Stmt(id) => {
            if let StmtKind::Let {
                local_index, value, ..
            } = &body.stmts[id].kind
            {
                match body.referenced_local(*value) {
                    Some((base, read)) if params.contains(&base) && base != *local_index => {
                        defs.push(AliasDef {
                            local: *local_index,
                            base,
                            read,
                        });
                    }
                    _ => {
                        defined.insert(*local_index);
                    }
                }
            }
        }
        NodeRef::Expr(id) => {
            if let ExprKind::Assign { target, .. } = &body.exprs[id].kind
                && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
            {
                defined.insert(*index);
            }
        }
        NodeRef::Block(_) | NodeRef::Pat(_) => {}
    });
    settle_aliases(&defs, &defined, &IndexSet::default())
}

/// One `let r = &base;` or `r = &base;`: the referencing local, the local it
/// names, and the node reading that local.
struct AliasDef {
    local: u32,
    base: u32,
    read: ExprId,
}

/// The alias edges that survive, and the reads they justify. A local is a second
/// name for one object only while it names exactly one: bound to two different
/// bases, or defined some other way as well, it is neither name's root. Both
/// come out of the same pass so a dropped edge cannot leave its read behind —
/// a read classified with no edge to narrow through is a write the base never
/// hears about.
fn settle_aliases(
    defs: &[AliasDef],
    defined: &IndexSet<u32>,
    invalid: &IndexSet<u32>,
) -> (IndexMap<u32, u32>, IndexSet<ExprId>) {
    let mut alias_of: IndexMap<u32, u32> = IndexMap::default();
    let mut rebound: IndexSet<u32> = IndexSet::default();
    for def in defs {
        let defined_otherwise = defined.contains(&def.local) || invalid.contains(&def.local);
        let names_another_base = alias_of.get(&def.local).is_some_and(|b| *b != def.base);
        if defined_otherwise || names_another_base {
            rebound.insert(def.local);
        } else {
            alias_of.insert(def.local, def.base);
        }
    }
    alias_of.retain(|alias, _| !rebound.contains(alias));
    let reads = defs
        .iter()
        .filter(|def| alias_of.contains_key(&def.local))
        .map(|def| def.read)
        .collect();
    (alias_of, reads)
}

/// Point every alias at the root of its chain, so one pass reaches a reference
/// to a reference.
fn flatten_aliases(alias_of: &mut IndexMap<u32, u32>) {
    let resolved: Vec<(u32, u32)> = alias_of
        .iter()
        .map(|(alias, base)| {
            let mut base = *base;
            let mut steps = 0;
            while let Some(next) = alias_of.get(&base) {
                base = *next;
                steps += 1;
                assert!(
                    steps <= alias_of.len(),
                    "an alias is bound once, to an earlier local, so the chain cannot cycle"
                );
            }
            (*alias, base)
        })
        .collect();
    for (alias, base) in resolved {
        alias_of.insert(alias, base);
    }
}

/// Give a reference and its referent one answer: what either use narrows away is
/// narrowed away from both, since the two names reach one object.
fn merge_aliases(roots: &mut IndexMap<u32, FieldConsts>, alias_of: &IndexMap<u32, u32>) {
    for (alias, base) in alias_of {
        let narrowed = roots.get(alias).cloned().unwrap_or_default();
        if let Some(consts) = roots.get_mut(base) {
            consts.retain(|field, c| narrowed.get(field) == Some(c));
        }
    }
    for (alias, base) in alias_of {
        let merged = roots.get(base).cloned().unwrap_or_default();
        if let Some(consts) = roots.get_mut(alias) {
            *consts = merged;
        }
    }
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
    bindings: Vec<Binding>,
    /// In a cold run. Such a call is pointed at a clone
    /// other calls pay for, and never has one minted for it: a clone for a call
    /// that rarely runs buys no time for its size.
    cold: bool,
}

fn select_sites(
    locals: &[NirLocal],
    body: &Body,
    types: &TypeTable,
    signatures: &Signatures,
    facts: &IndexMap<(FuncId, u32), ParamFacts>,
    roots: &IndexMap<u32, FieldConsts>,
    constants: &CallConsts,
    state: &ParamSpecState,
) -> Vec<Site> {
    let mut sites = Vec::new();
    body.for_each_reachable_node(|node| {
        let NodeRef::Expr(id) = node else {
            return;
        };
        let (callee, arguments): (FuncId, Vec<(usize, Operand, bool)>) = match &body.exprs[id].kind
        {
            ExprKind::Call { func_id, args, .. } => (
                *func_id,
                args.iter()
                    .enumerate()
                    .map(|(i, a)| (i, a.expr, a.is_mut))
                    .collect(),
            ),
            _ => return,
        };
        let signature = &signatures.of[callee.index()];
        if !signature.has_body {
            return;
        }
        let unanimous = constants.get(&callee);
        let mut bindings: Vec<Binding> = Vec::new();
        for (position, arg, is_mut) in arguments {
            let Some(&param_local) = signature.param_locals.get(position) else {
                continue;
            };
            match signature.param_struct.get(position).copied().flatten() {
                Some(pointee) => select_field_bindings(
                    SelectField {
                        locals,
                        body,
                        types,
                        immutable: &signatures.immutable_globals,
                        facts,
                        roots,
                    },
                    (callee, param_local, pointee),
                    arg,
                    &mut bindings,
                ),
                // A scalar every caller agrees on is propagated in place, so
                // only a position they disagree on is worth a clone.
                None => {
                    if !is_mut
                        && signature.scalar_specializable
                        && signature.param_fixed[position]
                        && unanimous.is_some_and(|u| u.get(position).is_some_and(Option::is_none))
                        && let Some(value) = FieldConst::of_scalar(body, types, arg)
                    {
                        bindings.push((param_local, Slot::Whole, value));
                    }
                }
            }
        }
        // A clone is specialized further on the slots it left open, so a
        // constant that settles a round after another still reaches it. A
        // bound slot stays bound: its parameter is still passed the constant.
        if let Some(spec) = state.specs.get(&callee) {
            let locals = &signature.param_locals;
            let names = &signature.param_names;
            bindings.retain(|(param, slot, _)| {
                let position = locals.iter().position(|p| p == param);
                let name = &names[position.expect("a binding names a parameter")];
                !spec.bindings.iter().any(|(n, s, _)| n == name && s == slot)
            });
        }
        if bindings.is_empty() {
            return;
        }
        bindings.sort_unstable_by_key(|(param, slot, _)| (*param, *slot));
        assert!(
            bindings
                .windows(2)
                .all(|w| (w[0].0, w[0].1) != (w[1].0, w[1].1)),
            "a call binds each slot once"
        );
        sites.push(Site {
            call: id,
            callee,
            bindings,
            cold: false,
        });
    });
    sites
}

/// The caller-side facts `select_field_bindings` reads, gathered so the call
/// keeps one argument per idea.
struct SelectField<'a> {
    locals: &'a [NirLocal],
    body: &'a Body,
    types: &'a TypeTable,
    immutable: &'a IndexSet<GlobalKey>,
    facts: &'a IndexMap<(FuncId, u32), ParamFacts>,
    roots: &'a IndexMap<u32, FieldConsts>,
}

/// Bind the constant fields of a struct the caller passes by reference, for
/// each field the callee reads and never writes.
fn select_field_bindings(
    caller: SelectField,
    (callee, param_local, pointee): (FuncId, u32, TypeKey),
    arg: Operand,
    bindings: &mut Vec<Binding>,
) {
    let literal;
    let consts = match argument_local(caller.body, arg) {
        Some((local, _)) => {
            let local_struct = caller
                .locals
                .get(local as usize)
                .and_then(|l| struct_base(l.type_id, caller.types));
            match caller.roots.get(&local) {
                Some(consts) if local_struct == Some(pointee) => consts,
                Some(_) | None => return,
            }
        }
        // A literal written at the call is the only name its object has.
        None => match struct_literal_consts(
            caller.body,
            caller.types,
            caller.immutable,
            arg,
            Some(pointee),
        ) {
            Some(consts) => {
                literal = consts;
                &literal
            }
            None => return,
        },
    };
    let Some(param) = caller.facts.get(&(callee, param_local)) else {
        return;
    };
    if param.opaque {
        return;
    }
    for (field, value) in consts {
        if param.writes.contains(field) || !param.reads.contains(field) {
            continue;
        }
        bindings.push((param_local, Slot::Field(*field), value.clone()));
    }
}

// ---------------------------------------------------------------------------
// Pass entry
// ---------------------------------------------------------------------------

/// Specialize callees on the constants their callers pass — a scalar argument,
/// or a field of a struct passed by reference. Returns whether anything changed.
///
/// Runs its rounds to a fixpoint: each round's constants reach one call deeper
/// (a clone seeds its own callees, a propagated scalar becomes its callees'
/// argument), and a chain deeper than the optimizer's iteration count must
/// still converge.
pub(super) fn specialize_const_params(
    project: &mut NirPackage,
    state: &mut ParamSpecState,
    gate: &mut FunctionGate,
    descriptors: &mut DescriptorCache,
    reachability: &mut ReachabilityCache,
) -> bool {
    let cached: Vec<_> = state
        .clones
        .values()
        .chain(project.sroa_param_clones.iter())
        .copied()
        .collect();
    let mut reachable =
        reachable_function_positions(project, descriptors, reachability, gate.exec(), cached);
    let mut changed = false;
    while specialize_round(project, state, gate, &mut reachable) {
        changed = true;
    }
    gate.catch_up(GatedPass::ParamSpec, project.functions.len());
    changed
}

/// One step deeper. `reachable` holds the callers whose arguments the call
/// constants are taken over, and gains the clones the round mints.
fn specialize_round(
    project: &mut NirPackage,
    state: &mut ParamSpecState,
    gate: &mut FunctionGate,
    reachable: &mut IndexSet<usize>,
) -> bool {
    let constants = collect_call_constants(project, reachable, gate.exec());
    let propagated = propagate_scalar_constants(project, state, &constants, gate);
    let signatures = {
        let types = project.type_table.borrow();
        let recursive =
            recursive_functions(state.callees.refresh(project, gate.exec(), body_callees));
        Signatures::build(project, &types, &recursive, gate.exec())
    };
    let facts = summarize_params(project, &signatures, gate.exec());
    let cold = project.builtin_func_id(COLD_PATH_INTRINSIC);
    let per_caller = collect_sites(
        project,
        state,
        &signatures,
        &facts,
        &constants,
        cold,
        gate.exec(),
    );
    if per_caller.is_empty() {
        return propagated;
    }
    let (retarget, planned) = plan_clones(project, state, &per_caller);
    if retarget.is_empty() {
        return propagated;
    }
    // A callee cloned this round may be a caller retargeted this round, and its
    // clone must inherit the retarget. A clone that kept calling the original
    // would become its only caller, whose constants then fold the original
    // into a copy of the clone.
    retarget_calls(project, &retarget);
    let minted = build_clones(project, state, &planned);
    // A clone is reachable through the call just pointed at it, and reaches
    // nothing its original did not.
    let first_minted = project.functions.len();
    project.functions.extend(minted);
    reachable.extend(first_minted..project.functions.len());
    for (caller, _, _) in retarget {
        gate.mark_changed(FuncId::new(caller));
    }
    true
}

/// The constant every call site passes at each argument position, per callee.
/// `None` at a position the callers disagree on, or pass something non-constant
/// at — which is what tells `select_sites` a clone is the only way to reach it.
type CallConsts = IndexMap<FuncId, Vec<Option<FieldConst>>>;

fn collect_call_constants(
    project: &NirPackage,
    reachable: &IndexSet<usize>,
    exec: &Executor,
) -> CallConsts {
    let functions = &project.functions;
    let types = project.type_table.borrow();
    let types: &TypeTable = &types;
    let positions: Vec<usize> = reachable.iter().copied().collect();
    let per_function = exec.map(&positions, |&pos| {
        let func = functions[pos].borrow();
        let mut constants = CallConsts::default();
        if !func.is_dead
            && let Some(body) = &func.body
        {
            collect_body_call_constants(body, types, &mut constants);
        }
        constants
    });
    let mut constants = CallConsts::default();
    for (callee, here) in per_function.into_iter().flatten() {
        join_call(&mut constants, callee, here);
    }
    for global in &project.globals {
        collect_body_call_constants(global.init.slot_expr().body(), types, &mut constants);
    }
    constants
}

/// Join one call's arguments into what the earlier calls of `callee` agree on.
fn join_call(into: &mut CallConsts, callee: FuncId, here: Vec<Option<FieldConst>>) {
    let Some(previous) = into.get_mut(&callee) else {
        into.insert(callee, here);
        return;
    };
    assert_eq!(previous.len(), here.len());
    for (old, new) in previous.iter_mut().zip(here) {
        if *old != new {
            *old = None;
        }
    }
}

fn collect_body_call_constants(body: &Body, types: &TypeTable, constants: &mut CallConsts) {
    body.for_each_reachable_node(|node| {
        let NodeRef::Expr(e) = node else { return };
        let ExprKind::Call { func_id, args, .. } = &body.exprs[e].kind else {
            return;
        };
        let here: Vec<_> = args
            .iter()
            .map(|arg| {
                (!arg.is_mut)
                    .then(|| FieldConst::of_scalar(body, types, arg.expr))
                    .flatten()
            })
            .collect();
        join_call(constants, *func_id, here);
    });
}

/// A scalar agreed on by every caller needs no clone. DAE removes the parameter
/// after its reads have folded. Reference and mutable parameters keep their slots.
fn propagate_scalar_constants(
    project: &mut NirPackage,
    state: &ParamSpecState,
    constants: &CallConsts,
    gate: &mut FunctionGate,
) -> bool {
    let mut changed = false;
    let mut buffers = EngineBuffers::default();
    let types = project.type_table.borrow();
    for (id, values) in constants {
        let (id, mut func) = (*id, project.functions[id.index()].borrow_mut());
        // Cached clones and compiler items can gain callers in a later round;
        // their current call sites are not their whole contract.
        if !is_dae_sroa_eligible(&func, false)
            || func.compiler_item.is_some()
            || state.is_clone(id)
            || project.sroa_param_clones.contains(&id)
        {
            continue;
        }
        let bindings: IndexMap<_, _> = func
            .params
            .iter()
            .zip(values)
            .filter_map(|(p, value)| {
                is_param_fixed(&func, p)
                    .then(|| value.clone())
                    .flatten()
                    .map(|v| (p.local_index, v))
            })
            .collect();
        if bindings.is_empty() {
            continue;
        }
        let FuncParts { body, locals, .. } = func.parts();
        let body = body.expect("eligible function has a body");
        if substitute_locals(body, locals, &mut buffers, &types, &bindings) {
            gate.mark_changed(id);
            changed = true;
        }
    }
    changed
}

/// The specializable call sites of every live function, as
/// `(caller store position, sites)`. Holds only shared borrows of `project`.
fn collect_sites(
    project: &NirPackage,
    state: &ParamSpecState,
    signatures: &Signatures,
    facts: &IndexMap<(FuncId, u32), ParamFacts>,
    constants: &CallConsts,
    cold: Option<FuncId>,
    exec: &Executor,
) -> Vec<(usize, Vec<Site>)> {
    let types = project.type_table.borrow();
    let types: &TypeTable = &types;
    let functions = &project.functions;
    exec.filter_map_indices(functions.len(), |index| {
        let func = functions[index].borrow();
        let body = func.body.as_ref().filter(|_| !func.is_dead)?;
        let seed = state.param_consts.get(&FuncId::new(index));
        let roots = collect_roots(&func, body, types, signatures, facts, seed);
        let cold_calls = ColdRuns::of(body, cold).exprs(body);
        let mut sites = select_sites(
            &func.locals,
            body,
            types,
            signatures,
            facts,
            &roots,
            constants,
            state,
        );
        // A call returning `!` runs at most once per call of its caller, so a
        // clone for it buys nothing, wherever it sits.
        for site in &mut sites {
            site.cold =
                cold_calls.contains(&site.call) || types.is_never(body.exprs[site.call].type_id);
        }
        (!sites.is_empty()).then_some((index, sites))
    })
}

/// One call to point at a clone: `(caller store position, call node, clone)`.
type Retarget = (usize, ExprId, FuncId);

/// A clone to build: the site it is minted for, its id, and its ordinal among
/// its callee's clones.
type Planned<'s> = (&'s Site, FuncId, usize);

/// Assign a clone per distinct binding set, reusing one already cached. Returns
/// the retargets to apply and the clones to build. Hot sites go first, so a
/// cold one finds the clone a hot one planned this round.
fn plan_clones<'s>(
    project: &NirPackage,
    state: &mut ParamSpecState,
    per_caller: &'s [(usize, Vec<Site>)],
) -> (Vec<Retarget>, Vec<Planned<'s>>) {
    let mut retarget: Vec<Retarget> = Vec::new();
    let mut planned: Vec<Planned<'s>> = Vec::new();
    let mut next_id = project.next_func_id().index();
    // A clone minted here is not in the store yet; it presents its source's
    // signature, which a key reached through a callee of another arity lacks.
    let mut minted_from: IndexMap<FuncId, FuncId> = IndexMap::default();
    let hot_then_cold = [false, true].into_iter().flat_map(|cold| {
        per_caller.iter().flat_map(move |(caller, sites)| {
            sites
                .iter()
                .filter(move |s| s.cold == cold)
                .map(move |s| (caller, s))
        })
    });
    for (caller, site) in hot_then_cold {
        let key = spec_key(project, state, site);
        let cached = state.clones.get(&key).copied();
        if let Some(existing) = cached
            && signatures_match(
                project,
                site.callee,
                minted_from.get(&existing).copied().unwrap_or(existing),
            )
        {
            retarget.push((*caller, site.call, existing));
            continue;
        }
        let ordinal = state.per_callee.get(&site.callee).copied().unwrap_or(0);
        if site.cold
            || state.budget_exhausted()
            || body_exprs(project, site.callee) > MAX_CLONE_EXPRS
        {
            continue;
        }
        compiler_trace!(
            "param_spec",
            "specialized {} on {} binding(s)",
            project.functions[site.callee.index()].borrow().name,
            site.bindings.len()
        );
        let id = FuncId::new(next_id);
        next_id += 1;
        // A clone reached by another path whose signature has since moved
        // keeps the key; this one is minted beside it.
        if cached.is_none() {
            state.clones.insert(key.clone(), id);
        }
        state.specs.insert(id, key);
        minted_from.insert(id, site.callee);
        state.per_callee.insert(site.callee, ordinal + 1);
        retarget.push((*caller, site.call, id));
        planned.push((site, id, ordinal));
    }
    (retarget, planned)
}

/// The identity of the clone `site` asks for: its callee's own, extended by
/// the site's bindings.
fn spec_key(project: &NirPackage, state: &ParamSpecState, site: &Site) -> SpecKey {
    let mut key = state.specs.get(&site.callee).cloned().unwrap_or(SpecKey {
        root: site.callee,
        bindings: Vec::new(),
    });
    let callee = project.functions[site.callee.index()].borrow();
    for (param_local, slot, value) in &site.bindings {
        let param = callee.params.iter().find(|p| p.local_index == *param_local);
        let param = param.expect("a binding names a parameter");
        key.bindings
            .push((param.name.clone(), *slot, value.clone()));
    }
    key.bindings
        .sort_unstable_by(|(a, x, _), (b, y, _)| (a, x).cmp(&(b, y)));
    key
}

/// Build the planned clones, in id order, to append to the store.
fn build_clones(
    project: &mut NirPackage,
    state: &mut ParamSpecState,
    planned: &[Planned<'_>],
) -> Vec<FuncRef> {
    if planned.is_empty() {
        return Vec::new();
    }
    let mut settler = BranchSettler::new(project);
    settler.run_clones_as_originals(planned);
    planned
        .iter()
        .map(|&(site, id, ordinal)| {
            let clone = build_clone(project, site, id, ordinal, &settler);
            // A clone of a clone knows what its source did, and what it adds.
            let mut seeds = state
                .param_consts
                .get(&site.callee)
                .cloned()
                .unwrap_or_default();
            for (name, seed) in clone.param_consts {
                match seeds.get_mut(&name) {
                    Some(known) if known.pointee == seed.pointee => {
                        known.consts.extend(seed.consts);
                    }
                    Some(_) | None => {
                        seeds.insert(name, seed);
                    }
                }
            }
            state.param_consts.insert(id, seeds);
            clone.function
        })
        .collect()
}

/// Point each selected call at its clone — a `func_id` swap, no analysis.
fn retarget_calls(project: &mut NirPackage, retarget: &[Retarget]) {
    for (caller, call, target) in retarget {
        let mut func = project.functions[*caller].borrow_mut();
        let Some(body) = &mut func.body else {
            continue;
        };
        match &mut body.exprs[*call].kind {
            ExprKind::Call { func_id, .. } => *func_id = *target,
            _ => panic!("param_spec site is not a call"),
        }
    }
}

/// Folds the constants a clone was minted on and prunes the branches they
/// decide. The next round collects the clone's call sites, and one on a dead
/// branch would spend the clone budget its live siblings need.
struct BranchSettler {
    callees: CalleeMap,
    ctfe_builtins: CtfeBuiltinMap,
    pure_builtin_callees: IndexSet<FuncId>,
}

impl BranchSettler {
    fn new(project: &NirPackage) -> Self {
        BranchSettler {
            callees: build_callee_map(project, &Executor::default(), |_| false),
            ctfe_builtins: build_ctfe_builtin_map(project),
            pure_builtin_callees: project.pure_builtin_callee_ids(),
        }
    }

    /// A call retargeted this round names a clone not yet in the store. The
    /// clone computes what its original does on the constants it was minted
    /// for, which are the only ones it is called with, so a fold runs the
    /// original in its place.
    fn run_clones_as_originals(&mut self, planned: &[Planned<'_>]) {
        for &(site, id, _) in planned {
            if let Some(original) = self.callees.get(&site.callee) {
                let callee = Callee::new(original.func.clone());
                self.callees.insert(id, callee);
            }
            if self.pure_builtin_callees.contains(&site.callee) {
                self.pure_builtin_callees.insert(id);
            }
        }
    }

    fn settle(
        &self,
        body: &mut Body,
        locals: &mut Tracked<Vec<NirLocal>>,
        buffers: &mut EngineBuffers,
        types: &TypeTable,
    ) {
        let fold = ConstFoldRule::new(types, &self.callees, &self.ctfe_builtins);
        let prune = BranchPruneRule;
        let mut engine = Engine::new(body, buffers, locals);
        engine.set_value_graph_type_table(types);
        engine.set_pure_builtin_callees(&self.pure_builtin_callees);
        engine.run(&[&fold, &prune]);
    }
}

/// A freshly minted clone and the facts recorded for it.
struct Clone {
    function: FuncRef,
    param_consts: IndexMap<String, ParamSeed>,
}

/// Clone `site.callee` with its bindings substituted, registered so its call
/// sites are born resolved. The size budget is the caller's to check.
///
/// The clone's identity must differ from the original's along every axis the
/// call graph keys on, `method_name` included — DCE resolves a method call
/// through `method_info`, and would prune the clone as unreachable.
fn build_clone(
    project: &mut NirPackage,
    site: &Site,
    id: FuncId,
    ordinal: usize,
    settler: &BranchSettler,
) -> Clone {
    let mut clone = project.functions[site.callee.index()].borrow().clone();
    let origin = (clone.module_source.clone(), clone.name.clone());
    let name = param_spec_name(&clone.name, ordinal);
    clone.name.clone_from(&name);
    if let Some(info) = &mut clone.method_info {
        info.method_name = param_spec_name(&info.method_name, ordinal);
    }
    clone.id = Some(id);
    clone.visibility = Visibility::Private;
    clone.is_export = false;
    // The marker names the one canonical function behind a compiler item, and
    // that stays the original, which the passes resolving the item synthesize
    // calls to.
    clone.compiler_item = None;

    let mut bound: IndexMap<u32, FieldConsts> = IndexMap::default();
    let mut whole: IndexMap<u32, FieldConst> = IndexMap::default();
    for (param_local, slot, value) in &site.bindings {
        match slot {
            Slot::Whole => {
                whole.insert(*param_local, value.clone());
            }
            Slot::Field(field) => {
                bound
                    .entry(*param_local)
                    .or_default()
                    .insert(*field, value.clone());
            }
        }
    }

    let types = project.type_table.borrow();
    let body = clone.body.as_mut().expect("specialized callee has a body");
    let mut buffers = EngineBuffers::default();
    let params = bound.keys().copied().collect();
    let mut read_through = bound.clone();
    for (alias, base) in param_aliases(body, &params).0 {
        read_through.insert(alias, bound[&base].clone());
    }
    substitute_fields(body, &mut clone.locals, &mut buffers, &types, &read_through);
    substitute_locals(body, &mut clone.locals, &mut buffers, &types, &whole);
    settler.settle(body, &mut clone.locals, &mut buffers, &types);

    let param_consts: IndexMap<String, ParamSeed> = clone
        .params
        .iter()
        .filter_map(|param| {
            let consts = bound.get(&param.local_index)?.clone();
            let pointee = held_struct(param.type_id, &types)?;
            Some((param.name.clone(), ParamSeed { pointee, consts }))
        })
        .collect();
    drop(types);

    let key = FunctionRef::from_resolved(&clone, clone.module_source.clone()).function_id();
    project.func_index.insert(key, id);
    if let Some(reshape) = project.reshapes.get(&site.callee) {
        let method_name = clone.method_info.as_ref().map(|m| m.method_name.clone());
        let respelled = reshape.respelled(name.clone(), method_name);
        project.reshapes.insert(id, respelled);
    }
    copy_function_strings(project, &origin, (clone.module_source.clone(), name));

    Clone {
        function: FuncCell::new(clone),
        param_consts,
    }
}

/// Give the clone its own entry in `function_strings`, which is name-keyed:
/// DCE reads it to decide which string literals survive, so without one the
/// clone's literals are pruned out from under it.
fn copy_function_strings(
    project: &mut NirPackage,
    origin: &(ModuleSource, String),
    clone: (ModuleSource, String),
) {
    if let Some(strings) = project.function_strings.get(origin).cloned() {
        project.function_strings.insert(clone, strings);
    }
}

/// The expression-node count of a function's body; `0` for a bodyless record.
fn body_exprs(project: &NirPackage, func: FuncId) -> usize {
    project.functions[func.index()]
        .borrow()
        .body
        .as_ref()
        .map_or(0, |body| body.exprs.len())
}

/// Whether `clone` still presents the same signature as `original`, so a call
/// written against the original can be retargeted to it unchanged.
fn signatures_match(project: &NirPackage, original: FuncId, clone: FuncId) -> bool {
    let original = project.functions[original.index()].borrow();
    let clone = project.functions[clone.index()].borrow();
    !clone.is_dead
        && clone.params.len() == original.params.len()
        && clone.return_type == original.return_type
        && clone
            .params
            .iter()
            .zip(&original.params)
            .all(|(c, o)| c.type_id == o.type_id && c.local_index == o.local_index)
}

/// Replace every value-position read of a bound field with its constant.
/// Matching nothing is normal: a pass-through callee reads none of them
/// itself, and its clone exists to seed the callee it forwards to.
fn substitute_fields(
    body: &mut Body,
    locals: &mut Tracked<Vec<NirLocal>>,
    buffers: &mut EngineBuffers,
    types: &TypeTable,
    bindings: &IndexMap<u32, FieldConsts>,
) -> bool {
    let mut engine = Engine::new(body, buffers, locals);
    let mut reads: Vec<(ExprId, FieldConst)> = Vec::new();
    engine.body.for_each_reachable_node(|node| {
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
            reads.push((id, value.clone()));
        }
    });
    redirect_const_reads(&mut engine, types, reads)
}

/// Replace every read of a bound local with its constant.
fn substitute_locals(
    body: &mut Body,
    locals: &mut Tracked<Vec<NirLocal>>,
    buffers: &mut EngineBuffers,
    types: &TypeTable,
    bindings: &IndexMap<u32, FieldConst>,
) -> bool {
    let mut engine = Engine::new(body, buffers, locals);
    let mut reads: Vec<(ExprId, FieldConst)> = Vec::new();
    engine.body.for_each_reachable_node(|node| {
        if let NodeRef::Expr(id) = node
            && let ExprKind::Local { index, .. } = &engine.body.exprs[id].kind
            && let Some(value) = bindings.get(index)
        {
            reads.push((id, value.clone()));
        }
    });
    redirect_const_reads(&mut engine, types, reads)
}

/// Point each read at its constant, leaving a place read — a read that names
/// storage rather than a value — alone. Returns whether anything moved.
///
/// A literal is built once, by a `let` opening the body, and its reads name
/// that local: the caller handed over one object, and a copy per read would
/// allocate where the original only loaded, a loop's every iteration included.
fn redirect_const_reads(
    engine: &mut Engine,
    types: &TypeTable,
    reads: Vec<(ExprId, FieldConst)>,
) -> bool {
    let mut built: IndexMap<*const ConstLiteral, u32> = IndexMap::default();
    let mut lets = Vec::new();
    let mut rewritten = false;
    for (read, value) in reads {
        if is_place_read(engine, read) && !reads_fresh_literal(engine, read, &value) {
            continue;
        }
        let node = &engine.body.exprs[read];
        let (type_id, span) = (node.type_id, node.span);
        let operand = match &value {
            FieldConst::Literal(literal) => {
                if !literal.ty().admits(types, type_id) {
                    continue;
                }
                let key = Arc::as_ptr(literal);
                let local = if let Some(&local) = built.get(&key) {
                    local
                } else {
                    let Some(init) = literal.build(engine, types, type_id, span) else {
                        continue;
                    };
                    let local = engine.alloc_minted_local("bound", type_id, false);
                    lets.push(engine.alloc_stmt(
                        StmtKind::Let {
                            name: engine.local_name(local),
                            local_index: local,
                            is_mut: false,
                            type_id,
                            value: Operand::Expr(init),
                            skip_value_copy: true,
                        },
                        span,
                    ));
                    built.insert(key, local);
                    local
                };
                let name = engine.local_name(local);
                Operand::Expr(engine.alloc_expr(
                    ExprKind::Local { index: local, name },
                    type_id,
                    span,
                ))
            }
            FieldConst::Int(..)
            | FieldConst::Float(..)
            | FieldConst::Bool(_)
            | FieldConst::Char(_)
            | FieldConst::GlobalRef { .. } => {
                match value.operand_at(engine, types, type_id, span) {
                    Some(operand) => operand,
                    None => continue,
                }
            }
        };
        engine.redirect_expr(read, operand);
        rewritten = true;
    }
    if !lets.is_empty() {
        let root = engine.body.root();
        lets.extend(engine.body.blocks[root].stmts.iter().copied());
        engine.set_block_stmts(root, lets);
    }
    rewritten
}

/// Whether a place read may name a fresh copy of the literal instead: one that
/// only reads through it, `&s`, `s.used` or `&s.repr`, sees what the copy holds.
fn reads_fresh_literal(engine: &Engine, read: ExprId, value: &FieldConst) -> bool {
    matches!(value, FieldConst::Literal(_)) && only_read_through(engine, read)
}

fn only_read_through(engine: &Engine, place: ExprId) -> bool {
    let Some(NodeRef::Expr(parent)) = engine.parent_of(NodeRef::Expr(place)) else {
        return false;
    };
    match engine.body.exprs[parent].kind {
        ExprKind::Unary {
            op: NirUnaryOp::Ref,
            ..
        } => true,
        ExprKind::FieldAccess { .. } => {
            !is_place_read(engine, parent) || only_read_through(engine, parent)
        }
        _ => false,
    }
}
