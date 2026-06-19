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

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::tir::{PrimitiveType, TypeId};

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
        _ => return None,
    })
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
/// Side-effecting nodes (`Call`, `MethodCall`, `Assign`-to-heap, …) stay
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
    String(String),
    Null,
    Unit,

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
    /// is the kind for `id`, with its children canonicalized to their class
    /// representatives. A non-representative id's entry is read only via
    /// [`ValuePool::find`].
    values: Vec<ValueKind>,
    /// Reverse index: canonical kind → `ValueId`. The hash-cons (e-graph memo).
    /// Keyed by a kind whose children are class representatives.
    interned: IndexMap<ValueKind, ValueId>,
    /// Next `OpaqueId` to allocate.
    next_opaque: u32,
    /// Union-find parent pointers, indexed by raw `ValueId`. `parent[i] == i`
    /// for a class representative. [`ValuePool::union`] merges classes;
    /// [`ValuePool::find`] resolves the representative with path halving.
    parent: Vec<u32>,
    /// For each class representative (by raw id), the ids of the nodes that
    /// reference it as a child. [`ValuePool::rebuild`] re-canonicalizes these
    /// after a union so structurally-equal parents re-merge (congruence).
    class_parents: IndexMap<u32, Vec<ValueId>>,
    /// Classes merged since the last [`ValuePool::rebuild`], pending congruence
    /// repair.
    pending: Vec<ValueId>,
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
    /// structurally-equal value exists. Children are canonicalized to their
    /// class representatives first, so a node whose children were unioned dedups
    /// against the congruent existing node.
    ///
    /// `kind` is cloned only on a fresh allocation (the clone goes into the
    /// `values` vector); a repeat lookup hits the index and returns the
    /// existing id without copying.
    pub fn intern(&mut self, kind: ValueKind) -> ValueId {
        let kind = self.canonicalize(kind);
        if let Some(&id) = self.interned.get(&kind) {
            return self.find(id);
        }
        let id = ValueId(self.values.len() as u32);
        self.parent.push(id.0);
        // A typed literal carries its width in the kind; record it so extraction
        // (which reads `type_of`) sees it without a separate `set_type` call.
        let carried_type = match kind {
            ValueKind::Int(_, t)
            | ValueKind::Float(_, t)
            | ValueKind::Binary { ty: t, .. }
            | ValueKind::Unary { ty: t, .. }
            | ValueKind::Cast { target: t, .. } => Some(t),
            _ => None,
        };
        self.values.push(kind.clone());
        self.types.push(carried_type);
        self.register_parent_links(id, &kind);
        self.interned.insert(kind, id);
        id
    }

    /// Record the source type of a value (its NIR `ExprNode` type before
    /// promotion). Idempotent; a later call overwrites. Stored on `id`'s own raw
    /// slot — resolve the representative with [`ValuePool::find`] before reading
    /// if the class may have been unioned.
    #[inline]
    pub fn set_type(&mut self, id: ValueId, type_id: TypeId) {
        self.types[id.0 as usize] = Some(type_id);
    }

    /// The recorded source type of `id`, if any. Prefer passing a representative
    /// (`find(id)`); a non-representative slot may be unset.
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
        self.parent.push(id.0);
        self.types.push(Some(type_id));
        id
    }

    /// The class representative of `id`, with path halving.
    pub fn find(&mut self, id: ValueId) -> ValueId {
        let mut x = id.0;
        while self.parent[x as usize] != x {
            let gp = self.parent[self.parent[x as usize] as usize];
            self.parent[x as usize] = gp;
            x = gp;
        }
        ValueId(x)
    }

    /// Class representative without path compression — the `&self` form of
    /// [`ValuePool::find`], for read-only walks that cannot take `&mut`.
    pub fn find_imm(&self, id: ValueId) -> ValueId {
        let mut x = id.0;
        while self.parent[x as usize] != x {
            x = self.parent[x as usize];
        }
        ValueId(x)
    }

    /// Merge the classes of `a` and `b`, returning the surviving representative
    /// (the smaller raw id, for determinism). The merge is recorded for the
    /// next [`ValuePool::rebuild`], which restores congruence. A no-op (and
    /// nothing pending) when they already share a class.
    pub fn union(&mut self, a: ValueId, b: ValueId) -> ValueId {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return ra;
        }
        // Prefer a constant representative: a class containing a literal should
        // resolve to it, so extraction materializes the constant. Among equal
        // ranks, the smaller raw id wins, for determinism.
        let (ra_rank, rb_rank) = (self.rep_rank(ra), self.rep_rank(rb));
        let ra_wins = ra_rank < rb_rank || (ra_rank == rb_rank && ra.0 <= rb.0);
        let (win, lose) = if ra_wins { (ra, rb) } else { (rb, ra) };
        self.parent[lose.0 as usize] = win.0;
        // Keep a known type on the surviving representative.
        if self.types[win.0 as usize].is_none() {
            self.types[win.0 as usize] = self.types[lose.0 as usize];
        }
        if let Some(losers) = self.class_parents.swap_remove(&lose.0) {
            self.class_parents.entry(win.0).or_default().extend(losers);
        }
        self.pending.push(win);
        win
    }

    /// Restore congruence after a batch of unions: re-canonicalize the parents
    /// of every merged class and re-merge any that became structurally equal,
    /// to a fixed point. Cheap when nothing was unioned (empty `pending`).
    pub fn rebuild(&mut self) {
        while let Some(c) = self.pending.pop() {
            let c = self.find(c);
            self.repair(c);
        }
    }

    /// Re-canonicalize and re-hash-cons the parents of class `c`, unioning any
    /// pair that now denotes the same node. See [`ValuePool::rebuild`].
    fn repair(&mut self, c: ValueId) {
        let parents = self.class_parents.get(&c.0).cloned().unwrap_or_default();
        // Drop every parent's stale memo entry before re-inserting, so a
        // canonical kind that now collides with another parent is detected.
        for &p in &parents {
            let old = self.values[p.0 as usize].clone();
            self.interned.swap_remove(&old);
        }
        for &p in &parents {
            let canon = self.canonicalize(self.values[p.0 as usize].clone());
            self.values[p.0 as usize] = canon.clone();
            match self.interned.get(&canon).copied() {
                Some(q) if self.find(q) != self.find(p) => {
                    self.union(p, q);
                    let r = self.find(p);
                    self.interned.insert(canon, r);
                }
                _ => {
                    let r = self.find(p);
                    self.interned.insert(canon, r);
                }
            }
        }
    }

    /// Representative-preference rank for `id`: a constant kind ranks 0 (most
    /// preferred), everything else 1. [`ValuePool::union`] keeps the
    /// lower-ranked side, so a class containing a literal resolves to it.
    fn rep_rank(&self, id: ValueId) -> u8 {
        match self.values[id.0 as usize] {
            ValueKind::Int(_, _)
            | ValueKind::Float(_, _)
            | ValueKind::Bool(_)
            | ValueKind::Char(_)
            | ValueKind::String(_)
            | ValueKind::Null
            | ValueKind::Unit => 0,
            _ => 1,
        }
    }

    /// Replace each child of `kind` with its class representative.
    fn canonicalize(&mut self, kind: ValueKind) -> ValueKind {
        match kind {
            ValueKind::Binary { op, lhs, rhs, ty } => ValueKind::Binary {
                op,
                lhs: self.find(lhs),
                rhs: self.find(rhs),
                ty,
            },
            ValueKind::Unary { op, operand, ty } => ValueKind::Unary {
                op,
                operand: self.find(operand),
                ty,
            },
            ValueKind::Cast { operand, target } => ValueKind::Cast {
                operand: self.find(operand),
                target,
            },
            ValueKind::Select { cond, then, else_ } => ValueKind::Select {
                cond: self.find(cond),
                then: self.find(then),
                else_: self.find(else_),
            },
            ValueKind::LoopPhi { entry, body_iter } => ValueKind::LoopPhi {
                entry: self.find(entry),
                body_iter: self.find(body_iter),
            },
            ValueKind::FieldAccess {
                receiver,
                field_index,
                heap_ver,
            } => ValueKind::FieldAccess {
                receiver: self.find(receiver),
                field_index,
                heap_ver,
            },
            leaf => leaf,
        }
    }

    /// Register `id` as a parent of each of its (already-canonical) children's
    /// classes, so a later union of a child re-canonicalizes `id`.
    fn register_parent_links(&mut self, id: ValueId, kind: &ValueKind) {
        for child in Self::child_values(kind) {
            self.class_parents.entry(child.0).or_default().push(id);
        }
    }

    /// The child `ValueId`s referenced by `kind` (empty for literals / opaque).
    fn child_values(kind: &ValueKind) -> Vec<ValueId> {
        match *kind {
            ValueKind::Binary { lhs, rhs, .. } => vec![lhs, rhs],
            ValueKind::Unary { operand, .. } => vec![operand],
            ValueKind::Cast { operand, .. } => vec![operand],
            ValueKind::Select { cond, then, else_ } => vec![cond, then, else_],
            ValueKind::LoopPhi { entry, body_iter } => vec![entry, body_iter],
            ValueKind::FieldAccess { receiver, .. } => vec![receiver],
            _ => Vec::new(),
        }
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
        match self.kind(self.find_imm(v)).clone() {
            ValueKind::Opaque(oid) => {
                if let Some(OpaqueSource::Local(idx)) = self.opaque_source(oid) {
                    out.insert(idx);
                }
            }
            ValueKind::Binary { lhs, rhs, .. } => {
                self.collect_opaque_locals(lhs, out);
                self.collect_opaque_locals(rhs, out);
            }
            ValueKind::Unary { operand, .. } | ValueKind::Cast { operand, .. } => {
                self.collect_opaque_locals(operand, out);
            }
            ValueKind::Select { cond, then, else_ } => {
                self.collect_opaque_locals(cond, out);
                self.collect_opaque_locals(then, out);
                self.collect_opaque_locals(else_, out);
            }
            ValueKind::LoopPhi { entry, body_iter } => {
                self.collect_opaque_locals(entry, out);
                self.collect_opaque_locals(body_iter, out);
            }
            ValueKind::FieldAccess { receiver, .. } => self.collect_opaque_locals(receiver, out),
            _ => {}
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
        let rep = self.find(id);
        match self.kind(rep).clone() {
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
        let rep = self.find(v);
        match self.kind(rep).clone() {
            ValueKind::Binary { lhs, rhs, .. } => {
                if !seen.insert(rep) {
                    return true;
                }
                self.dup_work_walk(lhs, seen) || self.dup_work_walk(rhs, seen)
            }
            ValueKind::Unary { operand, .. } => {
                if !seen.insert(rep) {
                    return true;
                }
                self.dup_work_walk(operand, seen)
            }
            ValueKind::Select { cond, then, else_ } => {
                if !seen.insert(rep) {
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

    pub fn select(&mut self, cond: ValueId, then: ValueId, else_: ValueId) -> ValueId {
        let t = self.find(then);
        let e = self.find(else_);
        let c = self.find(cond);
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

    // ---- Union-find / congruence ----

    #[test]
    fn fresh_value_is_its_own_representative() {
        let mut pool = ValuePool::new();
        let a = pool.int_typed(1, crate::tir::TypeTable::I32);
        let b = pool.fresh_opaque();
        assert_eq!(pool.find(a), a);
        assert_eq!(pool.find(b), b);
    }

    #[test]
    fn union_makes_find_agree() {
        let mut pool = ValuePool::new();
        let a = pool.fresh_opaque();
        let b = pool.fresh_opaque();
        assert_ne!(pool.find(a), pool.find(b));
        let rep = pool.union(a, b);
        assert_eq!(pool.find(a), rep);
        assert_eq!(pool.find(b), rep);
    }

    #[test]
    fn union_is_idempotent_and_keeps_smaller_id() {
        let mut pool = ValuePool::new();
        let a = pool.fresh_opaque();
        let b = pool.fresh_opaque();
        let rep1 = pool.union(a, b);
        let rep2 = pool.union(b, a);
        assert_eq!(rep1, rep2);
        // The smaller raw id is the representative.
        assert_eq!(rep1, if a.index() <= b.index() { a } else { b });
    }

    #[test]
    fn rebuild_propagates_congruence_to_parents() {
        // f = Add(a, c), g = Add(b, c). After union(a, b) + rebuild, the two
        // sums denote the same value and must share a representative.
        let mut pool = ValuePool::new();
        let a = pool.fresh_opaque();
        let b = pool.fresh_opaque();
        let c = pool.fresh_opaque();
        let f = pool.binary(NirBinaryOp::Add, a, c, crate::tir::TypeTable::I32);
        let g = pool.binary(NirBinaryOp::Add, b, c, crate::tir::TypeTable::I32);
        assert_ne!(pool.find(f), pool.find(g));
        pool.union(a, b);
        pool.rebuild();
        assert_eq!(pool.find(f), pool.find(g));
    }

    #[test]
    fn congruence_propagates_through_two_levels() {
        // Neg(Add(a,c)) ≡ Neg(Add(b,c)) once a ≡ b.
        let mut pool = ValuePool::new();
        let a = pool.fresh_opaque();
        let b = pool.fresh_opaque();
        let c = pool.fresh_opaque();
        let f = pool.binary(NirBinaryOp::Add, a, c, crate::tir::TypeTable::I32);
        let g = pool.binary(NirBinaryOp::Add, b, c, crate::tir::TypeTable::I32);
        let nf = pool.unary(NirUnaryOp::Neg, f, crate::tir::TypeTable::I32);
        let ng = pool.unary(NirUnaryOp::Neg, g, crate::tir::TypeTable::I32);
        pool.union(a, b);
        pool.rebuild();
        assert_eq!(pool.find(nf), pool.find(ng));
    }

    #[test]
    fn interning_after_union_dedups_against_congruent_node() {
        // Build Add(a,c); union a≡b; a fresh Add(b,c) interns to the same id.
        let mut pool = ValuePool::new();
        let a = pool.fresh_opaque();
        let b = pool.fresh_opaque();
        let c = pool.fresh_opaque();
        let f = pool.binary(NirBinaryOp::Add, a, c, crate::tir::TypeTable::I32);
        pool.union(a, b);
        pool.rebuild();
        let g = pool.binary(NirBinaryOp::Add, b, c, crate::tir::TypeTable::I32);
        assert_eq!(pool.find(f), pool.find(g));
    }

    #[test]
    fn unrelated_values_stay_distinct_after_union() {
        let mut pool = ValuePool::new();
        let a = pool.fresh_opaque();
        let b = pool.fresh_opaque();
        let c = pool.fresh_opaque();
        let d = pool.fresh_opaque();
        let f = pool.binary(NirBinaryOp::Add, a, c, crate::tir::TypeTable::I32);
        let h = pool.binary(NirBinaryOp::Add, c, d, crate::tir::TypeTable::I32);
        pool.union(a, b);
        pool.rebuild();
        // `h` shares no unioned operand with `f`, so it stays its own class.
        assert_ne!(pool.find(f), pool.find(h));
    }

    #[test]
    fn rebuild_without_union_is_noop() {
        let mut pool = ValuePool::new();
        let a = pool.fresh_opaque();
        let c = pool.fresh_opaque();
        let f = pool.binary(NirBinaryOp::Add, a, c, crate::tir::TypeTable::I32);
        pool.rebuild();
        assert_eq!(pool.find(f), f);
        assert_eq!(pool.find(a), a);
    }
}
