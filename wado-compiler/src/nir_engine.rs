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
    ArmData, BlockId, BlockNode, Body, ExprId, ExprKind, ExprNode, NodeRef, PatId, PatKind,
    PatNode, StmtId, StmtKind, StmtNode,
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
        let mut engine = Self { body, buf, locals };
        engine.build_parents();
        engine.build_uses();
        engine.seed_post_order();
        engine
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
        let src_kind = std::mem::replace(&mut self.body.exprs[src].kind, ExprKind::Unit);
        // `src` is now `Unit`; if it named a local, that mention belongs at
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

    /// Edit API: allocate a fresh statement node.
    pub fn alloc_stmt(&mut self, kind: StmtKind, span: Span) -> StmtId {
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

    fn clone_block(&mut self, id: BlockId) -> BlockId {
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
                value: self.clone_expr(value),
            },
            ExprKind::Binary { left, op, right } => ExprKind::Binary {
                left: self.clone_expr(left),
                op,
                right: self.clone_expr(right),
            },
            ExprKind::Unary { op, expr } => ExprKind::Unary {
                op,
                expr: self.clone_expr(expr),
            },
            ExprKind::Assign { target, value } => ExprKind::Assign {
                target: self.clone_expr(target),
                value: self.clone_expr(value),
            },
            ExprKind::Cast { expr, target_type } => ExprKind::Cast {
                expr: self.clone_expr(expr),
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
                        expr: self.clone_expr(a.expr),
                        is_mut: a.is_mut,
                    })
                    .collect(),
            },
            ExprKind::CmRawCall { local_name, args } => ExprKind::CmRawCall {
                local_name,
                args: args.into_iter().map(|a| self.clone_expr(a)).collect(),
            },
            ExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
            } => ExprKind::MethodCall {
                receiver: self.clone_expr(receiver),
                func,
                type_args,
                args: args
                    .into_iter()
                    .map(|a| crate::nir_arena::ArenaCallArg {
                        expr: self.clone_expr(a.expr),
                        is_mut: a.is_mut,
                    })
                    .collect(),
            },
            ExprKind::FieldAccess {
                expr,
                field_index,
                field_name,
            } => ExprKind::FieldAccess {
                expr: self.clone_expr(expr),
                field_index,
                field_name,
            },
            ExprKind::Index { expr, index } => ExprKind::Index {
                expr: self.clone_expr(expr),
                index: self.clone_expr(index),
            },
            ExprKind::Block(b) => ExprKind::Block(self.clone_block(b)),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => ExprKind::If {
                condition: self.clone_expr(condition),
                then_branch: self.clone_block(then_branch),
                else_branch: else_branch.map(|b| self.clone_block(b)),
            },
            ExprKind::Match { expr, arms } => ExprKind::Match {
                expr: self.clone_expr(expr),
                arms: arms
                    .into_iter()
                    .map(|a| ArmData {
                        pattern: self.clone_pat(a.pattern),
                        guard: a.guard.map(|g| self.clone_expr(g)),
                        body: self.clone_expr(a.body),
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
                        value: self.clone_expr(f.value),
                        field_index: f.field_index,
                    })
                    .collect(),
            },
            ExprKind::TupleLiteral { elements } => ExprKind::TupleLiteral {
                elements: elements.into_iter().map(|e| self.clone_expr(e)).collect(),
            },
            ExprKind::ArrayLiteral { elements } => ExprKind::ArrayLiteral {
                elements: elements.into_iter().map(|e| self.clone_expr(e)).collect(),
            },
            ExprKind::IndirectCall { callee, args } => ExprKind::IndirectCall {
                callee: self.clone_expr(callee),
                args: args.into_iter().map(|a| self.clone_expr(a)).collect(),
            },
            ExprKind::ClosureToCanonical {
                functor,
                functor_id,
                target_fn_type,
                closure_module,
            } => ExprKind::ClosureToCanonical {
                functor: self.clone_expr(functor),
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
                payload: payload.map(|p| self.clone_expr(p)),
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
                expr: self.clone_expr(expr),
            },
            ExprKind::VariantTest {
                expr,
                case_index,
                case_name,
            } => ExprKind::VariantTest {
                expr: self.clone_expr(expr),
                case_index,
                case_name,
            },
            ExprKind::VariantPayload {
                expr,
                case_index,
                payload_type,
            } => ExprKind::VariantPayload {
                expr: self.clone_expr(expr),
                case_index,
                payload_type,
            },
            ExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => ExprKind::Switch {
                scrutinee: self.clone_expr(scrutinee),
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
                value: self.clone_expr(value),
                skip_value_copy,
            },
            StmtKind::Expr(e) => StmtKind::Expr(self.clone_expr(e)),
            StmtKind::Return { value } => StmtKind::Return {
                value: value.map(|e| self.clone_expr(e)),
            },
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => StmtKind::If {
                condition: self.clone_expr(condition),
                then_block: self.clone_block(then_block),
                else_block: else_block.map(|b| self.clone_block(b)),
            },
            StmtKind::Loop { body } => StmtKind::Loop {
                body: self.clone_block(body),
            },
            StmtKind::Break { label, value } => StmtKind::Break {
                label,
                value: value.map(|e| self.clone_expr(e)),
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
                value: self.clone_expr(value),
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
                expr: self.clone_expr(expr),
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
    fn lit(body: &mut Body, n: u64) -> ExprId {
        e(
            body,
            ExprKind::IntLiteral {
                value: n,
                repr: n.to_string(),
            },
        )
    }
    fn bin(body: &mut Body, left: ExprId, op: NirBinaryOp, right: ExprId) -> ExprId {
        e(body, ExprKind::Binary { left, op, right })
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
                value,
                skip_value_copy: false,
            },
        )
    }
    fn ret_x(body: &mut Body) -> StmtId {
        let xref = local0(body);
        s(body, StmtKind::Return { value: Some(xref) })
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
            let lv = match &e.body.exprs[l].kind {
                ExprKind::IntLiteral { value, .. } => *value,
                _ => return false,
            };
            let rv = match &e.body.exprs[r].kind {
                ExprKind::IntLiteral { value, .. } => *value,
                _ => return false,
            };
            let v = match op {
                NirBinaryOp::Add => lv.wrapping_add(rv),
                NirBinaryOp::Mul => lv.wrapping_mul(rv),
                _ => return false,
            };
            e.replace_expr_kind(
                id,
                ExprKind::IntLiteral {
                    value: v,
                    repr: v.to_string(),
                },
            );
            true
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
        // The let's value is now a single IntLiteral(12).
        let root = body.root;
        let s0 = body.blocks[root].stmts[0];
        let StmtKind::Let { value, .. } = &body.stmts[s0].kind else {
            panic!("expected let");
        };
        match &body.exprs[*value].kind {
            ExprKind::IntLiteral { value, .. } => assert_eq!(*value, 12),
            other => panic!("expected folded IntLiteral(12), got {other:?}"),
        }
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
        let original = *value;
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
        // A Binary plus its two literal operands were allocated.
        assert_eq!(body.exprs.len(), before + 3);
        assert_ne!(clone, original);
        let (new_left, new_right) = match &body.exprs[clone].kind {
            ExprKind::Binary { left, right, .. } => (*left, *right),
            other => panic!("expected cloned Binary, got {other:?}"),
        };
        // Deep copy: the clone's operands are fresh ids, not shared.
        assert_ne!(new_left, orig_left);
        assert_ne!(new_right, orig_right);
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
                        StmtKind::Expr(ex) if matches!(e.body.exprs[*ex].kind, ExprKind::Unit))
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
            let unit = e(b, ExprKind::Unit);
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
            let assign_stmt = s(b, StmtKind::Expr(assign));
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
            let s_x = s(b, StmtKind::Expr(lx));
            let one = lit(b, 1);
            let two = lit(b, 2);
            let add = bin(b, one, NirBinaryOp::Add, two);
            let s_add = s(b, StmtKind::Expr(add));
            vec![s_x, s_add]
        });
        let root = body.root;
        let StmtKind::Expr(lx) = body.stmts[body.blocks[root].stmts[0]].kind else {
            panic!("expected expr stmt");
        };
        let StmtKind::Expr(add) = body.stmts[body.blocks[root].stmts[1]].kind else {
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
            assert!(matches!(eng.body.exprs[lx].kind, ExprKind::Unit));
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
