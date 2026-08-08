//! NIR Value Graph (Layer 2 — hash-consed pure-value DAG).
//!
//! Hash-consed DAG of pure values. Each value has a [`ValueId`] (newtype
//! over `u32`); two structurally-equivalent values share one `ValueId`. CSE is
//! pure hash-consing — there are no e-class merges, so a `ValueId` is stable
//! once allocated. The
//! `SkelTree` (Layer 1 — see [`crate::nir_arena`]) references pure operands by
//! `ValueId`; pure values live exclusively here.
//!
//! Consumed by [`crate::nir_engine::Engine::value`] (which lazily builds the
//! per-function graph via [`builder::build`]) and through that by the CSE
//! and store-load-forward rules. See
//! `docs/wep-2026-06-05-worklist-rewrite-engine.md`.

pub mod builder;

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::tir::{PrimitiveType, TypeId, TypeTable};

/// Bridge a literal [`ValueKind`] to niri's [`crate::const_eval::Value`] for
/// constant folding, applying the same prim-consistency filter niri's own
/// `Value::from_operand` enforces: an `Int` only with an integer prim
/// (`is_int_prim` — excludes `i128`/`u128`/`v128`), a `Float` only with
/// `F32`/`F64`. `prim` is the operand's resolved primitive type (from its NIR
/// type), needed for the integer width / float precision. Returns `None` for a
/// non-literal kind, a missing prim, or a prim niri would refuse — so the
/// value-graph const-folder and `store_load_forward`'s literal synthesizer fold
/// exactly the set niri's CTFE folds, from one definition.
pub(crate) fn value_kind_to_const(
    kind: &ValueKind,
    prim: Option<PrimitiveType>,
) -> Option<crate::const_eval::Value> {
    use crate::const_eval::Value;
    Some(match kind {
        ValueKind::Int(value, _) => {
            let prim = prim.filter(|p| crate::const_eval::is_int_prim(*p))?;
            Value::Int {
                value: *value,
                prim,
            }
        }
        ValueKind::Float(bits, _) => {
            let prim = prim.filter(|p| matches!(p, PrimitiveType::F32 | PrimitiveType::F64))?;
            Value::Float {
                value: f64::from_bits(*bits),
                prim,
            }
        }
        ValueKind::Bool(b) => Value::Bool(*b),
        ValueKind::Char(c) => Value::Char(*c),
        // Already an evaluated constant; the prim filter above is a scalar
        // concern and does not apply.
        ValueKind::Const(key, _) => key.value().clone(),
        ValueKind::Null
        | ValueKind::Unit
        | ValueKind::Opaque(_)
        | ValueKind::Binary { .. }
        | ValueKind::Unary { .. }
        | ValueKind::Cast { .. }
        | ValueKind::Select { .. }
        | ValueKind::LoopPhi { .. }
        | ValueKind::FieldAccess { .. } => return None,
    })
}

/// Identity view of a compile-time constant, so the pool can hash-cons one.
///
/// [`crate::const_eval::Value`]'s `PartialEq` is the *numeric* relation the
/// evaluator needs — `NaN != NaN`, `+0.0 == -0.0`. Hash-consing needs the
/// *identity* relation instead, the one [`ValueKind::Float`] already spells by
/// keying on the bit pattern. Wrapping the value rather than giving it `Eq`
/// keeps the evaluator's relation honest: a value that compares unequal to
/// itself must never become a hash-cons key.
#[derive(Clone, Debug)]
pub struct ConstKey(std::rc::Rc<crate::const_eval::Value>);

impl ConstKey {
    /// Take identity ownership of an evaluated constant.
    #[must_use]
    pub fn new(value: crate::const_eval::Value) -> Self {
        Self(std::rc::Rc::new(value))
    }

    /// The constant itself.
    #[must_use]
    pub fn value(&self) -> &crate::const_eval::Value {
        &self.0
    }
}

impl PartialEq for ConstKey {
    fn eq(&self, other: &Self) -> bool {
        std::rc::Rc::ptr_eq(&self.0, &other.0) || const_identity_eq(&self.0, &other.0)
    }
}

impl Eq for ConstKey {}

impl std::hash::Hash for ConstKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        const_identity_hash(&self.0, state);
    }
}

/// Structural equality under the identity relation: floats compare by bit
/// pattern, so `NaN` equals itself and `+0.0` differs from `-0.0`.
fn const_identity_eq(a: &crate::const_eval::Value, b: &crate::const_eval::Value) -> bool {
    use crate::const_eval::Value;
    match (a, b) {
        (
            Value::Int {
                value: av,
                prim: ap,
            },
            Value::Int {
                value: bv,
                prim: bp,
            },
        ) => av == bv && ap == bp,
        (
            Value::Float {
                value: av,
                prim: ap,
            },
            Value::Float {
                value: bv,
                prim: bp,
            },
        ) => av.to_bits() == bv.to_bits() && ap == bp,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Char(a), Value::Char(b)) => a == b,
        (
            Value::Aggregate {
                type_id: at,
                fields: af,
            },
            Value::Aggregate {
                type_id: bt,
                fields: bf,
            },
        ) => {
            at == bt
                && af.len() == bf.len()
                && af
                    .iter()
                    .zip(bf.iter())
                    .all(|((ai, av), (bi, bv))| ai == bi && const_identity_eq(av, bv))
        }
        (
            Value::Seq {
                type_id: at,
                elements: ae,
            },
            Value::Seq {
                type_id: bt,
                elements: be,
            },
        ) => {
            at == bt
                && ae.len() == be.len()
                && ae
                    .iter()
                    .zip(be.iter())
                    .all(|(av, bv)| const_identity_eq(av, bv))
        }
        (
            Value::Variant {
                type_id: at,
                case_name: ac,
                payload: ap,
            },
            Value::Variant {
                type_id: bt,
                case_name: bc,
                payload: bp,
            },
        ) => {
            at == bt
                && ac == bc
                && match (ap, bp) {
                    (None, None) => true,
                    (Some(a), Some(b)) => const_identity_eq(a, b),
                    (None, Some(_)) | (Some(_), None) => false,
                }
        }
        (Value::Int { .. }, _)
        | (Value::Float { .. }, _)
        | (Value::Bool(_), _)
        | (Value::Char(_), _)
        | (Value::Aggregate { .. }, _)
        | (Value::Seq { .. }, _)
        | (Value::Variant { .. }, _) => false,
    }
}

/// The hash matching [`const_identity_eq`]. Discriminants are mixed in so two
/// shapes carrying the same leaves do not collide by construction.
fn const_identity_hash<H: std::hash::Hasher>(v: &crate::const_eval::Value, state: &mut H) {
    use crate::const_eval::Value;
    use std::hash::Hash;

    std::mem::discriminant(v).hash(state);
    match v {
        Value::Int { value, prim } => {
            value.hash(state);
            prim.hash(state);
        }
        Value::Float { value, prim } => {
            value.to_bits().hash(state);
            prim.hash(state);
        }
        Value::Bool(b) => b.hash(state),
        Value::Char(c) => c.hash(state),
        Value::Aggregate { type_id, fields } => {
            type_id.hash(state);
            fields.len().hash(state);
            for (index, field) in fields.iter() {
                index.hash(state);
                const_identity_hash(field, state);
            }
        }
        Value::Seq { type_id, elements } => {
            type_id.hash(state);
            elements.len().hash(state);
            for element in elements.iter() {
                const_identity_hash(element, state);
            }
        }
        Value::Variant {
            type_id,
            case_name,
            payload,
        } => {
            type_id.hash(state);
            case_name.hash(state);
            match payload {
                Some(p) => const_identity_hash(p, state),
                None => 0u8.hash(state),
            }
        }
    }
}

/// Opaque handle for a pure value. Two structurally-equivalent values share
/// one `ValueId` (hash-consed). Allocated by [`ValuePool::intern`].
///
/// `ValueId`s are scoped to one [`ValuePool`] — passing an id from one pool
/// into another is a logic bug. Pools are per-function in normal use.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ValueId(u32);

impl ValueId {
    /// Raw `u32` view, mainly for debugging / dumping.
    #[inline]
    pub fn index(self) -> u32 {
        self.0
    }
}

/// Fresh anonymous value identity. Used to seed parameters, opaque loop
/// locals, and other unknowns: every `OpaqueId` is unique within one
/// [`ValuePool`], so two `ValueKind::Opaque(a)` and `ValueKind::Opaque(b)`
/// are equal iff `a == b`.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct OpaqueId(u32);

impl OpaqueId {
    /// Raw `u32` view, mainly for debugging.
    #[inline]
    pub fn index(self) -> u32 {
        self.0
    }
}

/// Heap-version tag carried by [`ValueKind::FieldAccess`]. The Stage-2
/// builder bumps the version on every `SkelTree` node that may write the heap;
/// reads at the same `(receiver, field, heap_ver)` triple share a
/// `ValueId`, automatically forwarding stored values. Granularity is
/// per-`(receiver-root, field)`; `version_of` maxes the per-slot,
/// per-local, per-field, and default generations, so `Ord` is required.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct HeapVersion(u32);

impl HeapVersion {
    /// The initial version, used at function entry / loop entry.
    pub const INITIAL: HeapVersion = HeapVersion(0);

    /// Monotonically advance to the next version.
    #[inline]
    pub fn bump(self) -> Self {
        HeapVersion(self.0 + 1)
    }

    /// Raw `u32` view, mainly for debugging.
    #[inline]
    pub fn index(self) -> u32 {
        self.0
    }
}

/// A pure-value expression. Hash-consed by structural equality.
///
/// Side-effecting nodes (`Call`, `Assign`-to-heap, …) stay
/// in the `SkelTree`. Pure operand positions connect to their `ValueId`s
/// through the per-function side-table `value_of: IndexMap<ExprId,
/// ValueId>` populated by [`crate::nir_value_graph::builder`]. Stage 7
/// of the WEP would replace that table with `Operand::Value(ValueId)` on
/// Skel slots, but is deferred.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ValueKind {
    // ---- Literals ----
    /// An integer constant with its source `TypeId`. The type is part of the
    /// hash-cons key so a width-erased `7: i32` and `7: i64` are distinct values
    /// (extraction reads the width from the type) — the precondition for
    /// promoting integer values into operand slots before WIR build. Construct
    /// via [`ValuePool::int_typed`]; never `Int(_, _)` directly outside the pool.
    Int(u64, TypeId),
    /// `f64` bit pattern with its source `TypeId`. Distinct NaN payloads and
    /// `+0.0` / `-0.0` are distinct values; the type distinguishes `0.0: f32`
    /// from `0.0: f64`. Algebraic rules over floats must consult numeric
    /// equality separately — runtime `==` is not this relation.
    Float(u64, TypeId),
    Bool(bool),
    Char(char),
    Null,
    Unit,
    /// A constant the scalar cases above cannot name: a `String`, a `List`, a
    /// struct, a variant. Boxed because the payload is a tree, while a scalar
    /// stays unboxed — the pool is on the compile-speed path and scalars are
    /// the overwhelming majority.
    ///
    /// Invariant: a constant a scalar case *can* name is never a `Const`, so
    /// one constant has one `ValueId`. [`ValuePool::constant`] is the only
    /// place allowed to decide, and every producer goes through it.
    Const(ConstKey, TypeId),

    // ---- Opaque ----
    /// Anonymous unknown. Used for parameters and loop locals; Stage 6
    /// may promote recognised inductions to a tagged form.
    Opaque(OpaqueId),

    // ---- Pure arithmetic ----
    /// `lhs op rhs` with its result `TypeId`. The result type is part of the
    /// hash-cons key (like `Int` / `Float`) so the value carries its own width:
    /// extraction reads the op width from `ty` rather than a last-write
    /// `set_type`, and two structurally-equal ops at different widths never share
    /// an id. Operand widths come from each operand value's own `ty` recursively.
    Binary {
        op: NirBinaryOp,
        lhs: ValueId,
        rhs: ValueId,
        ty: TypeId,
    },
    Unary {
        op: NirUnaryOp,
        operand: ValueId,
        ty: TypeId,
    },
    Cast {
        operand: ValueId,
        target: TypeId,
    },

    // ---- Structural merge / loop recurrence ----
    /// Result of a structural If / Match / Switch endpoint:
    /// `if cond then then_v else else_v`. The Stage-2 builder constructs
    /// these at merge points where a local's value differs across arms.
    Select {
        cond: ValueId,
        then: ValueId,
        else_: ValueId,
    },
    /// Loop-recurrence placeholder. `entry` is the value at loop entry;
    /// `body_iter` is the value after one iteration of the loop body. MVP
    /// keeps `body_iter` set to an `Opaque` (no induction recognition); the
    /// kind exists so Stage 6 can fill it in without an enum change.
    LoopPhi {
        entry: ValueId,
        body_iter: ValueId,
    },

    // ---- Heap-bearing reads ----
    /// `receiver.field_index` at the given heap version. `field_index`
    /// rather than `field_name` because the receiver `ValueId` already
    /// pins the type: indices that collide across types belong to
    /// distinct receivers.
    FieldAccess {
        receiver: ValueId,
        field_index: u32,
        heap_ver: HeapVersion,
    },
}

/// Hash-consed pure-value pool. One instance per function. Also owns the
/// `OpaqueId` counter, so [`ValuePool::fresh_opaque`] returns a
/// pool-unique identity each call.
#[derive(Debug, Default, Clone)]
pub struct ValuePool {
    /// Allocated values, in `ValueId` order. `values[id.index() as usize]`
    /// is the kind for `id`. CSE is pure hash-consing (no e-class merges), so a
    /// `ValueId` is stable once allocated and needs no representative lookup.
    values: Vec<ValueKind>,
    /// Reverse index: kind → `ValueId`. The hash-cons memo.
    interned: IndexMap<ValueKind, ValueId>,
    /// Next `OpaqueId` to allocate.
    next_opaque: u32,
    /// Per-`ValueId` source type, indexed by raw id. `ValueKind` is type-erased
    /// (an `Int(u64)` carries no width), so extraction — which materialises a
    /// promoted `Operand::Value` back to WIR once the typed `ExprNode` is gone —
    /// reads the value's type here. Populated by the builder / `lower` at
    /// creation; `None` until set.
    types: Vec<Option<TypeId>>,
    /// Skeleton source for an `Opaque`: how extraction re-emits the leaf the
    /// graph cannot reconstruct. A `Local` opaque emits `local.get idx` (self-
    /// contained — no skeleton node to keep alive); an `Expr` opaque lowers a
    /// scheduled skeleton expression (a call result kept in the skeleton).
    /// Empty for opaques minted without a recorded source.
    opaque_sources: IndexMap<OpaqueId, OpaqueSource>,
    /// One stable `Opaque(Local idx)` per local, memoized so repeated requests
    /// (e.g. a loop-stability re-seed of a local's reads after maintenance
    /// dropped them) share a single identity — the precondition for matching the
    /// guard's and check's copies of an induction variable or a hoisted bound.
    canonical_locals: IndexMap<u32, ValueId>,
}

/// How the extractor re-emits an [`OpaqueId`]'s value (see
/// [`ValuePool::opaque_source`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpaqueSource {
    /// The runtime value of a local: extraction emits `local.get idx`.
    Local(u32),
    /// Produced by a skeleton expression (a call result kept in the skeleton):
    /// extraction lowers that expr.
    Expr(crate::nir_arena::ExprId),
}

impl ValuePool {
    /// A fresh, empty pool.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of distinct values allocated so far.
    #[inline]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether no values have been allocated.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Hash-cons: return the `ValueId` for `kind`, allocating a fresh one if no
    /// structurally-equal value exists.
    ///
    /// `kind` is cloned only on a fresh allocation (the clone goes into the
    /// `values` vector); a repeat lookup hits the index and returns the
    /// existing id without copying.
    pub fn intern(&mut self, kind: ValueKind) -> ValueId {
        if let Some(&id) = self.interned.get(&kind) {
            return id;
        }
        let id = ValueId(self.values.len() as u32);
        // A typed literal carries its width in the kind; record it so extraction
        // (which reads `type_of`) sees it without a separate `set_type` call.
        let carried_type = match &kind {
            ValueKind::Int(_, t)
            | ValueKind::Float(_, t)
            | ValueKind::Const(_, t)
            | ValueKind::Binary { ty: t, .. }
            | ValueKind::Unary { ty: t, .. }
            | ValueKind::Cast { target: t, .. } => Some(*t),
            ValueKind::Bool(_)
            | ValueKind::Char(_)
            | ValueKind::Null
            | ValueKind::Unit
            | ValueKind::Opaque(_)
            | ValueKind::Select { .. }
            | ValueKind::LoopPhi { .. }
            | ValueKind::FieldAccess { .. } => None,
        };
        self.values.push(kind.clone());
        self.types.push(carried_type);
        self.interned.insert(kind, id);
        id
    }

    /// Record the source type of a value (its NIR `ExprNode` type before
    /// promotion). Idempotent; a later call overwrites.
    #[inline]
    pub fn set_type(&mut self, id: ValueId, type_id: TypeId) {
        self.types[id.0 as usize] = Some(type_id);
    }

    /// The recorded source type of `id`, if any.
    #[inline]
    pub fn type_of(&self, id: ValueId) -> Option<TypeId> {
        self.types[id.0 as usize]
    }

    /// Every source type recorded for a promoted operand. Used by DCE to keep a
    /// type reachable when its only use is a promoted constant (which lives here,
    /// not in an `ExprNode`).
    pub fn recorded_types(&self) -> impl Iterator<Item = TypeId> + '_ {
        self.types.iter().filter_map(|t| *t)
    }

    /// Allocate a fresh, un-interned value carrying its use-site type. Unlike
    /// [`ValuePool::intern`] this never shares — a same-valued constant of a
    /// different type gets a distinct `ValueId`. Used by operand promotion,
    /// which runs last (values feed only extraction, never CSE) and must keep a
    /// constant's width: `ValueKind` is type-erased, so `7: i32` and `7: i64`
    /// would otherwise hash-cons to one id with one recorded type.
    pub fn alloc_unshared(&mut self, kind: ValueKind, type_id: TypeId) -> ValueId {
        let id = ValueId(self.values.len() as u32);
        self.values.push(kind);
        self.types.push(Some(type_id));
        id
    }

    /// Name an evaluated constant, whatever its shape.
    ///
    /// The single place that decides between an unboxed scalar kind and a
    /// boxed [`ValueKind::Const`], which is what keeps one constant to one
    /// `ValueId`: were a caller free to box an integer, `Int(7)` and
    /// `Const(Int 7)` would be two ids for one value and CSE would miss the
    /// pair. `ty` is the value's NIR type, needed because `ValueKind` is
    /// type-erased and extraction reads the width back off the pool.
    pub fn constant(&mut self, value: &crate::const_eval::Value, ty: TypeId) -> ValueId {
        use crate::const_eval::Value;
        let kind = match value {
            Value::Int { value, .. } => ValueKind::Int(*value, ty),
            Value::Float { value, .. } => ValueKind::Float(value.to_bits(), ty),
            Value::Bool(b) => ValueKind::Bool(*b),
            Value::Char(c) => ValueKind::Char(*c),
            Value::Aggregate { .. } | Value::Seq { .. } | Value::Variant { .. } => {
                ValueKind::Const(ConstKey::new(value.clone()), ty)
            }
        };
        let id = self.intern(kind);
        self.set_type(id, ty);
        id
    }

    /// Allocate a fresh `Opaque` value. Each call returns a `ValueId`
    /// distinct from every prior call to `fresh_opaque` on this pool.
    pub fn fresh_opaque(&mut self) -> ValueId {
        let opaque = OpaqueId(self.next_opaque);
        self.next_opaque += 1;
        self.intern(ValueKind::Opaque(opaque))
    }

    /// Allocate a fresh `Opaque` with a recorded extraction source.
    pub fn fresh_opaque_with_source(&mut self, source: OpaqueSource) -> ValueId {
        let opaque = OpaqueId(self.next_opaque);
        self.next_opaque += 1;
        self.opaque_sources.insert(opaque, source);
        self.intern(ValueKind::Opaque(opaque))
    }

    /// The recorded extraction source of an `Opaque`, if any.
    #[inline]
    pub fn opaque_source(&self, opaque: OpaqueId) -> Option<OpaqueSource> {
        self.opaque_sources.get(&opaque).copied()
    }

    /// A stable `Opaque(Local idx)` for `idx`, the same value on every call.
    /// Models "the value of `idx`" where the local holds one value across the
    /// reads being re-seeded (a loop-stable local); two reads of the same local
    /// — or two field copies of the same source — then share an identity.
    pub fn canonical_local(&mut self, idx: u32, ty: TypeId) -> ValueId {
        if let Some(&v) = self.canonical_locals.get(&idx) {
            return v;
        }
        let v = self.fresh_opaque_with_source(OpaqueSource::Local(idx));
        self.set_type(v, ty);
        self.canonical_locals.insert(idx, v);
        v
    }

    /// Every local index named by an `OpaqueSource::Local` (a promoted value
    /// extracted as `local.get idx`). A pass that decides a local is unused must
    /// treat these as reads — the read lives in the value pool, not the skeleton.
    pub fn opaque_local_sources(&self) -> impl Iterator<Item = u32> + '_ {
        self.opaque_sources.values().filter_map(|s| match s {
            OpaqueSource::Local(idx) => Some(*idx),
            OpaqueSource::Expr(_) => None,
        })
    }

    /// Collect into `out` every local index named by an `Opaque(Local)`
    /// reachable from value `v` — the locals a promoted value reads when
    /// extracted (a leaf `Opaque(Local)` is `local.get idx`). Recurses the pure
    /// value tree; a `FieldAccess` reads its receiver's locals.
    pub fn collect_opaque_locals(&self, v: ValueId, out: &mut IndexSet<u32>) {
        // Worklist with a visited set, not recursion: an induction `LoopPhi` is
        // self-referential (`body_iter` = `binary(phi, step)`), so a recursive walk
        // would not terminate. The visited set bounds the traversal at each value's
        // canonical id.
        let mut stack = vec![v];
        let mut seen: IndexSet<ValueId> = IndexSet::default();
        while let Some(v) = stack.pop() {
            if !seen.insert(v) {
                continue;
            }
            match self.kind(v).clone() {
                ValueKind::Opaque(oid) => {
                    if let Some(OpaqueSource::Local(idx)) = self.opaque_source(oid) {
                        out.insert(idx);
                    }
                }
                ValueKind::Binary { lhs, rhs, .. } => {
                    stack.push(lhs);
                    stack.push(rhs);
                }
                ValueKind::Unary { operand, .. } | ValueKind::Cast { operand, .. } => {
                    stack.push(operand);
                }
                ValueKind::Select { cond, then, else_ } => {
                    stack.push(cond);
                    stack.push(then);
                    stack.push(else_);
                }
                ValueKind::LoopPhi { entry, body_iter } => {
                    stack.push(entry);
                    stack.push(body_iter);
                }
                ValueKind::FieldAccess { receiver, .. } => stack.push(receiver),
                _ => {}
            }
        }
    }

    /// Remap every `OpaqueSource::Local` index through `remap` (old → new).
    /// A pass that renumbers a body's locals must call this so a promoted
    /// `Opaque` value (extracted as `local.get idx`) still names the right slot.
    /// A `None` entry marks a dropped local; a `Local` source pointing at one is
    /// a bug (a live promoted value reads a dead local), so it panics.
    pub fn remap_opaque_locals(&mut self, remap: &[Option<u32>]) {
        for src in self.opaque_sources.values_mut() {
            if let OpaqueSource::Local(idx) = src {
                *idx = remap[*idx as usize].expect("promoted Opaque reads a local DAE dropped");
            }
        }
    }

    /// Whether `id`'s value can be re-emitted by the extractor purely from the
    /// graph + side-effect-free, position-independent leaves: literal constants
    /// and `Local`-sourced opaques (a `local.get`), composed by `Binary` /
    /// `Unary` / `Cast`. Excludes `Opaque(Expr)` (effectful / unscheduled),
    /// `Select` / `LoopPhi` (flow merges — extraction-unsupported), `FieldAccess`,
    /// etc.
    ///
    /// A `Local` opaque is only sound when the local is **single-assignment**: a
    /// `local.get idx` at the frozen node's position must read the same value
    /// the opaque denotes. A reassigned (`is_mut`) local fails this — its value
    /// at an extraction point can differ from the opaque's version (e.g. a
    /// `mut` param read after `x = x*2`, or a loop counter), so `mut_locals`
    /// (the reassignable indices) are rejected. This also excludes loop-variant
    /// locals, since loop counters are `mut`.
    pub fn value_fully_reemittable_locally(
        &mut self,
        id: ValueId,
        mut_locals: &IndexSet<u32>,
    ) -> bool {
        match self.kind(id).clone() {
            ValueKind::Int(_, _)
            | ValueKind::Float(_, _)
            | ValueKind::Bool(_)
            | ValueKind::Char(_) => true,
            ValueKind::Opaque(op) => match self.opaque_source(op) {
                Some(OpaqueSource::Local(idx)) => !mut_locals.contains(&idx),
                _ => false,
            },
            // Arithmetic is width-uniform: a `Binary`/`Unary` and its operands
            // share the result type, so types stamp consistently from the
            // frozen node. `Cast` is excluded — its operand carries the *source*
            // type, unrecoverable from the type-erased value tree.
            ValueKind::Binary { lhs, rhs, .. } => {
                self.value_fully_reemittable_locally(lhs, mut_locals)
                    && self.value_fully_reemittable_locally(rhs, mut_locals)
            }
            ValueKind::Unary { operand, .. } => {
                self.value_fully_reemittable_locally(operand, mut_locals)
            }
            // A `Select` extracts as a value-producing `if` over its pure arms;
            // re-emittable when the condition and both arms are. The
            // duplication guard keeps a multi-use `Select` from recomputing the
            // `if` at each use.
            ValueKind::Select { cond, then, else_ } => {
                self.value_fully_reemittable_locally(cond, mut_locals)
                    && self.value_fully_reemittable_locally(then, mut_locals)
                    && self.value_fully_reemittable_locally(else_, mut_locals)
            }
            // `FieldAccess` is **never** inline-reemittable: re-emitting a load at
            // an arbitrary use is unsound once a pass moves the operand, and a value
            // *containing* a `FieldAccess` (e.g. `Binary(FieldAccess, x)`) must not
            // be inline-promoted either (that would re-emit the load inline). A
            // standalone `FieldAccess` promotes only via the source-point
            // materialiser, which checks the receiver's reemittability itself.
            _ => false,
        }
    }

    /// Whether re-emitting `v` would **duplicate a non-trivial computation**: a
    /// `Binary` / `Unary` sub-value reachable more than once. The extractor
    /// re-emits a value at each use, so freezing such a value recomputes it at
    /// every occurrence — worse than the `local.set` / `local.get` (and
    /// `local.tee`) the skeleton already shares. The freeze skips these (a
    /// conservative stand-in for the WEP's share-vs-duplicate cost model). Leaf
    /// duplication — `Opaque` (a `local.get`) or a constant — is cheap and
    /// allowed.
    pub fn extraction_duplicates_work(&mut self, v: ValueId) -> bool {
        let mut seen: IndexSet<ValueId> = IndexSet::default();
        self.dup_work_walk(v, &mut seen)
    }

    fn dup_work_walk(&mut self, v: ValueId, seen: &mut IndexSet<ValueId>) -> bool {
        match self.kind(v).clone() {
            ValueKind::Binary { lhs, rhs, .. } => {
                if !seen.insert(v) {
                    return true;
                }
                self.dup_work_walk(lhs, seen) || self.dup_work_walk(rhs, seen)
            }
            ValueKind::Unary { operand, .. } => {
                if !seen.insert(v) {
                    return true;
                }
                self.dup_work_walk(operand, seen)
            }
            ValueKind::Select { cond, then, else_ } => {
                if !seen.insert(v) {
                    return true;
                }
                self.dup_work_walk(cond, seen)
                    || self.dup_work_walk(then, seen)
                    || self.dup_work_walk(else_, seen)
            }
            _ => false,
        }
    }

    /// Read the kind of an allocated `ValueId`. Panics if `id` is out of
    /// range (i.e., came from a different pool).
    #[inline]
    pub fn kind(&self, id: ValueId) -> &ValueKind {
        &self.values[id.0 as usize]
    }

    /// Intern an integer constant `value` of type `type_id`. The type is part
    /// of the hash-cons key (see [`ValueKind::Int`]), so two same-valued ints of
    /// different width are distinct values.
    #[inline]
    pub fn int_typed(&mut self, value: u64, type_id: TypeId) -> ValueId {
        self.intern(ValueKind::Int(value, type_id))
    }

    /// Build a Float value from a raw `f64` bit pattern of type `type_id`. Use
    /// this when the caller already has bit-pattern semantics in hand (e.g.
    /// carrying a NaN payload through a rewrite). For an `f64` value, prefer
    /// [`ValuePool::float`].
    #[inline]
    pub fn float_bits(&mut self, bits: u64, type_id: TypeId) -> ValueId {
        self.intern(ValueKind::Float(bits, type_id))
    }

    /// Build a Float value from an `f64` of type `type_id`. The pool keys on the
    /// bit pattern (and type), so `+0.0` and `-0.0` are distinct values, as are
    /// distinct NaN payloads, and `0.0: f32` differs from `0.0: f64`.
    #[inline]
    pub fn float(&mut self, value: f64, type_id: TypeId) -> ValueId {
        self.float_bits(value.to_bits(), type_id)
    }

    #[inline]
    pub fn bool(&mut self, value: bool) -> ValueId {
        self.intern(ValueKind::Bool(value))
    }

    #[inline]
    pub fn char(&mut self, value: char) -> ValueId {
        self.intern(ValueKind::Char(value))
    }

    #[inline]
    pub fn null(&mut self) -> ValueId {
        self.intern(ValueKind::Null)
    }

    #[inline]
    pub fn unit(&mut self) -> ValueId {
        self.intern(ValueKind::Unit)
    }

    #[inline]
    pub fn binary(&mut self, op: NirBinaryOp, lhs: ValueId, rhs: ValueId, ty: TypeId) -> ValueId {
        self.intern(ValueKind::Binary { op, lhs, rhs, ty })
    }

    #[inline]
    pub fn unary(&mut self, op: NirUnaryOp, operand: ValueId, ty: TypeId) -> ValueId {
        self.intern(ValueKind::Unary { op, operand, ty })
    }

    #[inline]
    pub fn cast(&mut self, operand: ValueId, target: TypeId) -> ValueId {
        self.intern(ValueKind::Cast { operand, target })
    }

    /// [`ValuePool::binary`], collapsed to a literal when both operands are
    /// already constants.
    ///
    /// Interning is where every producer meets — the builder, the engine's
    /// maintenance re-derivation, and the scratch-pool reintern inlining runs —
    /// so folding here is what holds the invariant that no node in the pool is
    /// a foldable operation. A nested constant depends on it: the outer
    /// operation's operand is the inner operation's node, so a node interned
    /// raw is one no later reader can fold.
    ///
    /// Folding needs operand widths; without a `TypeTable` nothing folds.
    pub fn binary_folded(
        &mut self,
        op: NirBinaryOp,
        lhs: ValueId,
        rhs: ValueId,
        ty: TypeId,
        type_table: Option<&TypeTable>,
    ) -> ValueId {
        let folded = type_table.and_then(|tt| {
            let l = self.const_of(lhs, tt)?;
            let r = self.const_of(rhs, tt)?;
            crate::const_eval::eval_binary(l, op, r)
        });
        match folded {
            Some(v) => self.intern_const(v, ty),
            None => self.binary(op, lhs, rhs, ty),
        }
    }

    /// [`ValuePool::unary`] with the same constant collapse as
    /// [`ValuePool::binary_folded`].
    pub fn unary_folded(
        &mut self,
        op: NirUnaryOp,
        operand: ValueId,
        ty: TypeId,
        type_table: Option<&TypeTable>,
    ) -> ValueId {
        let folded = type_table.and_then(|tt| {
            let v = self.const_of(operand, tt)?;
            crate::const_eval::eval_unary(op, v)
        });
        match folded {
            Some(v) => self.intern_const(v, ty),
            None => self.unary(op, operand, ty),
        }
    }

    /// [`ValuePool::cast`] with the same constant collapse as
    /// [`ValuePool::binary_folded`].
    pub fn cast_folded(
        &mut self,
        operand: ValueId,
        target: TypeId,
        type_table: Option<&TypeTable>,
    ) -> ValueId {
        let folded = type_table.and_then(|tt| {
            let v = self.const_of(operand, tt)?;
            crate::const_eval::eval_cast(v, crate::const_eval::prim_of(target, tt)?)
        });
        match folded {
            Some(v) => self.intern_const(v, target),
            None => self.cast(operand, target),
        }
    }

    /// The constant a value denotes, reading its width from its own recorded
    /// type. `None` for a non-literal kind or an untyped / non-primitive value.
    pub fn const_of(
        &self,
        id: ValueId,
        type_table: &TypeTable,
    ) -> Option<crate::const_eval::Value> {
        let ty = self.type_of(id)?;
        value_kind_to_const(self.kind(id), crate::const_eval::prim_of(ty, type_table))
    }

    /// Intern a folded constant under `ty`. Arithmetic never yields an
    /// aggregate or a sequence, which have no pure-value form.
    pub fn intern_const(&mut self, value: crate::const_eval::Value, ty: TypeId) -> ValueId {
        match value {
            crate::const_eval::Value::Int { value, .. } => self.int_typed(value, ty),
            crate::const_eval::Value::Float { value, .. } => self.float(value, ty),
            crate::const_eval::Value::Bool(b) => self.bool(b),
            crate::const_eval::Value::Char(c) => self.char(c),
            // The pool models pure scalars; the arithmetic folds feeding this
            // never produce an aggregate, a sequence, or a variant.
            crate::const_eval::Value::Aggregate { .. }
            | crate::const_eval::Value::Seq { .. }
            | crate::const_eval::Value::Variant { .. } => {
                panic!(
                    "an aggregate, sequence, or variant constant cannot be interned as a pure value"
                )
            }
        }
    }

    pub fn select(&mut self, cond: ValueId, then: ValueId, else_: ValueId) -> ValueId {
        let (t, e, c) = (then, else_, cond);
        // A constant condition selects one arm: `Select(true, a, _) → a`,
        // `Select(false, _, b) → b`. Recovers `false || x` / `true && x` → `x`,
        // which lower to a const-condition merge under operand promotion.
        match self.kind(c) {
            ValueKind::Bool(true) => return t,
            ValueKind::Bool(false) => return e,
            _ => {}
        }
        // `Select(c, X, X) → X`: a pure-value merge whose arms are equal is that
        // value, regardless of the (side-effect-free) condition.
        if t == e {
            return t;
        }
        // `Select(c, true, false) → c`: boolean identity (`c` is already a bool
        // value). Recovers `x || false` / `x && true` → `x`, which lower to this
        // merge under operand promotion.
        if matches!(self.kind(t), ValueKind::Bool(true))
            && matches!(self.kind(e), ValueKind::Bool(false))
        {
            return cond;
        }
        self.intern(ValueKind::Select { cond, then, else_ })
    }

    #[inline]
    pub fn loop_phi(&mut self, entry: ValueId, body_iter: ValueId) -> ValueId {
        self.intern(ValueKind::LoopPhi { entry, body_iter })
    }

    /// Allocate a `LoopPhi` placeholder whose `body_iter` is initially `entry`,
    /// to be patched once the loop body's exit value is known
    /// ([`ValuePool::set_loop_phi_body_iter`]). Un-interned: an induction phi's
    /// final `body_iter` references the phi itself, so it must not hash-cons with
    /// another loop's phi by structure, and its kind is mutated after the body
    /// walk — both incompatible with the interned table.
    pub fn alloc_loop_phi(&mut self, entry: ValueId, type_id: TypeId) -> ValueId {
        self.alloc_unshared(
            ValueKind::LoopPhi {
                entry,
                body_iter: entry,
            },
            type_id,
        )
    }

    /// Patch the `body_iter` of a phi made by [`ValuePool::alloc_loop_phi`] to the
    /// loop body's exit value (which may reference `phi` itself — a sound
    /// self-reference, traversals are visited-set/stop-at-phi guarded).
    pub fn set_loop_phi_body_iter(&mut self, phi: ValueId, body_iter: ValueId) {
        let entry = match self.values[phi.0 as usize] {
            ValueKind::LoopPhi { entry, .. } => entry,
            ref k => panic!("set_loop_phi_body_iter on non-phi {k:?}"),
        };
        self.values[phi.0 as usize] = ValueKind::LoopPhi { entry, body_iter };
    }

    #[inline]
    pub fn field_access(
        &mut self,
        receiver: ValueId,
        field_index: u32,
        heap_ver: HeapVersion,
    ) -> ValueId {
        self.intern(ValueKind::FieldAccess {
            receiver,
            field_index,
            heap_ver,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir::{NirBinaryOp, NirUnaryOp};
    use crate::tir::TypeId;

    // ---- Hash-cons dedup ----

    #[test]
    fn fresh_pool_is_empty() {
        let pool = ValuePool::new();
        assert_eq!(pool.len(), 0);
        assert!(pool.is_empty());
    }

    #[test]
    fn intern_same_int_twice_returns_same_id() {
        let mut pool = ValuePool::new();
        let a = pool.int_typed(42, crate::tir::TypeTable::I32);
        let b = pool.int_typed(42, crate::tir::TypeTable::I32);
        assert_eq!(a, b);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn intern_different_ints_returns_different_ids() {
        let mut pool = ValuePool::new();
        let a = pool.int_typed(1, crate::tir::TypeTable::I32);
        let b = pool.int_typed(2, crate::tir::TypeTable::I32);
        assert_ne!(a, b);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn same_int_different_width_are_distinct_values() {
        // The width is part of the hash-cons key, so `7: i32` and `7: i64` are
        // distinct values that each carry their own recorded type — the
        // precondition for promoting integer values into operand slots before
        // WIR build (extraction reads the width from `type_of`).
        let mut pool = ValuePool::new();
        let a = pool.int_typed(7, crate::tir::TypeTable::I32);
        let b = pool.int_typed(7, crate::tir::TypeTable::I64);
        assert_ne!(a, b);
        assert_eq!(pool.type_of(a), Some(crate::tir::TypeTable::I32));
        assert_eq!(pool.type_of(b), Some(crate::tir::TypeTable::I64));
        // A repeat of the same (value, type) still dedups.
        let a2 = pool.int_typed(7, crate::tir::TypeTable::I32);
        assert_eq!(a, a2);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn different_kinds_with_same_payload_are_distinct() {
        let mut pool = ValuePool::new();
        let i0 = pool.int_typed(0, crate::tir::TypeTable::I32);
        let b_false = pool.bool(false);
        // Same numeric "0" but different ValueKind variants — distinct ids.
        assert_ne!(i0, b_false);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn bool_true_and_false_are_distinct() {
        let mut pool = ValuePool::new();
        let t = pool.bool(true);
        let f = pool.bool(false);
        assert_ne!(t, f);
    }

    #[test]
    fn null_and_unit_are_distinct_and_each_dedup() {
        let mut pool = ValuePool::new();
        let n1 = pool.null();
        let n2 = pool.null();
        let u1 = pool.unit();
        let u2 = pool.unit();
        assert_eq!(n1, n2);
        assert_eq!(u1, u2);
        assert_ne!(n1, u1);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn kind_lookup_round_trips() {
        let mut pool = ValuePool::new();
        let id = pool.int_typed(7, crate::tir::TypeTable::I32);
        assert_eq!(
            pool.kind(id),
            &ValueKind::Int(7, crate::tir::TypeTable::I32)
        );
    }

    // ---- Float bit-pattern semantics ----

    #[test]
    fn float_pos_zero_and_neg_zero_are_distinct() {
        let mut pool = ValuePool::new();
        let pz = pool.float(0.0, crate::tir::TypeTable::F64);
        let nz = pool.float(-0.0, crate::tir::TypeTable::F64);
        assert_ne!(pz, nz);
    }

    #[test]
    fn float_same_bit_pattern_dedupes() {
        let mut pool = ValuePool::new();
        let a = pool.float_bits(0x7ff8_0000_0000_0001, crate::tir::TypeTable::F64); // a NaN
        let b = pool.float_bits(0x7ff8_0000_0000_0001, crate::tir::TypeTable::F64);
        assert_eq!(a, b);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn float_distinct_nan_payloads_are_distinct_values() {
        let mut pool = ValuePool::new();
        let a = pool.float_bits(0x7ff8_0000_0000_0001, crate::tir::TypeTable::F64);
        let b = pool.float_bits(0x7ff8_0000_0000_0002, crate::tir::TypeTable::F64);
        assert_ne!(a, b);
    }

    // ---- Opaque ----

    #[test]
    fn opaque_is_always_fresh() {
        let mut pool = ValuePool::new();
        let a = pool.fresh_opaque();
        let b = pool.fresh_opaque();
        let c = pool.fresh_opaque();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
        assert_eq!(pool.len(), 3);
    }

    #[test]
    fn opaque_kinds_carry_increasing_ids() {
        let mut pool = ValuePool::new();
        let a = pool.fresh_opaque();
        let b = pool.fresh_opaque();
        match (pool.kind(a), pool.kind(b)) {
            (ValueKind::Opaque(ia), ValueKind::Opaque(ib)) => {
                assert_eq!(ib.index(), ia.index() + 1);
            }
            other => panic!("unexpected kinds: {other:?}"),
        }
    }

    // ---- Pure arithmetic ----

    #[test]
    fn same_binary_with_same_operands_dedupes() {
        let mut pool = ValuePool::new();
        let l = pool.int_typed(1, crate::tir::TypeTable::I32);
        let r = pool.int_typed(2, crate::tir::TypeTable::I32);
        let a = pool.binary(NirBinaryOp::Add, l, r, crate::tir::TypeTable::I32);
        let b = pool.binary(NirBinaryOp::Add, l, r, crate::tir::TypeTable::I32);
        assert_eq!(a, b);
    }

    #[test]
    fn binary_operand_order_matters() {
        let mut pool = ValuePool::new();
        let l = pool.int_typed(1, crate::tir::TypeTable::I32);
        let r = pool.int_typed(2, crate::tir::TypeTable::I32);
        let lr = pool.binary(NirBinaryOp::Sub, l, r, crate::tir::TypeTable::I32);
        let rl = pool.binary(NirBinaryOp::Sub, r, l, crate::tir::TypeTable::I32);
        assert_ne!(lr, rl); // Sub is non-commutative; hash-cons just checks structure.
    }

    #[test]
    fn binary_different_op_distinguishes() {
        let mut pool = ValuePool::new();
        let l = pool.int_typed(1, crate::tir::TypeTable::I32);
        let r = pool.int_typed(2, crate::tir::TypeTable::I32);
        let add = pool.binary(NirBinaryOp::Add, l, r, crate::tir::TypeTable::I32);
        let mul = pool.binary(NirBinaryOp::Mul, l, r, crate::tir::TypeTable::I32);
        assert_ne!(add, mul);
    }

    #[test]
    fn unary_dedupes() {
        let mut pool = ValuePool::new();
        let inner = pool.int_typed(5, crate::tir::TypeTable::I32);
        let a = pool.unary(NirUnaryOp::Neg, inner, crate::tir::TypeTable::I32);
        let b = pool.unary(NirUnaryOp::Neg, inner, crate::tir::TypeTable::I32);
        assert_eq!(a, b);
    }

    #[test]
    fn unary_different_op_distinguishes() {
        let mut pool = ValuePool::new();
        let inner = pool.int_typed(5, crate::tir::TypeTable::I32);
        let neg = pool.unary(NirUnaryOp::Neg, inner, crate::tir::TypeTable::I32);
        let not = pool.unary(NirUnaryOp::Not, inner, crate::tir::TypeTable::I32);
        assert_ne!(neg, not);
    }

    #[test]
    fn cast_dedupes_per_target_type() {
        let mut pool = ValuePool::new();
        let inner = pool.int_typed(5, crate::tir::TypeTable::I32);
        let t1 = TypeId(1);
        let t2 = TypeId(2);
        let a = pool.cast(inner, t1);
        let b = pool.cast(inner, t1);
        let c = pool.cast(inner, t2);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // ---- Structural merge ----

    #[test]
    fn select_dedupes_structurally() {
        let mut pool = ValuePool::new();
        let cond = pool.bool(true);
        let t = pool.int_typed(1, crate::tir::TypeTable::I32);
        let e = pool.int_typed(2, crate::tir::TypeTable::I32);
        let s1 = pool.select(cond, t, e);
        let s2 = pool.select(cond, t, e);
        assert_eq!(s1, s2);
    }

    #[test]
    fn select_distinguishes_arm_swap() {
        let mut pool = ValuePool::new();
        let cond = pool.bool(true);
        let t = pool.int_typed(1, crate::tir::TypeTable::I32);
        let e = pool.int_typed(2, crate::tir::TypeTable::I32);
        let normal = pool.select(cond, t, e);
        let swapped = pool.select(cond, e, t);
        assert_ne!(normal, swapped);
    }

    #[test]
    fn loop_phi_dedupes_structurally() {
        let mut pool = ValuePool::new();
        let entry = pool.int_typed(0, crate::tir::TypeTable::I32);
        let next = pool.fresh_opaque();
        let a = pool.loop_phi(entry, next);
        let b = pool.loop_phi(entry, next);
        assert_eq!(a, b);
    }

    // ---- Heap-bearing reads ----

    #[test]
    fn field_access_distinguishes_by_heap_version() {
        let mut pool = ValuePool::new();
        let recv = pool.fresh_opaque();
        let v0 = HeapVersion::INITIAL;
        let v1 = v0.bump();
        let read0 = pool.field_access(recv, 0, v0);
        let read1 = pool.field_access(recv, 0, v1);
        assert_ne!(read0, read1);
    }

    #[test]
    fn field_access_dedupes_at_same_heap_version() {
        let mut pool = ValuePool::new();
        let recv = pool.fresh_opaque();
        let v = HeapVersion::INITIAL;
        let a = pool.field_access(recv, 0, v);
        let b = pool.field_access(recv, 0, v);
        assert_eq!(a, b);
    }

    #[test]
    fn field_access_distinguishes_by_receiver() {
        let mut pool = ValuePool::new();
        let r1 = pool.fresh_opaque();
        let r2 = pool.fresh_opaque();
        let v = HeapVersion::INITIAL;
        let read1 = pool.field_access(r1, 0, v);
        let read2 = pool.field_access(r2, 0, v);
        assert_ne!(read1, read2);
    }

    #[test]
    fn field_access_distinguishes_by_field_index() {
        let mut pool = ValuePool::new();
        let recv = pool.fresh_opaque();
        let v = HeapVersion::INITIAL;
        let read0 = pool.field_access(recv, 0, v);
        let read1 = pool.field_access(recv, 1, v);
        assert_ne!(read0, read1);
    }

    // ---- HeapVersion ----

    #[test]
    fn heap_version_bump_advances_monotonically() {
        let v0 = HeapVersion::INITIAL;
        let v1 = v0.bump();
        let v2 = v1.bump();
        assert_ne!(v0, v1);
        assert_ne!(v1, v2);
        assert!(v0.index() < v1.index());
        assert!(v1.index() < v2.index());
    }

    // ---- Nested / mixed-kind sanity ----

    #[test]
    fn nested_arithmetic_dedupes() {
        let mut pool = ValuePool::new();
        let a = pool.int_typed(1, crate::tir::TypeTable::I32);
        let b = pool.int_typed(2, crate::tir::TypeTable::I32);
        let c = pool.int_typed(3, crate::tir::TypeTable::I32);
        // (a + b) * c, twice.
        let lhs1 = pool.binary(NirBinaryOp::Add, a, b, crate::tir::TypeTable::I32);
        let outer1 = pool.binary(NirBinaryOp::Mul, lhs1, c, crate::tir::TypeTable::I32);
        let lhs2 = pool.binary(NirBinaryOp::Add, a, b, crate::tir::TypeTable::I32);
        let outer2 = pool.binary(NirBinaryOp::Mul, lhs2, c, crate::tir::TypeTable::I32);
        assert_eq!(lhs1, lhs2);
        assert_eq!(outer1, outer2);
    }

    #[test]
    fn full_pipeline_keeps_pool_compact() {
        let mut pool = ValuePool::new();
        let _p = pool.fresh_opaque(); // a parameter
        let _z = pool.int_typed(0, crate::tir::TypeTable::I32);
        let _o = pool.int_typed(1, crate::tir::TypeTable::I32);
        let _t = pool.int_typed(2, crate::tir::TypeTable::I32);
        // Re-intern everything.
        let p2 = pool.fresh_opaque();
        let z2 = pool.int_typed(0, crate::tir::TypeTable::I32);
        let o2 = pool.int_typed(1, crate::tir::TypeTable::I32);
        let t2 = pool.int_typed(2, crate::tir::TypeTable::I32);
        // `fresh_opaque` always allocates; the literals dedupe.
        assert_ne!(_p, p2);
        assert_eq!(_z, z2);
        assert_eq!(_o, o2);
        assert_eq!(_t, t2);
        // 4 originals + 1 new opaque = 5 entries.
        assert_eq!(pool.len(), 5);
    }
}
