//! NIR Value Graph (Layer 2 — pure-value e-graph).
//!
//! Hash-consed DAG of pure values. Each value has a [`ValueId`] (newtype
//! over `u32`); two structurally-equivalent values share one `ValueId`. The
//! `SkelTree` (Layer 1 — see [`crate::nir_arena`]) references pure operands by
//! `ValueId`; pure values live exclusively here.
//!
//! Consumed by [`crate::nir_engine::Engine::value`] (which lazily builds the
//! per-function graph via [`builder::build`]) and through that by the CSE
//! and store-load-forward rules. See
//! `docs/wep-2026-06-05-worklist-rewrite-engine.md`.

pub mod builder;

use crate::hashmap::IndexMap;
use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::tir::TypeId;

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
/// per-field in the MVP.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
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
/// Side-effecting nodes (`Call`, `MethodCall`, `Assign`-to-heap, …) stay
/// in the `SkelTree`. Pure operand positions connect to their `ValueId`s
/// through the per-function side-table `value_of: IndexMap<ExprId,
/// ValueId>` populated by [`crate::nir_value_graph::builder`]. Stage 7
/// of the WEP would replace that table with `Operand::Value(ValueId)` on
/// Skel slots, but is deferred.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ValueKind {
    // ---- Literals ----
    Int(u64),
    /// `f64` bit pattern: distinct NaN payloads and `+0.0` / `-0.0` are
    /// distinct values. Algebraic rules over floats must consult numeric
    /// equality separately — runtime `==` is not this relation.
    Float(u64),
    Bool(bool),
    Char(char),
    String(String),
    Null,
    Unit,

    // ---- Opaque ----
    /// Anonymous unknown. Used for parameters and loop locals; Stage 6
    /// may promote recognised inductions to a tagged form.
    Opaque(OpaqueId),

    // ---- Pure arithmetic ----
    Binary {
        op: NirBinaryOp,
        lhs: ValueId,
        rhs: ValueId,
    },
    Unary {
        op: NirUnaryOp,
        operand: ValueId,
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
#[derive(Debug, Default)]
pub struct ValuePool {
    /// Allocated values, in `ValueId` order. `values[id.index() as usize]`
    /// is the kind for `id`.
    values: Vec<ValueKind>,
    /// Reverse index: kind → `ValueId`. The hash-cons key.
    interned: IndexMap<ValueKind, ValueId>,
    /// Next `OpaqueId` to allocate.
    next_opaque: u32,
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

    /// Hash-cons: return the `ValueId` for `kind`, allocating a fresh one if
    /// `kind` has not been seen before.
    ///
    /// `kind` is cloned only on a fresh allocation (the clone goes into the
    /// `values` vector); a repeat lookup hits the index and returns the
    /// existing id without copying.
    pub fn intern(&mut self, kind: ValueKind) -> ValueId {
        if let Some(&id) = self.interned.get(&kind) {
            return id;
        }
        let id = ValueId(self.values.len() as u32);
        self.values.push(kind.clone());
        self.interned.insert(kind, id);
        id
    }

    /// Allocate a fresh `Opaque` value. Each call returns a `ValueId`
    /// distinct from every prior call to `fresh_opaque` on this pool.
    pub fn fresh_opaque(&mut self) -> ValueId {
        let opaque = OpaqueId(self.next_opaque);
        self.next_opaque += 1;
        self.intern(ValueKind::Opaque(opaque))
    }

    /// Read the kind of an allocated `ValueId`. Panics if `id` is out of
    /// range (i.e., came from a different pool).
    #[inline]
    pub fn kind(&self, id: ValueId) -> &ValueKind {
        &self.values[id.0 as usize]
    }

    #[inline]
    pub fn int(&mut self, value: u64) -> ValueId {
        self.intern(ValueKind::Int(value))
    }

    /// Build a Float value from a raw `f64` bit pattern. Use this when the
    /// caller already has bit-pattern semantics in hand (e.g. carrying a
    /// NaN payload through a rewrite). For an `f64` value, prefer
    /// [`ValuePool::float`].
    #[inline]
    pub fn float_bits(&mut self, bits: u64) -> ValueId {
        self.intern(ValueKind::Float(bits))
    }

    /// Build a Float value from an `f64`. The pool keys on the bit pattern,
    /// so `+0.0` and `-0.0` are distinct values, as are distinct NaN
    /// payloads.
    #[inline]
    pub fn float(&mut self, value: f64) -> ValueId {
        self.float_bits(value.to_bits())
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
    pub fn string(&mut self, value: String) -> ValueId {
        self.intern(ValueKind::String(value))
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
    pub fn binary(&mut self, op: NirBinaryOp, lhs: ValueId, rhs: ValueId) -> ValueId {
        self.intern(ValueKind::Binary { op, lhs, rhs })
    }

    #[inline]
    pub fn unary(&mut self, op: NirUnaryOp, operand: ValueId) -> ValueId {
        self.intern(ValueKind::Unary { op, operand })
    }

    #[inline]
    pub fn cast(&mut self, operand: ValueId, target: TypeId) -> ValueId {
        self.intern(ValueKind::Cast { operand, target })
    }

    #[inline]
    pub fn select(&mut self, cond: ValueId, then: ValueId, else_: ValueId) -> ValueId {
        self.intern(ValueKind::Select { cond, then, else_ })
    }

    #[inline]
    pub fn loop_phi(&mut self, entry: ValueId, body_iter: ValueId) -> ValueId {
        self.intern(ValueKind::LoopPhi { entry, body_iter })
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
        let a = pool.int(42);
        let b = pool.int(42);
        assert_eq!(a, b);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn intern_different_ints_returns_different_ids() {
        let mut pool = ValuePool::new();
        let a = pool.int(1);
        let b = pool.int(2);
        assert_ne!(a, b);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn different_kinds_with_same_payload_are_distinct() {
        let mut pool = ValuePool::new();
        let i0 = pool.int(0);
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
        let id = pool.int(7);
        assert_eq!(pool.kind(id), &ValueKind::Int(7));
    }

    // ---- Float bit-pattern semantics ----

    #[test]
    fn float_pos_zero_and_neg_zero_are_distinct() {
        let mut pool = ValuePool::new();
        let pz = pool.float(0.0);
        let nz = pool.float(-0.0);
        assert_ne!(pz, nz);
    }

    #[test]
    fn float_same_bit_pattern_dedupes() {
        let mut pool = ValuePool::new();
        let a = pool.float_bits(0x7ff8_0000_0000_0001); // a NaN
        let b = pool.float_bits(0x7ff8_0000_0000_0001);
        assert_eq!(a, b);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn float_distinct_nan_payloads_are_distinct_values() {
        let mut pool = ValuePool::new();
        let a = pool.float_bits(0x7ff8_0000_0000_0001);
        let b = pool.float_bits(0x7ff8_0000_0000_0002);
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
        let l = pool.int(1);
        let r = pool.int(2);
        let a = pool.binary(NirBinaryOp::Add, l, r);
        let b = pool.binary(NirBinaryOp::Add, l, r);
        assert_eq!(a, b);
    }

    #[test]
    fn binary_operand_order_matters() {
        let mut pool = ValuePool::new();
        let l = pool.int(1);
        let r = pool.int(2);
        let lr = pool.binary(NirBinaryOp::Sub, l, r);
        let rl = pool.binary(NirBinaryOp::Sub, r, l);
        assert_ne!(lr, rl); // Sub is non-commutative; hash-cons just checks structure.
    }

    #[test]
    fn binary_different_op_distinguishes() {
        let mut pool = ValuePool::new();
        let l = pool.int(1);
        let r = pool.int(2);
        let add = pool.binary(NirBinaryOp::Add, l, r);
        let mul = pool.binary(NirBinaryOp::Mul, l, r);
        assert_ne!(add, mul);
    }

    #[test]
    fn unary_dedupes() {
        let mut pool = ValuePool::new();
        let inner = pool.int(5);
        let a = pool.unary(NirUnaryOp::Neg, inner);
        let b = pool.unary(NirUnaryOp::Neg, inner);
        assert_eq!(a, b);
    }

    #[test]
    fn unary_different_op_distinguishes() {
        let mut pool = ValuePool::new();
        let inner = pool.int(5);
        let neg = pool.unary(NirUnaryOp::Neg, inner);
        let not = pool.unary(NirUnaryOp::Not, inner);
        assert_ne!(neg, not);
    }

    #[test]
    fn cast_dedupes_per_target_type() {
        let mut pool = ValuePool::new();
        let inner = pool.int(5);
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
        let t = pool.int(1);
        let e = pool.int(2);
        let s1 = pool.select(cond, t, e);
        let s2 = pool.select(cond, t, e);
        assert_eq!(s1, s2);
    }

    #[test]
    fn select_distinguishes_arm_swap() {
        let mut pool = ValuePool::new();
        let cond = pool.bool(true);
        let t = pool.int(1);
        let e = pool.int(2);
        let normal = pool.select(cond, t, e);
        let swapped = pool.select(cond, e, t);
        assert_ne!(normal, swapped);
    }

    #[test]
    fn loop_phi_dedupes_structurally() {
        let mut pool = ValuePool::new();
        let entry = pool.int(0);
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
        let a = pool.int(1);
        let b = pool.int(2);
        let c = pool.int(3);
        // (a + b) * c, twice.
        let lhs1 = pool.binary(NirBinaryOp::Add, a, b);
        let outer1 = pool.binary(NirBinaryOp::Mul, lhs1, c);
        let lhs2 = pool.binary(NirBinaryOp::Add, a, b);
        let outer2 = pool.binary(NirBinaryOp::Mul, lhs2, c);
        assert_eq!(lhs1, lhs2);
        assert_eq!(outer1, outer2);
    }

    #[test]
    fn full_pipeline_keeps_pool_compact() {
        let mut pool = ValuePool::new();
        let _p = pool.fresh_opaque(); // a parameter
        let _z = pool.int(0);
        let _o = pool.int(1);
        let _t = pool.int(2);
        // Re-intern everything.
        let p2 = pool.fresh_opaque();
        let z2 = pool.int(0);
        let o2 = pool.int(1);
        let t2 = pool.int(2);
        // `fresh_opaque` always allocates; the literals dedupe.
        assert_ne!(_p, p2);
        assert_eq!(_z, z2);
        assert_eq!(_o, o2);
        assert_eq!(_t, t2);
        // 4 originals + 1 new opaque = 5 entries.
        assert_eq!(pool.len(), 5);
    }
}
