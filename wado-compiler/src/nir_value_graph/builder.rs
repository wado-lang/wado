//! Per-function `ValueGraph` builder.
//!
//! Walks the `SkelTree` (`Body`) once and assigns a [`ValueId`] to every pure
//! [`ExprId`]. Impure or allocation-bearing expressions (calls, struct/array
//! literals, control flow, etc.) get no entry in `value_of`.
//!
//! Consumed lazily by [`crate::nir_engine::Engine::value`]; see the WEP at
//! `docs/wep-2026-06-05-worklist-rewrite-engine.md`.
//!
//! # Flow handling
//!
//! - Parameters seed `current_value` with a fresh `Opaque` each.
//! - `Let` / `Assign`-to-bare-`Local` updates `current_value[local_idx]` to
//!   the RHS's value (or `Opaque` if the RHS is impure).
//! - `If` snapshots `current_value`, walks both branches, then merges:
//!   diverging locals hash-cons a [`ValueKind::Select`] keyed on the
//!   condition's value, falling back to `Opaque` if the condition is impure.
//!   Heap state joins per arm over fall-through arms only ([`Builder::join_heap`]).
//! - `Match` / `Switch` walk every arm and merge n-ary: if every arm agrees
//!   on a local, that value carries; otherwise the local goes `Opaque`.
//!   N-ary `Select` chains are not yet constructed.
//! - `Loop` pre-scans the body for locals it may write, snapshots
//!   `current_value` into [`ValueGraphBuild::loop_entry_values`], and
//!   reassigns each written local to a fresh `Opaque` before walking the
//!   body; post-loop those locals stay `Opaque`.
//! - `LabeledBlock` marks every local written in its subtree `Opaque` on
//!   exit, since `break` paths can carry writes the fall-through state
//!   never observes.
//! - Pattern bindings (`LetDestructure`, `Match` arm bindings) are seeded
//!   with `Opaque`.

use crate::const_eval;
use crate::hashmap::IndexMap;
use crate::nir::{FuncId, NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{
    ArmData, BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatId, PatKind, StmtId, StmtKind,
};

use super::{HeapVersion, OpaqueSource, ValueId, ValueKind, ValuePool};

/// Per-function heap-version tracker. The builder threads one `HeapState`
/// through the walk; on every Skel node that may write the heap, the
/// appropriate generation bumps to a fresh value. A read's effective
/// version is the max of every generation that could cover it
/// ([`HeapState::version_of`]); `field_store` keys carry that version, so a
/// stale seed is naturally unreachable once any covering generation bumps.
///
/// Granularity is per-`(receiver-root, field)`:
/// - `per_slot[(root, field)]` — bumped by a direct `root.field = …` store
///   on a non-aliased bare-`Local` receiver, so a write to `a.f` leaves
///   `b.f` (a different object, same `field_index`) untouched.
/// - `per_local[root]` — bumped when every field of `root` may have changed:
///   a call while `root` is reference-aliased (the callee can reach its
///   object). Non-aliased locals' fields survive a call.
/// - `field_global[field]` — bumped by a write to `field` through an
///   aliased or non-bare-`Local` receiver (`a.b.f`, `r.f` for a reference
///   `r`): every alias of that field is invalidated without tracking which
///   locals alias which.
/// - `default_version` — covers everything; [`HeapState::bump_all`] advances
///   it for truly opaque writes (deref / index store, global set, indirect
///   call, loop entry).
///
/// Branch endpoints join per arm instead of bumping ([`Builder::join_heap`]).
struct HeapState {
    /// Next fresh version to hand out.
    next: HeapVersion,
    per_slot: IndexMap<(u32, u32), HeapVersion>,
    per_local: IndexMap<u32, HeapVersion>,
    field_global: IndexMap<u32, HeapVersion>,
    /// Version covering slots in none of the maps above.
    default_version: HeapVersion,
    /// Version covering module-scope globals. Separate from `default_version`
    /// because what invalidates a global is not what invalidates a field: a
    /// call bumps this whether or not it touches any caller local, since the
    /// callee may write a `global mut`.
    global_version: HeapVersion,
}

impl HeapState {
    fn new() -> Self {
        Self {
            next: HeapVersion::INITIAL.bump(),
            per_slot: IndexMap::default(),
            per_local: IndexMap::default(),
            field_global: IndexMap::default(),
            default_version: HeapVersion::INITIAL,
            global_version: HeapVersion::INITIAL,
        }
    }

    /// The version a read of any global sees.
    fn global_version(&self) -> HeapVersion {
        self.global_version
    }

    fn bump_globals(&mut self) {
        self.global_version = self.fresh();
    }

    fn fresh(&mut self) -> HeapVersion {
        let v = self.next;
        self.next = self.next.bump();
        v
    }

    /// The effective version a read of `root.field` sees: the max of every
    /// generation that could have invalidated it. `root` is `None` when the
    /// receiver is not a determinable bare `Local`, so per-slot / per-local
    /// precision does not apply — only `field_global` and `default`.
    fn version_of(&self, root: Option<u32>, field: u32) -> HeapVersion {
        let mut v = self.default_version;
        if let Some(&fg) = self.field_global.get(&field) {
            v = v.max(fg);
        }
        if let Some(r) = root {
            if let Some(&pl) = self.per_local.get(&r) {
                v = v.max(pl);
            }
            if let Some(&ps) = self.per_slot.get(&(r, field)) {
                v = v.max(ps);
            }
        }
        v
    }

    fn bump_slot(&mut self, root: u32, field: u32) {
        let v = self.fresh();
        self.per_slot.insert((root, field), v);
    }

    fn bump_local(&mut self, root: u32) {
        let v = self.fresh();
        self.per_local.insert(root, v);
    }

    fn bump_field_global(&mut self, field: u32) {
        let v = self.fresh();
        self.field_global.insert(field, v);
    }

    fn bump_all(&mut self) {
        let v = self.fresh();
        self.per_slot.clear();
        self.per_local.clear();
        self.field_global.clear();
        self.default_version = v;
        self.global_version = v;
    }

    /// Snapshot the read-visible state only. `next` is a monotonic counter
    /// shared across the whole function, so arms restored from the snapshot
    /// never reuse a version another arm allocated.
    fn snapshot(&self) -> HeapSnapshot {
        HeapSnapshot {
            per_slot: self.per_slot.clone(),
            per_local: self.per_local.clone(),
            field_global: self.field_global.clone(),
            default_version: self.default_version,
            global_version: self.global_version,
        }
    }

    fn restore(&mut self, snap: HeapSnapshot) {
        self.per_slot = snap.per_slot;
        self.per_local = snap.per_local;
        self.field_global = snap.field_global;
        self.default_version = snap.default_version;
        self.global_version = snap.global_version;
    }

    /// Seed a fresh `HeapState` (as in [`build_scoped`]) with a snapshot taken at
    /// a call site of the enclosing build, so a re-valued field read sees the
    /// caller's version rather than a fresh `INITIAL`. `next` is advanced past
    /// every restored version: a write inside the re-valued region then allocates
    /// a generation that cannot collide with a caller-allocated one (the snapshot
    /// does not carry the build's `next` counter).
    fn seed_from(&mut self, snap: &HeapSnapshot) {
        self.per_slot.clone_from(&snap.per_slot);
        self.per_local.clone_from(&snap.per_local);
        self.field_global.clone_from(&snap.field_global);
        self.default_version = snap.default_version;
        self.global_version = snap.global_version;
        let max = snap
            .per_slot
            .values()
            .chain(snap.per_local.values())
            .chain(snap.field_global.values())
            .chain(std::iter::once(&snap.default_version))
            .chain(std::iter::once(&snap.global_version))
            .copied()
            .max()
            .unwrap_or(HeapVersion::INITIAL);
        self.next = max.bump();
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HeapSnapshot {
    per_slot: IndexMap<(u32, u32), HeapVersion>,
    per_local: IndexMap<u32, HeapVersion>,
    field_global: IndexMap<u32, HeapVersion>,
    default_version: HeapVersion,
    global_version: HeapVersion,
}

/// A snapshot of *all* flow-sensitive builder state at a program point:
/// per-local values (`current_value`), heap versions (`heap_state`), and
/// reference look-through targets (`ref_targets`). Captured, restored, and
/// merged as a unit ([`Builder::flow_snapshot`] / [`Builder::flow_restore`] /
/// [`Builder::flow_join_two`] / [`Builder::flow_join_n`]) so a control-flow
/// join can never handle one component and silently forget another — the bug
/// class that let a branch/loop-reassigned reference forward a stale pointee.
#[derive(Clone)]
struct FlowSnapshot {
    current_value: IndexMap<u32, ValueId>,
    heap: HeapSnapshot,
    ref_targets: IndexMap<u32, u32>,
}

/// One arm's exit state for a branch join, plus whether control falls through
/// to the merge point. A non-fall-through arm (`break` / `return` /
/// `continue`) contributes only its heap writes to the join — see
/// [`Builder::join_heap`].
struct FlowArm {
    state: FlowSnapshot,
    falls_through: bool,
}

/// The result of running [`build`] over a function body. The expr→value
/// side-table (`value_of`) is retired — values are carried by promoted operands
/// and the value pool — so the only persisted product is `loop_entry_values`,
/// which licm reads for hoist legality.
#[derive(Debug, Clone)]
pub struct ValueGraphBuild {
    /// Per-loop pre-header snapshot of `current_value`, keyed by the loop
    /// body's `BlockId`. Hoisting to the pre-header requires each `Local`
    /// leaf's use-site value to equal this entry value — cross-iteration
    /// invariance is not enough (`loop { x = 5; … x + n … }` has an
    /// invariant use value that differs from the pre-header `x`).
    pub loop_entry_values: IndexMap<BlockId, IndexMap<u32, ValueId>>,
}

/// Build the `ValueGraph` for one function body.
///
/// `param_locals` are the local indices of the function's parameters; each
/// seeds `current_value` with one fresh `Opaque`, so a `Local` read returns
/// that Opaque every time until the parameter is reassigned (which the
/// builder picks up the same way as any other `Assign`). An unseeded local's
/// first read mints an equivalent fallback `Opaque`, but only up-front
/// seeding makes parameters visible in the loop-entry snapshots
/// (`loop_entry_values`), which are taken before any in-loop read.
///
/// `aliased` are locals whose object is reference-aliased — address-taken,
/// `with stores[p]`, or reference-typed (`let r = &x`, `Box`, `List`, `&T`).
/// A write to such a local's field, or a call that may reach its object,
/// invalidates the field through the conservative `field_global` / `per_local`
/// generations rather than the precise `per_slot` one. `untrackable` is the
/// `stores`-aliased subset whose fields are never seeded (their aliasing
/// escapes entirely). A non-aliased local's object is reachable only through
/// that local, so its `per_slot` fields survive calls and other objects'
/// same-`field_index` writes (see [`HeapState`]).
///
/// `mut_escaped` is the subset of `aliased` a call may actually mutate (locals
/// with a *mutable* escape — `&mut v`, a mut-ref argument, a `&mut self`
/// receiver, or a `stores` stash). [`Builder::bump_call_effects`] bumps only
/// these across a call; a reference-aliased local whose every escape is an
/// immutable `&v` keeps its forwarded fields, since no callee can mutate it.
#[allow(clippy::too_many_arguments)]
pub fn build(
    body: &mut Body,
    param_locals: &[u32],
    aliased: &crate::hashmap::IndexSet<u32>,
    untrackable: &crate::hashmap::IndexSet<u32>,
    mut_escaped: &crate::hashmap::IndexSet<u32>,
    pure_calls: &crate::hashmap::IndexSet<ExprId>,
    pure_builtin_callees: &crate::hashmap::IndexSet<FuncId>,
    type_table: Option<&crate::tir::TypeTable>,
) -> ValueGraphBuild {
    // Build into the body's own pool: take it out as the seed (so a promoted
    // `Operand::Value` resolves through the same ids), grow it during the walk,
    // and write it back. `body.values` is the one persistent pool — ids stay
    // stable across builds (it only grows), the prerequisite for build-once.
    let seed = std::mem::take(&mut body.values);
    let (pool, loop_entry_values) = {
        let mut b = Builder::new(&*body, aliased, untrackable, mut_escaped, type_table, seed);
        b.pure_calls.clone_from(pure_calls);
        b.pure_builtin_callees.clone_from(pure_builtin_callees);
        b.seed_params(param_locals);
        b.walk_block(body.root);
        (b.pool, b.loop_entry_values)
    };
    body.values = pool;
    ValueGraphBuild { loop_entry_values }
}

/// Scoped re-valuation of one self-contained inlined block, seeded with the
/// call site's `param → value` map (Method A — splice-point growth). Walks only
/// `block` after its first `skip` param-binding statements (params pre-seeded),
/// with a fresh heap (a field read on a param is conservatively fresh), using
/// `body`'s existing pool so produced values share ids with the graph. Returns
/// only **constant-literal** entries: a scoped walk's non-constant values carry
/// context local to the walk (a `FieldAccess`'s `HeapVersion` numbers from the
/// fresh heap and would over-merge; an `Opaque` of a remapped local is
/// walk-local), while a constant equals what a fresh build assigns. Never walks
/// the caller's untouched remainder, so it adds no `builder::build`
/// (`rebuilds = 0`) and parks no cache.
///
/// `scratch` is a pool the caller clones from `body.values` once and reuses
/// across every inlined block of the function: the walk stamps types
/// (`set_type`) on the values it interns, and doing that on a value the main
/// graph shares would mutate its main-graph type and perturb the structural
/// passes — so the walk runs in `scratch`, leaving `body.values` untouched
/// (seeded `ValueId`s stay valid; the clone preserves ids). Only the constant
/// *literals* are re-interned into the main pool, which is idempotent.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_scoped(
    body: &mut Body,
    block: BlockId,
    skip: usize,
    seed: &IndexMap<u32, ValueId>,
    aliased: &crate::hashmap::IndexSet<u32>,
    untrackable: &crate::hashmap::IndexSet<u32>,
    mut_escaped: &crate::hashmap::IndexSet<u32>,
    type_table: Option<&crate::tir::TypeTable>,
    pure_builtin_callees: &crate::hashmap::IndexSet<FuncId>,
    scratch: &mut ValuePool,
    heap_seed: Option<&HeapSnapshot>,
    live_base: u32,
) -> IndexMap<ExprId, ValueId> {
    let scoped: Vec<(ExprId, ValueId)> = {
        let pool = std::mem::take(scratch);
        let mut b = Builder::new(&*body, aliased, untrackable, mut_escaped, type_table, pool);
        b.pure_builtin_callees.clone_from(pure_builtin_callees);
        b.current_value.clone_from(seed);
        // Seed the heap with the caller's version state at the call site so a
        // spliced field read carries the version a fresh whole-function build
        // would assign (a fresh `INITIAL` heap collapses distinct versions — e.g.
        // a `pop()`-shrunk `.used` — and would over-merge).
        if let Some(h) = heap_seed {
            b.heap_state.seed_from(h);
        }
        let stmts: Vec<StmtId> = b.body.blocks[block]
            .stmts
            .iter()
            .skip(skip)
            .copied()
            .collect();
        for s in stmts {
            b.walk_stmt(s);
        }
        let scoped = b.value_of.iter().map(|(&e, &v)| (e, v)).collect();
        *scratch = b.pool;
        scoped
    };
    // Surface every caller-rooted re-emittable value (constants, plus
    // `FieldAccess` / arithmetic over the call-site args at their true version),
    // re-interned into the live pool. A walk-local value (an `Opaque` of a
    // remapped callee local) is dropped — it has no caller-pool identity.
    let mut out = IndexMap::default();
    for (e, sv) in scoped {
        if let Some(lv) = reintern_live_rooted(scratch, &mut body.values, sv, live_base, type_table)
        {
            out.insert(e, lv);
        }
    }
    out
}

/// Whether `id`'s value is a build-context-free constant safe to freeze into an
/// operand at the early freeze (see `extract::freeze_pure_arith`'s constant-leaf
/// promotion). Not every constant qualifies — see
/// [`ValueKind::is_operand_constant`].
pub(crate) fn is_const_value(pool: &ValuePool, id: ValueId) -> bool {
    pool.kind(id).is_operand_constant()
}

/// Re-intern a constant `ValueKind` into `pool`, returning its id there.
fn reintern_const(pool: &mut ValuePool, k: ValueKind) -> ValueId {
    match k {
        ValueKind::Int(v, t) => pool.int_typed(v, t),
        ValueKind::Float(b, t) => pool.float_bits(b, t),
        ValueKind::Bool(b) => pool.bool(b),
        ValueKind::Char(c) => pool.char(c),
        ValueKind::Null => pool.null(),
        ValueKind::Unit => pool.unit(),
        ValueKind::Const(key, t) => pool.constant(key.value(), t),
        ValueKind::Opaque(_)
        | ValueKind::Binary { .. }
        | ValueKind::Unary { .. }
        | ValueKind::Cast { .. }
        | ValueKind::Select { .. }
        | ValueKind::LoopPhi { .. }
        | ValueKind::GlobalRead { .. }
        | ValueKind::FieldAccess { .. } => {
            unreachable!("the caller's match admits only constant kinds")
        }
    }
}

/// Re-intern a value computed in a [`build_scoped`] `scratch` pool into the live
/// `live` pool, when it is rooted entirely in **caller** values — every leaf an
/// id below `live_base` (the pool length the scratch was cloned at, so those ids
/// are shared and valid in `live`). Such a value is a constant, or a
/// `FieldAccess` / arithmetic / `Select` tree over the call-site argument values,
/// and it carries the caller's heap version (from the seeded heap), so it equals
/// what a fresh whole-function build assigns the spliced node — sound to surface.
///
/// Returns `None` for a walk-local `Opaque` (a remapped callee local with no seed
/// value) or a `LoopPhi`: those have no meaning in the caller pool. The recursion
/// is the cross-pool copy [`build_scoped`]'s constant case did, widened past
/// constants to the re-emittable caller-rooted values (WEP: promote the spliced
/// `FieldAccess` at its true version).
fn reintern_live_rooted(
    scratch: &ValuePool,
    live: &mut ValuePool,
    id: ValueId,
    live_base: u32,
    type_table: Option<&crate::tir::TypeTable>,
) -> Option<ValueId> {
    if id.index() < live_base {
        return Some(id);
    }
    let kind = scratch.kind(id).clone();
    Some(match kind {
        ValueKind::Int(..)
        | ValueKind::Float(..)
        | ValueKind::Bool(_)
        | ValueKind::Char(_)
        | ValueKind::Null
        | ValueKind::Unit
        | ValueKind::Const(..) => reintern_const(live, kind),
        ValueKind::Binary { op, lhs, rhs, ty } => {
            let l = reintern_live_rooted(scratch, live, lhs, live_base, type_table)?;
            let r = reintern_live_rooted(scratch, live, rhs, live_base, type_table)?;
            live.binary_folded(op, l, r, ty, type_table)
        }
        ValueKind::Unary { op, operand, ty } => {
            let o = reintern_live_rooted(scratch, live, operand, live_base, type_table)?;
            live.unary_folded(op, o, ty, type_table)
        }
        ValueKind::Cast { operand, target } => {
            let o = reintern_live_rooted(scratch, live, operand, live_base, type_table)?;
            live.cast_folded(o, target, type_table)
        }
        ValueKind::FieldAccess {
            receiver,
            field_index,
            heap_ver,
        } => {
            let r = reintern_live_rooted(scratch, live, receiver, live_base, type_table)?;
            live.field_access(r, field_index, heap_ver)
        }
        ValueKind::Select { cond, then, else_ } => {
            let c = reintern_live_rooted(scratch, live, cond, live_base, type_table)?;
            let t = reintern_live_rooted(scratch, live, then, live_base, type_table)?;
            let e = reintern_live_rooted(scratch, live, else_, live_base, type_table)?;
            live.select(c, t, e)
        }
        // A global read carries the walk's own heap version, which names
        // nothing in the caller's pool.
        ValueKind::GlobalRead { .. } | ValueKind::Opaque(_) | ValueKind::LoopPhi { .. } => {
            return None;
        }
    })
}

struct Builder<'a> {
    body: &'a Body,
    pool: ValuePool,
    value_of: IndexMap<ExprId, ValueId>,
    block_writes: IndexMap<BlockId, std::rc::Rc<crate::hashmap::IndexSet<u32>>>,
    fresh_invalidations: crate::hashmap::IndexSet<u32>,
    mut_escaped_sorted: Vec<u32>,
    /// `local_index → current Value` at the current program point. Cloned at
    /// branch entries so each arm walks from the pre-branch snapshot.
    current_value: IndexMap<u32, ValueId>,
    /// Heap-version tracker. See [`HeapState`].
    heap_state: HeapState,
    /// Store→load forwarding for fields: the value last stored to
    /// `(receiver, field, version)`, returned for reads at the same triple.
    /// Versions are monotonic and never reused, so a write or branch join
    /// bumps past a stale entry — no invalidation needed.
    field_store: IndexMap<(ValueId, u32, HeapVersion), ValueId>,
    /// Reference-aliased locals; their field writes / calls invalidate
    /// conservatively. See [`build`].
    aliased: crate::hashmap::IndexSet<u32>,
    /// `stores`-aliased locals whose fields are never seeded. See [`build`].
    untrackable: crate::hashmap::IndexSet<u32>,
    /// Locals a call may mutate (mutable escape). Only these are bumped by
    /// [`Builder::bump_call_effects`]. See [`build`].
    mut_escaped: crate::hashmap::IndexSet<u32>,
    /// `local → pointee local` for `let r = &v` references, so `r.f` forwards
    /// from `v`'s field slot (reference look-through). Cleared when `r` or the
    /// pointee is reassigned ([`Builder::update_ref_target`]). This is
    /// flow-sensitive state and follows the same join discipline as
    /// `current_value`: every branch joins it ([`Builder::merge_ref_targets`] —
    /// an entry survives only if all arms agree) and every loop / labeled-block
    /// drops the entries its body may reassign ([`Builder::drop_ref_targets_for`]),
    /// so a reference whose target diverges becomes unknown rather than
    /// forwarding a stale pointee.
    ref_targets: IndexMap<u32, u32>,
    /// Type table for constant folding of pure arithmetic on literal operands
    /// (`Binary` / `Unary`). `None` disables folding (the value graph still
    /// builds structural nodes). See [`Builder::fold_binary_const`].
    type_table: Option<&'a crate::tir::TypeTable>,
    /// Per-loop pre-header `current_value` snapshots. See
    /// [`ValueGraphBuild::loop_entry_values`].
    loop_entry_values: IndexMap<BlockId, IndexMap<u32, ValueId>>,
    /// `ExprId` indices of calls that mutate no caller local. See
    /// [`BuildConfig::pure_calls`].
    pure_calls: crate::hashmap::IndexSet<ExprId>,
    /// [`FuncId`]s of pure builtin intrinsics (`array_get`, `array_len`, …): a
    /// call to one writes no heap, so a loop body that only calls such
    /// intrinsics does not invalidate forwarded field versions. Empty is
    /// conservative (every call is a heap write). See [`is_builtin_pure_call`].
    pure_builtin_callees: crate::hashmap::IndexSet<FuncId>,
}

impl<'a> Builder<'a> {
    fn new(
        body: &'a Body,
        aliased: &crate::hashmap::IndexSet<u32>,
        untrackable: &crate::hashmap::IndexSet<u32>,
        mut_escaped: &crate::hashmap::IndexSet<u32>,
        type_table: Option<&'a crate::tir::TypeTable>,
        pool: ValuePool,
    ) -> Self {
        Self {
            body,
            pool,
            value_of: IndexMap::default(),
            block_writes: IndexMap::default(),
            fresh_invalidations: crate::hashmap::IndexSet::default(),
            current_value: IndexMap::default(),
            heap_state: HeapState::new(),
            field_store: IndexMap::default(),
            aliased: aliased.clone(),
            untrackable: untrackable.clone(),
            mut_escaped: mut_escaped.clone(),
            mut_escaped_sorted: {
                let mut v: Vec<u32> = mut_escaped.iter().copied().collect();
                v.sort_unstable();
                v
            },
            ref_targets: IndexMap::default(),
            type_table,
            loop_entry_values: IndexMap::default(),
            pure_calls: crate::hashmap::IndexSet::default(),
            pure_builtin_callees: crate::hashmap::IndexSet::default(),
        }
    }

    /// If both operands are constant literals and `op` is a const-foldable
    /// pure arithmetic / comparison / bitwise op, fold it exactly as niri's
    /// CTFE would ([`crate::const_eval::eval_binary`]) and intern the resulting
    /// literal `ValueId`. Each operand's `PrimitiveType` is read from its own
    /// NIR type, so integer wrapping matches the runtime width and a
    /// mixed-prim op (which niri refuses) is not folded. Returns `None` when an
    /// operand is non-constant, a type is unavailable, or the op is not
    /// foldable — the caller then builds the structural `Binary` node.
    /// Type of an operand during the build: from the skeleton expr, or from the
    /// build pool for a promoted constant (its source type was seeded there).
    fn operand_type(&self, op: Operand) -> crate::tir::TypeId {
        match op {
            Operand::Expr(e) => self.body.exprs[e].type_id,
            Operand::Value(v) => self
                .pool
                .type_of(v)
                .expect("promoted operand has a recorded type"),
        }
    }

    fn fold_binary_const(
        &mut self,
        op: NirBinaryOp,
        lhs: ValueId,
        rhs: ValueId,
        left: Operand,
        right: Operand,
        result_type: crate::tir::TypeId,
    ) -> Option<ValueId> {
        let tt = self.type_table?;
        // Reflexivity: operands sharing a `ValueId` are the same value, so
        // `x == x` folds to `true` and `x != x` to `false` without a literal
        // (e.g. an identity reinterpret `v as SameType == v`). Floats
        // (`NaN != NaN`) and `v128` (no scalar `==`) are excluded; every other
        // operand reaching here is reflexively equal.
        if lhs == rhs
            && matches!(op, NirBinaryOp::Eq | NirBinaryOp::NotEq)
            && !matches!(
                const_eval::prim_of(self.operand_type(left), tt),
                Some(
                    crate::tir::PrimitiveType::F32
                        | crate::tir::PrimitiveType::F64
                        | crate::tir::PrimitiveType::V128
                )
            )
        {
            return Some(
                self.const_to_value(const_eval::Value::Bool(op == NirBinaryOp::Eq), result_type),
            );
        }
        // Logical identities with one constant-bool operand. `&&` / `||`
        // short-circuit, but both operands reach here as pure values (the
        // conditional rhs walk above already accounted for any effects), so
        // dropping the non-taken arm is sound: `false || x → x`,
        // `true || x → true`, `true && x → x`, `false && x → false` (and the
        // mirror cases with the constant on the right).
        if matches!(op, NirBinaryOp::And | NirBinaryOp::Or) {
            let lb = self.pool.kind(lhs).as_bool();
            let rb = self.pool.kind(rhs).as_bool();
            let folded = match op {
                NirBinaryOp::Or => match (lb, rb) {
                    (Some(true), _) | (_, Some(true)) => {
                        Some(self.const_to_value(const_eval::Value::Bool(true), result_type))
                    }
                    (Some(false), _) => Some(rhs),
                    (_, Some(false)) => Some(lhs),
                    _ => None,
                },
                NirBinaryOp::And => match (lb, rb) {
                    (Some(false), _) | (_, Some(false)) => {
                        Some(self.const_to_value(const_eval::Value::Bool(false), result_type))
                    }
                    (Some(true), _) => Some(rhs),
                    (_, Some(true)) => Some(lhs),
                    _ => None,
                },
                _ => None,
            };
            if let Some(v) = folded {
                return Some(v);
            }
        }
        let lv = self.value_to_const(lhs, left, tt)?;
        let rv = self.value_to_const(rhs, right, tt)?;
        let result = const_eval::eval_binary(lv, op, rv)?;
        Some(self.const_to_value(result, result_type))
    }

    /// Const-fold a `Unary` (`Neg` / `Not` / `BitNot`) on a literal operand.
    fn fold_unary_const(
        &mut self,
        op: NirUnaryOp,
        operand: ValueId,
        inner: Operand,
        result_type: crate::tir::TypeId,
    ) -> Option<ValueId> {
        let tt = self.type_table?;
        let v = self.value_to_const(operand, inner, tt)?;
        let result = const_eval::eval_unary(op, v)?;
        Some(self.const_to_value(result, result_type))
    }

    /// Bridge a literal `ValueId` to niri's [`crate::const_eval::Value`], reading the
    /// operand's `PrimitiveType` from `expr`'s NIR type (needed for the integer
    /// width / float precision). `None` for non-literal kinds or a missing prim.
    fn value_to_const(
        &self,
        vn: ValueId,
        op: Operand,
        tt: &crate::tir::TypeTable,
    ) -> Option<const_eval::Value> {
        let prim = const_eval::prim_of(self.operand_type(op), tt);
        super::value_kind_to_const(self.pool.kind(vn), prim)
    }

    /// Intern a folded constant, carrying the folded expr's NIR type as the
    /// width-bearing type.
    fn const_to_value(&mut self, v: const_eval::Value, result_type: crate::tir::TypeId) -> ValueId {
        self.pool.intern_const(v, result_type)
    }

    /// Record / clear `local`'s reference target from its new RHS. `let r = &v`
    /// (a bare-`Local` `v`) records `r → v` so a later `r.f` read forwards from
    /// `v`'s field slot ([`Builder::reference_lookthrough`]). Any other RHS
    /// clears `r`'s entry, and a reassignment of any local invalidates every
    /// reference still pointing at it (the old pointee may have moved).
    fn update_ref_target(&mut self, local: u32, value: ExprId) {
        // Reassigning `local` invalidates references that pointed at it.
        self.ref_targets.retain(|_, &mut pointee| pointee != local);
        let target = match &self.body.exprs[value].kind {
            ExprKind::Unary {
                op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                expr: inner,
            } => inner
                .as_expr()
                .and_then(|ie| match &self.body.exprs[ie].kind {
                    ExprKind::Local { index, .. } => Some(*index),
                    _ => None,
                }),
            _ => None,
        };
        match target {
            Some(pointee) if pointee != local => {
                self.ref_targets.insert(local, pointee);
            }
            _ => {
                self.ref_targets.swap_remove(&local);
            }
        }
    }

    /// If `recv_expr` is a bare `Local` known to reference a pointee local
    /// (`let r = &v`), return that pointee's current receiver `ValueId` and
    /// root local, so `r.f` forwards from `v`'s field slot. The pointee's slot
    /// state (including call / write invalidation) is used as-is, so a stale
    /// forward is impossible — at worst the lookup misses and `r.f` re-derives.
    fn reference_lookthrough(&self, recv_expr: ExprId) -> Option<(ValueId, u32)> {
        let ExprKind::Local { index, .. } = &self.body.exprs[recv_expr].kind else {
            return None;
        };
        let pointee = *self.ref_targets.get(index)?;
        let pointee_vn = *self.current_value.get(&pointee)?;
        Some((pointee_vn, pointee))
    }

    /// The bare-`Local` root of a (possibly nested) field-access place, or
    /// `None` if the receiver is not rooted in a `Local` (a call result, an
    /// index, a deref, …). `a.b.f` roots at `a`.
    fn receiver_root(&self, recv_expr: ExprId) -> Option<u32> {
        match &self.body.exprs[recv_expr].kind {
            ExprKind::Local { index, .. } => Some(*index),
            ExprKind::FieldAccess { expr, .. } => {
                expr.as_expr().and_then(|e| self.receiver_root(e))
            }
            _ => None,
        }
    }

    /// A direct / method call may mutate any field of a local the callee can
    /// reach *and* mutate — one with a mutable escape (`&mut v`, a mut-ref
    /// argument, a `&mut self` receiver, or a `stores` stash). The callee
    /// reaches such an object via an escaped mutable reference or a global, and
    /// a mutable reference may have been retained, so any later call is a
    /// potential mutation point — bump every `mut_escaped` local, not only this
    /// call's arguments. Non-`mut_escaped` locals (non-aliased, or aliased only
    /// through an immutable `&v`) cannot be mutated by any callee, so their
    /// fields survive the call.
    /// Invalidate the locals a call at `call` may mutate. A call proven to mutate
    /// no caller local ([`BuildConfig::pure_calls`]) bumps only the `untrackable`
    /// (stashed) locals any call can reach; otherwise every `mut_escaped` local is
    /// bumped (conservative). Skipping the bump for a pure accessor keeps a
    /// `mut_escaped` receiver's field version stable across it.
    fn bump_call_effects(&mut self, call: ExprId) {
        // A global read survives only across a call the graph knows writes
        // nothing. `pure_calls` is about caller locals, so it cannot answer
        // this — a callee that touches no argument may still write a global.
        let writes_globals = match &self.body.exprs[call].kind {
            ExprKind::Call { func_id, .. } => {
                !is_builtin_pure_call(&self.pure_builtin_callees, *func_id)
            }
            _ => true,
        };
        if writes_globals {
            self.heap_state.bump_globals();
        }
        let pure = self.pure_calls.contains(&call);
        // Iterate ascending local index, not `mut_escaped`'s insertion order:
        // opaque `ValueId`s and heap versions are handed out in visit order, so
        // this keeps the value graph a deterministic function of the program
        // regardless of how the alias sets were built (#1440).
        for i in 0..self.mut_escaped_sorted.len() {
            let l = self.mut_escaped_sorted[i];
            if pure && !self.untrackable.contains(&l) {
                continue;
            }
            self.heap_state.bump_local(l);
            self.invalidate_local_with_source(l, Some(OpaqueSource::Local(l)));
        }
    }

    fn seed_params(&mut self, param_locals: &[u32]) {
        for &idx in param_locals {
            // A parameter's value re-emits as `local.get idx` — record the
            // source so the extractor can materialise a promoted value over it.
            let opaque = self.pool.fresh_opaque_with_source(OpaqueSource::Local(idx));
            self.set_local_value(idx, opaque);
        }
    }

    fn walk_block(&mut self, block: crate::nir_arena::BlockId) {
        let stmts = self.body.blocks[block].stmts.clone();
        for s in stmts {
            self.walk_stmt(s);
        }
    }

    fn walk_stmt(&mut self, stmt: StmtId) {
        match self.body.stmts[stmt].kind.clone() {
            StmtKind::Let {
                local_index, value, ..
            } => {
                let v = self.walk_operand(value).unwrap_or_else(|| {
                    self.pool
                        .fresh_opaque_with_source(OpaqueSource::Local(local_index))
                });
                self.set_local_value(local_index, v);
                // `let x = S { f: lit, … }` binds `x` to a fresh opaque; seed
                // each pure field so a later `x.f` read forwards the literal.
                // A promoted constant binding has no struct fields / ref target.
                if let Some(ve) = value.as_expr() {
                    self.seed_struct_literal_fields(local_index, v, ve);
                    self.update_ref_target(local_index, ve);
                }
            }
            StmtKind::LetDestructure { pattern, value, .. } => {
                self.walk_operand(value);
                // Destructured bindings are Opaque for now; field-projection
                // Value kinds for them are a follow-up.
                self.bind_pattern_opaque(pattern);
            }
            StmtKind::Expr(e) => {
                self.walk_operand(e);
            }
            StmtKind::Return { value } => {
                if let Some(v) = value {
                    self.walk_operand(v);
                }
            }
            StmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.walk_operand(v);
                }
            }
            StmtKind::Continue => {}
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let cond_v = self.walk_operand(condition);
                let pre = self.flow_snapshot();
                self.walk_block(then_block);
                let then_arm = FlowArm {
                    falls_through: self.block_falls_through(then_block),
                    state: self.flow_snapshot(),
                };
                self.flow_restore(&pre);
                let else_arm = if let Some(eb) = else_block {
                    self.walk_block(eb);
                    let arm = FlowArm {
                        falls_through: self.block_falls_through(eb),
                        state: self.flow_snapshot(),
                    };
                    self.flow_restore(&pre);
                    arm
                } else {
                    FlowArm {
                        state: pre.clone(),
                        falls_through: true,
                    }
                };
                self.flow_join_two(cond_v, &pre, then_arm, else_arm);
            }
            StmtKind::Loop { body: lb } => {
                self.walk_loop(lb);
            }
            StmtKind::LabeledBlock { block, .. } => {
                // A `break` to this label (or an enclosing one) can carry a
                // write whose effect never reaches the fall-through state.
                // Mark every local written anywhere in the subtree Opaque;
                // a fall-through-diff check would miss break-only-path
                // writes.
                self.walk_block(block);
                self.dirty_all_writes_in_block(block);
                self.heap_state.bump_all();
            }
        }
    }

    /// Walk `expr` and return its `ValueGraph` id if the expression is pure.
    /// Impure expressions return `None`; their pure children are still walked
    /// for their side of the side-table.
    fn walk_expr(&mut self, expr: ExprId) -> Option<ValueId> {
        let id = self.compute_value(expr);
        if let Some(id) = id {
            self.value_of.insert(expr, id);
            // Record the source type so extraction can materialise this value
            // once the typed `ExprNode` is promoted away.
            self.pool.set_type(id, self.body.exprs[expr].type_id);
        }
        id
    }

    /// Walk an operand: a promoted pure value is its own id; an effectful
    /// subtree walks its skeleton.
    fn walk_operand(&mut self, op: Operand) -> Option<ValueId> {
        match op {
            Operand::Value(v) => Some(v),
            Operand::Expr(e) => self.walk_expr(e),
        }
    }

    fn compute_value(&mut self, expr: ExprId) -> Option<ValueId> {
        match self.body.exprs[expr].kind.clone() {
            // ---- Literals ----
            // Orphaned tombstone: no value.
            ExprKind::Dead => None,

            // ---- Local read ----
            ExprKind::Local { index, .. } => Some(self.read_local(index)),

            // ---- Pure arithmetic ----
            ExprKind::Binary { left, op, right } => {
                // Always walk both operands for their side effects on
                // `current_value` and `heap_state`, even when one of them is
                // impure (a `?` short-circuit on `lhs` would skip the rhs
                // walk and miss any local assignments / heap writes inside
                // it).
                let lhs = self.walk_operand(left);
                let rhs = if matches!(op, NirBinaryOp::And | NirBinaryOp::Or) {
                    // Short-circuit `&&` / `||`: the rhs runs conditionally, so
                    // its effects "may or may not have happened" — any local it
                    // mutates goes Opaque and any field it writes is
                    // invalidated (both below). Else `false && { x = 2; true }`
                    // would commit `x = 2` and forward the never-stored value.
                    self.fresh_invalidations.clear();
                    let saved_cur = self.current_value.clone();
                    let rhs = self.walk_operand(right);
                    let changed: crate::hashmap::IndexSet<u32> = self
                        .current_value
                        .iter()
                        .filter_map(|(&k, &v)| {
                            saved_cur.get(&k).and_then(|s| (*s != v).then_some(k))
                        })
                        .collect();
                    for &k in &changed {
                        let opaque = self.pool.fresh_opaque();
                        self.set_local_value(k, opaque);
                    }
                    // A reference the conditionally-run rhs reassigned (or whose
                    // pointee it reassigned) no longer has a known target.
                    self.drop_ref_targets_for(&changed);
                    // Invalidate only what the rhs writes, not a blanket
                    // `bump_all`: a pure rhs leaves unrelated fields intact.
                    if let Some(re) = right.as_expr() {
                        let eff = collect_node_heap_effects(
                            self.body,
                            &self.pure_builtin_callees,
                            NodeRef::Expr(re),
                        );
                        self.apply_loop_heap_effects(&eff);
                    }
                    rhs
                } else {
                    self.walk_operand(right)
                };
                let lhs = lhs?;
                let rhs = rhs?;
                let result_type = self.body.exprs[expr].type_id;
                if let Some(folded) = self.fold_binary_const(op, lhs, rhs, left, right, result_type)
                {
                    return Some(folded);
                }
                Some(self.pool.binary(op, lhs, rhs, result_type))
            }
            ExprKind::Unary { op, expr: inner } => {
                // `Ref` / `MutRef` / `Deref` are address-taking / heap-bearing
                // operations — not pure values. Walk the child (so pure
                // subtrees still land in `value_of`) but do not assign an id
                // to this expr.
                if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref) {
                    self.walk_operand(inner);
                    None
                } else {
                    let operand = self.walk_operand(inner)?;
                    let result_type = self.body.exprs[expr].type_id;
                    if let Some(folded) = self.fold_unary_const(op, operand, inner, result_type) {
                        return Some(folded);
                    }
                    Some(self.pool.unary(op, operand, result_type))
                }
            }
            ExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                let operand = self.walk_operand(inner)?;
                Some(self.pool.cast(operand, target_type))
            }

            // ---- Mutation: side-effect, never pure ----
            ExprKind::Assign { target, value } => {
                // Walk target operands FIRST: runtime evaluates the place
                // (receiver / index / deref operand) before the stored
                // value, so a write inside `value` must not be visible to
                // those reads.
                let target_kind = self.body.exprs[target].kind.clone();
                // Capture the field place's root local, receiver `ValueId`,
                // and whether it is a bare `Local` — so the post-bump version
                // can be seeded for store→load forwarding.
                let field_place = match &target_kind {
                    ExprKind::Local { .. } => None,
                    // An assign target is an lvalue, so its receiver is a place
                    // (never promoted); `Some` here matches the consumer's `expect`.
                    ExprKind::FieldAccess { expr: recv, .. } => {
                        let recv_e = recv
                            .as_expr()
                            .expect("assign-target field receiver is a place");
                        let bare_local =
                            matches!(&self.body.exprs[recv_e].kind, ExprKind::Local { .. });
                        let root = self.receiver_root(recv_e);
                        let recv_v = self.walk_operand(*recv);
                        Some((root, recv_v, bare_local))
                    }
                    _ => {
                        self.walk_expr(target);
                        None
                    }
                };
                let v = self
                    .walk_operand(value)
                    .unwrap_or_else(|| self.pool.fresh_opaque());
                match target_kind {
                    ExprKind::Local { index, .. } => {
                        self.set_local_value(index, v);
                        // `local = S { f: lit, … }` rebinds `local` to a fresh
                        // object; seed each pure field like the `Let` case so a
                        // later `local.f` read forwards the literal.
                        if let Some(ve) = value.as_expr() {
                            self.seed_struct_literal_fields(index, v, ve);
                            self.update_ref_target(index, ve);
                        }
                    }
                    ExprKind::FieldAccess { field_index, .. } => {
                        let (root, recv_v, bare_local) = field_place.expect("field target");
                        // A non-aliased bare-`Local` root takes the precise
                        // per-slot bump — a write to `a.f` leaves every other
                        // object's `f` untouched. An aliased root, or a deeper
                        // place (`a.b.f`, whose pointee may be shared), bumps
                        // the field globally so every alias of `f` is
                        // invalidated.
                        match root {
                            Some(r) if bare_local && !self.aliased.contains(&r) => {
                                self.heap_state.bump_slot(r, field_index);
                            }
                            _ => self.heap_state.bump_field_global(field_index),
                        }
                        // Seed forwarding for a bare-`Local`, non-`untrackable`
                        // receiver: a later `recv.f` read at the same version
                        // forwards `v`.
                        if let (Some(rv), Some(r)) = (recv_v, root)
                            && bare_local
                            && !self.untrackable.contains(&r)
                        {
                            let ver = self.heap_state.version_of(Some(r), field_index);
                            self.field_store.insert((rv, field_index, ver), v);
                        }
                    }
                    _ => {
                        self.heap_state.bump_all();
                    }
                }
                None
            }
            ExprKind::GlobalVarSet { value, .. } => {
                self.walk_operand(value);
                // Globals share the heap from the optimizer's perspective.
                self.heap_state.bump_all();
                None
            }

            // ---- Control-flow expressions ----
            ExprKind::Block(block) => {
                // A single-expression block `{ e }` (e.g. an inlined getter
                // `{ x.used }`) forwards `e`'s value, so a later read unifies
                // with the same access written directly. Multi-statement blocks
                // stay opaque: forwarding the tail would let CSE drop the side
                // effects of the leading statements (e.g. `{ cold_path(); e }`).
                let tail = if let [only] = self.body.blocks[block].stmts[..]
                    && let StmtKind::Expr(Operand::Expr(e)) = &self.body.stmts[only].kind
                {
                    Some(*e)
                } else {
                    None
                };
                if let Some(e) = tail {
                    self.walk_expr(e)
                } else {
                    self.walk_block(block);
                    None
                }
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond_v = self.walk_operand(condition);
                let pre = self.flow_snapshot();
                self.walk_block(then_branch);
                let then_arm = FlowArm {
                    falls_through: self.block_falls_through(then_branch),
                    state: self.flow_snapshot(),
                };
                self.flow_restore(&pre);
                let else_arm = if let Some(eb) = else_branch {
                    self.walk_block(eb);
                    let arm = FlowArm {
                        falls_through: self.block_falls_through(eb),
                        state: self.flow_snapshot(),
                    };
                    self.flow_restore(&pre);
                    arm
                } else {
                    FlowArm {
                        state: pre.clone(),
                        falls_through: true,
                    }
                };
                self.flow_join_two(cond_v, &pre, then_arm, else_arm);
                None
            }
            ExprKind::LabeledBlock { block, .. } => {
                // Same break-only-write hazard as the `StmtKind::LabeledBlock` arm.
                self.walk_block(block);
                self.dirty_all_writes_in_block(block);
                self.heap_state.bump_all();
                None
            }
            ExprKind::Match { expr: scrut, arms } => {
                self.walk_operand(scrut);
                self.walk_match_arms(&arms);
                None
            }
            ExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.walk_operand(scrutinee);
                let pre = self.flow_snapshot();
                let mut flow_arms: Vec<FlowArm> = Vec::with_capacity(arms.len() + 1);
                for arm in arms.iter().copied().chain(std::iter::once(default)) {
                    self.flow_restore(&pre);
                    self.walk_block(arm);
                    flow_arms.push(FlowArm {
                        falls_through: self.block_falls_through(arm),
                        state: self.flow_snapshot(),
                    });
                }
                self.flow_join_n(&pre, flow_arms);
                None
            }

            // ---- Heap-bearing reads ----
            ExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } => {
                // The receiver must be a pure value for the FieldAccess to
                // get a ValueId — an impure receiver (a Call result, for
                // instance) propagates None.
                let walked = self.walk_operand(inner)?;
                // Reference look-through: a read `r.f` where `r = &v` forwards
                // from `v`'s field slot (the pointee's current VN / root),
                // using `v`'s live slot state so a stale forward is impossible.
                // A promoted `Operand::Value` receiver has no skeleton place: no
                // reference target and no receiver root local.
                let inner_e = inner.as_expr();
                let (recv, root) = match inner_e.and_then(|ie| self.reference_lookthrough(ie)) {
                    Some((pointee_vn, pointee)) => (pointee_vn, Some(pointee)),
                    None => (walked, inner_e.and_then(|ie| self.receiver_root(ie))),
                };
                let heap_ver = self.heap_state.version_of(root, field_index);
                // Store→load forwarding: a value stored to this exact
                // `(receiver, field, version)` is the value this read sees.
                if let Some(&stored) = self.field_store.get(&(recv, field_index, heap_ver)) {
                    return Some(stored);
                }
                Some(self.pool.field_access(recv, field_index, heap_ver))
            }
            ExprKind::Index { expr: inner, index } => {
                self.walk_operand(inner);
                self.walk_operand(index);
                None
            }
            ExprKind::VariantTag { expr: inner }
            | ExprKind::VariantTest { expr: inner, .. }
            | ExprKind::VariantPayload { expr: inner, .. } => {
                self.walk_operand(inner);
                None
            }

            // ---- Allocation-bearing constructors (Skel-side per Q1) ----
            ExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    self.walk_operand(f.value);
                }
                None
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for e in elements {
                    self.walk_operand(e);
                }
                None
            }
            ExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload {
                    self.walk_operand(p);
                }
                None
            }
            // An enum case is a compile-time scalar constant: its discriminant.
            // Interning it as the integer it lowers to (`WirInstr::I32Const`)
            // makes every constant-propagating rule — store→load forwarding,
            // copy propagation, CSE — see through a `let k = Enum::Case`.
            ExprKind::EnumConstruct {
                enum_type,
                case_index,
                ..
            } => Some(self.pool.int_typed(u64::from(case_index), enum_type)),
            ExprKind::ClosureToCanonical { functor, .. } => {
                self.walk_operand(functor);
                None
            }

            // ---- Calls (effectful, may write the heap) ----
            ExprKind::Call { args, .. } => {
                for a in args {
                    self.walk_operand(a.expr);
                }
                self.bump_call_effects(expr);
                None
            }
            ExprKind::CmRawCall { args, .. } => {
                for a in args {
                    self.walk_operand(a);
                }
                // Raw CM calls have opaque captures; stay fully conservative.
                self.heap_state.bump_all();
                None
            }
            ExprKind::IndirectCall { callee, args } => {
                self.walk_operand(callee);
                for a in args {
                    self.walk_operand(a);
                }
                self.heap_state.bump_all();
                None
            }

            // ---- Other Skel-side leaves ----
            // The backing bytes of a `String` / `List<u8>` literal are a
            // constant the pool can name, so a literal keeps its identity
            // through a binding instead of going opaque at the first `let`.
            // Bounded by `MAX_SEQ_ELEMENTS`: past it the walk would cost more
            // than any fold it enables, and `Value::seq` declines.
            ExprKind::PackedArray(bytes) => {
                let elements = bytes
                    .iter()
                    .map(|b| crate::const_eval::Value::Int {
                        value: u64::from(*b),
                        prim: crate::tir::PrimitiveType::U8,
                    })
                    .collect();
                let ty = self.body.exprs[expr].type_id;
                crate::const_eval::Value::seq(ty, elements).map(|seq| self.pool.constant(&seq, ty))
            }
            // Name the read, do not evaluate it: what the global holds is the
            // evaluator's question. Naming alone is what makes `let s = G`
            // transparent and what lets two reads of one global share an id.
            ExprKind::GlobalVarGet {
                module_source,
                name,
            } => {
                let slot = self.pool.global_slot(&module_source, &name);
                let ver = self.heap_state.global_version();
                let id = self.pool.global_read(slot, ver);
                self.pool.set_type(id, self.body.exprs[expr].type_id);
                Some(id)
            }
        }
    }

    /// Seed the field-store map from a `let x = S { f: v, … }` binding so a
    /// later `x.f` read forwards `v`. Wrappers are peered through to the
    /// sole producing tail: an unlabeled `Block`'s trailing expression, or a
    /// `LabeledBlock` whose only `break label:` is the trailing
    /// value-carrying statement (any other break to the label means
    /// multiple producers — stop, soundness over coverage).
    fn seed_struct_literal_fields(&mut self, root: u32, recv: ValueId, value_expr: ExprId) {
        // `untrackable` (`stores`-aliased) receivers never seed.
        if self.untrackable.contains(&root) {
            return;
        }
        let mut producer = value_expr;
        loop {
            match &self.body.exprs[producer].kind {
                ExprKind::StructLiteral { .. } => break,
                // `let r = &S` / `&mut S` where `S` is an inline aggregate (e.g.
                // the inlined `len(&self)` binds `self = &String { … }`). Peel
                // the borrow and seed `r`'s fields from `S` directly, so a later
                // `r.field` read forwards — the pointee has no own local to track
                // via `ref_targets`, so seeding onto `r` is the only handle.
                ExprKind::Unary {
                    op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                    expr: inner,
                } => match inner.as_expr() {
                    Some(e) => producer = e,
                    None => return,
                },
                // The producing tail is a bare `Local` (e.g. inlining
                // `fn f(mut p: S) -> S { p = S { … }; return p }` makes the
                // break value the reassigned local `p`, not the literal). Copy
                // that local's live field slots to the new binding — value
                // semantics deep-copy the struct, so the binding observes the
                // same field constants.
                ExprKind::Local { index, .. } => {
                    let src = *index;
                    self.copy_local_field_slots(src, root, recv);
                    return;
                }
                ExprKind::Block(b) => {
                    let Some(&last) = self.body.blocks[*b].stmts.last() else {
                        return;
                    };
                    let StmtKind::Expr(Operand::Expr(tail)) = &self.body.stmts[last].kind else {
                        return;
                    };
                    producer = *tail;
                }
                ExprKind::LabeledBlock { block, label, .. } => {
                    let (block, label) = (*block, label.clone());
                    let stmts = &self.body.blocks[block].stmts;
                    let Some(&last) = stmts.last() else {
                        return;
                    };
                    let StmtKind::Break {
                        label: Some(brk),
                        value: Some(value),
                    } = &self.body.stmts[last].kind
                    else {
                        return;
                    };
                    if *brk != label {
                        return;
                    }
                    let value = *value;
                    // The trailing break must be the sole producer: no other
                    // break to this label anywhere else in the block or in
                    // the carried value.
                    let earlier_break = stmts[..stmts.len() - 1]
                        .iter()
                        .any(|s| block_breaks_to_node(self.body, NodeRef::Stmt(*s), &label));
                    if earlier_break
                        || value.as_expr().is_some_and(|ve| {
                            block_breaks_to_node(self.body, NodeRef::Expr(ve), &label)
                        })
                    {
                        return;
                    }
                    let Some(pe) = value.as_expr() else {
                        return;
                    };
                    producer = pe;
                }
                _ => return,
            }
        }
        let ExprKind::StructLiteral { fields, .. } = &self.body.exprs[producer].kind else {
            return;
        };
        // Clone out the (field_index, value-expr) pairs to release the body
        // borrow before mutating `field_store`.
        let pairs: Vec<(u32, Operand)> = fields.iter().map(|f| (f.field_index, f.value)).collect();
        for (field_index, field_value) in pairs {
            // A promoted constant field is its own `ValueId`; a skeleton field is
            // resolved through `value_of`.
            let fv = match field_value {
                Operand::Value(v) => Some(v),
                Operand::Expr(e) => self.value_of.get(&e).copied(),
            };
            if let Some(fv) = fv {
                let ver = self.heap_state.version_of(Some(root), field_index);
                self.field_store.insert((recv, field_index, ver), fv);
            }
        }
    }

    /// Copy `src`'s currently-live field slots onto the new binding
    /// `(dst_root, dst_recv)` — for `let dst = …` whose producing tail is a
    /// bare `Local src` (the inlined `mut`-param-return shape), where value
    /// semantics make `dst` observe `src`'s field constants. Keyed by the
    /// destination's own version, so a later write bumps the field version and
    /// the stale copy becomes unreachable (the seeding monotonicity argument).
    /// `untrackable` (`stores`-aliased) locals, which can't be versioned, are
    /// excluded.
    fn copy_local_field_slots(&mut self, src: u32, dst_root: u32, dst_recv: ValueId) {
        if src == dst_root
            || self.untrackable.contains(&src)
            || self.untrackable.contains(&dst_root)
        {
            return;
        }
        // The scan below is O(field_store); short-circuit the common case of a
        // function that seeded no struct-literal fields at all.
        if self.field_store.is_empty() {
            return;
        }
        let Some(&src_recv) = self.current_value.get(&src) else {
            return;
        };
        // Collect the live (field, value) pairs first to release the borrow on
        // `field_store` before inserting.
        let live: Vec<(u32, ValueId)> = self
            .field_store
            .iter()
            .filter_map(|(&(recv, field, ver), &stored)| {
                (recv == src_recv && ver == self.heap_state.version_of(Some(src), field))
                    .then_some((field, stored))
            })
            .collect();
        for (field, stored) in live {
            let dst_ver = self.heap_state.version_of(Some(dst_root), field);
            self.field_store.insert((dst_recv, field, dst_ver), stored);
        }
    }

    fn read_local(&mut self, idx: u32) -> ValueId {
        if let Some(&v) = self.current_value.get(&idx) {
            self.fresh_invalidations.swap_remove(&idx);
            v
        } else {
            // Unbound locals shouldn't occur on well-typed NIR; cache a
            // fresh Opaque so subsequent reads agree.
            let v = self.pool.fresh_opaque_with_source(OpaqueSource::Local(idx));
            self.set_local_value(idx, v);
            v
        }
    }

    fn bind_pattern_opaque(&mut self, pat: PatId) {
        match self.body.pats[pat].kind.clone() {
            PatKind::Binding { local_index, .. } => {
                let v = self
                    .pool
                    .fresh_opaque_with_source(OpaqueSource::Local(local_index));
                self.set_local_value(local_index, v);
            }
            PatKind::Tuple(children, _) => {
                for c in children {
                    self.bind_pattern_opaque(c);
                }
            }
            PatKind::Or(children) => {
                for c in children {
                    self.bind_pattern_opaque(c);
                }
            }
            PatKind::Variant { bindings, .. } => {
                for c in bindings {
                    self.bind_pattern_opaque(c);
                }
            }
            PatKind::Struct { fields, .. } => {
                for f in fields {
                    self.bind_pattern_opaque(f.pattern);
                }
            }
            PatKind::ConstantValue { expr } => {
                self.walk_operand(expr);
            }
            PatKind::Wildcard
            | PatKind::Literal(_)
            | PatKind::Enum { .. }
            | PatKind::Range { .. } => {}
        }
    }

    /// Capture all flow-sensitive state (`current_value`, `heap_state`,
    /// `ref_targets`) at the current program point as one [`FlowSnapshot`].
    fn flow_snapshot(&mut self) -> FlowSnapshot {
        self.fresh_invalidations.clear();
        FlowSnapshot {
            current_value: self.current_value.clone(),
            heap: self.heap_state.snapshot(),
            ref_targets: self.ref_targets.clone(),
        }
    }

    /// Reset all flow-sensitive state to a previously captured snapshot. Used
    /// between branch arms so each arm walks from the common pre-branch state.
    fn flow_restore(&mut self, snap: &FlowSnapshot) {
        self.fresh_invalidations.clear();
        self.current_value.clone_from(&snap.current_value);
        self.heap_state.restore(snap.heap.clone());
        self.ref_targets.clone_from(&snap.ref_targets);
    }

    fn set_local_value(&mut self, idx: u32, v: ValueId) {
        self.fresh_invalidations.swap_remove(&idx);
        self.current_value.insert(idx, v);
    }

    fn invalidate_local_with_source(&mut self, idx: u32, source: Option<OpaqueSource>) {
        if self.current_value.contains_key(&idx) && self.fresh_invalidations.insert(idx) {
            let opaque = match source {
                Some(s) => self.pool.fresh_opaque_with_source(s),
                None => self.pool.fresh_opaque(),
            };
            self.current_value.insert(idx, opaque);
        }
    }

    fn invalidate_local(&mut self, idx: u32) {
        self.invalidate_local_with_source(idx, None);
    }

    /// Join an if-style two-arm endpoint over all three flow components at
    /// once: values via [`Builder::merge_two_arms`] (Select-aware), heap via
    /// [`Builder::join_heap`], references via [`Builder::merge_ref_targets`].
    fn flow_join_two(
        &mut self,
        cond_v: Option<ValueId>,
        pre: &FlowSnapshot,
        then_arm: FlowArm,
        else_arm: FlowArm,
    ) {
        // Start from the pre-branch base so arm-local bindings (keys absent
        // from `pre`) do not leak past the merge; `merge_two_arms` then
        // overwrites every pre-branch key with its joined value.
        self.current_value.clone_from(&pre.current_value);
        self.merge_two_arms(
            cond_v,
            &pre.current_value,
            &then_arm.state.current_value,
            &else_arm.state.current_value,
        );
        self.merge_ref_targets(&[then_arm.state.ref_targets, else_arm.state.ref_targets]);
        self.join_heap(
            &pre.heap,
            &[
                (then_arm.state.heap, then_arm.falls_through),
                (else_arm.state.heap, else_arm.falls_through),
            ],
        );
    }

    /// Join an n-arm endpoint (Switch / Match) over all three flow components
    /// at once. Consumes the arms to split them into the per-component vectors
    /// the underlying joins expect without extra clones.
    fn flow_join_n(&mut self, pre: &FlowSnapshot, arms: Vec<FlowArm>) {
        let mut states: Vec<IndexMap<u32, ValueId>> = Vec::with_capacity(arms.len());
        let mut heaps: Vec<(HeapSnapshot, bool)> = Vec::with_capacity(arms.len());
        let mut refs: Vec<IndexMap<u32, u32>> = Vec::with_capacity(arms.len());
        for a in arms {
            states.push(a.state.current_value);
            heaps.push((a.state.heap, a.falls_through));
            refs.push(a.state.ref_targets);
        }
        // See `flow_join_two`: reset to the pre-branch base before merging.
        self.current_value.clone_from(&pre.current_value);
        self.merge_n_arms(&pre.current_value, &states);
        self.merge_ref_targets(&refs);
        self.join_heap(&pre.heap, &heaps);
    }

    /// Merge an if-style two-arm structural endpoint. For each local that
    /// existed before the branch:
    /// - Both arms agree → keep that value.
    /// - Arms differ AND `cond_v` is known → hash-cons a `Select`.
    /// - Otherwise → fresh `Opaque` (the merged value is unknown).
    ///
    /// New bindings introduced inside a single arm are dropped from the
    /// post-merge state — they are scoped to the arm in well-formed NIR.
    fn merge_two_arms(
        &mut self,
        cond_v: Option<ValueId>,
        saved: &IndexMap<u32, ValueId>,
        then_state: &IndexMap<u32, ValueId>,
        else_state: &IndexMap<u32, ValueId>,
    ) {
        for (&idx, &saved_v) in saved {
            let then_v = then_state.get(&idx).copied().unwrap_or(saved_v);
            let else_v = else_state.get(&idx).copied().unwrap_or(saved_v);
            let merged = if then_v == else_v {
                then_v
            } else if let Some(cond) = cond_v {
                self.pool.select(cond, then_v, else_v)
            } else {
                self.pool.fresh_opaque()
            };
            self.set_local_value(idx, merged);
        }
    }

    /// Merge n arms (Match / Switch). For each pre-branch local: if every
    /// arm agrees, keep that value; otherwise fall back to `Opaque`. N-ary
    /// `Select` chains are not yet constructed.
    fn merge_n_arms(
        &mut self,
        saved: &IndexMap<u32, ValueId>,
        arm_states: &[IndexMap<u32, ValueId>],
    ) {
        for (&idx, &saved_v) in saved {
            let mut consensus: Option<ValueId> = None;
            let mut diverged = false;
            for arm in arm_states {
                let arm_v = arm.get(&idx).copied().unwrap_or(saved_v);
                match consensus {
                    None => consensus = Some(arm_v),
                    Some(c) if c == arm_v => {}
                    Some(_) => {
                        diverged = true;
                        break;
                    }
                }
            }
            let merged = if diverged {
                self.pool.fresh_opaque()
            } else {
                consensus.unwrap_or(saved_v)
            };
            self.set_local_value(idx, merged);
        }
    }

    /// Join `ref_targets` across branch arms walked from a common pre-state:
    /// a `r → v` look-through survives only if every arm ends with that exact
    /// mapping. A reference reassigned (or cleared) in some arm becomes unknown
    /// and is dropped, so a post-branch `r.f` re-derives rather than forwarding
    /// a stale pointee. This is the `ref_targets` counterpart to
    /// [`Builder::merge_n_arms`]: a diverging reference is dropped exactly as a
    /// diverging value falls to `Opaque`. Each `arm_refs` entry is the arm's
    /// post-walk map, so an entry untouched by every arm (inherited from the
    /// shared pre-state) is present in all and survives.
    fn merge_ref_targets(&mut self, arm_refs: &[IndexMap<u32, u32>]) {
        let Some((first, rest)) = arm_refs.split_first() else {
            return;
        };
        let mut merged: IndexMap<u32, u32> = IndexMap::default();
        for (&r, &v) in first {
            if rest.iter().all(|a| a.get(&r) == Some(&v)) {
                merged.insert(r, v);
            }
        }
        self.ref_targets = merged;
    }

    /// Drop `ref_targets` entries a loop / labeled-block body may invalidate by
    /// reassigning `writes`: a reference among `writes` loses its known target,
    /// and a reference whose pointee is among `writes` may point at a moved
    /// object. The `ref_targets` counterpart to those constructs' `Opaque`
    /// reassignment of `current_value` for the same locals.
    fn drop_ref_targets_for(&mut self, writes: &crate::hashmap::IndexSet<u32>) {
        self.ref_targets
            .retain(|src, pointee| !writes.contains(src) && !writes.contains(pointee));
    }

    /// Join the heap state at a branch endpoint: each overlay generation
    /// keeps its pre-branch version iff every fall-through arm left it
    /// unchanged; otherwise it bumps fresh. Non-fall-through arms (terminated
    /// by `break` / `return` / `continue`) are excluded — their writes never
    /// reach code after the branch. No fall-through arm ⇒ post-state is `pre`.
    ///
    /// Each of `per_slot` / `per_local` / `field_global` joins independently
    /// (an absent overlay entry contributes the snapshot's `default_version`
    /// in [`HeapState::version_of`], so the per-overlay join mirrors the
    /// single-map case sharing the joined default). `version_of` maxes the
    /// overlays, so a join that is too coarse only raises a read's version —
    /// never lowers it below an arm's — keeping the join sound.
    fn join_heap(&mut self, pre: &HeapSnapshot, arms: &[(HeapSnapshot, bool)]) {
        let live: Vec<&HeapSnapshot> = arms
            .iter()
            .filter_map(|(h, falls)| falls.then_some(h))
            .collect();
        if live.is_empty() {
            self.heap_state.restore(pre.clone());
            return;
        }
        let default_changed = live
            .iter()
            .any(|a| a.default_version != pre.default_version);
        let new_default = if default_changed {
            self.heap_state.fresh()
        } else {
            pre.default_version
        };
        let new_per_slot = self.join_overlay(
            new_default,
            &pre.per_slot,
            pre.default_version,
            live.iter().map(|a| (&a.per_slot, a.default_version)),
        );
        let new_per_local = self.join_overlay(
            new_default,
            &pre.per_local,
            pre.default_version,
            live.iter().map(|a| (&a.per_local, a.default_version)),
        );
        let new_field_global = self.join_overlay(
            new_default,
            &pre.field_global,
            pre.default_version,
            live.iter().map(|a| (&a.field_global, a.default_version)),
        );
        // A global read survives the join only if no arm touched a global; the
        // versions carry no overlay, so the test is direct.
        let globals_changed = live.iter().any(|a| a.global_version != pre.global_version);
        self.heap_state.per_slot = new_per_slot;
        self.heap_state.per_local = new_per_local;
        self.heap_state.field_global = new_field_global;
        self.heap_state.default_version = new_default;
        if globals_changed {
            self.heap_state.bump_globals();
        } else {
            self.heap_state.global_version = pre.global_version;
        }
    }

    /// Join one overlay map across the live arms. A key keeps its pre version
    /// iff every arm's effective version (its own entry, else that arm's
    /// `default_version`) equals the pre effective version; otherwise it
    /// bumps fresh. Survivors equal to `new_default` are dropped (the default
    /// already covers them); other survivors are pinned so a raised default
    /// does not swallow them.
    fn join_overlay<'s, K>(
        &mut self,
        new_default: HeapVersion,
        pre_map: &IndexMap<K, HeapVersion>,
        pre_default: HeapVersion,
        arms: impl Iterator<Item = (&'s IndexMap<K, HeapVersion>, HeapVersion)> + Clone,
    ) -> IndexMap<K, HeapVersion>
    where
        K: Copy + Eq + std::hash::Hash + 's,
    {
        let mut keys: crate::hashmap::IndexSet<K> = crate::hashmap::IndexSet::default();
        for k in pre_map.keys() {
            keys.insert(*k);
        }
        for (m, _) in arms.clone() {
            for k in m.keys() {
                keys.insert(*k);
            }
        }
        let mut out: IndexMap<K, HeapVersion> = IndexMap::default();
        for k in keys {
            let pre_v = pre_map.get(&k).copied().unwrap_or(pre_default);
            let unchanged = arms
                .clone()
                .all(|(m, d)| m.get(&k).copied().unwrap_or(d) == pre_v);
            if unchanged {
                if pre_v != new_default {
                    out.insert(k, pre_v);
                }
            } else {
                let fresh = self.heap_state.fresh();
                out.insert(k, fresh);
            }
        }
        out
    }

    /// Whether control can reach the bottom of `block`. Mirrors
    /// `const_folding`'s `block_falls_through` minus never-type detection
    /// (the builder has no `TypeTable`): a `panic()`-terminated arm is
    /// conservatively treated as falling through, which only costs
    /// precision — it modifies no field, so including it in a heap join
    /// keeps the pre-branch versions anyway.
    fn block_falls_through(&self, block: crate::nir_arena::BlockId) -> bool {
        match self.body.blocks[block].stmts.last() {
            None => true,
            Some(&last) => self.stmt_falls_through(last),
        }
    }

    fn stmt_falls_through(&self, s: StmtId) -> bool {
        match &self.body.stmts[s].kind {
            StmtKind::Return { .. } | StmtKind::Break { .. } | StmtKind::Continue => false,
            StmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                self.block_falls_through(*then_block)
                    || else_block.is_none_or(|eb| self.block_falls_through(eb))
            }
            // A break targeting the block's OWN label resumes right after
            // it — that IS fall-through, and the tail-only walk cannot see
            // early self-breaks, so scan the subtree too. Over-approximating
            // the break's reachability only adds an arm to the join (sound).
            StmtKind::LabeledBlock { block, label } => {
                self.block_falls_through(*block) || block_breaks_to(self.body, *block, label)
            }
            // A `Loop` falls through only via `break`, which we do not
            // analyse here; treat it as falling through.
            StmtKind::Loop { .. } => true,
            _ => true,
        }
    }

    fn walk_match_arms(&mut self, arms: &[ArmData]) {
        // Guards are evaluated sequentially at runtime: when guard 0 has
        // side effects and returns false, guard 1 and arm 1's body run
        // with those effects visible. Restoring the heap state and
        // outer locals to the pre-match snapshot between arms would
        // hide those effects from later arms. To stay sound without
        // threading the post-guard state forward, conservatively dirty
        // every outer local any guard could write and bump the heap
        // once before walking arms.
        let mut guard_writes: crate::hashmap::IndexSet<u32> = crate::hashmap::IndexSet::default();
        let mut any_guard = false;
        for arm in arms {
            if let Some(g) = arm.guard {
                any_guard = true;
                // A promoted constant guard (`Operand::Value`) writes nothing.
                if let Some(ge) = g.as_expr() {
                    collect_writes_in_expr(
                        self.body,
                        ge,
                        &mut guard_writes,
                        &mut self.block_writes,
                    );
                }
            }
        }
        for &idx in &guard_writes {
            self.invalidate_local(idx);
        }
        self.drop_ref_targets_for(&guard_writes);
        if any_guard {
            self.heap_state.bump_all();
        }

        let pre = self.flow_snapshot();
        let mut flow_arms: Vec<FlowArm> = Vec::with_capacity(arms.len());
        for arm in arms {
            self.flow_restore(&pre);
            self.bind_pattern_opaque(arm.pattern);
            if let Some(g) = arm.guard {
                self.walk_operand(g);
            }
            self.walk_operand(arm.body);
            // Match arm bodies are expressions; without a `TypeTable` the
            // builder cannot detect a never-typed (`=> return …`) body, so
            // every arm is conservatively treated as falling through. A
            // returning arm contributes only its field writes to the join,
            // which is sound (those fields bump) if imprecise.
            flow_arms.push(FlowArm {
                state: self.flow_snapshot(),
                falls_through: true,
            });
        }
        self.flow_join_n(&pre, flow_arms);
    }

    /// Reassign every local the body may write to a fresh `Opaque`, and
    /// invalidate the heap fields the body may write, before and after the
    /// walk. The body may run 0..N times, so in-body reads must not share
    /// `ValueId`s with pre-loop reads, and post-loop reads must not share them
    /// with in-body reads. (Locals declared inside the loop need no pre-seed:
    /// they get fresh `Opaque`s as the body walks.)
    ///
    /// Heap invalidation is selective ([`collect_loop_heap_effects`]): a field
    /// the body never writes — directly, through a reference, or via a
    /// non-builtin call — keeps its pre-loop version, so a `table.used = 256`
    /// before a builtin-only loop still forwards inside and after it.
    fn walk_loop(&mut self, body_block: crate::nir_arena::BlockId) {
        let writes = writes_of_block(self.body, body_block, &mut self.block_writes);
        let heap_effects =
            collect_loop_heap_effects(self.body, &self.pure_builtin_callees, body_block);
        // Snapshot before the reassigned-local opaques below overwrite the
        // written locals' pre-loop values.
        self.fresh_invalidations.clear();
        self.loop_entry_values
            .insert(body_block, self.current_value.clone());
        // Born-as-operands: give each written local a `LoopPhi { entry, body_iter }`
        // at the loop head instead of a fresh `Opaque`. `entry` is the snapshotted
        // pre-loop value (the phi's first-iteration meaning); the body walk then
        // resolves reads *before* the in-loop write to the phi and reads *after* to
        // the post-write value (phase split handled by ordinary propagation), and
        // `body_iter` is patched to the post-body value below. A local whose entry
        // value has no recorded type falls back to a fresh opaque.
        let mut phis: Vec<(u32, ValueId)> = Vec::new();
        for &idx in writes.iter() {
            let Some(&entry) = self.current_value.get(&idx) else {
                continue;
            };
            let phi = match self.pool.type_of(entry) {
                Some(ty) => {
                    let phi = self.pool.alloc_loop_phi(entry, ty);
                    phis.push((idx, phi));
                    phi
                }
                None => self.pool.fresh_opaque(),
            };
            self.set_local_value(idx, phi);
        }
        self.drop_ref_targets_for(&writes);
        self.apply_loop_heap_effects(&heap_effects);
        self.walk_block(body_block);
        // Patch each phi's `body_iter` to the body's exit value (which may
        // reference the phi — a sound self-reference; traversals are guarded). The
        // in-loop reads already took the phi (their stable identity — what
        // condition_implication needs); post-loop, re-opaque every written local as
        // the original did. Using the phi as the post-loop value is more precise
        // but miscompiles nested LR loop-entry branches (package-gale
        // `DropLoopEntryBranchInLRRule_3`), so keep the conservative post-loop value.
        for (idx, phi) in &phis {
            if let Some(&body_val) = self.current_value.get(idx) {
                self.pool.set_loop_phi_body_iter(*phi, body_val);
            }
        }
        for &idx in writes.iter() {
            self.invalidate_local(idx);
        }
        self.drop_ref_targets_for(&writes);
        self.apply_loop_heap_effects(&heap_effects);
    }

    /// Invalidate the heap generations a loop body may write (see
    /// [`collect_loop_heap_effects`]). A written `local.field` bumps that
    /// local's `per_slot` (or `field_global` when the local is aliased); a
    /// `&mut`/method/mut-arg borrow bumps the local's `per_local`; an external
    /// write (non-builtin call, indirect call, opaque store) invalidates every
    /// `mut_escaped` local's fields. Non-touched fields survive — and an
    /// *immutably*-`&`-escaped local (`&config` passed to `fn process(&Config)`)
    /// is **not** `mut_escaped`, so its fields survive the call, matching the
    /// per-call [`Builder::bump_call_effects`] (which the loop path must agree
    /// with — using the wider `aliased` here lost the forward for an immutable
    /// reference field read across a loop call: `opt_licm_immut_ref`).
    fn apply_loop_heap_effects(&mut self, eff: &LoopHeapEffects) {
        for &(local, field) in &eff.written_fields {
            if self.aliased.contains(&local) {
                self.heap_state.bump_field_global(field);
            } else {
                self.heap_state.bump_slot(local, field);
            }
        }
        for &local in &eff.mut_borrowed {
            self.heap_state.bump_local(local);
        }
        if eff.has_external_writes {
            for &local in &self.mut_escaped {
                self.heap_state.bump_local(local);
            }
        }
    }

    /// After a flow-opaque construct (`LabeledBlock` with potential breaks),
    /// every local written anywhere in `block`'s subtree becomes Opaque —
    /// including locals written on a `break`-only path that fall-through
    /// never sees. Locals not written in the subtree keep their pre-block
    /// value.
    fn dirty_all_writes_in_block(&mut self, block: crate::nir_arena::BlockId) {
        let writes = writes_of_block(self.body, block, &mut self.block_writes);
        for &idx in writes.iter() {
            self.invalidate_local(idx);
        }
        self.drop_ref_targets_for(&writes);
    }
}

fn writes_of_block(
    body: &Body,
    block: crate::nir_arena::BlockId,
    cache: &mut IndexMap<BlockId, std::rc::Rc<crate::hashmap::IndexSet<u32>>>,
) -> std::rc::Rc<crate::hashmap::IndexSet<u32>> {
    if let Some(ws) = cache.get(&block) {
        return std::rc::Rc::clone(ws);
    }
    let mut out = crate::hashmap::IndexSet::default();
    let stmts = body.blocks[block].stmts.clone();
    for s in stmts {
        collect_writes_in_stmt(body, s, &mut out, cache);
    }
    let rc = std::rc::Rc::new(out);
    cache.insert(block, std::rc::Rc::clone(&rc));
    rc
}

/// Whether `block`'s subtree contains a `break` targeting `label`. Used by
/// [`Builder::stmt_falls_through`] to classify a labeled block whose body
/// exits via a break to its own label as falling through.
fn block_breaks_to(body: &Body, block: BlockId, label: &str) -> bool {
    block_breaks_to_node(body, NodeRef::Block(block), label)
}

fn block_breaks_to_node(body: &Body, node: NodeRef, label: &str) -> bool {
    if let NodeRef::Stmt(s) = node
        && let StmtKind::Break {
            label: Some(brk), ..
        } = &body.stmts[s].kind
        && brk == label
    {
        return true;
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    kids.into_iter()
        .any(|c| block_breaks_to_node(body, c, label))
}

/// A loop body's heap-write effects, used to invalidate exactly the fields a
/// loop may mutate (mirrors `const_folding`'s `LoopWriteEffects`). Reassigned
/// locals are handled separately by `walk_loop`'s `Opaque` reassignment.
#[derive(Default)]
struct LoopHeapEffects {
    /// `local.field = …` (bare-`Local` receiver) targets.
    written_fields: crate::hashmap::IndexSet<(u32, u32)>,
    /// `&mut local`, `&mut local.field`, a `&mut` call arg, or a method
    /// receiver — the callee may store through the reference.
    mut_borrowed: crate::hashmap::IndexSet<u32>,
    /// A non-builtin call, indirect / CM call, or opaque-target store
    /// (`(*p).f`, `arr[i]`, deep field) that may mutate aliased state from
    /// outside the straight-line walk.
    has_external_writes: bool,
}

/// True when `func_id` is a builtin / monomorphized-builtin intrinsic that
/// operates below the struct-field layer (`array_set`, `memory_grow`, …) and
/// so never mutates a tracked `(root, field)` slot. Classified by the callee's
/// `func_id` (the call node carries no `FunctionRef`); an empty set is
/// conservative (treat every call as an external write).
fn is_builtin_pure_call(
    pure_builtin_callees: &crate::hashmap::IndexSet<FuncId>,
    func_id: FuncId,
) -> bool {
    pure_builtin_callees.contains(&func_id)
}

/// Walk down a `local.f.g.…` field chain to its rooted local index, or `None`
/// if rooted at a non-`Local` (e.g. `(*p).f`).
fn root_local_of(body: &Body, op: Operand) -> Option<u32> {
    let e = op.as_expr()?;
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. } => root_local_of(body, *inner),
        _ => None,
    }
}

fn collect_loop_heap_effects(
    body: &Body,
    pure_builtin_callees: &crate::hashmap::IndexSet<FuncId>,
    block: BlockId,
) -> LoopHeapEffects {
    collect_node_heap_effects(body, pure_builtin_callees, NodeRef::Block(block))
}

/// The heap-write effects of a node's subtree, so a caller can invalidate
/// exactly what it writes rather than `bump_all`. Used by `walk_loop` (body
/// block) and the short-circuit arm (conditional rhs).
fn collect_node_heap_effects(
    body: &Body,
    pure_builtin_callees: &crate::hashmap::IndexSet<FuncId>,
    node: NodeRef,
) -> LoopHeapEffects {
    let mut eff = LoopHeapEffects::default();
    collect_loop_heap_node(body, pure_builtin_callees, node, &mut eff);
    eff
}

fn collect_loop_heap_node(
    body: &Body,
    pure_builtin_callees: &crate::hashmap::IndexSet<FuncId>,
    node: NodeRef,
    eff: &mut LoopHeapEffects,
) {
    if let NodeRef::Expr(e) = node {
        record_loop_heap_write(body, pure_builtin_callees, e, eff);
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_loop_heap_node(body, pure_builtin_callees, c, eff);
    }
}

fn record_loop_heap_write(
    body: &Body,
    pure_builtin_callees: &crate::hashmap::IndexSet<FuncId>,
    e: ExprId,
    eff: &mut LoopHeapEffects,
) {
    match &body.exprs[e].kind {
        ExprKind::Assign { target, .. } => match &body.exprs[*target].kind {
            // Reassigned locals are handled by `walk_loop`'s opaque reassign.
            ExprKind::Local { .. } => {}
            ExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } => match inner.as_expr().map(|ie| &body.exprs[ie].kind) {
                Some(ExprKind::Local { index, .. }) => {
                    eff.written_fields.insert((*index, *field_index));
                }
                // `(*p).f`, `a.b.f` — opaque receiver; aliased state may move.
                _ => eff.has_external_writes = true,
            },
            // `arr[i] = …` writes an array element, not a tracked field; deref
            // / other lvalues are opaque. Treat both as external.
            _ => eff.has_external_writes = true,
        },
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => match inner.as_expr().map(|ie| &body.exprs[ie].kind) {
            Some(ExprKind::Local { index, .. }) => {
                eff.mut_borrowed.insert(*index);
            }
            Some(ExprKind::FieldAccess {
                expr: receiver,
                field_index,
                ..
            }) => match receiver.as_expr().map(|re| &body.exprs[re].kind) {
                Some(ExprKind::Local { index, .. }) => {
                    eff.written_fields.insert((*index, *field_index));
                }
                _ => match root_local_of(body, *receiver) {
                    Some(root) => {
                        eff.mut_borrowed.insert(root);
                    }
                    None => eff.has_external_writes = true,
                },
            },
            _ => eff.has_external_writes = true,
        },
        ExprKind::Call {
            func_id,
            args,
            has_receiver,
            ..
        } => {
            for (i, arg) in args.iter().enumerate() {
                // A receiver is borrowed for the whole call whether or not the
                // method declares `&mut self` — the callee reaches its storage.
                let borrowed = arg.is_mut || (*has_receiver && i == 0);
                if borrowed
                    && let Some(ExprKind::Local { index, .. }) =
                        arg.expr.as_expr().map(|ae| &body.exprs[ae].kind)
                {
                    eff.mut_borrowed.insert(*index);
                }
            }
            if !is_builtin_pure_call(pure_builtin_callees, *func_id) {
                eff.has_external_writes = true;
            }
        }
        ExprKind::IndirectCall { .. } | ExprKind::CmRawCall { .. } => {
            eff.has_external_writes = true;
        }
        _ => {}
    }
}

/// Collect every local index that an `Assign`-to-bare-`Local`, a `Let`, or
/// a `LetDestructure` binding writes anywhere in `block`'s subtree. Used
/// by `walk_loop` and `dirty_all_writes_in_block` to mark the right set
/// of locals `Opaque` on entry/exit of flow-opaque constructs.
fn collect_writes_in_block(
    body: &Body,
    block: crate::nir_arena::BlockId,
    out: &mut crate::hashmap::IndexSet<u32>,
    cache: &mut IndexMap<BlockId, std::rc::Rc<crate::hashmap::IndexSet<u32>>>,
) {
    let ws = writes_of_block(body, block, cache);
    out.extend(ws.iter().copied());
}

fn collect_writes_in_stmt(
    body: &Body,
    stmt: StmtId,
    out: &mut crate::hashmap::IndexSet<u32>,
    cache: &mut IndexMap<BlockId, std::rc::Rc<crate::hashmap::IndexSet<u32>>>,
) {
    match &body.stmts[stmt].kind {
        StmtKind::Let {
            local_index, value, ..
        } => {
            out.insert(*local_index);
            if let Some(ve) = value.as_expr() {
                collect_writes_in_expr(body, ve, out, cache);
            }
        }
        StmtKind::LetDestructure { pattern, value, .. } => {
            collect_writes_in_pattern(body, *pattern, out, cache);
            collect_writes_in_operand(body, *value, out, cache);
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Stmt(stmt), |c| kids.push(c));
            for c in kids {
                match c {
                    NodeRef::Expr(e) => collect_writes_in_expr(body, e, out, cache),
                    NodeRef::Stmt(s) => collect_writes_in_stmt(body, s, out, cache),
                    NodeRef::Block(b) => collect_writes_in_block(body, b, out, cache),
                    NodeRef::Pat(p) => collect_writes_in_pattern(body, p, out, cache),
                }
            }
        }
    }
}

fn collect_writes_in_operand(
    body: &Body,
    op: Operand,
    out: &mut crate::hashmap::IndexSet<u32>,
    cache: &mut IndexMap<BlockId, std::rc::Rc<crate::hashmap::IndexSet<u32>>>,
) {
    if let Some(e) = op.as_expr() {
        collect_writes_in_expr(body, e, out, cache);
    }
}

fn collect_writes_in_expr(
    body: &Body,
    expr: ExprId,
    out: &mut crate::hashmap::IndexSet<u32>,
    cache: &mut IndexMap<BlockId, std::rc::Rc<crate::hashmap::IndexSet<u32>>>,
) {
    if let ExprKind::Assign { target, .. } = &body.exprs[expr].kind
        && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
    {
        out.insert(*index);
    }
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Expr(expr), |c| kids.push(c));
    for c in kids {
        match c {
            NodeRef::Expr(e) => collect_writes_in_expr(body, e, out, cache),
            NodeRef::Stmt(s) => collect_writes_in_stmt(body, s, out, cache),
            NodeRef::Block(b) => collect_writes_in_block(body, b, out, cache),
            NodeRef::Pat(p) => collect_writes_in_pattern(body, p, out, cache),
        }
    }
}

fn collect_writes_in_pattern(
    body: &Body,
    pat: PatId,
    out: &mut crate::hashmap::IndexSet<u32>,
    cache: &mut IndexMap<BlockId, std::rc::Rc<crate::hashmap::IndexSet<u32>>>,
) {
    if let PatKind::Binding { local_index, .. } = &body.pats[pat].kind {
        out.insert(*local_index);
    } else {
        let mut kids = Vec::new();
        body.for_each_child(NodeRef::Pat(pat), |c| kids.push(c));
        for c in kids {
            match c {
                NodeRef::Pat(p) => collect_writes_in_pattern(body, p, out, cache),
                NodeRef::Expr(e) => collect_writes_in_expr(body, e, out, cache),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tir::TypeTable;

    // ----- Body builders for tests -----

    fn empty_body() -> Body {
        Body::empty()
    }

    fn int_lit(body: &mut Body, value: u64) -> Operand {
        Operand::Value(body.values.alloc_unshared(
            crate::nir_value_graph::ValueKind::Int(value, TypeTable::I32),
            TypeTable::I32,
        ))
    }

    // ----- Tests -----

    // ----- FieldAccess heap-version behavior -----

    // ----- Per-arm heap snapshot -----

    // ----- LabeledBlock break-only-path writes -----

    // ----- Reachability-aware heap join at branch endpoints -----

    // ----- Field store→load forwarding -----

    // ----- Loop-entry value snapshots -----

    #[test]
    fn builder_records_value_types_for_extraction() {
        // `let a = <i32 lit>;` — the value carries its source type so extraction
        // can materialise it once the typed ExprNode is promoted away.
        let mut body = empty_body();
        // A promoted scalar carries its source type in the pool directly.
        let Operand::Value(v) = int_lit(&mut body, 7) else {
            unreachable!("int_lit yields a pool value")
        };
        assert_eq!(body.values.type_of(v), Some(TypeTable::I32));
    }

    /// `f(); a + b`, where locals `a` (0) and `b` (1) are both mutably escaped:
    /// the call re-mints an opaque for each, and the `a + b` that follows reads
    /// those post-call opaques. Opaque `ValueId`s are handed out in mint order,
    /// so a build that minted them in `mut_escaped`'s iteration order would
    /// produce a differently-shaped graph under a permuted set.
    fn call_then_add_body() -> Body {
        use crate::nir::{FuncId, NirLocal};
        use crate::nir_arena::{BlockNode, ExprNode, StmtNode};
        use crate::token::Span;
        let span = Span::new(0, 0, 0, 0);
        let mut b = Body::empty();
        let mut_local = |name: &str| NirLocal {
            name: name.to_string(),
            type_id: TypeTable::I32,
            is_mut: true,
        };
        b.locals = vec![mut_local("a"), mut_local("b")];
        let call = b.exprs.push(ExprNode {
            kind: ExprKind::Call {
                func_id: FuncId::from_u32(0),
                type_args: vec![],
                args: vec![],
                has_receiver: false,
            },
            type_id: TypeTable::I32,
            span,
        });
        let read = |b: &mut Body, index: u32, name: &str| {
            Operand::Expr(b.exprs.push(ExprNode {
                kind: ExprKind::Local {
                    index,
                    name: name.to_string(),
                },
                type_id: TypeTable::I32,
                span,
            }))
        };
        let left = read(&mut b, 0, "a");
        let right = read(&mut b, 1, "b");
        let add = b.exprs.push(ExprNode {
            kind: ExprKind::Binary {
                left,
                op: NirBinaryOp::Add,
                right,
            },
            type_id: TypeTable::I32,
            span,
        });
        let s_call = b.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Expr(call)),
            span,
        });
        let s_add = b.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Expr(add)),
            span,
        });
        b.root = b.blocks.push(BlockNode {
            stmts: vec![s_call, s_add],
            span,
        });
        b
    }

    #[test]
    fn cse_independent_of_mut_escaped_iteration_order() {
        use crate::hashmap::IndexSet;
        let empty = IndexSet::default();
        let no_calls = IndexSet::default();
        let no_callees = IndexSet::default();
        let escaped = |order: [u32; 2]| order.into_iter().collect::<IndexSet<u32>>();

        let build_with = |mut_escaped: &IndexSet<u32>| {
            let mut body = call_then_add_body();
            build(
                &mut body,
                &[],
                &empty,
                &empty,
                mut_escaped,
                &no_calls,
                &no_callees,
                None,
            );
            body.values.values
        };

        assert_eq!(build_with(&escaped([0, 1])), build_with(&escaped([1, 0])));
    }
}
