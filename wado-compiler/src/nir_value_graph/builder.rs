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

use crate::hashmap::IndexMap;
use crate::nir::{FunctionRef, NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{
    ArmData, BlockId, Body, ExprId, ExprKind, NodeRef, PatId, PatKind, StmtId, StmtKind,
};

use super::{HeapVersion, ValueId, ValuePool};

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
}

impl HeapState {
    fn new() -> Self {
        Self {
            next: HeapVersion::INITIAL.bump(),
            per_slot: IndexMap::default(),
            per_local: IndexMap::default(),
            field_global: IndexMap::default(),
            default_version: HeapVersion::INITIAL,
        }
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
        }
    }

    fn restore(&mut self, snap: HeapSnapshot) {
        self.per_slot = snap.per_slot;
        self.per_local = snap.per_local;
        self.field_global = snap.field_global;
        self.default_version = snap.default_version;
    }
}

#[derive(Clone)]
struct HeapSnapshot {
    per_slot: IndexMap<(u32, u32), HeapVersion>,
    per_local: IndexMap<u32, HeapVersion>,
    field_global: IndexMap<u32, HeapVersion>,
    default_version: HeapVersion,
}

/// The result of running [`build`] over a function body: the populated
/// pool plus the side-table mapping pure `ExprId`s to their `ValueId`.
///
/// Impure positions are absent from `value_of`. A rule that wants "this
/// expression's value" should look it up and treat the absence as "no
/// value-graph identity available" rather than panicking.
#[derive(Debug)]
pub struct ValueGraphBuild {
    pub pool: ValuePool,
    pub value_of: IndexMap<ExprId, ValueId>,
    /// For each literal `ValueId`, the first `ExprId` we observed producing
    /// it. Lets a consumer (e.g. store-load-forward) clone the original
    /// literal `ExprKind` — including its source `repr` — when replacing a
    /// `Local` read with the forwarded literal, avoiding `repr` churn in
    /// diagnostic output and NIR dumps.
    pub literal_source: IndexMap<ValueId, ExprId>,
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
pub fn build(
    body: &Body,
    param_locals: &[u32],
    aliased: &crate::hashmap::IndexSet<u32>,
    untrackable: &crate::hashmap::IndexSet<u32>,
    mut_escaped: &crate::hashmap::IndexSet<u32>,
) -> ValueGraphBuild {
    let mut b = Builder::new(body, aliased, untrackable, mut_escaped);
    b.seed_params(param_locals);
    b.walk_block(body.root);
    ValueGraphBuild {
        pool: b.pool,
        value_of: b.value_of,
        literal_source: b.literal_source,
        loop_entry_values: b.loop_entry_values,
    }
}

struct Builder<'a> {
    body: &'a Body,
    pool: ValuePool,
    value_of: IndexMap<ExprId, ValueId>,
    /// `local_index → current Value` at the current program point. Cloned at
    /// branch entries so each arm walks from the pre-branch snapshot.
    current_value: IndexMap<u32, ValueId>,
    /// Heap-version tracker. See [`HeapState`].
    heap_state: HeapState,
    /// `ValueId` → first source `ExprId` for literal values, so consumers can
    /// reuse the original `repr`. See [`ValueGraphBuild::literal_source`].
    literal_source: IndexMap<ValueId, ExprId>,
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
    /// pointee is reassigned. See [`Builder::update_ref_target`].
    ref_targets: IndexMap<u32, u32>,
    /// Per-loop pre-header `current_value` snapshots. See
    /// [`ValueGraphBuild::loop_entry_values`].
    loop_entry_values: IndexMap<BlockId, IndexMap<u32, ValueId>>,
}

impl<'a> Builder<'a> {
    fn new(
        body: &'a Body,
        aliased: &crate::hashmap::IndexSet<u32>,
        untrackable: &crate::hashmap::IndexSet<u32>,
        mut_escaped: &crate::hashmap::IndexSet<u32>,
    ) -> Self {
        Self {
            body,
            pool: ValuePool::new(),
            value_of: IndexMap::default(),
            current_value: IndexMap::default(),
            heap_state: HeapState::new(),
            literal_source: IndexMap::default(),
            field_store: IndexMap::default(),
            aliased: aliased.clone(),
            untrackable: untrackable.clone(),
            mut_escaped: mut_escaped.clone(),
            ref_targets: IndexMap::default(),
            loop_entry_values: IndexMap::default(),
        }
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
            } => match &self.body.exprs[*inner].kind {
                ExprKind::Local { index, .. } => Some(*index),
                _ => None,
            },
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
            ExprKind::FieldAccess { expr, .. } => self.receiver_root(*expr),
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
    fn bump_call_effects(&mut self) {
        for &l in &self.mut_escaped {
            self.heap_state.bump_local(l);
        }
    }

    fn seed_params(&mut self, param_locals: &[u32]) {
        for &idx in param_locals {
            let opaque = self.pool.fresh_opaque();
            self.current_value.insert(idx, opaque);
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
                let v = self
                    .walk_expr(value)
                    .unwrap_or_else(|| self.pool.fresh_opaque());
                self.current_value.insert(local_index, v);
                // `let x = S { f: lit, … }` binds `x` to a fresh opaque; seed
                // each pure field so a later `x.f` read forwards the literal.
                self.seed_struct_literal_fields(local_index, v, value);
                self.update_ref_target(local_index, value);
            }
            StmtKind::LetDestructure { pattern, value, .. } => {
                self.walk_expr(value);
                // Destructured bindings are Opaque for now; field-projection
                // Value kinds for them are a follow-up.
                self.bind_pattern_opaque(pattern);
            }
            StmtKind::Expr(e) => {
                self.walk_expr(e);
            }
            StmtKind::Return { value } => {
                if let Some(v) = value {
                    self.walk_expr(v);
                }
            }
            StmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.walk_expr(v);
                }
            }
            StmtKind::Continue => {}
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let cond_v = self.walk_expr(condition);
                let saved = self.current_value.clone();
                let saved_heap = self.heap_state.snapshot();
                self.walk_block(then_block);
                let then_state = std::mem::replace(&mut self.current_value, saved.clone());
                let then_heap = self.heap_state.snapshot();
                let then_falls = self.block_falls_through(then_block);
                self.heap_state.restore(saved_heap.clone());
                let (else_heap, else_falls) = if let Some(eb) = else_block {
                    self.walk_block(eb);
                    let h = self.heap_state.snapshot();
                    let f = self.block_falls_through(eb);
                    self.heap_state.restore(saved_heap.clone());
                    (h, f)
                } else {
                    (saved_heap.clone(), true)
                };
                let else_state = std::mem::replace(&mut self.current_value, saved.clone());
                self.merge_two_arms(cond_v, &saved, &then_state, &else_state);
                self.join_heap(
                    &saved_heap,
                    &[(then_heap, then_falls), (else_heap, else_falls)],
                );
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
        }
        id
    }

    fn compute_value(&mut self, expr: ExprId) -> Option<ValueId> {
        match self.body.exprs[expr].kind.clone() {
            // ---- Literals ----
            ExprKind::IntLiteral { value, .. } => {
                let v = self.pool.int(value);
                self.record_literal(expr, v);
                Some(v)
            }
            ExprKind::FloatLiteral { value, .. } => {
                let v = self.pool.float(value);
                self.record_literal(expr, v);
                Some(v)
            }
            ExprKind::BoolLiteral(b) => {
                let v = self.pool.bool(b);
                self.record_literal(expr, v);
                Some(v)
            }
            ExprKind::CharLiteral(c) => {
                let v = self.pool.char(c);
                self.record_literal(expr, v);
                Some(v)
            }
            ExprKind::StringLiteral(s) => {
                let v = self.pool.string(s);
                self.record_literal(expr, v);
                Some(v)
            }
            ExprKind::Null => {
                let v = self.pool.null();
                self.record_literal(expr, v);
                Some(v)
            }
            ExprKind::Unit => {
                let v = self.pool.unit();
                self.record_literal(expr, v);
                Some(v)
            }

            // ---- Local read ----
            ExprKind::Local { index, .. } => Some(self.read_local(index)),

            // ---- Pure arithmetic ----
            ExprKind::Binary { left, op, right } => {
                // Always walk both operands for their side effects on
                // `current_value` and `heap_state`, even when one of them is
                // impure (a `?` short-circuit on `lhs` would skip the rhs
                // walk and miss any local assignments / heap writes inside
                // it).
                let lhs = self.walk_expr(left);
                let rhs = if matches!(op, NirBinaryOp::And | NirBinaryOp::Or) {
                    // Short-circuit logical ops: the rhs is conditionally
                    // evaluated at runtime. Walk it inside a snapshot, then
                    // model "may or may not have happened" by dirtying any
                    // local the walk mutated and bumping the heap. Without
                    // this, a write inside the rhs (e.g. `false && { x =
                    // 2; true }`) would commit unconditionally to
                    // `current_value` and let store-load forwarding
                    // substitute later reads with the never-stored value.
                    let saved_cur = self.current_value.clone();
                    let rhs = self.walk_expr(right);
                    let changed: Vec<u32> = self
                        .current_value
                        .iter()
                        .filter_map(|(&k, &v)| {
                            saved_cur.get(&k).and_then(|s| (*s != v).then_some(k))
                        })
                        .collect();
                    for k in changed {
                        let opaque = self.pool.fresh_opaque();
                        self.current_value.insert(k, opaque);
                    }
                    self.heap_state.bump_all();
                    rhs
                } else {
                    self.walk_expr(right)
                };
                let lhs = lhs?;
                let rhs = rhs?;
                Some(self.pool.binary(op, lhs, rhs))
            }
            ExprKind::Unary { op, expr: inner } => {
                // `Ref` / `MutRef` / `Deref` are address-taking / heap-bearing
                // operations — not pure values. Walk the child (so pure
                // subtrees still land in `value_of`) but do not assign an id
                // to this expr.
                if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref) {
                    self.walk_expr(inner);
                    None
                } else {
                    let operand = self.walk_expr(inner)?;
                    Some(self.pool.unary(op, operand))
                }
            }
            ExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                let operand = self.walk_expr(inner)?;
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
                    ExprKind::FieldAccess { expr: recv, .. } => {
                        let bare_local =
                            matches!(&self.body.exprs[*recv].kind, ExprKind::Local { .. });
                        let root = self.receiver_root(*recv);
                        let recv_v = self.walk_expr(*recv);
                        Some((root, recv_v, bare_local))
                    }
                    _ => {
                        self.walk_expr(target);
                        None
                    }
                };
                let v = self
                    .walk_expr(value)
                    .unwrap_or_else(|| self.pool.fresh_opaque());
                match target_kind {
                    ExprKind::Local { index, .. } => {
                        self.current_value.insert(index, v);
                        // `local = S { f: lit, … }` rebinds `local` to a fresh
                        // object; seed each pure field like the `Let` case so a
                        // later `local.f` read forwards the literal.
                        self.seed_struct_literal_fields(index, v, value);
                        self.update_ref_target(index, value);
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
                self.walk_expr(value);
                // Globals share the heap from the optimizer's perspective.
                self.heap_state.bump_all();
                None
            }

            // ---- Control-flow expressions ----
            ExprKind::Block(block) => {
                // Block expressions are walked for the side-table but get
                // no `ValueId`.
                self.walk_block(block);
                None
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond_v = self.walk_expr(condition);
                let saved = self.current_value.clone();
                let saved_heap = self.heap_state.snapshot();
                self.walk_block(then_branch);
                let then_state = std::mem::replace(&mut self.current_value, saved.clone());
                let then_heap = self.heap_state.snapshot();
                let then_falls = self.block_falls_through(then_branch);
                self.heap_state.restore(saved_heap.clone());
                let (else_heap, else_falls) = if let Some(eb) = else_branch {
                    self.walk_block(eb);
                    let h = self.heap_state.snapshot();
                    let f = self.block_falls_through(eb);
                    self.heap_state.restore(saved_heap.clone());
                    (h, f)
                } else {
                    (saved_heap.clone(), true)
                };
                let else_state = std::mem::replace(&mut self.current_value, saved.clone());
                self.merge_two_arms(cond_v, &saved, &then_state, &else_state);
                self.join_heap(
                    &saved_heap,
                    &[(then_heap, then_falls), (else_heap, else_falls)],
                );
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
                self.walk_expr(scrut);
                self.walk_match_arms(&arms);
                None
            }
            ExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.walk_expr(scrutinee);
                let saved = self.current_value.clone();
                let saved_heap = self.heap_state.snapshot();
                let mut arm_states: Vec<IndexMap<u32, ValueId>> =
                    Vec::with_capacity(arms.len() + 1);
                let mut arm_heaps: Vec<(HeapSnapshot, bool)> = Vec::with_capacity(arms.len() + 1);
                for arm in &arms {
                    self.current_value.clone_from(&saved);
                    self.heap_state.restore(saved_heap.clone());
                    self.walk_block(*arm);
                    arm_states.push(self.current_value.clone());
                    arm_heaps.push((self.heap_state.snapshot(), self.block_falls_through(*arm)));
                }
                self.current_value.clone_from(&saved);
                self.heap_state.restore(saved_heap.clone());
                self.walk_block(default);
                arm_heaps.push((
                    self.heap_state.snapshot(),
                    self.block_falls_through(default),
                ));
                arm_states.push(std::mem::replace(&mut self.current_value, saved.clone()));
                self.merge_n_arms(&saved, &arm_states);
                self.join_heap(&saved_heap, &arm_heaps);
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
                let walked = self.walk_expr(inner)?;
                // Reference look-through: a read `r.f` where `r = &v` forwards
                // from `v`'s field slot (the pointee's current VN / root),
                // using `v`'s live slot state so a stale forward is impossible.
                let (recv, root) = match self.reference_lookthrough(inner) {
                    Some((pointee_vn, pointee)) => (pointee_vn, Some(pointee)),
                    None => (walked, self.receiver_root(inner)),
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
                self.walk_expr(inner);
                self.walk_expr(index);
                None
            }
            ExprKind::VariantTag { expr: inner }
            | ExprKind::VariantTest { expr: inner, .. }
            | ExprKind::VariantPayload { expr: inner, .. } => {
                self.walk_expr(inner);
                None
            }

            // ---- Allocation-bearing constructors (Skel-side per Q1) ----
            ExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    self.walk_expr(f.value);
                }
                None
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                for e in elements {
                    self.walk_expr(e);
                }
                None
            }
            ExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload {
                    self.walk_expr(p);
                }
                None
            }
            ExprKind::EnumConstruct { .. } => None,
            ExprKind::ClosureToCanonical { functor, .. } => {
                self.walk_expr(functor);
                None
            }

            // ---- Calls (effectful, may write the heap) ----
            ExprKind::Call { args, .. } => {
                for a in args {
                    self.walk_expr(a.expr);
                }
                self.bump_call_effects();
                None
            }
            ExprKind::CmRawCall { args, .. } => {
                for a in args {
                    self.walk_expr(a);
                }
                // Raw CM calls have opaque captures; stay fully conservative.
                self.heap_state.bump_all();
                None
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.walk_expr(receiver);
                for a in args {
                    self.walk_expr(a.expr);
                }
                self.bump_call_effects();
                None
            }
            ExprKind::IndirectCall { callee, args } => {
                self.walk_expr(callee);
                for a in args {
                    self.walk_expr(a);
                }
                self.heap_state.bump_all();
                None
            }

            // ---- Other Skel-side leaves ----
            ExprKind::GlobalVarGet { .. } | ExprKind::BytesLiteral(_) => None,
        }
    }

    fn record_literal(&mut self, expr: ExprId, value: ValueId) {
        self.literal_source.entry(value).or_insert(expr);
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
                ExprKind::Block(b) => {
                    let Some(&last) = self.body.blocks[*b].stmts.last() else {
                        return;
                    };
                    let StmtKind::Expr(tail) = &self.body.stmts[last].kind else {
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
                        || block_breaks_to_node(self.body, NodeRef::Expr(value), &label)
                    {
                        return;
                    }
                    producer = value;
                }
                _ => return,
            }
        }
        let ExprKind::StructLiteral { fields, .. } = &self.body.exprs[producer].kind else {
            return;
        };
        // Clone out the (field_index, value-expr) pairs to release the body
        // borrow before mutating `field_store`.
        let pairs: Vec<(u32, ExprId)> = fields.iter().map(|f| (f.field_index, f.value)).collect();
        for (field_index, field_value) in pairs {
            if let Some(&fv) = self.value_of.get(&field_value) {
                let ver = self.heap_state.version_of(Some(root), field_index);
                self.field_store.insert((recv, field_index, ver), fv);
            }
        }
    }

    fn read_local(&mut self, idx: u32) -> ValueId {
        if let Some(&v) = self.current_value.get(&idx) {
            v
        } else {
            // Unbound locals shouldn't occur on well-typed NIR; cache a
            // fresh Opaque so subsequent reads agree.
            let v = self.pool.fresh_opaque();
            self.current_value.insert(idx, v);
            v
        }
    }

    fn bind_pattern_opaque(&mut self, pat: PatId) {
        match self.body.pats[pat].kind.clone() {
            PatKind::Binding { local_index, .. } => {
                let v = self.pool.fresh_opaque();
                self.current_value.insert(local_index, v);
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
                self.walk_expr(expr);
            }
            PatKind::Wildcard
            | PatKind::Literal(_)
            | PatKind::Enum { .. }
            | PatKind::Range { .. } => {}
        }
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
            self.current_value.insert(idx, merged);
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
            self.current_value.insert(idx, merged);
        }
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
        self.heap_state.per_slot = new_per_slot;
        self.heap_state.per_local = new_per_local;
        self.heap_state.field_global = new_field_global;
        self.heap_state.default_version = new_default;
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
                collect_writes_in_expr(self.body, g, &mut guard_writes);
            }
        }
        for idx in &guard_writes {
            if self.current_value.contains_key(idx) {
                let opaque = self.pool.fresh_opaque();
                self.current_value.insert(*idx, opaque);
            }
        }
        if any_guard {
            self.heap_state.bump_all();
        }

        let saved = self.current_value.clone();
        let saved_heap = self.heap_state.snapshot();
        let mut states: Vec<IndexMap<u32, ValueId>> = Vec::with_capacity(arms.len());
        let mut arm_heaps: Vec<(HeapSnapshot, bool)> = Vec::with_capacity(arms.len());
        for arm in arms {
            self.current_value.clone_from(&saved);
            self.heap_state.restore(saved_heap.clone());
            self.bind_pattern_opaque(arm.pattern);
            if let Some(g) = arm.guard {
                self.walk_expr(g);
            }
            self.walk_expr(arm.body);
            states.push(self.current_value.clone());
            // Match arm bodies are expressions; without a `TypeTable` the
            // builder cannot detect a never-typed (`=> return …`) body, so
            // every arm is conservatively treated as falling through. A
            // returning arm contributes only its field writes to the join,
            // which is sound (those fields bump) if imprecise.
            arm_heaps.push((self.heap_state.snapshot(), true));
        }
        self.current_value.clone_from(&saved);
        self.merge_n_arms(&saved, &states);
        self.join_heap(&saved_heap, &arm_heaps);
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
        let mut writes: crate::hashmap::IndexSet<u32> = crate::hashmap::IndexSet::default();
        collect_writes_in_block(self.body, body_block, &mut writes);
        let heap_effects = collect_loop_heap_effects(self.body, body_block);
        // Snapshot before the reassigned-local opaques below overwrite the
        // written locals' pre-loop values.
        self.loop_entry_values
            .insert(body_block, self.current_value.clone());
        for idx in &writes {
            if self.current_value.contains_key(idx) {
                let opaque = self.pool.fresh_opaque();
                self.current_value.insert(*idx, opaque);
            }
        }
        self.apply_loop_heap_effects(&heap_effects);
        self.walk_block(body_block);
        for idx in &writes {
            if self.current_value.contains_key(idx) {
                let opaque = self.pool.fresh_opaque();
                self.current_value.insert(*idx, opaque);
            }
        }
        self.apply_loop_heap_effects(&heap_effects);
    }

    /// Invalidate the heap generations a loop body may write (see
    /// [`collect_loop_heap_effects`]). A written `local.field` bumps that
    /// local's `per_slot` (or `field_global` when the local is aliased); a
    /// `&mut`/method/mut-arg borrow bumps the local's `per_local`; an external
    /// write (non-builtin call, indirect call, opaque store) invalidates every
    /// reference-aliased local's fields. Non-touched fields survive.
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
            for &local in &self.aliased {
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
        let mut writes: crate::hashmap::IndexSet<u32> = crate::hashmap::IndexSet::default();
        collect_writes_in_block(self.body, block, &mut writes);
        for idx in &writes {
            if self.current_value.contains_key(idx) {
                let opaque = self.pool.fresh_opaque();
                self.current_value.insert(*idx, opaque);
            }
        }
    }
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

/// True when `func` is a builtin / monomorphized-builtin intrinsic that
/// operates below the struct-field layer (`array_set`, `memory_grow`, …) and
/// so never mutates a tracked `(root, field)` slot. Mirrors
/// `const_folding::is_field_env_pure_call`.
fn is_builtin_pure_call(func: &FunctionRef) -> bool {
    func.builtin_name().is_some() || func.monomorphized_builtin_name().is_some()
}

/// Walk down a `local.f.g.…` field chain to its rooted local index, or `None`
/// if rooted at a non-`Local` (e.g. `(*p).f`).
fn root_local_of(body: &Body, e: ExprId) -> Option<u32> {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. } => root_local_of(body, *inner),
        _ => None,
    }
}

fn collect_loop_heap_effects(body: &Body, block: BlockId) -> LoopHeapEffects {
    let mut eff = LoopHeapEffects::default();
    collect_loop_heap_node(body, NodeRef::Block(block), &mut eff);
    eff
}

fn collect_loop_heap_node(body: &Body, node: NodeRef, eff: &mut LoopHeapEffects) {
    if let NodeRef::Expr(e) = node {
        record_loop_heap_write(body, e, eff);
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_loop_heap_node(body, c, eff);
    }
}

fn record_loop_heap_write(body: &Body, e: ExprId, eff: &mut LoopHeapEffects) {
    match &body.exprs[e].kind {
        ExprKind::Assign { target, .. } => match &body.exprs[*target].kind {
            // Reassigned locals are handled by `walk_loop`'s opaque reassign.
            ExprKind::Local { .. } => {}
            ExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } => match &body.exprs[*inner].kind {
                ExprKind::Local { index, .. } => {
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
        } => match &body.exprs[*inner].kind {
            ExprKind::Local { index, .. } => {
                eff.mut_borrowed.insert(*index);
            }
            ExprKind::FieldAccess {
                expr: receiver,
                field_index,
                ..
            } => match &body.exprs[*receiver].kind {
                ExprKind::Local { index, .. } => {
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
        ExprKind::Call { func, args, .. } => {
            for arg in args {
                if arg.is_mut
                    && let ExprKind::Local { index, .. } = &body.exprs[arg.expr].kind
                {
                    eff.mut_borrowed.insert(*index);
                }
            }
            if !is_builtin_pure_call(func) {
                eff.has_external_writes = true;
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            if let ExprKind::Local { index, .. } = &body.exprs[*receiver].kind {
                eff.mut_borrowed.insert(*index);
            }
            for arg in args {
                if arg.is_mut
                    && let ExprKind::Local { index, .. } = &body.exprs[arg.expr].kind
                {
                    eff.mut_borrowed.insert(*index);
                }
            }
            eff.has_external_writes = true;
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
) {
    let stmts = body.blocks[block].stmts.clone();
    for s in stmts {
        collect_writes_in_stmt(body, s, out);
    }
}

fn collect_writes_in_stmt(body: &Body, stmt: StmtId, out: &mut crate::hashmap::IndexSet<u32>) {
    match &body.stmts[stmt].kind {
        StmtKind::Let {
            local_index, value, ..
        } => {
            out.insert(*local_index);
            collect_writes_in_expr(body, *value, out);
        }
        StmtKind::LetDestructure { pattern, value, .. } => {
            collect_writes_in_pattern(body, *pattern, out);
            collect_writes_in_expr(body, *value, out);
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Stmt(stmt), |c| kids.push(c));
            for c in kids {
                match c {
                    NodeRef::Expr(e) => collect_writes_in_expr(body, e, out),
                    NodeRef::Stmt(s) => collect_writes_in_stmt(body, s, out),
                    NodeRef::Block(b) => collect_writes_in_block(body, b, out),
                    NodeRef::Pat(p) => collect_writes_in_pattern(body, p, out),
                }
            }
        }
    }
}

fn collect_writes_in_expr(body: &Body, expr: ExprId, out: &mut crate::hashmap::IndexSet<u32>) {
    if let ExprKind::Assign { target, .. } = &body.exprs[expr].kind
        && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
    {
        out.insert(*index);
    }
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Expr(expr), |c| kids.push(c));
    for c in kids {
        match c {
            NodeRef::Expr(e) => collect_writes_in_expr(body, e, out),
            NodeRef::Stmt(s) => collect_writes_in_stmt(body, s, out),
            NodeRef::Block(b) => collect_writes_in_block(body, b, out),
            NodeRef::Pat(p) => collect_writes_in_pattern(body, p, out),
        }
    }
}

fn collect_writes_in_pattern(body: &Body, pat: PatId, out: &mut crate::hashmap::IndexSet<u32>) {
    if let PatKind::Binding { local_index, .. } = &body.pats[pat].kind {
        out.insert(*local_index);
    } else {
        let mut kids = Vec::new();
        body.for_each_child(NodeRef::Pat(pat), |c| kids.push(c));
        for c in kids {
            match c {
                NodeRef::Pat(p) => collect_writes_in_pattern(body, p, out),
                NodeRef::Expr(e) => collect_writes_in_expr(body, e, out),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir::{NirBinaryOp, NirParam};
    use crate::nir_arena::{BlockNode, ExprNode, StmtNode};
    use crate::nir_value_graph::ValueKind;
    use crate::tir::TypeTable;
    use crate::token::Span;

    // ----- Body builders for tests -----

    fn build_t(body: &Body, params: &[NirParam]) -> ValueGraphBuild {
        let param_locals: Vec<u32> = params.iter().map(|p| p.local_index).collect();
        let empty = crate::hashmap::IndexSet::default();
        build(body, &param_locals, &empty, &empty, &empty)
    }

    /// `build` with an explicit reference-aliased set (no `untrackable`). The
    /// aliased set doubles as `mut_escaped` so these tests exercise the
    /// call-invalidation path (a mutably-aliased local is clobbered by calls).
    fn build_aliased(
        body: &Body,
        params: &[NirParam],
        aliased: &crate::hashmap::IndexSet<u32>,
    ) -> ValueGraphBuild {
        let param_locals: Vec<u32> = params.iter().map(|p| p.local_index).collect();
        let empty = crate::hashmap::IndexSet::default();
        build(body, &param_locals, aliased, &empty, aliased)
    }

    fn empty_body() -> Body {
        Body::empty()
    }

    fn alloc_expr(body: &mut Body, kind: ExprKind) -> ExprId {
        body.exprs.push(ExprNode {
            kind,
            type_id: TypeTable::UNIT,
            span: Span::default(),
        })
    }

    fn alloc_stmt(body: &mut Body, kind: StmtKind) -> StmtId {
        body.stmts.push(StmtNode {
            kind,
            span: Span::default(),
        })
    }

    fn root_with(body: &mut Body, stmts: Vec<StmtId>) {
        body.root = body.blocks.push(BlockNode {
            stmts,
            span: Span::default(),
        });
    }

    fn int_lit(body: &mut Body, value: u64) -> ExprId {
        alloc_expr(
            body,
            ExprKind::IntLiteral {
                value,
                repr: value.to_string(),
            },
        )
    }

    fn local_ref(body: &mut Body, idx: u32) -> ExprId {
        alloc_expr(
            body,
            ExprKind::Local {
                index: idx,
                name: format!("__l{idx}"),
            },
        )
    }

    fn let_stmt(body: &mut Body, idx: u32, value: ExprId, is_mut: bool) -> StmtId {
        alloc_stmt(
            body,
            StmtKind::Let {
                name: format!("__l{idx}"),
                local_index: idx,
                is_mut,
                is_reactive: false,
                type_id: TypeTable::UNIT,
                value,
                skip_value_copy: false,
            },
        )
    }

    fn assign_stmt(body: &mut Body, idx: u32, value: ExprId) -> StmtId {
        let target = local_ref(body, idx);
        let assign = alloc_expr(body, ExprKind::Assign { target, value });
        alloc_stmt(body, StmtKind::Expr(assign))
    }

    fn binary(body: &mut Body, op: NirBinaryOp, left: ExprId, right: ExprId) -> ExprId {
        alloc_expr(body, ExprKind::Binary { left, op, right })
    }

    fn bool_lit(body: &mut Body, b: bool) -> ExprId {
        alloc_expr(body, ExprKind::BoolLiteral(b))
    }

    fn block_with(body: &mut Body, stmts: Vec<StmtId>) -> crate::nir_arena::BlockId {
        body.blocks.push(BlockNode {
            stmts,
            span: Span::default(),
        })
    }

    fn field_access(body: &mut Body, expr: ExprId, field_index: u32) -> ExprId {
        alloc_expr(
            body,
            ExprKind::FieldAccess {
                expr,
                field_index,
                field_name: format!("__f{field_index}"),
            },
        )
    }

    fn field_assign_stmt(body: &mut Body, recv: ExprId, field_index: u32, value: ExprId) -> StmtId {
        let target = field_access(body, recv, field_index);
        let assign = alloc_expr(body, ExprKind::Assign { target, value });
        alloc_stmt(body, StmtKind::Expr(assign))
    }

    fn call_void(body: &mut Body) -> ExprId {
        use crate::module_source::ModuleSource;
        use crate::nir::FunctionRef;
        alloc_expr(
            body,
            ExprKind::Call {
                func: FunctionRef {
                    module_source: ModuleSource::entry_point_synthetic(),
                    name: "foo".to_string(),
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: vec![],
                args: vec![],
            },
        )
    }

    fn param_seed() -> NirParam {
        NirParam {
            name: "obj".to_string(),
            type_id: TypeTable::UNIT,
            local_index: 0,
            is_mut: false,
            span: Span::default(),
        }
    }

    // ----- Tests -----

    #[test]
    fn literal_int_gets_value_id() {
        let mut body = empty_body();
        let lit = int_lit(&mut body, 42);
        let s = alloc_stmt(&mut body, StmtKind::Expr(lit));
        root_with(&mut body, vec![s]);
        let r = build_t(&body, &[]);
        let v = r.value_of[&lit];
        assert_eq!(r.pool.kind(v), &ValueKind::Int(42));
    }

    #[test]
    fn let_then_read_returns_same_value() {
        // let x = 1; x
        let mut body = empty_body();
        let lit = int_lit(&mut body, 1);
        let let_s = let_stmt(&mut body, 0, lit, false);
        let read = local_ref(&mut body, 0);
        let s2 = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, s2]);
        let r = build_t(&body, &[]);
        let lit_v = r.value_of[&lit];
        let read_v = r.value_of[&read];
        assert_eq!(lit_v, read_v);
        assert_eq!(r.pool.kind(read_v), &ValueKind::Int(1));
    }

    #[test]
    fn equivalent_arithmetic_dedupes() {
        // let a = 1 + 2; let b = 1 + 2;
        let mut body = empty_body();
        let one_a = int_lit(&mut body, 1);
        let two_a = int_lit(&mut body, 2);
        let add_a = binary(&mut body, NirBinaryOp::Add, one_a, two_a);
        let let_a = let_stmt(&mut body, 0, add_a, false);
        let one_b = int_lit(&mut body, 1);
        let two_b = int_lit(&mut body, 2);
        let add_b = binary(&mut body, NirBinaryOp::Add, one_b, two_b);
        let let_b = let_stmt(&mut body, 1, add_b, false);
        root_with(&mut body, vec![let_a, let_b]);
        let r = build_t(&body, &[]);
        assert_eq!(r.value_of[&add_a], r.value_of[&add_b]);
    }

    #[test]
    fn reassignment_updates_local_value() {
        // let mut x = 1; x = 2; x
        let mut body = empty_body();
        let one = int_lit(&mut body, 1);
        let let_s = let_stmt(&mut body, 0, one, true);
        let two = int_lit(&mut body, 2);
        let assign = assign_stmt(&mut body, 0, two);
        let read = local_ref(&mut body, 0);
        let s_read = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, assign, s_read]);
        let r = build_t(&body, &[]);
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(2));
    }

    #[test]
    fn if_merge_builds_select() {
        // let mut x = 1; if true { x = 2; } else { x = 3; }; x
        let mut body = empty_body();
        let one = int_lit(&mut body, 1);
        let let_s = let_stmt(&mut body, 0, one, true);
        let cond = bool_lit(&mut body, true);
        let two = int_lit(&mut body, 2);
        let assign_then = assign_stmt(&mut body, 0, two);
        let then_block = block_with(&mut body, vec![assign_then]);
        let three = int_lit(&mut body, 3);
        let assign_else = assign_stmt(&mut body, 0, three);
        let else_block = block_with(&mut body, vec![assign_else]);
        let if_s = alloc_stmt(
            &mut body,
            StmtKind::If {
                condition: cond,
                then_block,
                else_block: Some(else_block),
            },
        );
        let read = local_ref(&mut body, 0);
        let s_read = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, if_s, s_read]);
        let r = build_t(&body, &[]);
        let read_v = r.value_of[&read];
        match r.pool.kind(read_v) {
            ValueKind::Select { cond, then, else_ } => {
                assert_eq!(r.pool.kind(*cond), &ValueKind::Bool(true));
                assert_eq!(r.pool.kind(*then), &ValueKind::Int(2));
                assert_eq!(r.pool.kind(*else_), &ValueKind::Int(3));
            }
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn if_without_else_merges_with_pre_value() {
        // let mut x = 1; if true { x = 2; }; x
        let mut body = empty_body();
        let one = int_lit(&mut body, 1);
        let let_s = let_stmt(&mut body, 0, one, true);
        let cond = bool_lit(&mut body, true);
        let two = int_lit(&mut body, 2);
        let assign_then = assign_stmt(&mut body, 0, two);
        let then_block = block_with(&mut body, vec![assign_then]);
        let if_s = alloc_stmt(
            &mut body,
            StmtKind::If {
                condition: cond,
                then_block,
                else_block: None,
            },
        );
        let read = local_ref(&mut body, 0);
        let s_read = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, if_s, s_read]);
        let r = build_t(&body, &[]);
        match r.pool.kind(r.value_of[&read]) {
            ValueKind::Select { cond, then, else_ } => {
                assert_eq!(r.pool.kind(*cond), &ValueKind::Bool(true));
                assert_eq!(r.pool.kind(*then), &ValueKind::Int(2));
                assert_eq!(r.pool.kind(*else_), &ValueKind::Int(1));
            }
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn if_with_agreeing_arms_skips_select() {
        // let mut x = 1; if true { x = 2; } else { x = 2; }; x
        let mut body = empty_body();
        let one = int_lit(&mut body, 1);
        let let_s = let_stmt(&mut body, 0, one, true);
        let cond = bool_lit(&mut body, true);
        let two_a = int_lit(&mut body, 2);
        let assign_then = assign_stmt(&mut body, 0, two_a);
        let then_block = block_with(&mut body, vec![assign_then]);
        let two_b = int_lit(&mut body, 2);
        let assign_else = assign_stmt(&mut body, 0, two_b);
        let else_block = block_with(&mut body, vec![assign_else]);
        let if_s = alloc_stmt(
            &mut body,
            StmtKind::If {
                condition: cond,
                then_block,
                else_block: Some(else_block),
            },
        );
        let read = local_ref(&mut body, 0);
        let s_read = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, if_s, s_read]);
        let r = build_t(&body, &[]);
        // Both arms wrote Int(2); the merge picks that without a Select.
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(2));
    }

    #[test]
    fn loop_modified_local_becomes_opaque() {
        // let mut i = 0; loop { i = i + 1; }; i
        let mut body = empty_body();
        let zero = int_lit(&mut body, 0);
        let let_s = let_stmt(&mut body, 0, zero, true);
        let i_read = local_ref(&mut body, 0);
        let one = int_lit(&mut body, 1);
        let plus = binary(&mut body, NirBinaryOp::Add, i_read, one);
        let assign = assign_stmt(&mut body, 0, plus);
        let lb = block_with(&mut body, vec![assign]);
        let loop_s = alloc_stmt(&mut body, StmtKind::Loop { body: lb });
        let read = local_ref(&mut body, 0);
        let s_read = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, loop_s, s_read]);
        let r = build_t(&body, &[]);
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::Opaque(_)
        ));
    }

    #[test]
    fn loop_unmodified_local_keeps_value() {
        // let x = 1; let mut i = 0; loop { i = i + 1; }; x
        let mut body = empty_body();
        let one = int_lit(&mut body, 1);
        let let_x = let_stmt(&mut body, 0, one, false);
        let zero = int_lit(&mut body, 0);
        let let_i = let_stmt(&mut body, 1, zero, true);
        let i_read = local_ref(&mut body, 1);
        let lit_one = int_lit(&mut body, 1);
        let plus = binary(&mut body, NirBinaryOp::Add, i_read, lit_one);
        let assign = assign_stmt(&mut body, 1, plus);
        let lb = block_with(&mut body, vec![assign]);
        let loop_s = alloc_stmt(&mut body, StmtKind::Loop { body: lb });
        let read = local_ref(&mut body, 0);
        let s_read = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_x, let_i, loop_s, s_read]);
        let r = build_t(&body, &[]);
        // `x` is not touched by the loop, so it retains its Int(1).
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(1));
    }

    #[test]
    fn function_parameter_is_opaque_and_reads_dedup() {
        // fn(x: i32) { x; x; }
        let mut body = empty_body();
        let read1 = local_ref(&mut body, 0);
        let read2 = local_ref(&mut body, 0);
        let s1 = alloc_stmt(&mut body, StmtKind::Expr(read1));
        let s2 = alloc_stmt(&mut body, StmtKind::Expr(read2));
        root_with(&mut body, vec![s1, s2]);
        let param = NirParam {
            name: "x".to_string(),
            type_id: TypeTable::I32,
            local_index: 0,
            is_mut: false,
            span: Span::default(),
        };
        let r = build_t(&body, &[param]);
        let v1 = r.value_of[&read1];
        let v2 = r.value_of[&read2];
        assert_eq!(v1, v2);
        assert!(matches!(r.pool.kind(v1), ValueKind::Opaque(_)));
    }

    #[test]
    fn binary_on_param_reads_dedupes() {
        // fn(x: i32) { x + 1; x + 1; }
        let mut body = empty_body();
        let read1 = local_ref(&mut body, 0);
        let one_a = int_lit(&mut body, 1);
        let add_a = binary(&mut body, NirBinaryOp::Add, read1, one_a);
        let read2 = local_ref(&mut body, 0);
        let one_b = int_lit(&mut body, 1);
        let add_b = binary(&mut body, NirBinaryOp::Add, read2, one_b);
        let s1 = alloc_stmt(&mut body, StmtKind::Expr(add_a));
        let s2 = alloc_stmt(&mut body, StmtKind::Expr(add_b));
        root_with(&mut body, vec![s1, s2]);
        let param = NirParam {
            name: "x".to_string(),
            type_id: TypeTable::I32,
            local_index: 0,
            is_mut: false,
            span: Span::default(),
        };
        let r = build_t(&body, &[param]);
        // Both `x + 1` expressions share a ValueId because `x` is a stable
        // (write-once) Opaque and `1` is hash-consed.
        assert_eq!(r.value_of[&add_a], r.value_of[&add_b]);
    }

    #[test]
    fn impure_let_value_gets_opaque_binding() {
        // let x = call(); x
        // We synthesise a Call node here; the builder treats it as impure so
        // `x` gets a fresh Opaque rather than tracking the Call's value.
        use crate::module_source::ModuleSource;
        use crate::nir::FunctionRef;
        let mut body = empty_body();
        let call = alloc_expr(
            &mut body,
            ExprKind::Call {
                func: FunctionRef {
                    module_source: ModuleSource::entry_point_synthetic(),
                    name: "foo".to_string(),
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: vec![],
                args: vec![],
            },
        );
        let let_s = let_stmt(&mut body, 0, call, false);
        let read = local_ref(&mut body, 0);
        let s = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_s, s]);
        let r = build_t(&body, &[]);
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::Opaque(_)
        ));
        // The call itself has no value_of entry.
        assert!(!r.value_of.contains_key(&call));
    }

    // ----- FieldAccess heap-version behavior -----

    #[test]
    fn two_field_reads_same_field_share_value_id() {
        // fn(obj) { obj.f; obj.f; }
        let mut body = empty_body();
        let recv1 = local_ref(&mut body, 0);
        let read1 = field_access(&mut body, recv1, 0);
        let recv2 = local_ref(&mut body, 0);
        let read2 = field_access(&mut body, recv2, 0);
        let s1 = alloc_stmt(&mut body, StmtKind::Expr(read1));
        let s2 = alloc_stmt(&mut body, StmtKind::Expr(read2));
        root_with(&mut body, vec![s1, s2]);
        let r = build_t(&body, &[param_seed()]);
        assert_eq!(r.value_of[&read1], r.value_of[&read2]);
        assert!(matches!(
            r.pool.kind(r.value_of[&read1]),
            ValueKind::FieldAccess { .. }
        ));
    }

    #[test]
    fn field_write_invalidates_only_that_field() {
        // fn(obj) {
        //     let a = obj.f;
        //     let b = obj.g;
        //     obj.f = 1;
        //     let a2 = obj.f;   // distinct VN from `a` (different heap_ver)
        //     let b2 = obj.g;   // same VN as `b` (bump_field(f) did not touch g)
        // }
        let mut body = empty_body();
        let recv_a = local_ref(&mut body, 0);
        let read_a = field_access(&mut body, recv_a, 0);
        let let_a = let_stmt(&mut body, 1, read_a, false);

        let recv_b = local_ref(&mut body, 0);
        let read_b = field_access(&mut body, recv_b, 1);
        let let_b = let_stmt(&mut body, 2, read_b, false);

        let one = int_lit(&mut body, 1);
        let recv_w = local_ref(&mut body, 0);
        let write = field_assign_stmt(&mut body, recv_w, 0, one);

        let recv_a2 = local_ref(&mut body, 0);
        let read_a2 = field_access(&mut body, recv_a2, 0);
        let let_a2 = let_stmt(&mut body, 3, read_a2, false);

        let recv_b2 = local_ref(&mut body, 0);
        let read_b2 = field_access(&mut body, recv_b2, 1);
        let let_b2 = let_stmt(&mut body, 4, read_b2, false);

        root_with(&mut body, vec![let_a, let_b, write, let_a2, let_b2]);
        let r = build_t(&body, &[param_seed()]);

        // `obj.f` reads straddle a write of `f`: different heap versions, distinct VN.
        assert_ne!(r.value_of[&read_a], r.value_of[&read_a2]);
        // `obj.g` is not touched by writing `obj.f`: heap version unchanged, same VN.
        assert_eq!(r.value_of[&read_b], r.value_of[&read_b2]);
    }

    #[test]
    fn call_invalidates_aliased_fields_only() {
        // fn(obj) { let a = obj.f; foo(); let a2 = obj.f; }
        // A call may reach a reference-aliased object, so an aliased `obj`'s
        // field re-derives across the call (distinct VN). A non-aliased
        // `obj`'s object is unreachable, so its field survives (same VN).
        let build_call_straddle = |aliased: Option<u32>| {
            let mut body = empty_body();
            let recv1 = local_ref(&mut body, 0);
            let read1 = field_access(&mut body, recv1, 0);
            let let_1 = let_stmt(&mut body, 1, read1, false);
            let call = call_void(&mut body);
            let call_s = alloc_stmt(&mut body, StmtKind::Expr(call));
            let recv2 = local_ref(&mut body, 0);
            let read2 = field_access(&mut body, recv2, 0);
            let let_2 = let_stmt(&mut body, 2, read2, false);
            root_with(&mut body, vec![let_1, call_s, let_2]);
            let mut set = crate::hashmap::IndexSet::default();
            if let Some(idx) = aliased {
                set.insert(idx);
            }
            let r = build_aliased(&body, &[param_seed()], &set);
            (r.value_of[&read1], r.value_of[&read2])
        };
        // Aliased `obj`: the call invalidates its field.
        let (a1, a2) = build_call_straddle(Some(0));
        assert_ne!(a1, a2);
        // Non-aliased `obj`: the field survives the call.
        let (n1, n2) = build_call_straddle(None);
        assert_eq!(n1, n2);
    }

    #[test]
    fn field_access_with_impure_receiver_yields_no_value() {
        // fn() { call().f; }
        let mut body = empty_body();
        let call = call_void(&mut body);
        let fa = field_access(&mut body, call, 0);
        let s = alloc_stmt(&mut body, StmtKind::Expr(fa));
        root_with(&mut body, vec![s]);
        let r = build_t(&body, &[]);
        assert!(!r.value_of.contains_key(&fa));
    }

    // ----- Per-arm heap snapshot -----

    #[test]
    fn switch_arm_field_writes_do_not_leak_across_arms() {
        // fn(obj) {
        //     switch (0) {
        //         0 => { obj.f = 1; }
        //         1 => { obj.g; }                  // VN must match obj.g read at TOP
        //         default => {}
        //     }
        // }
        // The two `obj.g` reads (one inside arm 1, one before the switch) must
        // share a VN: arm 0's `obj.f = 1` bumps field `f` only, but without
        // per-arm snapshot the bump would leak into arm 1's heap state and
        // give `obj.g` a fresh heap version.
        let mut body = empty_body();
        // Read obj.g before the switch.
        let recv_pre = local_ref(&mut body, 0);
        let read_pre = field_access(&mut body, recv_pre, 1);
        let let_pre = let_stmt(&mut body, 1, read_pre, false);

        // Arm 0: obj.f = 1
        let recv_w = local_ref(&mut body, 0);
        let one = int_lit(&mut body, 1);
        let arm0_write = field_assign_stmt(&mut body, recv_w, 0, one);
        let arm0 = block_with(&mut body, vec![arm0_write]);

        // Arm 1: read obj.g
        let recv_in = local_ref(&mut body, 0);
        let read_in_arm = field_access(&mut body, recv_in, 1);
        let let_in_arm = let_stmt(&mut body, 2, read_in_arm, false);
        let arm1 = block_with(&mut body, vec![let_in_arm]);

        // Default: empty
        let default = block_with(&mut body, vec![]);

        let scrut = int_lit(&mut body, 0);
        let switch_e = alloc_expr(
            &mut body,
            ExprKind::Switch {
                scrutinee: scrut,
                min_value: 0,
                arms: vec![arm0, arm1],
                default,
            },
        );
        let switch_s = alloc_stmt(&mut body, StmtKind::Expr(switch_e));

        root_with(&mut body, vec![let_pre, switch_s]);
        let r = build_t(&body, &[param_seed()]);
        // The read inside arm 1 must share a VN with the pre-switch read.
        assert_eq!(r.value_of[&read_pre], r.value_of[&read_in_arm]);
    }

    #[test]
    fn if_branch_field_writes_do_not_leak_into_else() {
        // fn(obj) {
        //     let g_pre = obj.g;
        //     if true { obj.f = 1; }
        //     else    { let g_in_else = obj.g; }
        // }
        // The else-branch `obj.g` read must share a VN with the pre-If
        // read: the then-branch's `bump_field(f)` is rolled back at the
        // arm boundary so it does not pollute the else-branch heap state.
        let mut body = empty_body();
        let recv_pre = local_ref(&mut body, 0);
        let read_pre = field_access(&mut body, recv_pre, 1);
        let let_pre = let_stmt(&mut body, 1, read_pre, false);

        let cond = bool_lit(&mut body, true);

        let recv_w = local_ref(&mut body, 0);
        let one = int_lit(&mut body, 1);
        let then_write = field_assign_stmt(&mut body, recv_w, 0, one);
        let then_block = block_with(&mut body, vec![then_write]);

        let recv_else = local_ref(&mut body, 0);
        let read_in_else = field_access(&mut body, recv_else, 1);
        let let_in_else = let_stmt(&mut body, 2, read_in_else, false);
        let else_block = block_with(&mut body, vec![let_in_else]);

        let if_s = alloc_stmt(
            &mut body,
            StmtKind::If {
                condition: cond,
                then_block,
                else_block: Some(else_block),
            },
        );
        root_with(&mut body, vec![let_pre, if_s]);
        let r = build_t(&body, &[param_seed()]);
        assert_eq!(r.value_of[&read_pre], r.value_of[&read_in_else]);
    }

    // ----- LabeledBlock break-only-path writes -----

    #[test]
    fn labeled_block_break_only_write_marks_local_opaque() {
        // fn() {
        //     let mut x = 1;
        //     'lb: { if cond { x = 2; break 'lb; } else {} }
        //     x   // must be Opaque — the break path wrote 2 but fall-through didn't
        // }
        let mut body = empty_body();
        let one = int_lit(&mut body, 1);
        let let_x = let_stmt(&mut body, 0, one, true);

        // Inside the LB: `if true { x = 2; break 'lb; }`
        let cond = bool_lit(&mut body, true);
        let two = int_lit(&mut body, 2);
        let assign_then = assign_stmt(&mut body, 0, two);
        let break_stmt = alloc_stmt(
            &mut body,
            StmtKind::Break {
                label: Some("lb".to_string()),
                value: None,
            },
        );
        let then_block = block_with(&mut body, vec![assign_then, break_stmt]);
        let else_block = block_with(&mut body, vec![]);
        let if_inside = alloc_stmt(
            &mut body,
            StmtKind::If {
                condition: cond,
                then_block,
                else_block: Some(else_block),
            },
        );
        let lb_block = block_with(&mut body, vec![if_inside]);
        let lb_stmt = alloc_stmt(
            &mut body,
            StmtKind::LabeledBlock {
                label: "lb".to_string(),
                block: lb_block,
            },
        );

        let read = local_ref(&mut body, 0);
        let s_read = alloc_stmt(&mut body, StmtKind::Expr(read));
        root_with(&mut body, vec![let_x, lb_stmt, s_read]);
        let r = build_t(&body, &[]);
        // Post-LB `x` must be Opaque — the break-path write of 2 means the
        // value is unknown, even though fall-through never observes it.
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::Opaque(_)
        ));
    }

    // ----- Reachability-aware heap join at branch endpoints -----

    #[test]
    fn break_guard_does_not_split_field_versions() {
        // fn(obj) {
        //     let a = obj.f;
        //     if cond { break; }      // guard arm: no field write, does not fall through
        //     let b = obj.f;          // must share VN with `a`
        // }
        // The previous unconditional `bump_all` after the `if` split the
        // heap version, denying every desugared loop's guard/body field VN
        // sharing. The reachability-aware join keeps it.
        let mut body = empty_body();
        let recv_a = local_ref(&mut body, 0);
        let read_a = field_access(&mut body, recv_a, 0);
        let let_a = let_stmt(&mut body, 1, read_a, false);

        let cond = bool_lit(&mut body, true);
        let brk = alloc_stmt(
            &mut body,
            StmtKind::Break {
                label: None,
                value: None,
            },
        );
        let then_block = block_with(&mut body, vec![brk]);
        let if_s = alloc_stmt(
            &mut body,
            StmtKind::If {
                condition: cond,
                then_block,
                else_block: None,
            },
        );

        let recv_b = local_ref(&mut body, 0);
        let read_b = field_access(&mut body, recv_b, 0);
        let let_b = let_stmt(&mut body, 2, read_b, false);

        root_with(&mut body, vec![let_a, if_s, let_b]);
        let r = build_t(&body, &[param_seed()]);
        assert_eq!(r.value_of[&read_a], r.value_of[&read_b]);
    }

    #[test]
    fn if_arm_field_write_bumps_after_merge() {
        // fn(obj) {
        //     let a = obj.f;
        //     if cond { obj.f = 1; }   // fall-through arm writes f
        //     let b = obj.f;           // value now unknown -> distinct VN
        // }
        let mut body = empty_body();
        let recv_a = local_ref(&mut body, 0);
        let read_a = field_access(&mut body, recv_a, 0);
        let let_a = let_stmt(&mut body, 1, read_a, false);

        let cond = bool_lit(&mut body, true);
        let recv_w = local_ref(&mut body, 0);
        let one = int_lit(&mut body, 1);
        let write = field_assign_stmt(&mut body, recv_w, 0, one);
        let then_block = block_with(&mut body, vec![write]);
        let if_s = alloc_stmt(
            &mut body,
            StmtKind::If {
                condition: cond,
                then_block,
                else_block: None,
            },
        );

        let recv_b = local_ref(&mut body, 0);
        let read_b = field_access(&mut body, recv_b, 0);
        let let_b = let_stmt(&mut body, 2, read_b, false);

        root_with(&mut body, vec![let_a, if_s, let_b]);
        let r = build_t(&body, &[param_seed()]);
        assert_ne!(r.value_of[&read_a], r.value_of[&read_b]);
    }

    #[test]
    fn if_writing_other_field_keeps_unwritten_field_version() {
        // fn(obj) {
        //     let g0 = obj.g;
        //     if cond { obj.f = 1; }   // writes f, not g
        //     let g1 = obj.g;          // g unchanged -> same VN
        //     let f1 = obj.f;          // f written on a fall-through arm
        //     let f2 = obj.f;          // two reads at the same post-merge version share
        // }
        let mut body = empty_body();
        let recv_g0 = local_ref(&mut body, 0);
        let read_g0 = field_access(&mut body, recv_g0, 1);
        let let_g0 = let_stmt(&mut body, 1, read_g0, false);

        let cond = bool_lit(&mut body, true);
        let recv_w = local_ref(&mut body, 0);
        let one = int_lit(&mut body, 1);
        let write = field_assign_stmt(&mut body, recv_w, 0, one);
        let then_block = block_with(&mut body, vec![write]);
        let if_s = alloc_stmt(
            &mut body,
            StmtKind::If {
                condition: cond,
                then_block,
                else_block: None,
            },
        );

        let recv_g1 = local_ref(&mut body, 0);
        let read_g1 = field_access(&mut body, recv_g1, 1);
        let let_g1 = let_stmt(&mut body, 2, read_g1, false);
        let recv_f1 = local_ref(&mut body, 0);
        let read_f1 = field_access(&mut body, recv_f1, 0);
        let let_f1 = let_stmt(&mut body, 3, read_f1, false);
        let recv_f2 = local_ref(&mut body, 0);
        let read_f2 = field_access(&mut body, recv_f2, 0);
        let let_f2 = let_stmt(&mut body, 4, read_f2, false);

        root_with(&mut body, vec![let_g0, if_s, let_g1, let_f1, let_f2]);
        let r = build_t(&body, &[param_seed()]);
        // `g` untouched across the if: same VN.
        assert_eq!(r.value_of[&read_g0], r.value_of[&read_g1]);
        // `f` reads after the merge are at one fresh post-merge version.
        assert_eq!(r.value_of[&read_f1], r.value_of[&read_f2]);
        // …but distinct from the pre-if `f` value (there was none here) is
        // not asserted; the merge gave `f` a fresh version, which the two
        // post reads share.
    }

    // ----- Field store→load forwarding -----

    #[test]
    fn field_store_forwards_to_later_read() {
        // fn(obj) { obj.f = 7; let y = obj.f; }
        let mut body = empty_body();
        let recv_w = local_ref(&mut body, 0);
        let seven = int_lit(&mut body, 7);
        let write = field_assign_stmt(&mut body, recv_w, 0, seven);
        let recv_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_r, 0);
        let let_y = let_stmt(&mut body, 1, read, false);
        root_with(&mut body, vec![write, let_y]);
        let r = build_t(&body, &[param_seed()]);
        let read_v = r.value_of[&read];
        assert_eq!(r.pool.kind(read_v), &ValueKind::Int(7));
        assert_eq!(read_v, r.value_of[&seven]);
    }

    #[test]
    fn field_store_does_not_forward_after_overwrite() {
        // fn(obj) { obj.f = 7; obj.f = 9; let y = obj.f; } -> sees 9
        let mut body = empty_body();
        let recv_w1 = local_ref(&mut body, 0);
        let seven = int_lit(&mut body, 7);
        let write1 = field_assign_stmt(&mut body, recv_w1, 0, seven);
        let recv_w2 = local_ref(&mut body, 0);
        let nine = int_lit(&mut body, 9);
        let write2 = field_assign_stmt(&mut body, recv_w2, 0, nine);
        let recv_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_r, 0);
        let let_y = let_stmt(&mut body, 1, read, false);
        root_with(&mut body, vec![write1, write2, let_y]);
        let r = build_t(&body, &[param_seed()]);
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(9));
    }

    #[test]
    fn aliased_field_store_does_not_forward_across_call() {
        // fn(obj) { obj.f = 7; foo(); let y = obj.f; } with `obj`
        // reference-aliased -> the call may reach obj's object, so the read
        // re-derives (opaque).
        let mut body = empty_body();
        let recv_w = local_ref(&mut body, 0);
        let seven = int_lit(&mut body, 7);
        let write = field_assign_stmt(&mut body, recv_w, 0, seven);
        let call = call_void(&mut body);
        let call_s = alloc_stmt(&mut body, StmtKind::Expr(call));
        let recv_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_r, 0);
        let let_y = let_stmt(&mut body, 1, read, false);
        root_with(&mut body, vec![write, call_s, let_y]);
        let mut aliased = crate::hashmap::IndexSet::default();
        aliased.insert(0u32);
        let r = build_aliased(&body, &[param_seed()], &aliased);
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::FieldAccess { .. }
        ));
    }

    #[test]
    fn nonaliased_field_store_forwards_across_call() {
        // Same body with `obj` NOT aliased: `foo()` cannot reach obj's object
        // (it never escaped), so `obj.f` still forwards 7 across the call —
        // the per-(root, field) precision the global model lacked.
        let mut body = empty_body();
        let recv_w = local_ref(&mut body, 0);
        let seven = int_lit(&mut body, 7);
        let write = field_assign_stmt(&mut body, recv_w, 0, seven);
        let call = call_void(&mut body);
        let call_s = alloc_stmt(&mut body, StmtKind::Expr(call));
        let recv_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_r, 0);
        let let_y = let_stmt(&mut body, 1, read, false);
        root_with(&mut body, vec![write, call_s, let_y]);
        let r = build_t(&body, &[param_seed()]);
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(7));
    }

    #[test]
    fn struct_literal_let_seeds_field_reads() {
        // fn() { let x = S { f0: 5, f1: 6 }; let n = x.f0; } -> n == 5
        let mut body = empty_body();
        let five = int_lit(&mut body, 5);
        let six = int_lit(&mut body, 6);
        let struct_lit = alloc_expr(
            &mut body,
            ExprKind::StructLiteral {
                struct_type: TypeTable::UNIT,
                struct_name: "S".to_string(),
                fields: vec![
                    crate::nir_arena::ArenaStructField {
                        name: "f0".to_string(),
                        value: five,
                        field_index: 0,
                    },
                    crate::nir_arena::ArenaStructField {
                        name: "f1".to_string(),
                        value: six,
                        field_index: 1,
                    },
                ],
            },
        );
        let let_x = let_stmt(&mut body, 0, struct_lit, false);
        let recv = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv, 0);
        let let_n = let_stmt(&mut body, 1, read, false);
        root_with(&mut body, vec![let_x, let_n]);
        let r = build_t(&body, &[]);
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(5));
    }

    /// `let v = S { f0: 7 }` then a one-field struct literal helper.
    fn one_field_struct(body: &mut Body, value: ExprId) -> ExprId {
        alloc_expr(
            body,
            ExprKind::StructLiteral {
                struct_type: TypeTable::UNIT,
                struct_name: "S".to_string(),
                fields: vec![crate::nir_arena::ArenaStructField {
                    name: "f0".to_string(),
                    value,
                    field_index: 0,
                }],
            },
        )
    }

    fn ref_of_local(body: &mut Body, idx: u32) -> ExprId {
        let inner = local_ref(body, idx);
        alloc_expr(
            body,
            ExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr: inner,
            },
        )
    }

    #[test]
    fn reference_lookthrough_forwards_pointee_field() {
        // fn() { let v = S { f0: 7 }; let r = &v; let y = r.f0; }
        // `r = &v`, so `r.f0` forwards from `v`'s seeded field slot.
        let mut body = empty_body();
        let seven = int_lit(&mut body, 7);
        let struct_lit = one_field_struct(&mut body, seven);
        let let_v = let_stmt(&mut body, 0, struct_lit, false);
        let ref_v = ref_of_local(&mut body, 0);
        let let_r = let_stmt(&mut body, 1, ref_v, false);
        let recv_r = local_ref(&mut body, 1);
        let read = field_access(&mut body, recv_r, 0);
        let let_y = let_stmt(&mut body, 2, read, false);
        root_with(&mut body, vec![let_v, let_r, let_y]);
        let r = build_t(&body, &[]);
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(7));
    }

    #[test]
    fn reference_lookthrough_dropped_when_pointee_reassigned() {
        // fn() { let mut v = S { f0: 7 }; let r = &v; v = S { f0: 9 }; let y = r.f0; }
        // Reassigning `v` clears `r`'s look-through (the old pointee may have
        // moved), so `r.f0` re-derives rather than forwarding the stale 7.
        let mut body = empty_body();
        let seven = int_lit(&mut body, 7);
        let struct_lit = one_field_struct(&mut body, seven);
        let let_v = let_stmt(&mut body, 0, struct_lit, true);
        let ref_v = ref_of_local(&mut body, 0);
        let let_r = let_stmt(&mut body, 1, ref_v, false);
        let nine = int_lit(&mut body, 9);
        let struct_lit2 = one_field_struct(&mut body, nine);
        let reassign = assign_stmt(&mut body, 0, struct_lit2);
        let recv_r = local_ref(&mut body, 1);
        let read = field_access(&mut body, recv_r, 0);
        let let_y = let_stmt(&mut body, 2, read, false);
        root_with(&mut body, vec![let_v, let_r, reassign, let_y]);
        let r = build_t(&body, &[]);
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::FieldAccess { .. }
        ));
    }

    #[test]
    fn block_wrapped_struct_literal_let_seeds_field_reads() {
        // fn(limit) { let x = { let n = limit + 1; S { f0: n } }; let m = x.f0; }
        // The block tail is the sole producer, so x.f0 forwards n's value
        // (`limit + 1`) — the constructor-inlining shape.
        let mut body = empty_body();
        let limit_read = local_ref(&mut body, 0);
        let one = int_lit(&mut body, 1);
        let n_value = binary(&mut body, NirBinaryOp::Add, limit_read, one);
        let let_n = let_stmt(&mut body, 1, n_value, false);
        let n_read = local_ref(&mut body, 1);
        let struct_lit = alloc_expr(
            &mut body,
            ExprKind::StructLiteral {
                struct_type: TypeTable::UNIT,
                struct_name: "S".to_string(),
                fields: vec![crate::nir_arena::ArenaStructField {
                    name: "f0".to_string(),
                    value: n_read,
                    field_index: 0,
                }],
            },
        );
        let tail_stmt = alloc_stmt(&mut body, StmtKind::Expr(struct_lit));
        let inner_block = block_with(&mut body, vec![let_n, tail_stmt]);
        let block_expr = alloc_expr(&mut body, ExprKind::Block(inner_block));
        let let_x = let_stmt(&mut body, 2, block_expr, false);
        let recv = local_ref(&mut body, 2);
        let read = field_access(&mut body, recv, 0);
        let let_m = let_stmt(&mut body, 3, read, false);
        root_with(&mut body, vec![let_x, let_m]);
        let param = NirParam {
            name: "limit".to_string(),
            type_id: TypeTable::I32,
            local_index: 0,
            is_mut: false,
            span: Span::default(),
        };
        let r = build_t(&body, &[param]);
        // x.f0 forwards n = limit + 1.
        assert_eq!(r.value_of[&read], r.value_of[&n_value]);
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::Binary {
                op: NirBinaryOp::Add,
                ..
            }
        ));
    }

    #[test]
    fn aliased_receiver_is_seeded_without_intervening_write() {
        // fn(obj) { obj.f = 7; let y = obj.f; } — even if `obj` is
        // reference-aliased, no heap write intervenes, so `obj.f` forwards 7.
        // A later aliased write would invalidate it (see the soundness test
        // `aliased_other_receiver_same_field_blocks_forward`).
        let mut body = empty_body();
        let recv_w = local_ref(&mut body, 0);
        let seven = int_lit(&mut body, 7);
        let write = field_assign_stmt(&mut body, recv_w, 0, seven);
        let recv_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_r, 0);
        let let_y = let_stmt(&mut body, 1, read, false);
        root_with(&mut body, vec![write, let_y]);
        let r = build_t(&body, &[param_seed()]);
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(7));
    }

    // ----- Loop-entry value snapshots -----

    #[test]
    fn loop_entry_values_distinguish_written_locals_from_params() {
        // fn(n) { let mut i = 0; loop { let s = i + n; i = i + 1; } }
        // In-body reads: `n` (unwritten param) matches its pre-header entry
        // value; `i` (loop-written) does not — the entry snapshot keeps the
        // pre-loop Int(0) while the body reads the fresh loop opaque.
        let mut body = empty_body();
        let zero = int_lit(&mut body, 0);
        let let_i = let_stmt(&mut body, 1, zero, true);
        let i_read = local_ref(&mut body, 1);
        let n_read = local_ref(&mut body, 0);
        let sum = binary(&mut body, NirBinaryOp::Add, i_read, n_read);
        let let_s = let_stmt(&mut body, 2, sum, false);
        let i_read2 = local_ref(&mut body, 1);
        let one = int_lit(&mut body, 1);
        let plus = binary(&mut body, NirBinaryOp::Add, i_read2, one);
        let assign = assign_stmt(&mut body, 1, plus);
        let lb = block_with(&mut body, vec![let_s, assign]);
        let loop_s = alloc_stmt(&mut body, StmtKind::Loop { body: lb });
        root_with(&mut body, vec![let_i, loop_s]);
        let n = NirParam {
            name: "n".to_string(),
            type_id: TypeTable::I32,
            local_index: 0,
            is_mut: false,
            span: Span::default(),
        };
        let r = build_t(&body, &[n]);
        let entries = &r.loop_entry_values[&lb];
        assert_eq!(entries.get(&0).copied(), Some(r.value_of[&n_read]));
        assert_eq!(r.pool.kind(entries[&1]), &ValueKind::Int(0));
        assert_ne!(entries[&1], r.value_of[&i_read]);
    }

    #[test]
    fn loop_seeded_constant_field_read_folds() {
        // fn(obj) { loop { obj.f = 1; let x = obj.f; } }
        // The store seeds obj.f = 1 within the iteration, so the read
        // folds to the literal even inside a loop body.
        let mut body = empty_body();
        let recv_w = local_ref(&mut body, 0);
        let one = int_lit(&mut body, 1);
        let write = field_assign_stmt(&mut body, recv_w, 0, one);
        let recv_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_r, 0);
        let let_x = let_stmt(&mut body, 1, read, false);
        let lb = block_with(&mut body, vec![write, let_x]);
        let loop_s = alloc_stmt(&mut body, StmtKind::Loop { body: lb });
        root_with(&mut body, vec![loop_s]);
        let r = build_t(&body, &[param_seed()]);
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(1));
    }

    #[test]
    fn nonaliased_field_survives_external_call_loop() {
        // fn(obj) { obj.f = 7; loop { foo(); }; let y = obj.f; }
        // The loop writes no field of `obj` (only an opaque call), and `obj`
        // is non-aliased, so the loop's external-write invalidation skips it:
        // obj.f still forwards 7 after the loop.
        let mut body = empty_body();
        let recv_w = local_ref(&mut body, 0);
        let seven = int_lit(&mut body, 7);
        let write = field_assign_stmt(&mut body, recv_w, 0, seven);
        let call = call_void(&mut body);
        let call_s = alloc_stmt(&mut body, StmtKind::Expr(call));
        let lb = block_with(&mut body, vec![call_s]);
        let loop_s = alloc_stmt(&mut body, StmtKind::Loop { body: lb });
        let recv_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_r, 0);
        let let_y = let_stmt(&mut body, 1, read, false);
        root_with(&mut body, vec![write, loop_s, let_y]);
        let r = build_t(&body, &[param_seed()]);
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(7));
    }

    #[test]
    fn aliased_field_dropped_by_external_call_loop() {
        // Same shape with `obj` reference-aliased: the loop's opaque call may
        // reach obj's object, so obj.f re-derives after the loop.
        let mut body = empty_body();
        let recv_w = local_ref(&mut body, 0);
        let seven = int_lit(&mut body, 7);
        let write = field_assign_stmt(&mut body, recv_w, 0, seven);
        let call = call_void(&mut body);
        let call_s = alloc_stmt(&mut body, StmtKind::Expr(call));
        let lb = block_with(&mut body, vec![call_s]);
        let loop_s = alloc_stmt(&mut body, StmtKind::Loop { body: lb });
        let recv_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_r, 0);
        let let_y = let_stmt(&mut body, 1, read, false);
        root_with(&mut body, vec![write, loop_s, let_y]);
        let mut aliased = crate::hashmap::IndexSet::default();
        aliased.insert(0u32);
        let r = build_aliased(&body, &[param_seed()], &aliased);
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::FieldAccess { .. }
        ));
    }

    #[test]
    fn field_written_in_loop_does_not_survive() {
        // fn(obj) { obj.f = 7; loop { obj.f = 8; }; let y = obj.f; }
        // The loop writes obj.f, so the pre-loop 7 is invalidated and the
        // post-loop read re-derives (the body may have run, leaving 8).
        let mut body = empty_body();
        let recv_w = local_ref(&mut body, 0);
        let seven = int_lit(&mut body, 7);
        let write = field_assign_stmt(&mut body, recv_w, 0, seven);
        let recv_lw = local_ref(&mut body, 0);
        let eight = int_lit(&mut body, 8);
        let loop_write = field_assign_stmt(&mut body, recv_lw, 0, eight);
        let lb = block_with(&mut body, vec![loop_write]);
        let loop_s = alloc_stmt(&mut body, StmtKind::Loop { body: lb });
        let recv_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_r, 0);
        let let_y = let_stmt(&mut body, 1, read, false);
        root_with(&mut body, vec![write, loop_s, let_y]);
        let r = build_t(&body, &[param_seed()]);
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::FieldAccess { .. }
        ));
    }

    #[test]
    fn nonaliased_other_receiver_same_field_forwards() {
        // fn(a, b) { a.f = 1; b.f = 5; let y = a.f; }
        // `a` and `b` are distinct non-aliased objects, so `b.f = 5` bumps
        // only `b`'s per-slot generation; `a.f` forwards 1 (the same-layout
        // precision the global per-field model lost).
        let mut body = empty_body();
        let recv_a_w = local_ref(&mut body, 0);
        let one = int_lit(&mut body, 1);
        let write_a = field_assign_stmt(&mut body, recv_a_w, 0, one);
        let recv_b_w = local_ref(&mut body, 1);
        let five = int_lit(&mut body, 5);
        let write_b = field_assign_stmt(&mut body, recv_b_w, 0, five);
        let recv_a_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_a_r, 0);
        let let_y = let_stmt(&mut body, 2, read, false);
        root_with(&mut body, vec![write_a, write_b, let_y]);
        let r = build_t(&body, &[param_seed()]);
        assert_eq!(r.pool.kind(r.value_of[&read]), &ValueKind::Int(1));
    }

    #[test]
    fn aliased_other_receiver_same_field_blocks_forward() {
        // fn(a, b) { a.f = 1; b.f = 5; let y = a.f; } with `b` reference-
        // aliased (e.g. `b = &a`). A write through an aliased receiver bumps
        // the field globally — every alias of `f` is invalidated — so `a.f`
        // re-derives instead of forwarding the stale 1. This is the soundness
        // guarantee the per-slot precision relies on.
        let mut body = empty_body();
        let recv_a_w = local_ref(&mut body, 0);
        let one = int_lit(&mut body, 1);
        let write_a = field_assign_stmt(&mut body, recv_a_w, 0, one);
        let recv_b_w = local_ref(&mut body, 1);
        let five = int_lit(&mut body, 5);
        let write_b = field_assign_stmt(&mut body, recv_b_w, 0, five);
        let recv_a_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_a_r, 0);
        let let_y = let_stmt(&mut body, 2, read, false);
        root_with(&mut body, vec![write_a, write_b, let_y]);
        let mut aliased = crate::hashmap::IndexSet::default();
        aliased.insert(1u32);
        let r = build_aliased(&body, &[param_seed()], &aliased);
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::FieldAccess { .. }
        ));
    }
}
