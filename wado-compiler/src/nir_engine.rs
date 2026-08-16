//! NIR rewrite engine: a worklist driving local rewrites over one function's
//! [`Body`] to a fixed point, visiting a node only when it might be reducible
//! rather than re-walking the whole tree per pass. Provides the session — parent
//! map, local use index and worklist, built once per function — the edit API
//! that keeps both coherent, the [`Rule`] trait, and `run`. See WEP 2026-06-05.

use std::cell::OnceCell;
use std::collections::VecDeque;

use cranelift_entity::EntityRef;

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::NirLocal;
use crate::nir_arena::{
    ArmData, BlockId, BlockNode, Body, ExprId, ExprKind, ExprNode, NodeRef, Operand, PatId,
    PatKind, PatNode, StmtId, StmtKind, StmtNode,
};
use crate::tir::TypeId;
use crate::token::Span;

/// A live-pool representative per field read, plus the versions a placement
/// needs: a representative's `heap_ver` proves its *uses* see one load, not
/// that an insertion point does. Both versions are in the scratch walk's
/// numbering, the only one they compare in.
#[derive(Default)]
pub struct FieldValues {
    pub reads: Vec<(ExprId, crate::nir_value_graph::ValueId)>,
    walk_version: IndexMap<crate::nir_value_graph::ValueId, crate::nir_value_graph::HeapVersion>,
    stmt_entry_version: IndexMap<StmtId, crate::nir_value_graph::HeapVersion>,
}

impl FieldValues {
    /// Whether pinning `rep`'s load before `stmt` still reads what its uses
    /// read — false when a mutation earlier in that statement wrote the slot.
    /// Unknown is refused.
    pub fn pinnable_before(&self, rep: crate::nir_value_graph::ValueId, stmt: StmtId) -> bool {
        match (
            self.walk_version.get(&rep),
            self.stmt_entry_version.get(&stmt),
        ) {
            (Some(v), Some(mark)) => v < mark,
            _ => false,
        }
    }
}

/// Per-local use information: where the local is defined and every node that
/// reads it. Used by the engine to re-enqueue a local's uses when its
/// definition is rewritten (copy propagation, const-fold through a `Let`, …).
#[derive(Default, Debug)]
pub struct LocalUses {
    /// The first `Let` statement binding the local, if any. Parameters have
    /// none, and neither does a `LetDestructure` binding (it binds through a
    /// pattern, which this index does not walk).
    pub def: Option<StmtId>,
    /// How many `Let` statements bind the local; a second is a second version.
    pub defs: u32,
    /// Every `Local { index }` expression node that names this local. (The
    /// place-form distinction — read vs write — is refined when a rule needs
    /// it; for re-enqueue purposes every mention is a use.)
    pub reads: Vec<ExprId>,
}

/// Route `node` to its slot among 4 parallel per-arena-kind slices. The
/// shared dispatch behind every dense per-node buffer in this module (parent
/// links, in-worklist bits): a `NodeRef` names one of 4 arenas, so a lookup
/// always reduces to "pick the matching slice, index by the id".
fn arena_slot<'a, T>(
    node: NodeRef,
    exprs: &'a [T],
    stmts: &'a [T],
    blocks: &'a [T],
    pats: &'a [T],
) -> &'a T {
    match node {
        NodeRef::Expr(id) => &exprs[id.index()],
        NodeRef::Stmt(id) => &stmts[id.index()],
        NodeRef::Block(id) => &blocks[id.index()],
        NodeRef::Pat(id) => &pats[id.index()],
    }
}

/// Mutable counterpart of [`arena_slot`].
fn arena_slot_mut<'a, T>(
    node: NodeRef,
    exprs: &'a mut [T],
    stmts: &'a mut [T],
    blocks: &'a mut [T],
    pats: &'a mut [T],
) -> &'a mut T {
    match node {
        NodeRef::Expr(id) => &mut exprs[id.index()],
        NodeRef::Stmt(id) => &mut stmts[id.index()],
        NodeRef::Block(id) => &mut blocks[id.index()],
        NodeRef::Pat(id) => &mut pats[id.index()],
    }
}

/// The reusable scratch buffers an [`Engine`] session needs: the parent map,
/// use index, and worklist. Constructing an engine for every dirty function in
/// every engine-based pass — `peephole` twice per fixed-point iteration,
/// `match_to_switch`, … — happens tens of thousands of times on a large
/// program, so these containers are owned by the caller once and lent to each
/// session via [`Engine::new`], which clears and right-sizes them. Reusing the
/// allocations keeps the optimizer off the global allocator's hot path.
#[derive(Default)]
pub struct EngineBuffers {
    expr_parent: Vec<Option<NodeRef>>,
    stmt_parent: Vec<Option<NodeRef>>,
    block_parent: Vec<Option<NodeRef>>,
    pat_parent: Vec<Option<NodeRef>>,
    /// Indexed directly by local index (dense, `0..locals.len()` — same
    /// invariant `LocalSet` relies on elsewhere), not hashed: a local-keyed
    /// `IndexMap` here would pay a hash + probe for every read/def recorded by
    /// `build_indices`, run on every one of the tens-of-thousands of sessions.
    uses: Vec<LocalUses>,
    worklist: VecDeque<NodeRef>,
    /// Traversal scratch for [`Engine::build_indices`], owned here for the same
    /// reason as everything else in this struct: a session per pass per
    /// function means the walk runs tens of thousands of times per compile.
    walk_stack: Vec<(NodeRef, bool)>,
    walk_children: Vec<NodeRef>,
    /// Dense in-worklist bit per node, one `Vec` per arena kind (mirrors the
    /// `*_parent` vecs above) — same dense-index rationale as `uses`:
    /// `enqueue`/`pop` run on nearly every rewrite edit, the engine's hottest
    /// path, so a `NodeRef`-keyed `IndexSet` here would pay a hash + probe on
    /// every one of them instead of a plain array index.
    expr_queued: Vec<bool>,
    stmt_queued: Vec<bool>,
    block_queued: Vec<bool>,
    pat_queued: Vec<bool>,
    /// Locals whose defining binding a rule deleted this session, audited once
    /// at [`Engine::run`]'s end. Only written under debug assertions — the
    /// audit is the sole reader. See [`Engine::note_elided_local`].
    elided_locals: Vec<u32>,
}

impl EngineBuffers {
    /// Clear every buffer and size the parent / queued-bit / use-index maps to
    /// `body`'s current node and local counts, readying them for a fresh
    /// session. Capacity is retained, so a reused `EngineBuffers` allocates
    /// only when a body is larger than any seen before.
    fn reset_for(&mut self, body: &Body, local_count: usize) {
        let fill = |v: &mut Vec<Option<NodeRef>>, len: usize| {
            v.clear();
            v.resize(len, None);
        };
        fill(&mut self.expr_parent, body.exprs.len());
        fill(&mut self.stmt_parent, body.stmts.len());
        fill(&mut self.block_parent, body.blocks.len());
        fill(&mut self.pat_parent, body.pats.len());
        let fill_bit = |v: &mut Vec<bool>, len: usize| {
            v.clear();
            v.resize(len, false);
        };
        fill_bit(&mut self.expr_queued, body.exprs.len());
        fill_bit(&mut self.stmt_queued, body.stmts.len());
        fill_bit(&mut self.block_queued, body.blocks.len());
        fill_bit(&mut self.pat_queued, body.pats.len());
        // Pre-sized to `local_count` like the vecs above, so `build_indices`
        // (which visits locals in body order, not index order) doesn't grow
        // `uses` through several reallocations; `uses_entry` still grows it
        // further on demand for a local `alloc_local`d past this count.
        self.uses.clear();
        self.uses.resize_with(local_count, LocalUses::default);
        self.worklist.clear();
        self.elided_locals.clear();
    }

    /// Mutable access to local `index`'s use record, growing `uses` on demand
    /// (a session may `alloc_local` past the count `reset_for` saw).
    fn uses_entry(&mut self, index: u32) -> &mut LocalUses {
        let i = index as usize;
        if self.uses.len() <= i {
            self.uses.resize_with(i + 1, LocalUses::default);
        }
        &mut self.uses[i]
    }

    /// Whether `node` currently has an in-worklist bit set.
    fn is_queued(&self, node: NodeRef) -> bool {
        *arena_slot(
            node,
            &self.expr_queued,
            &self.stmt_queued,
            &self.block_queued,
            &self.pat_queued,
        )
    }

    /// Set `node`'s in-worklist bit.
    fn set_queued(&mut self, node: NodeRef, value: bool) {
        *arena_slot_mut(
            node,
            &mut self.expr_queued,
            &mut self.stmt_queued,
            &mut self.block_queued,
            &mut self.pat_queued,
        ) = value;
    }
}

/// An engine session over one function body: the arena plus the [`EngineBuffers`]
/// scratch (parent map, use index, and worklist) the worklist discipline needs,
/// and the function's `locals` list so rules can allocate fresh locals.
pub struct Engine<'a> {
    pub body: &'a mut Body,
    buf: &'a mut EngineBuffers,
    /// The function's local list — the single source of truth for the local
    /// count, so `alloc_local` appends here and returns the new index. Sessions
    /// over a body with no owning function (a global initializer, a unit test)
    /// pass a scratch `Vec`; those bodies have no locals and their rules never
    /// allocate, so it stays empty.
    locals: &'a mut Vec<NirLocal>,
    /// Per-session cache of the body's live `&local` / `&mut local` scan
    /// (see [`Body::collect_address_taken_locals`]). Computed on first use,
    /// cleared with [`Engine::invalidate_address_taken`] so a rescan picks up
    /// edits. Consumed by `Local`-read exclusions (`store_load_forward`, licm's
    /// arithmetic hoist).
    body_address_taken: Option<IndexSet<u32>>,
    /// Reference-aliased locals (address-taken / `with stores[p]` /
    /// reference-typed). The `ValueGraph` builder invalidates these
    /// conservatively across field writes and calls; non-aliased locals get
    /// precise per-`(root, field)` forwarding. Empty unless a pass supplies it
    /// via [`Engine::set_alias_sets`] before the first `value` query.
    aliased_locals: IndexSet<u32>,
    /// The `stores`-aliased subset whose fields are never seeded.
    untrackable_locals: IndexSet<u32>,
    /// The subset of `aliased_locals` a call may actually mutate (locals with a
    /// mutable escape: `&mut v`, mut-ref args, `&mut self` receivers, or a
    /// `stores` stash). The `ValueGraph` builder bumps only these across calls,
    /// so immutable-`&v`-only locals keep their forwarded fields. Empty unless a
    /// pass supplies it via [`Engine::set_alias_sets`].
    mut_escaped_locals: IndexSet<u32>,
    /// Parameter local indices, seeded as stable `Opaque`s. Only up-front
    /// seeding makes parameters visible in the loop-entry snapshots, so
    /// passes consuming [`Engine::loop_entry_value`] must call
    /// [`Engine::set_param_locals`] first.
    param_locals: Vec<u32>,
    /// `ExprId` indices of calls that mutate no caller local
    /// ([`crate::optimize::alias::pure_calls`]); the build skips their per-call
    /// `mut_escaped` bump. Empty (conservative — every call bumps) unless a pass
    /// supplies it via [`Engine::set_pure_calls`] before the first value query.
    pure_calls: IndexSet<crate::nir_arena::ExprId>,
    /// Type table for the `ValueGraph` builder's constant folding of pure
    /// arithmetic. `None` (the default) disables folding. Set via
    /// [`Engine::set_value_graph_type_table`] before the first value query.
    vg_type_table: Option<&'a crate::tir::TypeTable>,
    /// [`FuncId`]s of the diverging panic / `unreachable` builtins, supplied by
    /// `condition_implication` so its bounds-check elimination identifies a
    /// panic block by callee id (not the call node's `FunctionRef`). `None` (the
    /// default) means "no callee is a panic". Set via
    /// [`Engine::set_panic_callee_ids`].
    panic_callee_ids: Option<&'a IndexSet<crate::nir::FuncId>>,
    /// [`FuncId`]s of pure builtin intrinsics (`array_get`, `array_len`, …),
    /// supplied by a value-graph pass so the builder knows such a call writes no
    /// heap — the call node no longer carries a `FunctionRef` to classify by.
    /// `None` (the default) is conservative: every call is a heap write. Set via
    /// [`Engine::set_pure_builtin_callees`] before the first value query.
    pure_builtin_callees: Option<&'a IndexSet<crate::nir::FuncId>>,
    /// Memoized promoted-read census ([`Body::promoted_read_counts`]) for the
    /// session. See [`Engine::promoted_read_count`].
    ///
    /// [`Engine::run`]'s audit covers only a session that asked, and only its
    /// final state.
    promoted_reads: OnceCell<IndexMap<u32, usize>>,
    /// Whether a local-naming operand sits in the arena unreached by the memo —
    /// see [`Engine::census_note_structure`].
    pending_local_naming: std::cell::Cell<bool>,
}

impl<'a> Engine<'a> {
    /// Build a session over `body`, reusing the caller-owned `buf`: one O(n)
    /// walk populates the parent map and use index and seeds the worklist in
    /// post-order (see `Engine::build_indices`). `locals` is the owning
    /// function's local list (see [`Engine::alloc_local`]).
    pub fn new(
        body: &'a mut Body,
        buf: &'a mut EngineBuffers,
        locals: &'a mut Vec<NirLocal>,
    ) -> Self {
        buf.reset_for(body, locals.len());
        let mut engine = Self {
            body,
            buf,
            locals,
            body_address_taken: None,
            aliased_locals: IndexSet::default(),
            untrackable_locals: IndexSet::default(),
            mut_escaped_locals: IndexSet::default(),
            param_locals: Vec::new(),
            pure_calls: IndexSet::default(),
            vg_type_table: None,
            panic_callee_ids: None,
            pure_builtin_callees: None,
            promoted_reads: OnceCell::new(),
            pending_local_naming: std::cell::Cell::new(false),
        };
        // The value graph lives on `Body` and is built once, then maintained
        // through every edit (`replace_expr_with_value` / `redirect_expr`, plus
        // structural-pass coarsening). A new session reuses it as-is — there is
        // no session-start drop and no rebuild path (build-once is invariant).
        engine.build_indices();
        engine
    }

    /// The `ValueId` the per-function `ValueGraph` assigned to `expr`, built
    /// lazily; `None` for an impure, allocation-bearing or control-flow
    /// expression, and for any `ExprId` allocated after the build.
    ///
    /// The graph is built once and maintained in place, never rebuilt: engine
    /// edits keep the operands current and a query re-derives through them
    /// (`Engine::maintain_pure_node`), a direct arena edit coarsens the region
    /// it touched, and a caller needing the reaching values regrows them into a
    /// scratch pool ([`Engine::scoped_const_reads`]).
    pub fn value(&mut self, expr: ExprId) -> Option<crate::nir_value_graph::ValueId> {
        self.ensure_value_graph();
        // There is no skeleton-expr → value side-table. An expr's value comes
        // only from promoted operands (born-as-operands) and pure re-derivation
        // over them; an unpromoted leaf (`Local` / `FieldAccess`) resolves to
        // `None`. Sound — `None` is the finest partition, so a consumer skips the
        // expr rather than over-merging it with another.
        self.maintain_pure_node(expr)
    }

    /// Re-derive `expr`'s value from its operands — the sole value source now
    /// that `value_of` is retired. Operand-determined kinds only, plus a read of
    /// a stable parameter; every other kind, and any unresolved operand, gives
    /// `None`. The parameter base case is load-bearing: without it only
    /// all-constant trees resolve and no promoted value can name a local.
    fn maintain_pure_node(&mut self, expr: ExprId) -> Option<crate::nir_value_graph::ValueId> {
        self.body.value_graph.as_ref()?;
        let kind = self.body.exprs[expr].kind.clone();
        let result_ty = self.body.exprs[expr].type_id;
        let tt = self.vg_type_table;
        let v = match kind {
            // One version-free `ValueId` per local is correct only where the
            // local has one version: a parameter is entry-defined, so its def
            // dominates every use, and unreassigned it holds one value.
            ExprKind::Local { index, .. } => {
                if !self.local_has_one_version(index) {
                    return None;
                }
                self.body.values.canonical_local(index, result_ty)
            }
            ExprKind::Binary { left, op, right } => {
                let lhs = self.operand_value(left)?;
                let rhs = self.operand_value(right)?;
                self.body.values.binary_folded(op, lhs, rhs, result_ty, tt)
            }
            ExprKind::Unary { op, expr: inner } => {
                use crate::nir::NirUnaryOp;
                if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref) {
                    return None;
                }
                let operand = self.operand_value(inner)?;
                self.body.values.unary_folded(op, operand, result_ty, tt)
            }
            ExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                let operand = self.operand_value(inner)?;
                self.body.values.cast_folded(operand, target_type, tt)
            }
            _ => return None,
        };
        Some(v)
    }

    /// The [`ValueId`](crate::nir_value_graph::ValueId) of an operand: the
    /// promoted value directly, or the skeleton expr's value from the graph.
    pub fn operand_value(&mut self, op: Operand) -> Option<crate::nir_value_graph::ValueId> {
        match op {
            Operand::Value(v) => Some(v),
            Operand::Expr(e) => self.value(e),
        }
    }

    /// Read-only view of a value's kind. The returned reference borrows the
    /// engine's value-graph cache; callers that need to hold the kind across
    /// further `engine` calls should clone it.
    pub fn value_kind(
        &mut self,
        id: crate::nir_value_graph::ValueId,
    ) -> &crate::nir_value_graph::ValueKind {
        self.ensure_value_graph();
        self.body.values.kind(id)
    }

    /// The `ValueId` a read of `local` would produce in the pre-header of
    /// the loop whose body block is `loop_body` (the builder's
    /// `current_value` snapshot at loop entry). `None` when the loop is
    /// unrecorded or the local was unbound at that point. A hoisting pass
    /// may move a pure expression to the pre-header exactly when each of
    /// its `Local` leaves' use-site value equals this pre-header value —
    /// see `ValueGraphBuild::loop_entry_values`.
    pub fn loop_entry_value(
        &mut self,
        loop_body: BlockId,
        local: u32,
    ) -> Option<crate::nir_value_graph::ValueId> {
        self.ensure_value_graph();
        self.body
            .value_graph
            .as_ref()?
            .loop_entry_values
            .get(&loop_body)?
            .get(&local)
            .copied()
    }

    /// Every read whose reaching value the use site can re-emit, recovered by a
    /// scratch re-walk — the same splice-point growth `inline` uses, with no live
    /// rebuild. Covers a `Local` read of anything in `forwardable`, plus a
    /// `FieldAccess` under `include_fields`; being address-taken or
    /// `stores`-aliased disqualifies a local, being an aggregate does not.
    pub fn scoped_const_reads(
        &mut self,
        forwardable: &IndexSet<u32>,
        include_fields: bool,
    ) -> Vec<(ExprId, crate::nir_value_graph::ValueId)> {
        use crate::nir_value_graph::builder;
        // Only grow an already-built graph: building it here would use this
        // session's alias config, which is sound only when the caller has set the
        // real sets (`store_load_forward` does). A missing graph is left for the
        // natural lazy build at the next query.
        if self.body.value_graph.is_none() || (forwardable.is_empty() && !include_fields) {
            return Vec::new();
        }
        let root = self.body.root;
        let live_base = self.body.values.len() as u32;
        let mut scratch = self.body.values.clone();
        let empty = IndexMap::default();
        // The real alias sets, not a maximally-conservative `all`: with `all` as
        // `mut_escaped`, every call would bump every local to a fresh opaque
        // (`bump_call_effects`), destroying a local's reaching value across the
        // calls between its store and a later read. A SROA scalar is
        // non-escaping, so the real sets leave it untouched by calls — exactly
        // the forwarding we need to recover. (`pure_calls` is left empty here:
        // bumping at every call is conservative — it only forgoes a forward.)
        let empty_builtins = IndexSet::default();
        let vo = builder::build_scoped(
            self.body,
            root,
            0,
            &empty,
            &self.aliased_locals,
            &self.untrackable_locals,
            &self.mut_escaped_locals,
            self.vg_type_table,
            self.pure_builtin_callees.unwrap_or(&empty_builtins),
            &mut scratch,
            None,
            live_base,
        );
        let mut out = Vec::new();
        for (e, v) in vo {
            if !builder::is_const_value(&self.body.values, v) {
                continue;
            }
            let keep = match &self.body.exprs[e].kind {
                ExprKind::Local { index, .. } => forwardable.contains(index),
                ExprKind::FieldAccess { .. } => include_fields,
                _ => false,
            };
            if keep {
                out.push((e, v));
            }
        }
        out
    }

    /// A live-pool `FieldAccess` per field read, shared between exactly the
    /// reads a scratch re-walk proves see one load.
    ///
    /// The walk is an equivalence oracle, not a value source — its ids mean
    /// nothing in the live pool — so each class is re-minted over
    /// `canonical_local(i)`, sound only where `i` holds one value, at a version
    /// above `max_heap_version` so it cannot collide with an existing triple.
    pub fn scoped_field_values(&mut self) -> FieldValues {
        use crate::nir_value_graph::{OpaqueSource, ValueKind, builder};
        self.ensure_value_graph();
        if self.body.value_graph.is_none() {
            return FieldValues::default();
        }
        let root = self.body.root;
        let mut scratch = self.body.values.clone();
        let empty = IndexMap::default();
        let empty_builtins = IndexSet::default();
        let scoped = builder::walk_scoped(
            self.body,
            root,
            0,
            &empty,
            &self.aliased_locals,
            &self.untrackable_locals,
            &self.mut_escaped_locals,
            self.vg_type_table,
            self.pure_builtin_callees.unwrap_or(&empty_builtins),
            &mut scratch,
            None,
        );
        // Collected before any live-pool mutation: `local_has_one_version`
        // borrows the engine.
        let mut classes: Vec<(ExprId, crate::nir_value_graph::ValueId, u32, u32)> = Vec::new();
        for (e, sv) in scoped.values {
            if !matches!(self.body.exprs[e].kind, ExprKind::FieldAccess { .. }) {
                continue;
            }
            // The extractor re-emits a `StructGet` keyed by field index, which
            // a tuple receiver — where the index means an element — has no
            // re-emission for. Asked nowhere else.
            let ValueKind::FieldAccess {
                receiver,
                field_index,
                ..
            } = *scratch.kind(sv)
            else {
                continue;
            };
            let source = match scratch.kind(receiver) {
                ValueKind::Opaque(o) => scratch.opaque_source(*o),
                _ => None,
            };
            let Some(OpaqueSource::Local(i)) = source else {
                continue;
            };
            if !self.local_is_struct_typed(i) {
                continue;
            }
            classes.push((e, sv, i, field_index));
        }
        let mut found = FieldValues {
            stmt_entry_version: scoped.stmt_entry_version,
            ..FieldValues::default()
        };
        let mut minted: IndexMap<crate::nir_value_graph::ValueId, crate::nir_value_graph::ValueId> =
            IndexMap::default();
        let mut next_ver = self.body.values.max_heap_version().bump();
        for (e, sv, local, field_index) in classes {
            if !self.local_has_one_version(local) {
                continue;
            }
            let recv_ty = self.locals()[local as usize].type_id;
            let live = if let Some(&v) = minted.get(&sv) {
                v
            } else {
                let recv = self.body.values.canonical_local(local, recv_ty);
                let v = self.body.values.field_access(recv, field_index, next_ver);
                next_ver = next_ver.bump();
                minted.insert(sv, v);
                let walk_ver = match scratch.kind(sv) {
                    ValueKind::FieldAccess { heap_ver, .. } => *heap_ver,
                    _ => unreachable!("classes only holds FieldAccess values"),
                };
                found.walk_version.insert(v, walk_ver);
                v
            };
            found.reads.push((e, live));
        }
        found
    }

    /// Drop the session's `&local` address-taken scan so the next
    /// [`Engine::body_address_taken`] recomputes it. The value graph is
    /// **never** dropped — it is built once and maintained in place (build-once
    /// is a structural invariant; there is no rebuild path).
    pub fn invalidate_address_taken(&mut self) {
        self.body_address_taken = None;
    }

    /// The session's cached body scan for live `&local` / `&mut local`
    /// shapes. Rules that exclude such locals (`store_load_forward`, licm's
    /// arithmetic hoist) read this instead of rescanning.
    pub fn body_address_taken(&mut self) -> &IndexSet<u32> {
        if self.body_address_taken.is_none() {
            let mut set = IndexSet::default();
            self.body.collect_address_taken_locals(&mut set);
            self.body_address_taken = Some(set);
        }
        self.body_address_taken.as_ref().unwrap()
    }

    /// Locals that escape by mutable reference — `&mut v`, a `&mut`-arg / `&mut
    /// self` call, or a `stores` stash — so a callee may mutate their fields.
    /// Set by [`Engine::set_alias_sets`]; a pass treating a field read as
    /// loop-/function-invariant must exclude these receivers.
    pub fn mut_escaped(&self) -> &IndexSet<u32> {
        &self.mut_escaped_locals
    }

    /// Record the owning function's parameter local indices so the value graph
    /// seeds them up front (see the field doc on `param_locals`). Used by the
    /// one build-once construction; the graph is never rebuilt on a later change.
    pub fn set_param_locals(&mut self, locals: Vec<u32>) {
        self.param_locals = locals;
    }

    /// Record the `ExprId` indices of calls that mutate no caller local, so the
    /// one build-once construction skips their per-call `mut_escaped` bump. Supply
    /// before the first value query (the build is lazy); the persisted config
    /// carries it so the verify oracle rebuilds consistently.
    pub fn set_pure_calls(&mut self, pure_calls: IndexSet<crate::nir_arena::ExprId>) {
        self.pure_calls = pure_calls;
    }

    /// Record the function's reference-aliased and `stores`-aliased locals so
    /// the value graph invalidates field forwarding for them at the right
    /// granularity. Used by the one build-once construction. Without it the
    /// builder treats every receiver as non-aliased — sound only when the
    /// function has no reference aliasing, so passes that may see aliasing must
    /// supply it before the first value query.
    pub fn set_alias_sets(
        &mut self,
        aliased: IndexSet<u32>,
        untrackable: IndexSet<u32>,
        mut_escaped: IndexSet<u32>,
    ) {
        self.aliased_locals = aliased;
        self.untrackable_locals = untrackable;
        self.mut_escaped_locals = mut_escaped;
    }

    /// Provide the type table so the value graph folds pure arithmetic on
    /// literal operands (`2 + 3 → 5`). Used by the one build-once construction.
    pub fn set_value_graph_type_table(&mut self, type_table: &'a crate::tir::TypeTable) {
        self.vg_type_table = Some(type_table);
    }

    /// Supply the panic / `unreachable` callee ids for `condition_implication`'s
    /// panic-block recognizer. See [`Engine::is_panic_callee`].
    pub fn set_panic_callee_ids(&mut self, ids: &'a IndexSet<crate::nir::FuncId>) {
        self.panic_callee_ids = Some(ids);
    }

    /// Supply the pure-builtin callee ids so the value-graph build classifies a
    /// call's heap effect by `func_id`. See `Engine::pure_builtin_callees`.
    /// Must be set before the first value query to take effect (the graph is
    /// built once and reused).
    pub fn set_pure_builtin_callees(&mut self, ids: &'a IndexSet<crate::nir::FuncId>) {
        self.pure_builtin_callees = Some(ids);
    }

    /// Whether `func_id` is one of the supplied panic / `unreachable` callees.
    /// `false` when no set was supplied.
    pub fn is_panic_callee(&self, func_id: crate::nir::FuncId) -> bool {
        self.panic_callee_ids.is_some_and(|s| s.contains(&func_id))
    }

    /// The type table supplied for value-graph folding, if any. Used by
    /// `store_load_forward` to synthesize a literal `ExprKind` for a folded
    /// value that has no pre-existing source literal.
    pub fn value_graph_type_table(&self) -> Option<&'a crate::tir::TypeTable> {
        self.vg_type_table
    }

    /// Build the persisted value graph now, for a caller that has already set
    /// the real alias sets.
    ///
    /// [`Self::scoped_const_reads`] only grows a graph that already exists, so
    /// whether a body has one is otherwise incidental — it depends on whether
    /// some earlier pass happened to run a value query on it.
    pub fn build_value_graph_now(&mut self) {
        self.ensure_value_graph();
    }

    fn ensure_value_graph(&mut self) {
        if self.body.value_graph.is_some() {
            return;
        }
        // Size-gate the persisted value graph: building it, plus the inline-time
        // `build_scoped` scratch-pool clone, scales with function size — under
        // `wado test`'s parallel compilation that OOMs on generated parsers
        // (ANTLR4 drivers). Past the threshold, skip it: every consumer already
        // guards `value_graph.is_none()`.
        const VG_MAX_EXPRS: usize = 5000;
        if self.body.exprs.len() > VG_MAX_EXPRS {
            return;
        }
        let empty_builtins = IndexSet::default();
        let build = crate::nir_value_graph::builder::build(
            &mut *self.body,
            &self.param_locals,
            &self.aliased_locals,
            &self.untrackable_locals,
            &self.mut_escaped_locals,
            &self.pure_calls,
            self.pure_builtin_callees.unwrap_or(&empty_builtins),
            self.vg_type_table,
        );
        self.body.value_graph = Some(build);
    }

    /// Report that this rewrite deleted the binding that defined `local`, so
    /// the session end can check no read of it survived.
    ///
    /// A rule must not run that check itself: `Body::surviving_read` walks the
    /// whole body, and per rule application that is quadratic — the cost that
    /// made one fixed-point iteration's `peephole` runs 4.9 s of a 9.5 s loop.
    /// Reporting here pays one walk per session instead, and keeps the audit
    /// independent of the use index and census memo the rewrite decided on.
    pub fn note_elided_local(&mut self, local: u32) {
        #[cfg(debug_assertions)]
        self.buf.elided_locals.push(local);
        #[cfg(not(debug_assertions))]
        let _ = local;
    }

    /// Whether `local` is **single-assignment** — bound once, never written again
    /// — which is what makes `ValuePool::canonical_local`'s one id per index
    /// denote one value. Holds per *execution* of the binding's scope, so a `let`
    /// in a loop body qualifies while naming its value across an iteration needs
    /// its own check (`materialise_point`'s refusal to place outside the loop).
    /// Read off the use index rather than `NirLocal::is_mut`, which a pass minting
    /// a temp sets by hand; the index holds every `Local` node whether reachable
    /// or not, so it fails safe. `extract`'s `multi_version_locals` is this same
    /// predicate, so the value query and the freeze agree by construction.
    pub fn local_has_one_version(&mut self, local: u32) -> bool {
        match self.local_bindings(local) {
            // No `Let`: a parameter, or a `LetDestructure` binding the use index
            // does not record — which then fails the membership test.
            0 => {
                return self.param_locals.contains(&local)
                    && self.locals().get(local as usize).is_some_and(|l| !l.is_mut);
            }
            1 => {}
            // `labeled_block_fusion` clones a block, `local_index` and all.
            _ => return false,
        }
        // A write through a reference leaves no assign target to see, so a local
        // whose address was taken is out regardless of how it reads.
        if self.body_address_taken().contains(&local) {
            return false;
        }
        !self
            .local_reads(local)
            .iter()
            .any(|&m| self.is_assign_target(m))
    }

    /// How many `Let` statements bind `local`.
    fn local_bindings(&self, local: u32) -> u32 {
        self.buf.uses.get(local as usize).map_or(0, |u| u.defs)
    }

    /// Whether `local` holds a struct, looking through a reference. `false`
    /// without a type table, and for every other shape a `FieldAccess` can name
    /// — a tuple above all, whose field index means something else entirely.
    fn local_is_struct_typed(&self, local: u32) -> bool {
        use crate::tir::ResolvedType;
        let Some(types) = self.vg_type_table else {
            return false;
        };
        let Some(l) = self.locals.get(local as usize) else {
            return false;
        };
        let mut ty = l.type_id;
        loop {
            match types.get(ty) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => ty = *inner,
                ResolvedType::Struct { .. } => return true,
                _ => return false,
            }
        }
    }

    /// Read-only view of the owning function's local list. Some rules
    /// (`labeled_block_fusion`) need to inspect existing locals' declared types
    /// (e.g. to confirm a pattern's payload binding slot matches the
    /// constructed payload's type).
    pub fn locals(&self) -> &[NirLocal] {
        self.locals
    }

    /// Allocate a fresh function local, returning its index. `locals.len()` is
    /// the local count (there is no separate counter — see
    /// [`crate::nir::NirFunction::local_count`]), so the new local takes the
    /// index `locals.len()` and the push makes it the next free index.
    pub fn alloc_local(&mut self, name: String, type_id: TypeId, is_mut: bool) -> u32 {
        let index = self.locals.len() as u32;
        self.locals.push(NirLocal {
            name,
            type_id,
            is_mut,
        });
        index
    }

    /// Build the session's three indices — parent map, local use index, and
    /// post-order worklist seed — in one walk of the live tree. Not an arena
    /// scan: the arena never compacts, and an orphan still referencing a live
    /// node would overwrite its parent edge. Parents and uses are recorded
    /// descending, the worklist ascending, so leaves fold before their contexts.
    fn build_indices(&mut self) {
        let mut stack = std::mem::take(&mut self.buf.walk_stack);
        let mut children = std::mem::take(&mut self.buf.walk_children);
        stack.clear();
        stack.push((NodeRef::Block(self.body.root), false));
        while let Some((node, processed)) = stack.pop() {
            if processed {
                self.enqueue(node);
                continue;
            }
            stack.push((node, true));
            match node {
                NodeRef::Expr(id) => {
                    if let ExprKind::Local { index, .. } = &self.body.exprs[id].kind {
                        let index = *index;
                        self.buf.uses_entry(index).reads.push(id);
                    }
                }
                NodeRef::Stmt(id) => {
                    if let StmtKind::Let { local_index, .. } = &self.body.stmts[id].kind {
                        let index = *local_index;
                        // First in source order, not last. A local should have
                        // one `Let`, but cloning a block (`labeled_block_fusion`)
                        // copies `local_index` with it, and `def` feeds
                        // `extract`'s dominance gate — so pick explicitly rather
                        // than let the traversal direction decide.
                        let entry = self.buf.uses_entry(index);
                        entry.defs += 1;
                        if entry.def.is_none() {
                            entry.def = Some(id);
                        }
                    }
                }
                // A pattern binding gets no `def` (the dominance gate wants a
                // statement) but still counts as a version.
                NodeRef::Pat(id) => {
                    if let PatKind::Binding { local_index, .. } = &self.body.pats[id].kind {
                        let index = *local_index;
                        self.buf.uses_entry(index).defs += 1;
                    }
                }
                NodeRef::Block(_) => {}
            }
            children.clear();
            self.body.for_each_child(node, |c| children.push(c));
            for &child in &children {
                let slot = arena_slot_mut(
                    child,
                    &mut self.buf.expr_parent,
                    &mut self.buf.stmt_parent,
                    &mut self.buf.block_parent,
                    &mut self.buf.pat_parent,
                );
                if let Some(prev) = *slot {
                    panic!(
                        "[NIR engine] {child:?} has two live parents ({prev:?} and {node:?}): \
                         an edit shared a child id between two nodes — move the kind \
                         (`become_expr`), do not share ids"
                    );
                }
                *slot = Some(node);
            }
            for &child in children.iter().rev() {
                stack.push((child, false));
            }
        }
        self.buf.walk_stack = stack;
        self.buf.walk_children = children;
    }

    /// Push a node onto the worklist unless it is already queued.
    pub fn enqueue(&mut self, node: NodeRef) {
        if !self.buf.is_queued(node) {
            self.buf.set_queued(node, true);
            self.buf.worklist.push_back(node);
        }
    }

    /// Pop the next node to process, clearing its in-queue bit.
    pub fn pop(&mut self) -> Option<NodeRef> {
        let node = self.buf.worklist.pop_front()?;
        self.buf.set_queued(node, false);
        Some(node)
    }

    /// The parent of a node (the nearest id-bearing ancestor), or `None` for
    /// the body root.
    pub fn parent_of(&self, node: NodeRef) -> Option<NodeRef> {
        *arena_slot(
            node,
            &self.buf.expr_parent,
            &self.buf.stmt_parent,
            &self.buf.block_parent,
            &self.buf.pat_parent,
        )
    }

    /// Every `Local { index }` expression node naming `local`.
    pub fn local_reads(&self, local: u32) -> &[ExprId] {
        self.buf.uses.get(local as usize).map_or(&[], |u| &u.reads)
    }

    /// The defining `Let` / `LetDestructure` statement of `local`, if any.
    pub fn local_def(&self, local: u32) -> Option<StmtId> {
        self.buf.uses.get(local as usize).and_then(|u| u.def)
    }

    /// Whether `local` is read anywhere — i.e. has any mention that is not the
    /// bare-`Local` target of an `Assign`. A bare `local = value` writes the
    /// local without observing it; `&local`, `local.field = …`, and every
    /// value-position `Local` are reads. This is the liveness signal the
    /// write-only-local elimination rule keys on.
    pub fn is_local_read(&self, local: u32) -> bool {
        self.local_reads(local)
            .iter()
            .any(|&mention| !self.is_assign_target(mention))
    }

    /// How many reachable promoted operands read `local` — the half of a use
    /// census that lives in the value pool rather than the skeleton, so
    /// [`Engine::local_reads`] cannot see it. A rule deciding a local is unused,
    /// or comparing a whole-body tally against a scoped one, adds this in.
    ///
    /// Memoized for the session and filled on first ask, so a rule that never
    /// gets past its cheaper gates never pays the walk. The census answers for
    /// every local at once because the walk that produces it costs a whole body
    /// either way.
    pub fn promoted_read_count(&self, local: u32) -> usize {
        self.promoted_read_counts()
            .get(&local)
            .copied()
            .unwrap_or(0)
    }

    /// Whether any reachable promoted operand reads `local`
    /// ([`Engine::promoted_read_count`] as a predicate).
    pub fn reads_promoted_local(&self, local: u32) -> bool {
        self.promoted_read_count(local) > 0
    }

    fn promoted_read_counts(&self) -> &IndexMap<u32, usize> {
        self.promoted_reads.get_or_init(|| {
            // Sticky: a true answer only ever means "drop the memo", so holding
            // it spares the probe on later fills. False on all 356 fills of a
            // `benchmark/sqlite_parse` `-O2` compile.
            if !self.pending_local_naming.get() {
                self.pending_local_naming
                    .set(self.body.any_operand_names_a_local());
            }
            self.body.promoted_read_counts()
        })
    }

    /// Record an operand this edit writes into the skeleton, dropping the
    /// memoized census if it can no longer be trusted.
    fn census_note_operand(&mut self, op: Operand) {
        if self.promoted_reads.get().is_none() {
            return;
        }
        if matches!(op, Operand::Value(v) if self.body.values.names_a_local(v)) {
            self.promoted_reads.take();
            return;
        }
        // An `Operand::Expr` attaches a whole subtree, so ruling out this slot
        // does not rule out the edit.
        self.census_note_structure();
    }

    /// [`Engine::census_note_operand`] over every operand slot of `node` — for
    /// an edit that installs a whole node kind rather than one slot.
    fn census_note_node_operands(&mut self, node: NodeRef) {
        if self.promoted_reads.get().is_none() {
            return;
        }
        let mut writes_local = false;
        self.body.for_each_operand(node, |op| {
            if let Operand::Value(v) = op {
                writes_local |= self.body.values.names_a_local(v);
            }
        });
        if writes_local {
            self.promoted_reads.take();
            return;
        }
        self.census_note_structure();
    }

    /// Drop the memoized census unless this edit provably cannot have moved it.
    /// An interned value is immutable, so only a change in which operands are
    /// reachable moves the answer. An empty census survives a removal — nothing
    /// conjures a read — and survives an attach unless something local-naming is
    /// pending. A non-empty one can shrink as well as grow, so any edit drops it.
    fn census_note_structure(&mut self) {
        if self.pending_local_naming.get()
            || self.promoted_reads.get().is_some_and(|c| !c.is_empty())
        {
            self.promoted_reads.take();
        }
    }

    /// Whether `mention` is the target slot (LHS place) of an `Assign`. Shared by
    /// the optimizer passes that must not treat a write place as a value read.
    pub(super) fn is_assign_target(&self, mention: ExprId) -> bool {
        let Some(NodeRef::Expr(parent)) = self.parent_of(NodeRef::Expr(mention)) else {
            return false;
        };
        matches!(&self.body.exprs[parent].kind, ExprKind::Assign { target, .. } if *target == mention)
    }

    fn set_parent(&mut self, child: NodeRef, parent: Option<NodeRef>) {
        *arena_slot_mut(
            child,
            &mut self.buf.expr_parent,
            &mut self.buf.stmt_parent,
            &mut self.buf.block_parent,
            &mut self.buf.pat_parent,
        ) = parent;
    }

    /// Edit API: rewrite every operand slot of `node` through `f`.
    ///
    /// For a pass rewriting a whole subtree's slots in one sweep (`licm`'s
    /// pre-header hoist). A promoted value carries no skeleton node, so there is
    /// no parent edge to move and nothing to enqueue — which is also why `f` may
    /// only rewrite one promoted value to another.
    pub fn map_operands(&mut self, node: NodeRef, f: &mut impl FnMut(Operand) -> Operand) {
        let mut spliced = false;
        self.body.for_each_operand_mut(node, |slot| {
            let new = f(*slot);
            spliced |= new != *slot && (new.as_expr().is_some() || slot.as_expr().is_some());
            *slot = new;
        });
        assert!(
            !spliced,
            "[NIR engine] map_operands({node:?}) moved a skeleton operand: installing \
             one is a splice and replacing one orphans its subtree, and either way \
             the parent map goes stale. Use `redirect_expr` / `replace_expr_kind`."
        );
        self.census_note_node_operands(node);
    }

    /// Edit API: redirect one operand slot of `node` holding the promoted value
    /// `from` to `new`, returning whether a slot changed. The `Operand::Value`
    /// twin of [`Engine::redirect_expr`], which keys on the `ExprId` a skeleton
    /// operand carries — a promoted value has none.
    ///
    /// One slot per call (see [`Body::replace_value_operand_once`]); loop until
    /// it returns `false` to cover a node that holds `from` twice.
    pub fn redirect_value_operand(
        &mut self,
        node: NodeRef,
        from: crate::nir_value_graph::ValueId,
        new: Operand,
    ) -> bool {
        if !self.body.replace_value_operand_once(node, from, new) {
            return false;
        }
        self.census_note_operand(new);
        if let Operand::Expr(e) = new {
            self.set_parent(NodeRef::Expr(e), Some(node));
            self.enqueue(NodeRef::Expr(e));
        }
        self.enqueue(node);
        true
    }

    /// Edit API: rewrite an expression node's kind in place. The id is stable,
    /// so parent links and worklist entries survive. Keeps the use index
    /// coherent (if the node was / becomes a `Local`), re-parents the new
    /// kind's id children, and re-enqueues the affected neighbourhood — the
    /// node's parent (its context may now reduce) and the new children.
    pub fn replace_expr_kind(&mut self, id: ExprId, new_kind: ExprKind) {
        // Drop the old `Local` mention, if any, from the use index.
        if let ExprKind::Local { index, .. } = &self.body.exprs[id].kind {
            let index = *index;
            if let Some(u) = self.buf.uses.get_mut(index as usize) {
                u.reads.retain(|&r| r != id);
            }
        }
        self.body.exprs[id].kind = new_kind;
        self.census_note_node_operands(NodeRef::Expr(id));
        // Register a new `Local` mention, if any.
        if let ExprKind::Local { index, .. } = &self.body.exprs[id].kind {
            let index = *index;
            self.buf.uses_entry(index).reads.push(id);
        }
        // Re-parent and re-enqueue the new kind's children.
        let mut children = Vec::new();
        self.body
            .for_each_child(NodeRef::Expr(id), |c| children.push(c));
        for c in children {
            self.set_parent(c, Some(NodeRef::Expr(id)));
            self.enqueue(c);
        }
        // The enclosing context may now be reducible.
        if let Some(p) = self.parent_of(NodeRef::Expr(id)) {
            self.enqueue(p);
        }
    }

    /// Edit API: promote the folded constant subtree `id` to an `Operand::Value`
    /// in its parent. Interns `value` width-preserving
    /// (carrying `id`'s recorded type) and swaps the parent's `Operand::Expr(id)`
    /// slot to the promoted value. `id`'s node is left orphaned (later DCE'd); its
    /// own `Local` mention, if any, is dropped from the use index, matching
    /// [`Self::replace_expr_kind`]. Returns `false` (a no-op) when `id` has no
    /// parent, the parent references it through a non-operand slot, or `value` is
    /// an aggregate (the pool models pure scalars only) — the caller then keeps
    /// the skeleton form.
    pub fn replace_expr_with_value(&mut self, id: ExprId, value: crate::const_eval::Value) -> bool {
        use crate::const_eval::Value;
        use crate::nir_value_graph::ValueKind;
        let type_id = self.body.exprs[id].type_id;
        let kind = match value {
            Value::Int { value, .. } => ValueKind::Int(value, type_id),
            Value::Float { value, .. } => ValueKind::Float(value.to_bits(), type_id),
            Value::Bool(b) => ValueKind::Bool(b),
            Value::Char(c) => ValueKind::Char(c),
            // `null` and `()` are constants the pool can name, but promoting
            // one is an extraction question this path does not answer: `()` has
            // no Wasm representation at all. Keep the skeleton form.
            Value::Null | Value::Unit => return false,
            Value::Aggregate { .. } | Value::Seq { .. } | Value::Variant { .. } => return false,
        };
        let vid = self.body.values.alloc_unshared(kind, type_id);
        self.redirect_expr(id, Operand::Value(vid))
    }

    /// Edit API: redirect `id`'s parent operand slot to `new` (WEP: The Live
    /// `ValueGraph`). Used to splice a promoted constant — or an existing
    /// sub-expression — into the position `id` occupies, orphaning `id`'s node.
    /// Returns `false` (a no-op) when `id` has no parent or the parent references
    /// it through a non-operand slot.
    pub fn redirect_expr(&mut self, id: ExprId, new: Operand) -> bool {
        let Some(parent) = self.parent_of(NodeRef::Expr(id)) else {
            return false;
        };
        if !self.body.replace_operand_to(parent, id, new) {
            return false;
        }
        self.census_note_operand(new);
        if let Operand::Expr(e) = new {
            self.set_parent(NodeRef::Expr(e), Some(parent));
            self.enqueue(NodeRef::Expr(e));
        }
        // `id` is now orphaned. Drop its own `Local` read mention so a binding it
        // named is not kept artificially live (the discarded subtree's deeper
        // mentions stay — a stale read is conservative, never drops a live one).
        if let ExprKind::Local { index, .. } = &self.body.exprs[id].kind {
            let index = *index;
            if let Some(u) = self.buf.uses.get_mut(index as usize) {
                u.reads.retain(|&r| r != id);
            }
        }
        self.set_parent(NodeRef::Expr(id), None);
        self.enqueue(parent);
        true
    }

    /// Edit API: promote `src`'s content into `dst`, leaving `src` a dead `Unit`
    /// — the arena analogue of `*dst = *src` when a rewrite collapses a wrapper
    /// onto a node it contains (`{ expr; }` → `expr`). `dst`'s former subtree is
    /// discarded without deep-unregistering its `Local` mentions: a stale one
    /// only reaches `is_local_read`, where an extra dead mention is conservative.
    pub fn become_expr(&mut self, dst: ExprId, src: ExprId) {
        if dst == src {
            return;
        }
        let moved = self.body.take_expr(src);
        let (type_id, span, src_kind) = (moved.type_id, moved.span, moved.kind);
        // `src` is now `Dead`; if it named a local, that mention belongs at
        // `dst` after the move, so drop it here — `replace_expr_kind` re-adds it
        // for `dst` when `src_kind` is itself a `Local`.
        if let ExprKind::Local { index, .. } = &src_kind {
            let index = *index;
            if let Some(u) = self.buf.uses.get_mut(index as usize) {
                u.reads.retain(|&r| r != src);
            }
        }
        self.body.exprs[dst].type_id = type_id;
        self.body.exprs[dst].span = span;
        self.replace_expr_kind(dst, src_kind);
    }

    /// Edit API: replace a block's statement list. Re-parents the new
    /// statements to the block, re-enqueues them, the block, and the block's
    /// parent. Statements dropped from the list become unreachable (dead nodes
    /// are not freed mid-run; the arena → tree lowering only walks the live
    /// tree). Use index entries for any `Let` in a dropped statement are left
    /// in place — they name a now-dead def and are simply never consulted.
    pub fn set_block_stmts(&mut self, block: BlockId, stmts: Vec<StmtId>) {
        self.census_note_structure();
        self.body.blocks[block].stmts = stmts;
        let kids = self.body.blocks[block].stmts.clone();
        for s in &kids {
            self.set_parent(NodeRef::Stmt(*s), Some(NodeRef::Block(block)));
        }
        for s in &kids {
            self.enqueue(NodeRef::Stmt(*s));
        }
        self.enqueue(NodeRef::Block(block));
        if let Some(p) = self.parent_of(NodeRef::Block(block)) {
            self.enqueue(p);
        }
    }

    /// Edit API: allocate a fresh expression node. Sets the parent of its id
    /// children, registers a `Local` mention, and enqueues it (and its
    /// children) so a freshly built subtree is visited.
    pub fn alloc_expr(&mut self, kind: ExprKind, type_id: TypeId, span: Span) -> ExprId {
        let id = self.body.exprs.push(ExprNode {
            kind,
            type_id,
            span,
        });
        self.buf.expr_parent.push(None);
        self.buf.expr_queued.push(false);
        self.census_note_node_operands(NodeRef::Expr(id));
        let mut children = Vec::new();
        self.body
            .for_each_child(NodeRef::Expr(id), |c| children.push(c));
        for c in children {
            self.set_parent(c, Some(NodeRef::Expr(id)));
        }
        if let ExprKind::Local { index, .. } = &self.body.exprs[id].kind {
            let index = *index;
            self.buf.uses_entry(index).reads.push(id);
        }
        self.enqueue(NodeRef::Expr(id));
        id
    }

    /// Edit API: intern a fresh constant value into the function's pool and
    /// return it as an `Operand::Value`. For passes
    /// that synthesize a constant in an operand position (a method arg, an
    /// assigned value) without a source node.
    pub fn const_operand(
        &mut self,
        kind: crate::nir_value_graph::ValueKind,
        type_id: crate::tir::TypeId,
    ) -> Operand {
        Operand::Value(self.body.values.alloc_unshared(kind, type_id))
    }

    /// Edit API: allocate a fresh statement node.
    pub fn alloc_stmt(&mut self, kind: StmtKind, span: Span) -> StmtId {
        let id = self.body.stmts.push(StmtNode { kind, span });
        self.buf.stmt_parent.push(None);
        self.buf.stmt_queued.push(false);
        self.census_note_node_operands(NodeRef::Stmt(id));
        let mut children = Vec::new();
        self.body
            .for_each_child(NodeRef::Stmt(id), |c| children.push(c));
        for c in children {
            self.set_parent(c, Some(NodeRef::Stmt(id)));
        }
        if let StmtKind::Let { local_index, .. } = &self.body.stmts[id].kind {
            let index = *local_index;
            let entry = self.buf.uses_entry(index);
            entry.defs += 1;
            entry.def = Some(id);
        }
        self.enqueue(NodeRef::Stmt(id));
        id
    }

    /// Edit API: allocate a fresh block node from a statement list.
    pub fn alloc_block(&mut self, stmts: Vec<StmtId>, span: Span) -> BlockId {
        let id = self.body.blocks.push(BlockNode { stmts, span });
        self.buf.block_parent.push(None);
        self.buf.block_queued.push(false);
        self.census_note_structure();
        let kids: Vec<StmtId> = self.body.blocks[id].stmts.clone();
        for s in kids {
            self.set_parent(NodeRef::Stmt(s), Some(NodeRef::Block(id)));
        }
        self.enqueue(NodeRef::Block(id));
        id
    }

    /// Edit API: allocate a fresh pattern node.
    pub fn alloc_pat(&mut self, kind: PatKind, span: Span) -> PatId {
        let id = self.body.pats.push(PatNode { kind, span });
        self.buf.pat_parent.push(None);
        self.buf.pat_queued.push(false);
        self.census_note_node_operands(NodeRef::Pat(id));
        let mut children = Vec::new();
        self.body
            .for_each_child(NodeRef::Pat(id), |c| children.push(c));
        for c in children {
            self.set_parent(c, Some(NodeRef::Pat(id)));
        }
        self.enqueue(NodeRef::Pat(id));
        id
    }

    /// Deep-copy the expression subtree rooted at `id` into fresh arena nodes,
    /// returning the new root. Needed when a rewrite must duplicate a subtree
    /// it cannot share — the arena is a tree (one parent per node), so two
    /// references to one id would break the parent invariant.
    pub fn clone_expr(&mut self, id: ExprId) -> ExprId {
        let node = self.body.exprs[id].clone();
        let kind = self.clone_expr_kind(node.kind);
        self.alloc_expr(kind, node.type_id, node.span)
    }

    /// Deep-copy an operand: a promoted pure value is shared (same pool); an
    /// effectful subtree is cloned through the engine's use-index-maintaining
    /// `clone_expr`.
    fn clone_operand(&mut self, op: Operand) -> Operand {
        match op {
            Operand::Value(v) => Operand::Value(v),
            Operand::Expr(e) => Operand::Expr(self.clone_expr(e)),
        }
    }

    /// Deep-copy the block subtree rooted at `id` into fresh arena nodes,
    /// returning the new root. Recurses through every nested stmt / expr / pat
    /// via the engine's `alloc_*` and `clone_*` paths, so the new subtree is
    /// fully registered in the parent map and use index. Needed for rewrites
    /// like `labeled_block_fusion` that splice cloned THEN/ELSE branches at
    /// each break site of an LB.
    pub fn clone_block(&mut self, id: BlockId) -> BlockId {
        let node = self.body.blocks[id].clone();
        let stmts = node.stmts.iter().map(|s| self.clone_stmt(*s)).collect();
        self.alloc_block(stmts, node.span)
    }

    fn clone_stmt(&mut self, id: StmtId) -> StmtId {
        let node = self.body.stmts[id].clone();
        let kind = self.clone_stmt_kind(node.kind);
        self.alloc_stmt(kind, node.span)
    }

    fn clone_pat(&mut self, id: PatId) -> PatId {
        let node = self.body.pats[id].clone();
        let kind = self.clone_pat_kind(node.kind);
        self.alloc_pat(kind, node.span)
    }

    fn clone_expr_kind(&mut self, kind: ExprKind) -> ExprKind {
        match kind {
            ExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => ExprKind::GlobalVarSet {
                module_source,
                name,
                value: self.clone_operand(value),
            },
            ExprKind::Binary { left, op, right } => ExprKind::Binary {
                left: self.clone_operand(left),
                op,
                right: self.clone_operand(right),
            },
            ExprKind::Unary { op, expr } => ExprKind::Unary {
                op,
                expr: self.clone_operand(expr),
            },
            ExprKind::Assign { target, value } => ExprKind::Assign {
                target: self.clone_expr(target),
                value: self.clone_operand(value),
            },
            ExprKind::Cast { expr, target_type } => ExprKind::Cast {
                expr: self.clone_operand(expr),
                target_type,
            },
            ExprKind::Call {
                func_id,
                type_args,
                args,
                has_receiver,
            } => ExprKind::Call {
                func_id,
                type_args,
                args: args
                    .into_iter()
                    .map(|a| crate::nir_arena::ArenaCallArg {
                        expr: self.clone_operand(a.expr),
                        is_mut: a.is_mut,
                    })
                    .collect(),
                has_receiver,
            },
            ExprKind::CmRawCall { target, args } => ExprKind::CmRawCall {
                target,
                args: args.into_iter().map(|a| self.clone_operand(a)).collect(),
            },
            ExprKind::FieldAccess {
                expr,
                field_index,
                field_name,
            } => ExprKind::FieldAccess {
                expr: self.clone_operand(expr),
                field_index,
                field_name,
            },
            ExprKind::Index { expr, index } => ExprKind::Index {
                expr: self.clone_operand(expr),
                index: self.clone_operand(index),
            },
            ExprKind::Block(b) => ExprKind::Block(self.clone_block(b)),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => ExprKind::If {
                condition: self.clone_operand(condition),
                then_branch: self.clone_block(then_branch),
                else_branch: else_branch.map(|b| self.clone_block(b)),
            },
            ExprKind::Match { expr, arms } => ExprKind::Match {
                expr: self.clone_operand(expr),
                arms: arms
                    .into_iter()
                    .map(|a| ArmData {
                        pattern: self.clone_pat(a.pattern),
                        guard: a.guard.map(|g| self.clone_operand(g)),
                        body: self.clone_operand(a.body),
                        span: a.span,
                    })
                    .collect(),
            },
            ExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => ExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields: fields
                    .into_iter()
                    .map(|f| crate::nir_arena::ArenaStructField {
                        name: f.name,
                        value: self.clone_operand(f.value),
                        field_index: f.field_index,
                    })
                    .collect(),
            },
            ExprKind::TupleLiteral { elements } => ExprKind::TupleLiteral {
                elements: elements
                    .into_iter()
                    .map(|e| self.clone_operand(e))
                    .collect(),
            },
            ExprKind::ArrayLiteral { elements } => ExprKind::ArrayLiteral {
                elements: elements
                    .into_iter()
                    .map(|e| self.clone_operand(e))
                    .collect(),
            },
            ExprKind::IndirectCall { callee, args } => ExprKind::IndirectCall {
                callee: self.clone_operand(callee),
                args: args.into_iter().map(|a| self.clone_operand(a)).collect(),
            },
            ExprKind::ClosureToCanonical {
                functor,
                functor_id,
                target_fn_type,
                closure_module,
            } => ExprKind::ClosureToCanonical {
                functor: self.clone_operand(functor),
                functor_id,
                target_fn_type,
                closure_module,
            },
            ExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload,
            } => ExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload: payload.map(|p| self.clone_operand(p)),
            },
            ExprKind::LabeledBlock {
                label,
                block,
                result_type,
            } => ExprKind::LabeledBlock {
                label,
                block: self.clone_block(block),
                result_type,
            },
            ExprKind::VariantTag { expr } => ExprKind::VariantTag {
                expr: self.clone_operand(expr),
            },
            ExprKind::VariantTest {
                expr,
                case_index,
                case_name,
            } => ExprKind::VariantTest {
                expr: self.clone_operand(expr),
                case_index,
                case_name,
            },
            ExprKind::VariantPayload {
                expr,
                case_index,
                payload_type,
            } => ExprKind::VariantPayload {
                expr: self.clone_operand(expr),
                case_index,
                payload_type,
            },
            ExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => ExprKind::Switch {
                scrutinee: self.clone_operand(scrutinee),
                min_value,
                arms: arms.into_iter().map(|a| self.clone_block(a)).collect(),
                default: self.clone_block(default),
            },
            // Leaves carry no id children.
            leaf => leaf,
        }
    }

    fn clone_stmt_kind(&mut self, kind: StmtKind) -> StmtKind {
        match kind {
            StmtKind::Let {
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                value,
                skip_value_copy,
            } => StmtKind::Let {
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                value: self.clone_operand(value),
                skip_value_copy,
            },
            StmtKind::Expr(e) => StmtKind::Expr(self.clone_operand(e)),
            StmtKind::Return { value } => StmtKind::Return {
                value: value.map(|e| self.clone_operand(e)),
            },
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => StmtKind::If {
                condition: self.clone_operand(condition),
                then_block: self.clone_block(then_block),
                else_block: else_block.map(|b| self.clone_block(b)),
            },
            StmtKind::Loop { body } => StmtKind::Loop {
                body: self.clone_block(body),
            },
            StmtKind::Break { label, value } => StmtKind::Break {
                label,
                value: value.map(|e| self.clone_operand(e)),
            },
            StmtKind::Continue => StmtKind::Continue,
            StmtKind::LabeledBlock { label, block } => StmtKind::LabeledBlock {
                label,
                block: self.clone_block(block),
            },
            StmtKind::LetDestructure {
                pattern,
                is_mut,
                value,
            } => StmtKind::LetDestructure {
                pattern: self.clone_pat(pattern),
                is_mut,
                value: self.clone_operand(value),
            },
        }
    }

    fn clone_pat_kind(&mut self, kind: PatKind) -> PatKind {
        match kind {
            PatKind::Tuple(ps, rest) => {
                PatKind::Tuple(ps.into_iter().map(|p| self.clone_pat(p)).collect(), rest)
            }
            PatKind::Or(ps) => PatKind::Or(ps.into_iter().map(|p| self.clone_pat(p)).collect()),
            PatKind::Variant {
                enum_type,
                variant_name,
                bindings,
                payload_type,
            } => PatKind::Variant {
                enum_type,
                variant_name,
                bindings: bindings.into_iter().map(|p| self.clone_pat(p)).collect(),
                payload_type,
            },
            PatKind::Struct {
                struct_type,
                fields,
                has_rest,
            } => PatKind::Struct {
                struct_type,
                fields: fields
                    .into_iter()
                    .map(|f| crate::nir_arena::ArenaStructPatternField {
                        field_name: f.field_name,
                        field_index: f.field_index,
                        pattern: self.clone_pat(f.pattern),
                    })
                    .collect(),
                has_rest,
            },
            PatKind::ConstantValue { expr } => PatKind::ConstantValue {
                expr: self.clone_operand(expr),
            },
            // Leaves carry no id children.
            leaf => leaf,
        }
    }

    /// Drive the worklist to a local fixed point with `rules`. Pops a node,
    /// tries the rules in priority order; the first that reports a change
    /// re-processes the node (its kind may now match a different rule). Edits
    /// re-enqueue the affected neighbourhood, so a node is revisited only when
    /// something near it changed. Returns `true` if any rule fired, so a
    /// caller threading the legacy global fixed-point's "changed" flag stays
    /// accurate.
    pub fn run(&mut self, rules: &[&dyn Rule]) -> bool {
        let mut any = false;
        while let Some(node) = self.pop() {
            loop {
                let mut changed = false;
                for rule in rules {
                    let fired = match node {
                        NodeRef::Expr(id) => rule.apply_expr(self, id),
                        NodeRef::Block(id) => rule.apply_block(self, id),
                        NodeRef::Stmt(_) | NodeRef::Pat(_) => false,
                    };
                    if fired {
                        changed = true;
                        any = true;
                        break;
                    }
                }
                if !changed {
                    break;
                }
            }
        }
        #[cfg(debug_assertions)]
        if let Some(local) = self.body.surviving_read(&self.buf.elided_locals) {
            panic!(
                "[NIR engine] a rule reported eliding local {local}, but a reachable \
                 read of it survived this session — the binding it named is gone and \
                 that read now has no definition. The reporting rules are \
                 `elide_local` and `labeled_block_fusion`."
            );
        }
        debug_assert!(
            self.promoted_reads
                .get()
                .is_none_or(|memo| *memo == self.body.promoted_read_counts()),
            "[NIR engine] the promoted-read census disagrees with a fresh walk, so \
             an edit changed which operands are reachable without reporting it. \
             Every mutating edit calls `census_note_*`, and a rule writes operands \
             through `Engine`, never `engine.body`. Unreported, `elide_local` \
             deletes a binding the value pool still reads."
        );
        any
    }
}

/// A single-node local rewrite. The engine applies rules at a node when it is
/// popped from the worklist or re-enqueued after a neighbouring change. A rule
/// overrides whichever entry point(s) it needs; the rest default to a no-op.
pub trait Rule {
    /// Try to rewrite the expression node `id`; return `true` if it changed.
    /// Most peephole rewrites are expression-typed.
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let _ = (engine, id);
        false
    }

    /// Try to rewrite the block node `id` — a statement-list rewrite that
    /// splices in, drops, or reorders statements; return `true` if it changed.
    fn apply_block(&self, engine: &mut Engine, id: BlockId) -> bool {
        let _ = (engine, id);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir::NirBinaryOp;
    use crate::nir_arena::{BlockNode, ExprNode, StmtNode};
    use crate::tir::TypeTable;
    use crate::token::Span;

    /// Build a `Body` whose root block holds the statements `build` produces.
    fn mk_body(build: impl FnOnce(&mut Body) -> Vec<StmtId>) -> Body {
        let mut body = Body::empty();
        let stmts = build(&mut body);
        body.root = body.blocks.push(BlockNode {
            stmts,
            span: Span::default(),
        });
        body
    }

    fn e(body: &mut Body, kind: ExprKind) -> ExprId {
        body.exprs.push(ExprNode {
            kind,
            type_id: TypeTable::UNIT,
            span: Span::default(),
        })
    }
    fn lit(body: &mut Body, n: u64) -> Operand {
        Operand::Value(body.values.alloc_unshared(
            crate::nir_value_graph::ValueKind::Int(n, TypeTable::I32),
            TypeTable::I32,
        ))
    }
    fn bin(
        body: &mut Body,
        left: impl Into<Operand>,
        op: NirBinaryOp,
        right: impl Into<Operand>,
    ) -> ExprId {
        e(
            body,
            ExprKind::Binary {
                left: left.into(),
                op,
                right: right.into(),
            },
        )
    }
    fn local0(body: &mut Body) -> ExprId {
        e(
            body,
            ExprKind::Local {
                index: 0,
                name: "x".to_string(),
            },
        )
    }
    fn s(body: &mut Body, kind: StmtKind) -> StmtId {
        body.stmts.push(StmtNode {
            kind,
            span: Span::default(),
        })
    }
    fn let_x(body: &mut Body, value: ExprId, is_mut: bool) -> StmtId {
        s(
            body,
            StmtKind::Let {
                name: "x".to_string(),
                local_index: 0,
                is_mut,
                is_reactive: false,
                type_id: TypeTable::UNIT,
                value: value.into(),
                skip_value_copy: false,
            },
        )
    }
    fn ret_x(body: &mut Body) -> StmtId {
        let xref = local0(body);
        s(
            body,
            StmtKind::Return {
                value: Some(xref.into()),
            },
        )
    }

    /// `{ let x = 1 + 2; return x; }`
    fn sample_body() -> Body {
        mk_body(|b| {
            let one = lit(b, 1);
            let two = lit(b, 2);
            let add = bin(b, one, NirBinaryOp::Add, two);
            let let_stmt = let_x(b, add, false);
            let ret = ret_x(b);
            vec![let_stmt, ret]
        })
    }

    #[test]
    fn use_index_tracks_def_and_reads() {
        let mut body = sample_body();
        let mut __buf_eng = EngineBuffers::default();
        let mut __locals_eng: Vec<NirLocal> = Vec::new();
        let eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
        // local 0 is read once (the `return x`) and defined once (the `let`).
        assert_eq!(eng.local_reads(0).len(), 1);
        assert!(eng.local_def(0).is_some());
    }

    /// A rule that reports eliding a local whose read is still reachable is a
    /// rewrite that deleted a binding out from under a live read. The session
    /// end catches it, so no rule has to walk the body per application.
    #[test]
    #[should_panic(expected = "reported eliding local 0")]
    #[cfg(debug_assertions)]
    fn elided_local_with_a_surviving_read_trips_the_session_audit() {
        let mut body = sample_body();
        let mut __buf_eng = EngineBuffers::default();
        let mut __locals_eng: Vec<NirLocal> = Vec::new();
        let mut eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
        // `sample_body` still contains `return x`, so claiming local 0 was
        // elided is exactly the bug the audit exists to catch.
        eng.note_elided_local(0);
        eng.run(&[]);
    }

    /// The audit is scoped to what a rule actually reported, so an untouched
    /// local — read or not — never trips it.
    #[test]
    fn an_unreported_local_never_trips_the_session_audit() {
        let mut body = sample_body();
        let mut __buf_eng = EngineBuffers::default();
        let mut __locals_eng: Vec<NirLocal> = Vec::new();
        let mut eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
        eng.run(&[]);
    }

    #[test]
    fn parents_link_children_to_their_node() {
        let mut body = sample_body();
        let root = body.root;
        let mut __buf_eng = EngineBuffers::default();
        let mut __locals_eng: Vec<NirLocal> = Vec::new();
        let eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
        // Every statement of the root block has the root block as its parent.
        for &s in &eng.body.blocks[root].stmts {
            assert_eq!(eng.parent_of(NodeRef::Stmt(s)), Some(NodeRef::Block(root)));
        }
        // The root block itself has no parent.
        assert_eq!(eng.parent_of(NodeRef::Block(root)), None);
        // Every expr's parent is set (none is orphaned except via the root).
        for id in eng.body.exprs.keys() {
            assert!(eng.parent_of(NodeRef::Expr(id)).is_some());
        }
    }

    /// Demo rule: fold `Binary(IntLiteral, Add|Mul, IntLiteral)` to a literal.
    /// Exercises the full engine loop (worklist + rule dispatch + edit API +
    /// parent re-enqueue) without depending on `niri`.
    struct FoldAddMulConst;
    impl Rule for FoldAddMulConst {
        fn apply_expr(&self, e: &mut Engine, id: ExprId) -> bool {
            let (op, l, r) = match &e.body.exprs[id].kind {
                ExprKind::Binary { left, op, right } => (*op, *left, *right),
                _ => return false,
            };
            let (Some(lv), Some(rv)) = (e.body.operand_const_int(l), e.body.operand_const_int(r))
            else {
                return false;
            };
            let v = match op {
                NirBinaryOp::Add => lv.wrapping_add(rv),
                NirBinaryOp::Mul => lv.wrapping_mul(rv),
                _ => return false,
            };
            // Promote the folded scalar into the parent operand slot.
            e.replace_expr_with_value(
                id,
                crate::const_eval::Value::Int {
                    value: v,
                    prim: crate::tir::PrimitiveType::I32,
                },
            )
        }
    }

    /// `{ let x = (1 + 2) * 4; return x; }` folds to `let x = 12;`.
    #[test]
    fn engine_folds_nested_arith_bottom_up() {
        let mut body = mk_body(|b| {
            let one = lit(b, 1);
            let two = lit(b, 2);
            let inner = bin(b, one, NirBinaryOp::Add, two);
            let four = lit(b, 4);
            let outer = bin(b, inner, NirBinaryOp::Mul, four);
            let let_stmt = let_x(b, outer, false);
            vec![let_stmt]
        });
        {
            let mut __buf_eng = EngineBuffers::default();
            let mut __locals_eng: Vec<NirLocal> = Vec::new();
            let mut eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
            eng.run(&[&FoldAddMulConst]);
        }
        // The let's value is now the promoted constant 12.
        let root = body.root;
        let s0 = body.blocks[root].stmts[0];
        let StmtKind::Let { value, .. } = &body.stmts[s0].kind else {
            panic!("expected let");
        };
        let Operand::Value(v) = value else {
            panic!("expected folded constant operand, got {value:?}");
        };
        assert!(matches!(
            body.values.kind(*v),
            crate::nir_value_graph::ValueKind::Int(12, _)
        ));
    }

    #[test]
    fn clone_expr_deep_copies_into_fresh_nodes() {
        // `{ let x = 1 + 2; return x; }`; clone the `1 + 2` subtree.
        let mut body = sample_body();
        let root = body.root;
        let s0 = body.blocks[root].stmts[0];
        let StmtKind::Let { value, .. } = &body.stmts[s0].kind else {
            panic!("expected let");
        };
        let original = value.as_expr().unwrap();
        let (orig_left, orig_right) = match &body.exprs[original].kind {
            ExprKind::Binary { left, right, .. } => (*left, *right),
            other => panic!("expected Binary, got {other:?}"),
        };
        let before = body.exprs.len();
        let clone = {
            let mut __buf_eng = EngineBuffers::default();
            let mut __locals_eng: Vec<NirLocal> = Vec::new();
            let mut eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
            eng.clone_expr(original)
        };
        // Only the Binary node is copied: its `1` and `2` are pure-value operands
        // (immutable pool values), shared by the clone, not deep-copied.
        assert_eq!(body.exprs.len(), before + 1);
        assert_ne!(clone, original);
        let (new_left, new_right) = match &body.exprs[clone].kind {
            ExprKind::Binary { left, right, .. } => (*left, *right),
            other => panic!("expected cloned Binary, got {other:?}"),
        };
        assert_eq!(new_left, orig_left);
        assert_eq!(new_right, orig_right);
    }

    /// Demo block rule: drop every `()`-valued expression statement from a
    /// block. Exercises `apply_block` dispatch and the `set_block_stmts` edit.
    struct DropUnitStmts;
    impl Rule for DropUnitStmts {
        fn apply_block(&self, e: &mut Engine, id: BlockId) -> bool {
            let stmts = e.body.blocks[id].stmts.clone();
            let kept: Vec<StmtId> = stmts
                .iter()
                .copied()
                .filter(|s| {
                    !matches!(&e.body.stmts[*s].kind,
                        StmtKind::Expr(op) if op.as_value().is_some_and(|v| matches!(e.body.values.kind(v), crate::nir_value_graph::ValueKind::Unit)))
                })
                .collect();
            if kept.len() == stmts.len() {
                return false;
            }
            e.set_block_stmts(id, kept);
            true
        }
    }

    /// `{ let x = 1 + 2; (); return x; }` drops the bare `()` statement.
    #[test]
    fn block_rule_splices_statement_list() {
        let mut body = mk_body(|b| {
            let one = lit(b, 1);
            let two = lit(b, 2);
            let add = bin(b, one, NirBinaryOp::Add, two);
            let let_stmt = let_x(b, add, false);
            let unit = Operand::Value(b.values.alloc_unshared(
                crate::nir_value_graph::ValueKind::Unit,
                crate::tir::TypeTable::UNIT,
            ));
            let unit_stmt = s(b, StmtKind::Expr(unit));
            let ret = ret_x(b);
            vec![let_stmt, unit_stmt, ret]
        });
        let root = body.root;
        assert_eq!(body.blocks[root].stmts.len(), 3);
        {
            let mut __buf_eng = EngineBuffers::default();
            let mut __locals_eng: Vec<NirLocal> = Vec::new();
            let mut eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
            assert!(eng.run(&[&DropUnitStmts]));
        }
        // The `()` statement is gone; the `let` and `return` remain in order.
        let kept = &body.blocks[root].stmts;
        assert_eq!(kept.len(), 2);
        assert!(matches!(body.stmts[kept[0]].kind, StmtKind::Let { .. }));
        assert!(matches!(body.stmts[kept[1]].kind, StmtKind::Return { .. }));
    }

    #[test]
    fn is_local_read_ignores_bare_assign_targets() {
        // `{ let x = 1 + 2; x = 7; }` — local 0 is written but never read, so
        // `is_local_read(0)` is false (its only mention is the assign target).
        let mut body = mk_body(|b| {
            let one = lit(b, 1);
            let two = lit(b, 2);
            let add = bin(b, one, NirBinaryOp::Add, two);
            let let_stmt = let_x(b, add, true);
            let target = local0(b);
            let seven = lit(b, 7);
            let assign = e(
                b,
                ExprKind::Assign {
                    target,
                    value: seven,
                },
            );
            let assign_stmt = s(b, StmtKind::Expr(assign.into()));
            vec![let_stmt, assign_stmt]
        });
        let mut __buf_eng = EngineBuffers::default();
        let mut __locals_eng: Vec<NirLocal> = Vec::new();
        let eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
        assert!(!eng.is_local_read(0));

        // The same body with a trailing `return x` makes local 0 read.
        let mut body2 = mk_body(|b| {
            let one = lit(b, 1);
            let two = lit(b, 2);
            let add2 = bin(b, one, NirBinaryOp::Add, two);
            let let2 = let_x(b, add2, true);
            let ret = ret_x(b);
            vec![let2, ret]
        });
        let mut __buf_eng2 = EngineBuffers::default();
        let mut __locals_eng2: Vec<NirLocal> = Vec::new();
        let eng2 = Engine::new(&mut body2, &mut __buf_eng2, &mut __locals_eng2);
        assert!(eng2.is_local_read(0));
    }

    #[test]
    fn become_expr_moves_node_and_updates_use_index() {
        // `{ x; 1 + 2; }` — promote the `x` local read into the `1 + 2` node.
        let mut body = mk_body(|b| {
            let lx = local0(b);
            let s_x = s(b, StmtKind::Expr(lx.into()));
            let one = lit(b, 1);
            let two = lit(b, 2);
            let add = bin(b, one, NirBinaryOp::Add, two);
            let s_add = s(b, StmtKind::Expr(add.into()));
            vec![s_x, s_add]
        });
        let root = body.root;
        let StmtKind::Expr(Operand::Expr(lx)) = body.stmts[body.blocks[root].stmts[0]].kind else {
            panic!("expected expr stmt");
        };
        let StmtKind::Expr(Operand::Expr(add)) = body.stmts[body.blocks[root].stmts[1]].kind else {
            panic!("expected expr stmt");
        };
        {
            let mut __buf_eng = EngineBuffers::default();
            let mut __locals_eng: Vec<NirLocal> = Vec::new();
            let mut eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
            // Before: local 0 is read only by the `x` node.
            assert_eq!(eng.local_reads(0), &[lx]);
            eng.become_expr(add, lx);
            // After: `add` now holds the local read; `lx` is dead and dropped,
            // and the use index tracks the read at its new home.
            assert!(matches!(
                eng.body.exprs[add].kind,
                ExprKind::Local { index: 0, .. }
            ));
            assert!(matches!(eng.body.exprs[lx].kind, ExprKind::Dead));
            assert_eq!(eng.local_reads(0), &[add]);
        }
    }

    #[test]
    fn worklist_seeds_every_node_once() {
        let mut body = sample_body();
        let total = body.exprs.len() + body.stmts.len() + body.blocks.len() + body.pats.len();
        let mut __buf_eng = EngineBuffers::default();
        let mut __locals_eng: Vec<NirLocal> = Vec::new();
        let mut eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
        let mut popped = 0;
        while eng.pop().is_some() {
            popped += 1;
        }
        assert_eq!(popped, total);
    }

    /// `alloc_block`/`alloc_pat` push a `false` into `block_queued`/`pat_queued`
    /// alongside the existing `block_parent`/`pat_parent` push. Neither path is
    /// exercised by any other test (`clone_expr_deep_copies_into_fresh_nodes`
    /// only clones a `Binary` over literal operands, never reaching
    /// `clone_block`/`clone_pat`), so a regression that drops or reorders the
    /// `_queued` push would panic here (out-of-bounds index in `enqueue`'s
    /// `is_queued` check) instead of on real optimizer input.
    #[test]
    fn alloc_block_and_alloc_pat_extend_the_queued_bitsets() {
        let mut body = sample_body();
        let mut __buf_eng = EngineBuffers::default();
        let mut __locals_eng: Vec<NirLocal> = Vec::new();
        let mut eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
        // Drain the worklist seeded at construction so only the freshly
        // `alloc_*`'d nodes remain below.
        while eng.pop().is_some() {}

        let new_block = eng.alloc_block(Vec::new(), Span::default());
        let new_pat = eng.alloc_pat(PatKind::Wildcard, Span::default());

        // Fresh nodes start unparented — `alloc_block`/`alloc_pat` only parent
        // their *children* (there are none here), not themselves.
        assert_eq!(eng.parent_of(NodeRef::Block(new_block)), None);
        assert_eq!(eng.parent_of(NodeRef::Pat(new_pat)), None);

        // Both were enqueued by their `alloc_*` call; draining the worklist
        // must surface exactly them, in allocation order.
        let mut popped = Vec::new();
        while let Some(n) = eng.pop() {
            popped.push(n);
        }
        assert_eq!(
            popped,
            vec![NodeRef::Block(new_block), NodeRef::Pat(new_pat)]
        );
    }

    /// `uses_entry` grows `EngineBuffers::uses` on demand for a local
    /// `alloc_local`'d after `reset_for` pre-sized it to the session's initial
    /// local count. No other test calls `alloc_local`, so this growth path was
    /// previously untested — a regression in `uses_entry`'s bounds check or
    /// `resize_with` length would surface here rather than only on a real
    /// program that allocates a local mid-pass.
    #[test]
    fn alloc_local_extends_the_use_index_past_reset_fors_initial_size() {
        // An empty body (not `sample_body()`, which already occupies local
        // index 0 for `x`) so the newly `alloc_local`'d index doesn't collide
        // with a pre-existing `Local` mention.
        let mut body = mk_body(|_| Vec::new());
        let mut __buf_eng = EngineBuffers::default();
        let mut __locals_eng: Vec<NirLocal> = Vec::new();
        let mut eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);

        // The session starts with 0 locals (`__locals_eng` is empty), so
        // `reset_for` pre-sizes `uses` to length 0; `alloc_local` mints an
        // index beyond that.
        let new_local = eng.alloc_local("y".to_string(), TypeTable::I32, false);
        assert_eq!(new_local, 0);
        let read = eng.alloc_expr(
            ExprKind::Local {
                index: new_local,
                name: "y".to_string(),
            },
            TypeTable::I32,
            Span::default(),
        );
        assert_eq!(eng.local_reads(new_local), &[read]);
        assert_eq!(eng.local_def(new_local), None);
    }

    /// A stale "no promoted read" is what lets `elide_local` delete a binding
    /// the value pool still reads, so an edit that plants one drops the memo.
    #[test]
    fn promoted_read_census_refreshes_after_an_edit() {
        // `{ let x = 1; }` — the binding, and no read of it anywhere.
        let mut body = mk_body(|b| {
            let one = lit(b, 1);
            let init = e(
                b,
                ExprKind::Cast {
                    expr: one,
                    target_type: TypeTable::I32,
                },
            );
            vec![let_x(b, init, false)]
        });
        let root = body.root;
        let mut __buf_eng = EngineBuffers::default();
        let mut __locals_eng: Vec<NirLocal> = vec![NirLocal::synth(0, TypeTable::I32, false)];
        let mut eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
        // Fills the memo: nothing in the pool reads `x` yet.
        assert!(!eng.reads_promoted_local(0));
        assert_eq!(eng.promoted_read_count(0), 0);

        // Splice in `x;` as a promoted `Opaque(Local 0)` operand — a read with
        // no skeleton node, invisible to the use index.
        let read = eng.body.values.canonical_local(0, TypeTable::I32);
        let stmt = eng.alloc_stmt(StmtKind::Expr(Operand::Value(read)), Span::default());
        let stmts = eng.body.blocks[root].stmts.clone();
        eng.set_block_stmts(root, [stmts, vec![stmt]].concat());

        assert!(
            eng.local_reads(0).is_empty(),
            "the read is not in the skeleton"
        );
        assert!(eng.reads_promoted_local(0));
        assert_eq!(eng.promoted_read_count(0), 1);
    }

    /// `def` is the first `Let` in source order. A local should have exactly
    /// one, but a cloned block carries its `local_index` along, so the pick is
    /// pinned here rather than left to the traversal direction.
    #[test]
    fn local_def_is_the_first_binding_in_source_order() {
        let mut first = None;
        let mut body = mk_body(|b| {
            let one = lit(b, 1);
            let a = e(
                b,
                ExprKind::Cast {
                    expr: one,
                    target_type: TypeTable::I32,
                },
            );
            let two = lit(b, 2);
            let c = e(
                b,
                ExprKind::Cast {
                    expr: two,
                    target_type: TypeTable::I32,
                },
            );
            let s1 = let_x(b, a, false);
            let s2 = let_x(b, c, false);
            first = Some(s1);
            vec![s1, s2]
        });
        let mut __buf_eng = EngineBuffers::default();
        let mut __locals_eng: Vec<NirLocal> = vec![NirLocal::synth(0, TypeTable::I32, false)];
        let eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
        assert_eq!(eng.local_def(0), first);
    }

    /// A `let mut` the body never reassigns holds one value: the flag records
    /// what the source wrote, the use index what the body does.
    #[test]
    fn an_unreassigned_mut_binding_has_one_version() {
        let mut body = mk_body(|b| {
            let one = lit(b, 1);
            let init = e(
                b,
                ExprKind::Cast {
                    expr: one,
                    target_type: TypeTable::I32,
                },
            );
            vec![let_x(b, init, /* is_mut */ true), ret_x(b)]
        });
        let mut __buf_eng = EngineBuffers::default();
        let mut __locals_eng: Vec<NirLocal> = vec![NirLocal::synth(0, TypeTable::I32, true)];
        let mut eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
        assert!(eng.local_has_one_version(0));
    }

    /// Two `Let`s binding one index are two versions, whatever the flag says.
    /// `labeled_block_fusion` clones a block, `local_index` and all.
    #[test]
    fn a_twice_bound_local_has_no_single_version() {
        let mut body = mk_body(|b| {
            let one = lit(b, 1);
            let a = e(
                b,
                ExprKind::Cast {
                    expr: one,
                    target_type: TypeTable::I32,
                },
            );
            let two = lit(b, 2);
            let c = e(
                b,
                ExprKind::Cast {
                    expr: two,
                    target_type: TypeTable::I32,
                },
            );
            vec![let_x(b, a, false), let_x(b, c, false), ret_x(b)]
        });
        let mut __buf_eng = EngineBuffers::default();
        let mut __locals_eng: Vec<NirLocal> = vec![NirLocal::synth(0, TypeTable::I32, false)];
        let mut eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);
        assert!(!eng.local_has_one_version(0));
    }

    /// A fill between the allocation and the splice sees the operand
    /// unreachable, and correctly leaves it out; the splice has to drop that
    /// memo. `licm`'s pre-header hoist has this shape (`all_hoist_stmts`), and
    /// never runs the worklist, so the end-of-`run` audit would not catch it.
    #[test]
    fn promoted_read_census_survives_an_allocate_then_attach() {
        let mut body = mk_body(|b| {
            let one = lit(b, 1);
            let init = e(
                b,
                ExprKind::Cast {
                    expr: one,
                    target_type: TypeTable::I32,
                },
            );
            vec![let_x(b, init, false)]
        });
        let root = body.root;
        let mut __buf_eng = EngineBuffers::default();
        let mut __locals_eng: Vec<NirLocal> = vec![NirLocal::synth(0, TypeTable::I32, false)];
        let mut eng = Engine::new(&mut body, &mut __buf_eng, &mut __locals_eng);

        // Allocated detached: the operand names `x`, but nothing reaches it.
        let read = eng.body.values.canonical_local(0, TypeTable::I32);
        let stmt = eng.alloc_stmt(StmtKind::Expr(Operand::Value(read)), Span::default());
        // Fill the memo while it is still detached — correctly empty.
        assert!(!eng.reads_promoted_local(0));

        let stmts = eng.body.blocks[root].stmts.clone();
        eng.set_block_stmts(root, [stmts, vec![stmt]].concat());

        assert!(
            eng.reads_promoted_local(0),
            "attaching a detached local-naming operand must not leave the empty census standing"
        );
    }
}
