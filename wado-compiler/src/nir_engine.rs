//! NIR rewrite engine (Phase 4).
//!
//! A worklist-driven engine that runs local (peephole) rewrites over one
//! function's [`Body`] to a local fixed point — visiting a node only when it
//! might be reducible, rather than the ~31 whole-tree passes the current
//! optimizer runs in a global fixed point.
//!
//! See `docs/wep-2026-06-05-nir-rewrite-engine-design.md`.
//!
//! The module provides the full engine: the *session* (the parent map, the
//! local use index, and the worklist, built once per function from a `Body`),
//! the mutating edit API (`replace_expr_kind`, `set_block_stmts`,
//! `alloc_*`, `clone_expr`) that keeps the parent map and use index coherent,
//! the [`Rule`] trait, and the `run` driver. Genuinely-local peephole passes
//! (`select_lowering`, `match_to_switch`, `string_push`, `array_literal`,
//! `elide_local`) run as rules over it.

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

/// Per-local use information: where the local is defined and every node that
/// reads it. Used by the engine to re-enqueue a local's uses when its
/// definition is rewritten (copy propagation, const-fold through a `Let`, …).
#[derive(Default, Debug)]
pub struct LocalUses {
    /// The `Let` / `LetDestructure` statement that binds the local, if any.
    /// Parameters have no defining statement.
    pub def: Option<StmtId>,
    /// Every `Local { index }` expression node that names this local. (The
    /// place-form distinction — read vs write — is refined when a rule needs
    /// it; for re-enqueue purposes every mention is a use.)
    pub reads: Vec<ExprId>,
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
    uses: IndexMap<u32, LocalUses>,
    worklist: VecDeque<NodeRef>,
    queued: IndexSet<NodeRef>,
}

impl EngineBuffers {
    /// Clear every buffer and size the parent maps to `body`'s current node
    /// counts (`None`-filled), readying them for a fresh session. Capacity is
    /// retained, so a reused `EngineBuffers` allocates only when a body is
    /// larger than any seen before.
    fn reset_for(&mut self, body: &Body) {
        let fill = |v: &mut Vec<Option<NodeRef>>, len: usize| {
            v.clear();
            v.resize(len, None);
        };
        fill(&mut self.expr_parent, body.exprs.len());
        fill(&mut self.stmt_parent, body.stmts.len());
        fill(&mut self.block_parent, body.blocks.len());
        fill(&mut self.pat_parent, body.pats.len());
        self.uses.clear();
        self.worklist.clear();
        self.queued.clear();
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
    /// Lazy per-session `ValueGraph` cache. `None` until the first
    /// [`Engine::value`] / [`Engine::value_kind`] call; cleared by
    /// [`Engine::invalidate_value_graph`]. See [`crate::nir_value_graph`]
    /// for the data model.
    value_graph: Option<crate::nir_value_graph::builder::ValueGraphBuild>,
    /// Per-session cache of the body's live `&local` / `&mut local` scan
    /// (see [`Body::collect_address_taken_locals`]). Computed on first use,
    /// cleared with [`Engine::invalidate_value_graph`] so a rebuild after
    /// edits rescans. Consumed by `Local`-read exclusions
    /// (`store_load_forward`, licm's arithmetic hoist).
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
    /// Type table for the `ValueGraph` builder's constant folding of pure
    /// arithmetic. `None` (the default) disables folding. Set via
    /// [`Engine::set_value_graph_type_table`] before the first value query.
    vg_type_table: Option<&'a crate::tir::TypeTable>,
}

impl<'a> Engine<'a> {
    /// Build a session over `body`, reusing the caller-owned `buf`: one O(n)
    /// pass populates the parent map and use index, then the worklist is seeded
    /// with every node in post-order (children before parents) so leaf
    /// reductions are seen before the contexts that might fold them. `locals` is
    /// the owning function's local list (see [`Engine::alloc_local`]).
    pub fn new(
        body: &'a mut Body,
        buf: &'a mut EngineBuffers,
        locals: &'a mut Vec<NirLocal>,
    ) -> Self {
        buf.reset_for(body);
        let mut engine = Self {
            body,
            buf,
            locals,
            value_graph: None,
            body_address_taken: None,
            aliased_locals: IndexSet::default(),
            untrackable_locals: IndexSet::default(),
            mut_escaped_locals: IndexSet::default(),
            param_locals: Vec::new(),
            vg_type_table: None,
        };
        engine.build_parents();
        engine.build_uses();
        engine.seed_post_order();
        engine
    }

    /// Return the [`ValueId`] of `expr` if the per-function `ValueGraph`
    /// assigned one. Returns `None` for impure / allocation-bearing /
    /// control-flow expressions and for any `ExprId` allocated after the
    /// cache was built. Built lazily on first call.
    ///
    /// # Invalidation contract
    ///
    /// Edits via `replace_expr_kind` / `set_block_stmts` / `alloc_*` do
    /// **not** invalidate this cache — it keeps returning the VNs from
    /// build time. Snapshot any VN results before editing, or call
    /// [`Engine::invalidate_value_graph`] to force a rebuild. See
    /// `optimize/cse.rs` and `optimize/store_load_forward.rs`.
    pub fn value(&mut self, expr: ExprId) -> Option<crate::nir_value_graph::ValueId> {
        self.ensure_value_graph();
        self.value_graph.as_ref()?.value_of.get(&expr).copied()
    }

    /// The [`ValueId`] of an operand: the promoted value directly, or the
    /// skeleton expr's value from the graph.
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
        self.value_graph
            .as_ref()
            .unwrap()
            .loop_entry_values
            .get(&loop_body)?
            .get(&local)
            .copied()
    }

    /// Record `expr`'s value in the live graph, keeping it current after an
    /// edit that introduced `expr` or re-pointed it to a known value (e.g. a
    /// pass that creates a temp bound to an already-computed value). The next
    /// graph query then resolves `expr` without a rebuild. No-op when the graph
    /// is not built yet — the lazy build derives the value from the body.
    pub fn set_value(&mut self, expr: ExprId, vid: crate::nir_value_graph::ValueId) {
        if let Some(vg) = self.value_graph.as_mut() {
            vg.value_of.insert(expr, vid);
        }
    }

    /// Maintain the live graph after a value-preserving edit introduced the pure
    /// expression `expr`: compute its `ValueId` from its operands' already-recorded
    /// values, hash-cons it, and record it — no rebuild. Returns the value, or
    /// `None` when the graph is not built, `expr` is impure, or an operand has no
    /// recorded value (a flow-sensitive `Local` / `FieldAccess`, conservatively
    /// skipped — a later query then sees no identity, never a wrong one).
    ///
    /// Sound exactly for the nodes whose value is fixed by their operands'
    /// values — the operand-promotion slots: literals, `Binary`, `Unary` (not the
    /// address-taking `Ref` / `MutRef` / `Deref`), `Cast`. It does not fold
    /// (`2 + 3` stays a `Binary` rather than `5`), so the maintained graph is
    /// never more precise than a fresh build, only equal or conservatively less —
    /// never wrong. This is the primitive structural passes will call at a splice
    /// point to grow the graph instead of forcing a rebuild.
    pub fn maintain_pure_value(&mut self, expr: ExprId) -> Option<crate::nir_value_graph::ValueId> {
        self.value_graph.as_ref()?;
        let kind = self.body.exprs[expr].kind.clone();
        // The pool lives on the body, `value_of` on the cached graph — disjoint
        // fields, borrowed separately so interning and operand lookups coexist.
        let v = {
            let pool = &mut self.body.values;
            let value_of = &self.value_graph.as_ref().unwrap().value_of;
            // A pure operand resolves to its promoted `ValueId` directly, or
            // through `value_of` for a skeleton subtree.
            let operand_value = |op: Operand| match op {
                Operand::Value(v) => Some(v),
                Operand::Expr(e) => value_of.get(&e).copied(),
            };
            match kind {
                ExprKind::Binary { left, op, right } => {
                    let lhs = operand_value(left)?;
                    let rhs = operand_value(right)?;
                    pool.binary(op, lhs, rhs)
                }
                ExprKind::Unary { op, expr: inner } => {
                    use crate::nir::NirUnaryOp;
                    if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref) {
                        return None;
                    }
                    let operand = operand_value(inner)?;
                    pool.unary(op, operand)
                }
                ExprKind::Cast {
                    expr: inner,
                    target_type,
                } => {
                    let operand = operand_value(inner)?;
                    pool.cast(operand, target_type)
                }
                _ => return None,
            }
        };
        self.value_graph.as_mut().unwrap().value_of.insert(expr, v);
        Some(v)
    }

    /// The class representative of `id` in the session's value graph. Two ids
    /// with the same representative denote the same value. See
    /// [`crate::nir_value_graph::ValuePool::find`].
    pub fn value_find(
        &mut self,
        id: crate::nir_value_graph::ValueId,
    ) -> crate::nir_value_graph::ValueId {
        self.ensure_value_graph();
        self.body.values.find(id)
    }

    /// Prove `a ≡ b` in the session's value graph by merging their classes,
    /// returning the surviving representative. Call
    /// [`Engine::rebuild_value_congruence`] after a batch of unions to restore
    /// congruence (so structurally-equal parents re-merge).
    ///
    /// Unions live in the cached graph; a rebuild of the graph
    /// ([`Engine::invalidate_value_graph`] or any `set_*` that drops the cache)
    /// discards them. A pure-value rewrite that unions must therefore not
    /// invalidate the graph before the result is used.
    pub fn value_union(
        &mut self,
        a: crate::nir_value_graph::ValueId,
        b: crate::nir_value_graph::ValueId,
    ) -> crate::nir_value_graph::ValueId {
        self.ensure_value_graph();
        self.body.values.union(a, b)
    }

    /// Restore congruence after a batch of [`Engine::value_union`] calls. See
    /// [`crate::nir_value_graph::ValuePool::rebuild`].
    pub fn rebuild_value_congruence(&mut self) {
        self.ensure_value_graph();
        self.body.values.rebuild();
    }

    /// Drop the cached `ValueGraph` so the next [`Engine::value`] call
    /// rebuilds. Used by rules that intend to query the value graph again
    /// after a structural rewrite that would invalidate the prior build.
    pub fn invalidate_value_graph(&mut self) {
        self.value_graph = None;
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

    /// Record the owning function's parameter local indices so the
    /// lazily-built `ValueGraph` seeds them up front (see the field doc on
    /// `param_locals`). Must be called before the first value query; a later
    /// change forces a rebuild on next query.
    pub fn set_param_locals(&mut self, locals: Vec<u32>) {
        if self.param_locals != locals {
            self.param_locals = locals;
            self.value_graph = None;
        }
    }

    /// Record the function's reference-aliased and `stores`-aliased locals so
    /// the lazily-built `ValueGraph` invalidates field forwarding for them at
    /// the right granularity. Must be called before the first value query; a
    /// later change forces a rebuild on next query. Without it the builder
    /// treats every receiver as non-aliased — sound only when the function has
    /// no reference aliasing, so passes that may see aliasing must supply it.
    pub fn set_alias_sets(
        &mut self,
        aliased: IndexSet<u32>,
        untrackable: IndexSet<u32>,
        mut_escaped: IndexSet<u32>,
    ) {
        if self.aliased_locals != aliased
            || self.untrackable_locals != untrackable
            || self.mut_escaped_locals != mut_escaped
        {
            self.aliased_locals = aliased;
            self.untrackable_locals = untrackable;
            self.mut_escaped_locals = mut_escaped;
            self.value_graph = None;
        }
    }

    /// Provide the type table so the lazily-built `ValueGraph` folds pure
    /// arithmetic on literal operands (`2 + 3 → 5`). Must be called before the
    /// first value query; a later change forces a rebuild on next query.
    pub fn set_value_graph_type_table(&mut self, type_table: &'a crate::tir::TypeTable) {
        if self.vg_type_table.map(std::ptr::from_ref) != Some(std::ptr::from_ref(type_table)) {
            self.vg_type_table = Some(type_table);
            self.value_graph = None;
        }
    }

    /// The type table supplied for value-graph folding, if any. Used by
    /// `store_load_forward` to synthesize a literal `ExprKind` for a folded
    /// value that has no pre-existing source literal.
    pub fn value_graph_type_table(&self) -> Option<&'a crate::tir::TypeTable> {
        self.vg_type_table
    }

    fn ensure_value_graph(&mut self) {
        if self.value_graph.is_some() {
            self.verify_maintained_graph();
            return;
        }
        let build = crate::nir_value_graph::builder::build(
            &mut *self.body,
            &self.param_locals,
            &self.aliased_locals,
            &self.untrackable_locals,
            &self.mut_escaped_locals,
            self.vg_type_table,
        );
        self.value_graph = Some(build);
    }

    /// Soundness net for per-edit maintenance (`WADO_VERIFY_VG`): rebuild a fresh
    /// graph with the same config and assert the maintained partition *refines*
    /// it ([`crate::nir_value_graph::builder::partition_refines`]) — the
    /// maintained graph may merge a pair only if a fresh build also merges it.
    /// An over-merge is a value the optimizer could miscompile through; fail
    /// loudly. Conservative coarsening (the maintained graph splitting a pair a
    /// fresh build merges) is sound — a missed optimization — and is allowed. Off
    /// (and free) unless `WADO_VERIFY_VG` is set; the fresh build makes it slow,
    /// so it is a small-input debugging mode, not a production path.
    fn verify_maintained_graph(&mut self) {
        if !crate::optimize::vg_measure::verify_enabled() || self.value_graph.is_none() {
            return;
        }
        let fresh = crate::nir_value_graph::builder::build(
            &mut *self.body,
            &self.param_locals,
            &self.aliased_locals,
            &self.untrackable_locals,
            &self.mut_escaped_locals,
            self.vg_type_table,
        );
        let maintained = self.value_graph.as_ref().unwrap();
        // Compare over the *live* exprs — exactly those a fresh build walks from
        // the root. An expr present only in `maintained.value_of` is orphaned
        // (a rewrite swapped its parent slot to a promoted value and dropped it
        // from the skeleton); its stale entry can collide with a live expr that
        // shares the same value and read as a spurious over-merge, but a dead
        // node is never emitted and so cannot be miscompiled. Every live pure
        // expr is in the fresh build, so this still checks every value the
        // optimizer could miscompile through.
        let exprs: Vec<ExprId> = fresh.value_of.keys().copied().collect();
        if let Err(msg) = crate::nir_value_graph::builder::partition_refines(
            &mut self.body.values,
            maintained,
            &fresh,
            &exprs,
        ) {
            panic!("WADO_VERIFY_VG: maintained graph over-merges vs a fresh build: {msg}");
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

    /// Set `parent[child] = node` for every id-bearing child of every node.
    ///
    /// Destructuring `self` lets the parent slices be written in place while
    /// `body` is borrowed immutably by `for_each_child`, so no intermediate
    /// edge buffer is materialized.
    fn build_parents(&mut self) {
        let body: &Body = &*self.body;
        let EngineBuffers {
            expr_parent,
            stmt_parent,
            block_parent,
            pat_parent,
            ..
        } = &mut *self.buf;
        let mut set = |child: NodeRef, parent: NodeRef| match child {
            NodeRef::Expr(id) => expr_parent[id.index()] = Some(parent),
            NodeRef::Stmt(id) => stmt_parent[id.index()] = Some(parent),
            NodeRef::Block(id) => block_parent[id.index()] = Some(parent),
            NodeRef::Pat(id) => pat_parent[id.index()] = Some(parent),
        };
        for id in body.exprs.keys() {
            let parent = NodeRef::Expr(id);
            body.for_each_child(parent, |c| set(c, parent));
        }
        for id in body.stmts.keys() {
            let parent = NodeRef::Stmt(id);
            body.for_each_child(parent, |c| set(c, parent));
        }
        for id in body.blocks.keys() {
            let parent = NodeRef::Block(id);
            body.for_each_child(parent, |c| set(c, parent));
        }
        for id in body.pats.keys() {
            let parent = NodeRef::Pat(id);
            body.for_each_child(parent, |c| set(c, parent));
        }
    }

    /// Record, per local index, the defining statement and every reading
    /// `Local` expression node. Walks the live tree from the root rather than
    /// iterating every arena slot, so dead nodes left by an earlier in-place
    /// pass (which never compacts `func.body`) are not counted — the use index
    /// reflects only what is reachable, matching a tree walk.
    fn build_uses(&mut self) {
        let mut stack = vec![NodeRef::Block(self.body.root)];
        while let Some(node) = stack.pop() {
            match node {
                NodeRef::Expr(id) => {
                    if let ExprKind::Local { index, .. } = &self.body.exprs[id].kind {
                        let index = *index;
                        self.buf.uses.entry(index).or_default().reads.push(id);
                    }
                }
                NodeRef::Stmt(id) => {
                    if let StmtKind::Let { local_index, .. } = &self.body.stmts[id].kind {
                        let index = *local_index;
                        self.buf.uses.entry(index).or_default().def = Some(id);
                    }
                }
                NodeRef::Block(_) | NodeRef::Pat(_) => {}
            }
            self.body.for_each_child(node, |c| stack.push(c));
        }
    }

    /// Seed the worklist with every node, children before parents.
    ///
    /// Iterative post-order over an explicit stack. Each node is pushed twice:
    /// once unprocessed (to expand its children) and once processed (to enqueue
    /// it after its subtree). Children are pushed in reverse so they pop — and
    /// thus enqueue — in source order, reproducing the recursive walk's exact
    /// post-order sequence. A single reused `scratch` buffer collects each
    /// node's children, avoiding a per-node allocation (the previous recursion
    /// allocated a fresh `Vec` for every node and could overflow the native
    /// stack on deep bodies).
    fn seed_post_order(&mut self) {
        let mut stack: Vec<(NodeRef, bool)> = vec![(NodeRef::Block(self.body.root), false)];
        let mut scratch: Vec<NodeRef> = Vec::new();
        while let Some((node, processed)) = stack.pop() {
            if processed {
                self.enqueue(node);
                continue;
            }
            stack.push((node, true));
            scratch.clear();
            self.body.for_each_child(node, |c| scratch.push(c));
            for &c in scratch.iter().rev() {
                stack.push((c, false));
            }
        }
    }

    /// Push a node onto the worklist unless it is already queued.
    pub fn enqueue(&mut self, node: NodeRef) {
        if self.buf.queued.insert(node) {
            self.buf.worklist.push_back(node);
        }
    }

    /// Pop the next node to process, clearing its in-queue bit.
    pub fn pop(&mut self) -> Option<NodeRef> {
        let node = self.buf.worklist.pop_front()?;
        self.buf.queued.swap_remove(&node);
        Some(node)
    }

    /// The parent of a node (the nearest id-bearing ancestor), or `None` for
    /// the body root.
    pub fn parent_of(&self, node: NodeRef) -> Option<NodeRef> {
        match node {
            NodeRef::Expr(id) => self.buf.expr_parent[id.index()],
            NodeRef::Stmt(id) => self.buf.stmt_parent[id.index()],
            NodeRef::Block(id) => self.buf.block_parent[id.index()],
            NodeRef::Pat(id) => self.buf.pat_parent[id.index()],
        }
    }

    /// Every `Local { index }` expression node naming `local`.
    pub fn local_reads(&self, local: u32) -> &[ExprId] {
        self.buf.uses.get(&local).map_or(&[], |u| &u.reads)
    }

    /// The defining `Let` / `LetDestructure` statement of `local`, if any.
    pub fn local_def(&self, local: u32) -> Option<StmtId> {
        self.buf.uses.get(&local).and_then(|u| u.def)
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

    /// Whether `mention` is the bare-`Local` target slot of an `Assign`.
    fn is_assign_target(&self, mention: ExprId) -> bool {
        let Some(NodeRef::Expr(parent)) = self.parent_of(NodeRef::Expr(mention)) else {
            return false;
        };
        matches!(&self.body.exprs[parent].kind, ExprKind::Assign { target, .. } if *target == mention)
    }

    fn set_parent(&mut self, child: NodeRef, parent: Option<NodeRef>) {
        match child {
            NodeRef::Expr(id) => self.buf.expr_parent[id.index()] = parent,
            NodeRef::Stmt(id) => self.buf.stmt_parent[id.index()] = parent,
            NodeRef::Block(id) => self.buf.block_parent[id.index()] = parent,
            NodeRef::Pat(id) => self.buf.pat_parent[id.index()] = parent,
        }
    }

    /// Edit API: rewrite an expression node's kind in place. The id is stable,
    /// so parent links and worklist entries survive. Keeps the use index
    /// coherent (if the node was / becomes a `Local`), re-parents the new
    /// kind's id children, and re-enqueues the affected neighbourhood — the
    /// node's parent (its context may now reduce) and the new children.
    pub fn replace_expr_kind(&mut self, id: ExprId, new_kind: ExprKind) {
        crate::optimize::vg_measure::record_inplace_edit();
        // Drop the old `Local` mention, if any, from the use index.
        if let ExprKind::Local { index, .. } = &self.body.exprs[id].kind {
            let index = *index;
            if let Some(u) = self.buf.uses.get_mut(&index) {
                u.reads.retain(|&r| r != id);
            }
        }
        self.body.exprs[id].kind = new_kind;
        // Register a new `Local` mention, if any.
        if let ExprKind::Local { index, .. } = &self.body.exprs[id].kind {
            let index = *index;
            self.buf.uses.entry(index).or_default().reads.push(id);
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
        // Keep the value graph current through this edit (WEP: maintenance, not
        // rebuild). Re-derive `id`'s value and propagate up its ancestors.
        if self.value_graph.is_some() {
            self.maintain_value_after_edit(id);
        }
    }

    /// Maintain the value graph after `id`'s kind changed in place: re-derive
    /// `id`'s value and propagate up its ancestor chain until a value is
    /// unchanged. An expr whose value can no longer be derived (impure /
    /// flow-dependent — e.g. a `FieldAccess`, a `Local`, a `Call`) has its
    /// `value_of` entry *removed* rather than left stale: a later query then
    /// sees no value identity (conservative), never a wrong one. This keeps the
    /// graph a sound description of the edited body without a rebuild — the
    /// `WADO_VERIFY_VG` harness checks it agrees with a fresh build.
    fn maintain_value_after_edit(&mut self, id: ExprId) {
        let mut cur = id;
        loop {
            let old = self
                .value_graph
                .as_ref()
                .unwrap()
                .value_of
                .get(&cur)
                .copied();
            // `maintain_pure_value` re-interns a pure kind and updates `value_of`;
            // for a kind it cannot derive it returns `None` and leaves the (now
            // stale) entry, which we drop.
            if self.maintain_pure_value(cur).is_none() {
                self.value_graph
                    .as_mut()
                    .unwrap()
                    .value_of
                    .swap_remove(&cur);
            }
            let new = self
                .value_graph
                .as_ref()
                .unwrap()
                .value_of
                .get(&cur)
                .copied();
            if new == old {
                break;
            }
            match self.parent_of(NodeRef::Expr(cur)) {
                Some(NodeRef::Expr(p)) => cur = p,
                _ => break,
            }
        }
    }

    /// Edit API: promote the folded constant subtree `id` to an `Operand::Value`
    /// in its parent (WEP: The Live ValueGraph). Interns `value` width-preserving
    /// (carrying `id`'s recorded type) and swaps the parent's `Operand::Expr(id)`
    /// slot to the promoted value. `id`'s node is left orphaned (later DCE'd); its
    /// own `Local` mention, if any, is dropped from the use index, matching
    /// [`Self::replace_expr_kind`]. Returns `false` (a no-op) when `id` has no
    /// parent, or the parent references it through a non-operand slot — the caller
    /// then keeps the skeleton form.
    pub fn replace_expr_with_value(&mut self, id: ExprId, value: crate::const_eval::Value) -> bool {
        use crate::const_eval::Value;
        use crate::nir_value_graph::ValueKind;
        let type_id = self.body.exprs[id].type_id;
        let kind = match value {
            Value::Int { value, .. } => ValueKind::Int(value, type_id),
            Value::Float { value, .. } => ValueKind::Float(value.to_bits(), type_id),
            Value::Bool(b) => ValueKind::Bool(b),
            Value::Char(c) => ValueKind::Char(c),
        };
        let vid = self.body.values.alloc_unshared(kind, type_id);
        self.redirect_expr(id, Operand::Value(vid))
    }

    /// Edit API: redirect `id`'s parent operand slot to `new` (WEP: The Live
    /// ValueGraph). Used to splice a promoted constant — or an existing
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
        crate::optimize::vg_measure::record_inplace_edit();
        if let Operand::Expr(e) = new {
            self.set_parent(NodeRef::Expr(e), Some(parent));
            self.enqueue(NodeRef::Expr(e));
        }
        // `id` is now orphaned. Drop its own `Local` read mention so a binding it
        // named is not kept artificially live (the discarded subtree's deeper
        // mentions stay — a stale read is conservative, never drops a live one).
        if let ExprKind::Local { index, .. } = &self.body.exprs[id].kind {
            let index = *index;
            if let Some(u) = self.buf.uses.get_mut(&index) {
                u.reads.retain(|&r| r != id);
            }
        }
        self.set_parent(NodeRef::Expr(id), None);
        self.enqueue(parent);
        // A promoted value feeds the parent's value derivation directly through
        // the swapped operand; re-derive up the ancestor chain. `id` is now
        // orphaned; its (dead) `value_of` entry is excluded from the verify
        // oracle by the live-expr filter in `verify_maintained_graph`.
        if self.value_graph.is_some() {
            if let Some(vid) = new.as_value() {
                self.value_graph.as_mut().unwrap().value_of.insert(id, vid);
            }
            if let NodeRef::Expr(p) = parent {
                self.maintain_value_after_edit(p);
            }
        }
        true
    }

    /// Edit API: promote `src`'s node content (kind + type + span) into `dst`,
    /// leaving `src` a dead `Unit`. The arena analogue of `*dst = *src` when a
    /// rewrite collapses a wrapper onto a nested node it contains — e.g.
    /// `{ expr; }` → `expr`, or `label: { break label: val }` → `val`.
    ///
    /// `dst`'s former subtree is discarded. As with [`set_block_stmts`], this
    /// does not deep-unregister `Local` mentions inside that discarded subtree:
    /// the only way a stale mention matters is `is_local_read`, where an extra
    /// (dead) mention is conservative — it can keep a binding alive, never drop
    /// a live one. Callers use this to lift a nested node whose discarded
    /// siblings carry no live reads (every production caller in
    /// `const_branch_prune` promotes the sole meaningful child of a wrapper).
    /// `src`'s own mention is moved off `src` so it is not left dangling.
    pub fn become_expr(&mut self, dst: ExprId, src: ExprId) {
        if dst == src {
            return;
        }
        let type_id = self.body.exprs[src].type_id;
        let span = self.body.exprs[src].span;
        let src_kind = std::mem::replace(&mut self.body.exprs[src].kind, ExprKind::Dead);
        // `src` is now `Dead`; if it named a local, that mention belongs at
        // `dst` after the move, so drop it here — `replace_expr_kind` re-adds it
        // for `dst` when `src_kind` is itself a `Local`.
        if let ExprKind::Local { index, .. } = &src_kind {
            let index = *index;
            if let Some(u) = self.buf.uses.get_mut(&index) {
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
        crate::optimize::vg_measure::record_inplace_edit();
        self.body.blocks[block].stmts = stmts;
        let kids = self.body.blocks[block].stmts.clone();
        for s in &kids {
            self.set_parent(NodeRef::Stmt(*s), Some(NodeRef::Block(block)));
        }
        for s in kids {
            self.enqueue(NodeRef::Stmt(s));
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
        crate::optimize::vg_measure::record_node_created();
        let id = self.body.exprs.push(ExprNode {
            kind,
            type_id,
            span,
        });
        self.buf.expr_parent.push(None);
        let mut children = Vec::new();
        self.body
            .for_each_child(NodeRef::Expr(id), |c| children.push(c));
        for c in children {
            self.set_parent(c, Some(NodeRef::Expr(id)));
        }
        if let ExprKind::Local { index, .. } = &self.body.exprs[id].kind {
            let index = *index;
            self.buf.uses.entry(index).or_default().reads.push(id);
        }
        self.enqueue(NodeRef::Expr(id));
        id
    }

    /// Edit API: intern a fresh constant value into the function's pool and
    /// return it as an `Operand::Value` (WEP: The Live ValueGraph). For passes
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
        crate::optimize::vg_measure::record_node_created();
        let id = self.body.stmts.push(StmtNode { kind, span });
        self.buf.stmt_parent.push(None);
        let mut children = Vec::new();
        self.body
            .for_each_child(NodeRef::Stmt(id), |c| children.push(c));
        for c in children {
            self.set_parent(c, Some(NodeRef::Stmt(id)));
        }
        if let StmtKind::Let { local_index, .. } = &self.body.stmts[id].kind {
            let index = *local_index;
            self.buf.uses.entry(index).or_default().def = Some(id);
        }
        self.enqueue(NodeRef::Stmt(id));
        id
    }

    /// Edit API: allocate a fresh block node from a statement list.
    pub fn alloc_block(&mut self, stmts: Vec<StmtId>, span: Span) -> BlockId {
        crate::optimize::vg_measure::record_node_created();
        let id = self.body.blocks.push(BlockNode { stmts, span });
        self.buf.block_parent.push(None);
        let kids: Vec<StmtId> = self.body.blocks[id].stmts.clone();
        for s in kids {
            self.set_parent(NodeRef::Stmt(s), Some(NodeRef::Block(id)));
        }
        self.enqueue(NodeRef::Block(id));
        id
    }

    /// Edit API: allocate a fresh pattern node.
    pub fn alloc_pat(&mut self, kind: PatKind, span: Span) -> PatId {
        crate::optimize::vg_measure::record_node_created();
        let id = self.body.pats.push(PatNode { kind, span });
        self.buf.pat_parent.push(None);
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
                func,
                type_args,
                args,
            } => ExprKind::Call {
                func,
                type_args,
                args: args
                    .into_iter()
                    .map(|a| crate::nir_arena::ArenaCallArg {
                        expr: self.clone_operand(a.expr),
                        is_mut: a.is_mut,
                    })
                    .collect(),
            },
            ExprKind::CmRawCall { local_name, args } => ExprKind::CmRawCall {
                local_name,
                args: args.into_iter().map(|a| self.clone_operand(a)).collect(),
            },
            ExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
            } => ExprKind::MethodCall {
                receiver: self.clone_operand(receiver),
                func,
                type_args,
                args: args
                    .into_iter()
                    .map(|a| crate::nir_arena::ArenaCallArg {
                        expr: self.clone_operand(a.expr),
                        is_mut: a.is_mut,
                    })
                    .collect(),
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
        Operand::Value(
            body.values
                .alloc_unshared(crate::nir_value_graph::ValueKind::Int(n, TypeTable::I32), TypeTable::I32),
        )
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
    fn set_value_records_a_value_for_a_new_expr() {
        // Build a graph, then introduce a fresh expr and pin its value, as a
        // pass would when creating a temp bound to a known value.
        let mut body = mk_body(|b| {
            let seven = lit(b, 7);
            let st = s(b, StmtKind::Expr(seven));
            vec![st]
        });
        // The promoted constant carries its own pool value.
        let v7 = {
            let st = body.blocks[body.root].stmts[0];
            let StmtKind::Expr(Operand::Value(v)) = body.stmts[st].kind else {
                unreachable!()
            };
            v
        };
        let mut buf = EngineBuffers::default();
        let mut locals: Vec<NirLocal> = Vec::new();
        let mut eng = Engine::new(&mut body, &mut buf, &mut locals);
        // A fresh orphan expr has no value in the already-built graph.
        let fresh = eng.alloc_expr(
            ExprKind::Local {
                index: 9,
                name: "u".to_string(),
            },
            TypeTable::I32,
            Span::default(),
        );
        assert_eq!(eng.value(fresh), None);
        // Pinning it keeps it resolvable without a rebuild.
        eng.set_value(fresh, v7);
        assert_eq!(eng.value(fresh), Some(v7));
    }

    #[test]
    fn maintain_pure_value_matches_the_built_value_without_rebuild() {
        // { let x = 1 + 2; return x; } — build the graph, then introduce a fresh
        // `1 + 2` as a structural pass would and maintain it pointwise. Hash-
        // consing makes the maintained sum share the original's identity, exactly
        // what a full rebuild would yield, with no rebuild.
        let mut body = sample_body();
        let add = {
            let st = body.blocks[body.root].stmts[0];
            let StmtKind::Let { value, .. } = body.stmts[st].kind else {
                unreachable!()
            };
            value
        };
        let mut buf = EngineBuffers::default();
        let mut locals: Vec<NirLocal> = Vec::new();
        let mut eng = Engine::new(&mut body, &mut buf, &mut locals);
        let add_e = add.as_expr().unwrap();
        let v_add = eng.value(add_e).unwrap();
        // Reuse the original sum's operands (pure-value pool ids): a freshly
        // spliced `1 + 2` over the same operand values hash-conses to the same
        // `ValueId` the build produced — pointwise maintenance, no rebuild.
        let (one, two) = match &eng.body.exprs[add_e].kind {
            ExprKind::Binary { left, right, .. } => (*left, *right),
            other => panic!("expected Binary, got {other:?}"),
        };
        let sum = eng.alloc_expr(
            ExprKind::Binary {
                left: one,
                op: NirBinaryOp::Add,
                right: two,
            },
            TypeTable::I32,
            Span::default(),
        );
        let v_sum = eng.maintain_pure_value(sum).unwrap();
        assert_eq!(v_sum, v_add);
        assert_eq!(eng.value(sum), Some(v_add));

        // A flow-sensitive operand (an un-valued `Local`) is conservatively
        // skipped — no value, never a wrong one.
        let lx = eng.alloc_expr(
            ExprKind::Local {
                index: 9,
                name: "u".to_string(),
            },
            TypeTable::I32,
            Span::default(),
        );
        let mixed = eng.alloc_expr(
            ExprKind::Binary {
                left: lx.into(),
                op: NirBinaryOp::Add,
                right: two,
            },
            TypeTable::I32,
            Span::default(),
        );
        assert_eq!(eng.maintain_pure_value(mixed), None);
    }

    #[test]
    fn value_union_and_congruence_through_engine() {
        // { x + 5; y + 5; } where x,y are local 0,1. The two sums start in
        // distinct classes; after unioning x≡y and rebuilding, they share one.
        fn local(body: &mut Body, index: u32) -> ExprId {
            e(
                body,
                ExprKind::Local {
                    index,
                    name: format!("v{index}"),
                },
            )
        }
        let mut body = mk_body(|b| {
            // Share the `5` operand across both sums: the difference under union
            // is only `x` vs `y`, isolating the congruence check.
            let c5 = lit(b, 5);
            let x = local(b, 0);
            let f = bin(b, x, NirBinaryOp::Add, c5);
            let y = local(b, 1);
            let g = bin(b, y, NirBinaryOp::Add, c5);
            let sf = s(b, StmtKind::Expr(f.into()));
            let sg = s(b, StmtKind::Expr(g.into()));
            vec![sf, sg]
        });
        // The two sum expressions are the second-and-third exprs after each
        // local+literal pair: recover them by walking the root statements.
        let (f_expr, g_expr, x_expr, y_expr) = {
            let stmts = &body.blocks[body.root].stmts;
            let StmtKind::Expr(Operand::Expr(f)) = body.stmts[stmts[0]].kind else {
                unreachable!()
            };
            let StmtKind::Expr(Operand::Expr(g)) = body.stmts[stmts[1]].kind else {
                unreachable!()
            };
            let ExprKind::Binary { left: xf, .. } = body.exprs[f].kind else {
                unreachable!()
            };
            let ExprKind::Binary { left: yg, .. } = body.exprs[g].kind else {
                unreachable!()
            };
            (f, g, xf, yg)
        };
        let mut buf = EngineBuffers::default();
        let mut locals: Vec<NirLocal> = Vec::new();
        let mut eng = Engine::new(&mut body, &mut buf, &mut locals);
        let vf = eng.value(f_expr).unwrap();
        let vg = eng.value(g_expr).unwrap();
        let vx = eng.value(x_expr.as_expr().unwrap()).unwrap();
        let vy = eng.value(y_expr.as_expr().unwrap()).unwrap();
        // x and y are distinct opaques, so the sums start distinct.
        assert_ne!(eng.value_find(vx), eng.value_find(vy));
        assert_ne!(eng.value_find(vf), eng.value_find(vg));
        // Prove x ≡ y; congruence must make the two sums equal.
        eng.value_union(vx, vy);
        eng.rebuild_value_congruence();
        assert_eq!(eng.value_find(vx), eng.value_find(vy));
        assert_eq!(eng.value_find(vf), eng.value_find(vg));
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
                    value: seven.into(),
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
}
