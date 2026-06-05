//! NIR rewrite engine (Phase 4).
//!
//! A worklist-driven engine that runs local (peephole) rewrites over one
//! function's [`Body`] to a local fixed point — visiting a node only when it
//! might be reducible, rather than the ~31 whole-tree passes the current
//! optimizer runs in a global fixed point.
//!
//! See `docs/wep-2026-06-05-nir-rewrite-engine-design.md`.
//!
//! This module currently provides stage A — the engine *session*: the parent
//! map, the local use index, and the worklist, built once per function from a
//! `Body`. The mutating edit API, the `Rule` trait, and the driver land in
//! stage B; the peephole passes become rules in stage C.

use std::collections::VecDeque;

use cranelift_entity::EntityRef;

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};

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

/// An engine session over one function body: the arena plus the parent map,
/// use index, and worklist the worklist discipline needs.
pub struct Engine<'a> {
    pub body: &'a mut Body,
    expr_parent: Vec<Option<NodeRef>>,
    stmt_parent: Vec<Option<NodeRef>>,
    block_parent: Vec<Option<NodeRef>>,
    pat_parent: Vec<Option<NodeRef>>,
    uses: IndexMap<u32, LocalUses>,
    worklist: VecDeque<NodeRef>,
    queued: IndexSet<NodeRef>,
}

impl<'a> Engine<'a> {
    /// Build a session: one O(n) pass populates the parent map and use index,
    /// then the worklist is seeded with every node in post-order (children
    /// before parents) so leaf reductions are seen before the contexts that
    /// might fold them.
    pub fn new(body: &'a mut Body) -> Self {
        let expr_parent = vec![None; body.exprs.len()];
        let stmt_parent = vec![None; body.stmts.len()];
        let block_parent = vec![None; body.blocks.len()];
        let pat_parent = vec![None; body.pats.len()];

        let mut engine = Self {
            body,
            expr_parent,
            stmt_parent,
            block_parent,
            pat_parent,
            uses: IndexMap::default(),
            worklist: VecDeque::new(),
            queued: IndexSet::default(),
        };
        engine.build_parents();
        engine.build_uses();
        engine.seed_post_order();
        engine
    }

    /// Set `parent[child] = node` for every id-bearing child of every node.
    fn build_parents(&mut self) {
        let body = &*self.body;
        let set = |child: NodeRef,
                   parent: NodeRef,
                   ep: &mut [Option<NodeRef>],
                   sp: &mut [Option<NodeRef>],
                   bp: &mut [Option<NodeRef>],
                   pp: &mut [Option<NodeRef>]| match child {
            NodeRef::Expr(id) => ep[id.index()] = Some(parent),
            NodeRef::Stmt(id) => sp[id.index()] = Some(parent),
            NodeRef::Block(id) => bp[id.index()] = Some(parent),
            NodeRef::Pat(id) => pp[id.index()] = Some(parent),
        };
        // Collect (parent, child) edges first to avoid borrowing `self` both
        // immutably (for `for_each_child`) and mutably (the parent slices).
        let mut edges: Vec<(NodeRef, NodeRef)> = Vec::new();
        for id in body.exprs.keys() {
            body.for_each_child(NodeRef::Expr(id), |c| edges.push((NodeRef::Expr(id), c)));
        }
        for id in body.stmts.keys() {
            body.for_each_child(NodeRef::Stmt(id), |c| edges.push((NodeRef::Stmt(id), c)));
        }
        for id in body.blocks.keys() {
            body.for_each_child(NodeRef::Block(id), |c| edges.push((NodeRef::Block(id), c)));
        }
        for id in body.pats.keys() {
            body.for_each_child(NodeRef::Pat(id), |c| edges.push((NodeRef::Pat(id), c)));
        }
        for (parent, child) in edges {
            set(
                child,
                parent,
                &mut self.expr_parent,
                &mut self.stmt_parent,
                &mut self.block_parent,
                &mut self.pat_parent,
            );
        }
    }

    /// Record, per local index, the defining statement and every reading
    /// `Local` expression node.
    fn build_uses(&mut self) {
        let body = &*self.body;
        for id in body.exprs.keys() {
            if let ExprKind::Local { index, .. } = &body.exprs[id].kind {
                self.uses.entry(*index).or_default().reads.push(id);
            }
        }
        for id in body.stmts.keys() {
            if let StmtKind::Let { local_index, .. } = &body.stmts[id].kind {
                self.uses.entry(*local_index).or_default().def = Some(id);
            }
        }
    }

    /// Seed the worklist with every node, children before parents.
    fn seed_post_order(&mut self) {
        let root = self.body.root;
        self.seed_node(NodeRef::Block(root));
    }

    fn seed_node(&mut self, node: NodeRef) {
        // Collect children first (immutable borrow), then recurse.
        let mut children = Vec::new();
        self.body.for_each_child(node, |c| children.push(c));
        for c in children {
            self.seed_node(c);
        }
        self.enqueue(node);
    }

    /// Push a node onto the worklist unless it is already queued.
    pub fn enqueue(&mut self, node: NodeRef) {
        if self.queued.insert(node) {
            self.worklist.push_back(node);
        }
    }

    /// Pop the next node to process, clearing its in-queue bit.
    pub fn pop(&mut self) -> Option<NodeRef> {
        let node = self.worklist.pop_front()?;
        self.queued.swap_remove(&node);
        Some(node)
    }

    /// The parent of a node (the nearest id-bearing ancestor), or `None` for
    /// the body root.
    pub fn parent_of(&self, node: NodeRef) -> Option<NodeRef> {
        match node {
            NodeRef::Expr(id) => self.expr_parent[id.index()],
            NodeRef::Stmt(id) => self.stmt_parent[id.index()],
            NodeRef::Block(id) => self.block_parent[id.index()],
            NodeRef::Pat(id) => self.pat_parent[id.index()],
        }
    }

    /// Every `Local { index }` expression node naming `local`.
    pub fn local_reads(&self, local: u32) -> &[ExprId] {
        self.uses.get(&local).map_or(&[], |u| &u.reads)
    }

    /// The defining `Let` / `LetDestructure` statement of `local`, if any.
    pub fn local_def(&self, local: u32) -> Option<StmtId> {
        self.uses.get(&local).and_then(|u| u.def)
    }

    fn set_parent(&mut self, child: NodeRef, parent: Option<NodeRef>) {
        match child {
            NodeRef::Expr(id) => self.expr_parent[id.index()] = parent,
            NodeRef::Stmt(id) => self.stmt_parent[id.index()] = parent,
            NodeRef::Block(id) => self.block_parent[id.index()] = parent,
            NodeRef::Pat(id) => self.pat_parent[id.index()] = parent,
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
            if let Some(u) = self.uses.get_mut(&index) {
                u.reads.retain(|&r| r != id);
            }
        }
        self.body.exprs[id].kind = new_kind;
        // Register a new `Local` mention, if any.
        if let ExprKind::Local { index, .. } = &self.body.exprs[id].kind {
            let index = *index;
            self.uses.entry(index).or_default().reads.push(id);
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

    /// Drive the worklist to a local fixed point with `rules`. Pops a node,
    /// tries the rules in priority order; the first that reports a change
    /// re-processes the node (its kind may now match a different rule). Edits
    /// re-enqueue the affected neighbourhood, so a node is revisited only when
    /// something near it changed.
    pub fn run(&mut self, rules: &[&dyn Rule]) {
        while let Some(node) = self.pop() {
            let NodeRef::Expr(id) = node else { continue };
            loop {
                let mut changed = false;
                for rule in rules {
                    if rule.apply_expr(self, id) {
                        changed = true;
                        break;
                    }
                }
                if !changed {
                    break;
                }
            }
        }
    }
}

/// A single-node local rewrite. The engine applies rules at a node when it is
/// popped from the worklist or re-enqueued after a neighbouring change.
pub trait Rule {
    /// Try to rewrite the expression node `id`; return `true` if it changed.
    /// Most peephole rewrites are expression-typed; statement / block entry
    /// points are added when a rule needs them.
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir::{NirBinaryOp, NirBlock, NirExpr, NirExprKind, NirStmt, NirStmtKind};
    use crate::tir::TypeTable;
    use crate::token::Span;

    /// `{ let x = 1 + 2; return x; }`
    fn sample_body() -> Body {
        let sp = Span::default();
        let ty = TypeTable::UNIT;
        let mk = |k| NirExpr::new(k, ty, sp);
        let one = mk(NirExprKind::IntLiteral {
            value: 1,
            repr: "1".to_string(),
        });
        let two = mk(NirExprKind::IntLiteral {
            value: 2,
            repr: "2".to_string(),
        });
        let add = mk(NirExprKind::Binary {
            left: Box::new(one),
            op: NirBinaryOp::Add,
            right: Box::new(two),
        });
        let let_stmt = NirStmt::new(
            NirStmtKind::Let {
                name: "x".to_string(),
                local_index: 0,
                is_mut: false,
                is_reactive: false,
                type_id: ty,
                value: add,
                skip_value_copy: false,
            },
            sp,
        );
        let xref = mk(NirExprKind::Local {
            index: 0,
            name: "x".to_string(),
        });
        let ret = NirStmt::new(NirStmtKind::Return { value: Some(xref) }, sp);
        Body::from_block(&NirBlock::new(vec![let_stmt, ret], sp))
    }

    #[test]
    fn use_index_tracks_def_and_reads() {
        let mut body = sample_body();
        let eng = Engine::new(&mut body);
        // local 0 is read once (the `return x`) and defined once (the `let`).
        assert_eq!(eng.local_reads(0).len(), 1);
        assert!(eng.local_def(0).is_some());
    }

    #[test]
    fn parents_link_children_to_their_node() {
        let mut body = sample_body();
        let root = body.root;
        let eng = Engine::new(&mut body);
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
        let sp = Span::default();
        let ty = TypeTable::UNIT;
        let mk = |k| NirExpr::new(k, ty, sp);
        let lit = |n: u64| {
            NirExprKind::IntLiteral {
                value: n,
                repr: n.to_string(),
            }
        };
        let inner = mk(NirExprKind::Binary {
            left: Box::new(mk(lit(1))),
            op: NirBinaryOp::Add,
            right: Box::new(mk(lit(2))),
        });
        let outer = mk(NirExprKind::Binary {
            left: Box::new(inner),
            op: NirBinaryOp::Mul,
            right: Box::new(mk(lit(4))),
        });
        let let_stmt = NirStmt::new(
            NirStmtKind::Let {
                name: "x".to_string(),
                local_index: 0,
                is_mut: false,
                is_reactive: false,
                type_id: ty,
                value: outer,
                skip_value_copy: false,
            },
            sp,
        );
        let mut body = Body::from_block(&NirBlock::new(vec![let_stmt], sp));
        {
            let mut eng = Engine::new(&mut body);
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
    fn worklist_seeds_every_node_once() {
        let mut body = sample_body();
        let total = body.exprs.len() + body.stmts.len() + body.blocks.len() + body.pats.len();
        let mut eng = Engine::new(&mut body);
        let mut popped = 0;
        while eng.pop().is_some() {
            popped += 1;
        }
        assert_eq!(popped, total);
    }
}
