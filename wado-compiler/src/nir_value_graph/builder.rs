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
//! - `Match` / `Switch` walk every arm and merge n-ary: if every arm agrees
//!   on a local, that value carries; otherwise the local goes `Opaque`.
//!   N-ary `Select` chains are not yet constructed.
//! - `Loop` pre-scans the body for locals it may write and reassigns each
//!   to a fresh `Opaque` before walking the body; post-loop those locals
//!   stay `Opaque`. Stage 6 swaps recurring-pattern locals to `LoopPhi`.
//! - `LabeledBlock` marks every local written in its subtree `Opaque` on
//!   exit, since `break` paths can carry writes the fall-through state
//!   never observes.
//! - Pattern bindings (`LetDestructure`, `Match` arm bindings) are seeded
//!   with `Opaque`.

use crate::hashmap::IndexMap;
use crate::nir::{NirBinaryOp, NirParam, NirUnaryOp};
use crate::nir_arena::{
    ArmData, BlockId, Body, ExprId, ExprKind, NodeRef, PatId, PatKind, StmtId, StmtKind,
};

use super::{HeapVersion, ValueId, ValueKind, ValuePool};

/// Per-function heap-version tracker. The builder threads one `HeapState`
/// through the walk; on every Skel node that may write the heap, the
/// appropriate field's version (or every field's version, for opaque
/// writes) bumps to a fresh value.
///
/// Granularity is per `field_index`: a direct write to `obj.f` bumps the
/// `f` slot only, so a later read of `obj.g` keeps its prior version and
/// shares a `ValueId` with earlier reads. Opaque writes (Call,
/// `Index` / `Deref` assign target, Loop entry, branch merge) call
/// [`HeapState::bump_all`], invalidating every field. Refinement to
/// `(receiver_root, field)` granularity via `mod_ref.rs` is a follow-up.
struct HeapState {
    /// Next fresh version to hand out.
    next: HeapVersion,
    /// Current version of each `field_index` we have seen written.
    per_field: IndexMap<u32, HeapVersion>,
    /// Version returned for `field_indices` not yet in `per_field`.
    /// `bump_all` advances this; `bump_field` does not.
    default_version: HeapVersion,
}

impl HeapState {
    fn new() -> Self {
        Self {
            next: HeapVersion::INITIAL.bump(),
            per_field: IndexMap::default(),
            default_version: HeapVersion::INITIAL,
        }
    }

    fn fresh(&mut self) -> HeapVersion {
        let v = self.next;
        self.next = self.next.bump();
        v
    }

    fn version_of(&self, field_index: u32) -> HeapVersion {
        self.per_field
            .get(&field_index)
            .copied()
            .unwrap_or(self.default_version)
    }

    fn bump_field(&mut self, field_index: u32) {
        let v = self.fresh();
        self.per_field.insert(field_index, v);
    }

    fn bump_all(&mut self) {
        let v = self.fresh();
        self.per_field.clear();
        self.default_version = v;
    }

    /// Snapshot the read-visible state (`per_field`, `default_version`)
    /// only. `next` is a monotonic counter shared across the whole
    /// function, so arms restored from the snapshot never reuse a version
    /// another arm allocated.
    fn snapshot(&self) -> HeapSnapshot {
        HeapSnapshot {
            per_field: self.per_field.clone(),
            default_version: self.default_version,
        }
    }

    fn restore(&mut self, snap: HeapSnapshot) {
        self.per_field = snap.per_field;
        self.default_version = snap.default_version;
    }
}

#[derive(Clone)]
struct HeapSnapshot {
    per_field: IndexMap<u32, HeapVersion>,
    default_version: HeapVersion,
}

impl HeapSnapshot {
    fn version_of(&self, field_index: u32) -> HeapVersion {
        self.per_field
            .get(&field_index)
            .copied()
            .unwrap_or(self.default_version)
    }
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
    /// Per-loop variance thresholds, keyed by the loop body's `BlockId`.
    /// Consulted by [`ValueGraphBuild::is_loop_invariant`]; see [`LoopScope`].
    pub loop_scopes: IndexMap<BlockId, LoopScope>,
}

/// Variance thresholds the builder records for one loop, so a consumer can
/// ask whether a `ValueId` is loop-invariant without rebuilding a
/// `ModifiedVars`-style analysis.
///
/// A value is loop-*variant* (changes across iterations) iff it
/// transitively depends on a loop-written local — an `Opaque` minted at the
/// loop's entry (the reassigned-local placeholder) or during the body walk —
/// or on a field the body writes — a `FieldAccess` at a heap version the body
/// produced. Everything else (parameters, pre-loop locals, literals,
/// arithmetic over invariant operands, reads of unwritten fields) is
/// invariant. See [`ValueGraphBuild::is_loop_invariant`].
#[derive(Clone, Copy, Debug)]
pub struct LoopScope {
    /// `ValueId`s whose index is `>=` this were minted at the loop's entry
    /// (reassigned-local opaques) or during the body; an `Opaque` among them
    /// varies across iterations.
    value_threshold: u32,
    /// `FieldAccess` reads at a `heap_ver` index `>=` this were written by
    /// the body. The entry `bump_all` sits just below it, so a field the body
    /// never writes keeps a sub-threshold version and stays invariant.
    version_threshold: u32,
}

impl ValueGraphBuild {
    /// Whether `v` is loop-invariant for the loop whose body is `loop_body`.
    /// Returns `false` (conservatively variant) for an unrecorded
    /// `loop_body`, so a caller that hoists on `true` never moves a varying
    /// value.
    pub fn is_loop_invariant(&self, loop_body: BlockId, v: ValueId) -> bool {
        let Some(&scope) = self.loop_scopes.get(&loop_body) else {
            return false;
        };
        let mut memo: IndexMap<ValueId, bool> = IndexMap::default();
        !self.is_variant(scope, v, &mut memo)
    }

    fn is_variant(&self, scope: LoopScope, v: ValueId, memo: &mut IndexMap<ValueId, bool>) -> bool {
        if let Some(&cached) = memo.get(&v) {
            return cached;
        }
        // Insert a provisional `false` so a (degenerate) cyclic reference
        // terminates; the MVP graph is acyclic (`LoopPhi` is handled below
        // without recursing), so this only guards against future kinds.
        memo.insert(v, false);
        let result = match self.pool.kind(v) {
            ValueKind::Opaque(_) => v.index() >= scope.value_threshold,
            ValueKind::FieldAccess {
                receiver, heap_ver, ..
            } => {
                let (receiver, heap_ver) = (*receiver, *heap_ver);
                heap_ver.index() >= scope.version_threshold
                    || self.is_variant(scope, receiver, memo)
            }
            ValueKind::Binary { lhs, rhs, .. } => {
                let (lhs, rhs) = (*lhs, *rhs);
                self.is_variant(scope, lhs, memo) || self.is_variant(scope, rhs, memo)
            }
            ValueKind::Unary { operand, .. } => {
                let operand = *operand;
                self.is_variant(scope, operand, memo)
            }
            ValueKind::Cast { operand, .. } => {
                let operand = *operand;
                self.is_variant(scope, operand, memo)
            }
            ValueKind::Select { cond, then, else_ } => {
                let (cond, then, else_) = (*cond, *then, *else_);
                self.is_variant(scope, cond, memo)
                    || self.is_variant(scope, then, memo)
                    || self.is_variant(scope, else_, memo)
            }
            // A loop recurrence varies by definition.
            ValueKind::LoopPhi { .. } => true,
            // Literals are invariant.
            ValueKind::Int(_)
            | ValueKind::Float(_)
            | ValueKind::Bool(_)
            | ValueKind::Char(_)
            | ValueKind::String(_)
            | ValueKind::Null
            | ValueKind::Unit => false,
        };
        memo.insert(v, result);
        result
    }
}

/// Build the `ValueGraph` for one function body.
///
/// `params` seed `current_value` with one fresh `Opaque` per parameter, so a
/// `Local { index: param.local_index }` read returns that Opaque every time
/// until the parameter is reassigned (which the builder picks up the same
/// way as any other `Assign`).
///
/// `alias_unsafe` are locals whose object is reference-aliased (the caller's
/// `address_taken_locals` / `stores_aliased_locals`, e.g. the `with stores[p]`
/// effect's `p`). The builder unions them with a body scan for live
/// `&local` / `&mut local` and suppresses field store→load seeding on those
/// receivers, matching `store_load_forward`'s own exclusion so the `stores`
/// effect's "no field forwarding for aliased locals" contract is upheld.
pub fn build(
    body: &Body,
    params: &[NirParam],
    alias_unsafe: &crate::hashmap::IndexSet<u32>,
) -> ValueGraphBuild {
    let mut b = Builder::new(body, alias_unsafe);
    b.seed_params(params);
    b.walk_block(body.root);
    ValueGraphBuild {
        pool: b.pool,
        value_of: b.value_of,
        literal_source: b.literal_source,
        loop_scopes: b.loop_scopes,
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
    /// Store→load forwarding for fields: the value most recently stored to
    /// `(receiver ValueId, field_index, HeapVersion)`. A `FieldAccess` read
    /// at the same triple returns the stored value's `ValueId` directly
    /// instead of interning an opaque `FieldAccess` kind. Keys carry the
    /// version, which is monotonic and never reused, so stale entries are
    /// simply never hit: any write to that field — on any receiver, since the
    /// heap model is per-field — bumps the version, and a branch join gives
    /// the field a fresh version, so a post-store read forwards only while
    /// the store provably reaches it.
    field_store: IndexMap<(ValueId, u32, HeapVersion), ValueId>,
    /// Locals whose object is reference-aliased; field seeding is suppressed
    /// for them (see [`build`]).
    alias_unsafe: crate::hashmap::IndexSet<u32>,
    /// Per-loop variance thresholds recorded by [`Self::walk_loop`]. See
    /// [`LoopScope`] / [`ValueGraphBuild::is_loop_invariant`].
    loop_scopes: IndexMap<BlockId, LoopScope>,
}

impl<'a> Builder<'a> {
    fn new(body: &'a Body, alias_unsafe: &crate::hashmap::IndexSet<u32>) -> Self {
        let mut unsafe_locals = alias_unsafe.clone();
        collect_address_taken_in_block(body, body.root, &mut unsafe_locals);
        Self {
            body,
            pool: ValuePool::new(),
            value_of: IndexMap::default(),
            current_value: IndexMap::default(),
            heap_state: HeapState::new(),
            literal_source: IndexMap::default(),
            field_store: IndexMap::default(),
            alias_unsafe: unsafe_locals,
            loop_scopes: IndexMap::default(),
        }
    }

    fn seed_params(&mut self, params: &[NirParam]) {
        for param in params {
            let opaque = self.pool.fresh_opaque();
            self.current_value.insert(param.local_index, opaque);
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
                // Skipped when `x` is reference-aliased.
                if !self.alias_unsafe.contains(&local_index) {
                    self.seed_struct_literal_fields(v, value);
                }
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
                // Capture the receiver's `ValueId` (and whether it is a
                // seed-safe bare `Local`) for a `obj.f = …` store, so the
                // post-bump version can be seeded for store→load forwarding.
                let recv_seed = match &target_kind {
                    ExprKind::Local { .. } => None,
                    ExprKind::FieldAccess { expr: recv, .. } => {
                        let recv_local = match &self.body.exprs[*recv].kind {
                            ExprKind::Local { index, .. } => Some(*index),
                            _ => None,
                        };
                        let recv_v = self.walk_expr(*recv);
                        // Seed only a bare, non-aliased `Local` receiver; an
                        // aliased object (`with stores[p]`) or a deeper place
                        // (`a.b.f = …`) is left un-seeded so a later read
                        // re-derives an opaque `FieldAccess`.
                        match (recv_v, recv_local) {
                            (Some(rv), Some(idx)) if !self.alias_unsafe.contains(&idx) => Some(rv),
                            _ => None,
                        }
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
                    }
                    ExprKind::FieldAccess { field_index, .. } => {
                        self.heap_state.bump_field(field_index);
                        if let Some(recv_v) = recv_seed {
                            let ver = self.heap_state.version_of(field_index);
                            self.field_store.insert((recv_v, field_index, ver), v);
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
                let recv = self.walk_expr(inner)?;
                let heap_ver = self.heap_state.version_of(field_index);
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
                self.heap_state.bump_all();
                None
            }
            ExprKind::CmRawCall { args, .. } => {
                for a in args {
                    self.walk_expr(a);
                }
                self.heap_state.bump_all();
                None
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.walk_expr(receiver);
                for a in args {
                    self.walk_expr(a.expr);
                }
                self.heap_state.bump_all();
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

    /// Seed the field-store map from a `let x = S { f: v, … }` binding, where
    /// `recv` is `x`'s (fresh-opaque) `ValueId` and `value_expr` is the bound
    /// expression. Each pure field value is seeded at the current version of
    /// its field, so a later `x.f` read forwards it.
    ///
    /// Unlabeled `Block` wrappers are peered through to their trailing
    /// expression — the shape constructor inlining leaves behind
    /// (`let arr = { let n = …; …; List { repr, used: n } }`); an unlabeled
    /// block has no break target, so the tail is its sole producer.
    /// `LabeledBlock` wrappers (whose value exits via `break label:`) are a
    /// follow-up; missing them only costs forwarding, never soundness.
    fn seed_struct_literal_fields(&mut self, recv: ValueId, value_expr: ExprId) {
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
                let ver = self.heap_state.version_of(field_index);
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

    /// Join the heap state at a branch endpoint over the fall-through arms.
    ///
    /// `pre` is the heap state before the branch; each `arms` entry is an
    /// arm's post-walk snapshot paired with whether the arm falls through
    /// (a `break` / `return` / `continue`-terminated arm does not, so its
    /// writes never reach code after the branch). A field whose version
    /// every fall-through arm left at the pre-branch version keeps it; a
    /// field some fall-through arm wrote (or that an arm's `bump_all`
    /// invalidated) gets a fresh version, since its post-merge value is
    /// unknown. With no fall-through arm the post-state is `pre` (code
    /// after the branch is unreachable).
    ///
    /// This replaces the previous unconditional `bump_all`, which split
    /// the heap version of every field read after any branch — including
    /// the `if !(cond) { break }` guard that opens every desugared loop,
    /// so `arr.used` read in the guard never shared a `ValueId` with a
    /// later bounds-check read.
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
        let mut field_keys: crate::hashmap::IndexSet<u32> = crate::hashmap::IndexSet::default();
        for k in pre.per_field.keys() {
            field_keys.insert(*k);
        }
        for a in &live {
            for k in a.per_field.keys() {
                field_keys.insert(*k);
            }
        }
        let mut new_per_field: IndexMap<u32, HeapVersion> = IndexMap::default();
        for f in field_keys {
            let pre_v = pre.version_of(f);
            let unchanged = live.iter().all(|a| a.version_of(f) == pre_v);
            if unchanged {
                // Only fields differing from the default need an explicit
                // entry; a default-changed join must pin survivors so they
                // are not read at the fresh default.
                if pre_v != new_default {
                    new_per_field.insert(f, pre_v);
                }
            } else {
                let fresh = self.heap_state.fresh();
                new_per_field.insert(f, fresh);
            }
        }
        self.heap_state.per_field = new_per_field;
        self.heap_state.default_version = new_default;
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
            StmtKind::LabeledBlock { block, .. } => self.block_falls_through(*block),
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

    /// Reassign every local the body may write to a fresh `Opaque` before
    /// and after the walk, and bump the heap state on both sides. The body
    /// may run 0..N times, so in-body reads must not share `ValueId`s
    /// with pre-loop reads, and post-loop reads must not share them with
    /// in-body reads. (Locals declared inside the loop need no pre-seed:
    /// they get fresh `Opaque`s as the body walks.)
    fn walk_loop(&mut self, body_block: crate::nir_arena::BlockId) {
        let mut writes: crate::hashmap::IndexSet<u32> = crate::hashmap::IndexSet::default();
        collect_writes_in_block(self.body, body_block, &mut writes);
        // Variance threshold for values: anything minted from here on (the
        // entry opaques below, plus everything the body walk interns) is a
        // candidate loop-variant — but only `Opaque`s actually vary; see
        // `is_variant`.
        let value_threshold = self.pool.len() as u32;
        for idx in &writes {
            if self.current_value.contains_key(idx) {
                let opaque = self.pool.fresh_opaque();
                self.current_value.insert(*idx, opaque);
            }
        }
        self.heap_state.bump_all();
        // Variance threshold for heap: the entry `bump_all` above gives every
        // field this version; a field the body writes bumps past it, so
        // `heap_ver >= version_threshold` ⟺ "written by the body".
        let version_threshold = self.heap_state.next.index();
        self.loop_scopes.insert(
            body_block,
            LoopScope {
                value_threshold,
                version_threshold,
            },
        );
        self.walk_block(body_block);
        for idx in &writes {
            if self.current_value.contains_key(idx) {
                let opaque = self.pool.fresh_opaque();
                self.current_value.insert(*idx, opaque);
            }
        }
        self.heap_state.bump_all();
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

/// Scan `block`'s subtree for `&local` / `&mut local` (`Unary::Ref` /
/// `Unary::MutRef` over a `Local`) and insert every targeted local into
/// `out`. Mirrors `store_load_forward::collect_address_taken_in_body`: the
/// canonical `address_taken_locals` / `stores_aliased_locals` sets go stale
/// after `inline` / `ref_elim` copy reference nodes, so this body scan
/// catches the transient post-inline aliases (the `Holder { pair: &p }`
/// shape an inlined `with stores[p]` callee leaves behind). Used to suppress
/// field store→load seeding on aliased receivers.
fn collect_address_taken_in_block(
    body: &Body,
    block: crate::nir_arena::BlockId,
    out: &mut crate::hashmap::IndexSet<u32>,
) {
    collect_address_taken_node(body, NodeRef::Block(block), out);
}

fn collect_address_taken_node(body: &Body, node: NodeRef, out: &mut crate::hashmap::IndexSet<u32>) {
    if let NodeRef::Expr(id) = node
        && let ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } = &body.exprs[id].kind
        && let ExprKind::Local { index, .. } = &body.exprs[*inner].kind
    {
        out.insert(*index);
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_address_taken_node(body, c, out);
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

    /// `build` with no reference-aliased locals — the common test case.
    fn build_t(body: &Body, params: &[NirParam]) -> ValueGraphBuild {
        build(body, params, &crate::hashmap::IndexSet::default())
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
    fn call_invalidates_all_fields() {
        // fn(obj) {
        //     let a = obj.f;
        //     foo();
        //     let a2 = obj.f;  // bump_all invalidates -> distinct VN
        // }
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
        let r = build_t(&body, &[param_seed()]);
        assert_ne!(r.value_of[&read1], r.value_of[&read2]);
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
    fn field_store_does_not_forward_across_call() {
        // fn(obj) { obj.f = 7; foo(); let y = obj.f; } -> opaque read
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
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::FieldAccess { .. }
        ));
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
    fn aliased_receiver_is_not_seeded() {
        // fn(obj) { obj.f = 7; let y = obj.f; } but `obj` (local 0) is
        // reference-aliased → no forwarding; the read stays opaque.
        let mut body = empty_body();
        let recv_w = local_ref(&mut body, 0);
        let seven = int_lit(&mut body, 7);
        let write = field_assign_stmt(&mut body, recv_w, 0, seven);
        let recv_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_r, 0);
        let let_y = let_stmt(&mut body, 1, read, false);
        root_with(&mut body, vec![write, let_y]);
        let mut aliased = crate::hashmap::IndexSet::default();
        aliased.insert(0u32);
        let r = build(&body, &[param_seed()], &aliased);
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::FieldAccess { .. }
        ));
    }

    // ----- Loop-invariance query -----

    #[test]
    fn loop_invariant_param_field_read() {
        // fn(obj) { loop { let x = obj.f; } }  -- obj.f not written -> invariant
        let mut body = empty_body();
        let recv = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv, 0);
        let let_x = let_stmt(&mut body, 1, read, false);
        let lb = block_with(&mut body, vec![let_x]);
        let loop_s = alloc_stmt(&mut body, StmtKind::Loop { body: lb });
        root_with(&mut body, vec![loop_s]);
        let r = build_t(&body, &[param_seed()]);
        let vid = r.value_of[&read];
        assert!(r.is_loop_invariant(lb, vid));
    }

    #[test]
    fn loop_body_call_makes_field_read_variant() {
        // fn(obj) { loop { foo(); let x = obj.f; } }
        // The call may mutate obj.f (bump_all), so the read is at a
        // body-produced heap version -> variant. (A `obj.f = lit; … obj.f`
        // would instead fold to that literal and be correctly invariant —
        // the seeded-constant case.)
        let mut body = empty_body();
        let call = call_void(&mut body);
        let call_s = alloc_stmt(&mut body, StmtKind::Expr(call));
        let recv_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_r, 0);
        let let_x = let_stmt(&mut body, 1, read, false);
        let lb = block_with(&mut body, vec![call_s, let_x]);
        let loop_s = alloc_stmt(&mut body, StmtKind::Loop { body: lb });
        root_with(&mut body, vec![loop_s]);
        let r = build_t(&body, &[param_seed()]);
        let vid = r.value_of[&read];
        assert!(!r.is_loop_invariant(lb, vid));
    }

    #[test]
    fn loop_seeded_constant_field_read_is_invariant() {
        // fn(obj) { loop { obj.f = 1; let x = obj.f; } }
        // The store seeds obj.f = 1, so the read folds to the literal 1,
        // which is loop-invariant (always 1 within the loop).
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
        let vid = r.value_of[&read];
        assert_eq!(r.pool.kind(vid), &ValueKind::Int(1));
        assert!(r.is_loop_invariant(lb, vid));
    }

    #[test]
    fn loop_induction_local_read_is_variant() {
        // fn() { let mut i = 0; loop { let j = i; i = i + 1; } } -- j reads i (variant)
        let mut body = empty_body();
        let zero = int_lit(&mut body, 0);
        let let_i = let_stmt(&mut body, 0, zero, true);
        let i_read = local_ref(&mut body, 0);
        let let_j = let_stmt(&mut body, 1, i_read, false);
        let i_read2 = local_ref(&mut body, 0);
        let one = int_lit(&mut body, 1);
        let plus = binary(&mut body, NirBinaryOp::Add, i_read2, one);
        let assign = assign_stmt(&mut body, 0, plus);
        let lb = block_with(&mut body, vec![let_j, assign]);
        let loop_s = alloc_stmt(&mut body, StmtKind::Loop { body: lb });
        root_with(&mut body, vec![let_i, loop_s]);
        let r = build_t(&body, &[]);
        let vid = r.value_of[&i_read];
        assert!(!r.is_loop_invariant(lb, vid));
    }

    #[test]
    fn loop_invariant_arithmetic_of_params() {
        // fn(n, m) { loop { let s = n + m; } } -- both params -> invariant
        let mut body = empty_body();
        let n_read = local_ref(&mut body, 0);
        let m_read = local_ref(&mut body, 1);
        let sum = binary(&mut body, NirBinaryOp::Add, n_read, m_read);
        let let_s = let_stmt(&mut body, 2, sum, false);
        let lb = block_with(&mut body, vec![let_s]);
        let loop_s = alloc_stmt(&mut body, StmtKind::Loop { body: lb });
        root_with(&mut body, vec![loop_s]);
        let n = NirParam {
            name: "n".to_string(),
            type_id: TypeTable::I32,
            local_index: 0,
            is_mut: false,
            span: Span::default(),
        };
        let m = NirParam {
            name: "m".to_string(),
            type_id: TypeTable::I32,
            local_index: 1,
            is_mut: false,
            span: Span::default(),
        };
        let r = build_t(&body, &[n, m]);
        let vid = r.value_of[&sum];
        assert!(r.is_loop_invariant(lb, vid));
    }

    #[test]
    fn loop_arithmetic_with_induction_is_variant() {
        // fn(n) { let mut i = 0; loop { let s = i + n; i = i + 1; } }
        // `i + n` mixes the induction variable -> variant.
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
        let vid = r.value_of[&sum];
        assert!(!r.is_loop_invariant(lb, vid));
    }

    #[test]
    fn body_scanned_address_taken_receiver_is_not_seeded() {
        // fn(obj) { obj.f = 7; let r = &obj; let y = obj.f; }
        // The live `&obj` makes `obj` address-taken by the builder's own
        // body scan, so the store is not seeded even without a passed set.
        let mut body = empty_body();
        let recv_w = local_ref(&mut body, 0);
        let seven = int_lit(&mut body, 7);
        let write = field_assign_stmt(&mut body, recv_w, 0, seven);
        let obj_ref_inner = local_ref(&mut body, 0);
        let obj_ref = alloc_expr(
            &mut body,
            ExprKind::Unary {
                op: NirUnaryOp::Ref,
                expr: obj_ref_inner,
            },
        );
        let let_r = let_stmt(&mut body, 1, obj_ref, false);
        let recv_r = local_ref(&mut body, 0);
        let read = field_access(&mut body, recv_r, 0);
        let let_y = let_stmt(&mut body, 2, read, false);
        root_with(&mut body, vec![write, let_r, let_y]);
        let r = build_t(&body, &[param_seed()]);
        assert!(matches!(
            r.pool.kind(r.value_of[&read]),
            ValueKind::FieldAccess { .. }
        ));
    }
}
