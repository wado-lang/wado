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
            match &body.stmts[id].kind {
                StmtKind::Let { local_index, .. } => {
                    self.uses.entry(*local_index).or_default().def = Some(id);
                }
                _ => {}
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

    #[test]
    fn worklist_seeds_every_node_once() {
        let mut body = sample_body();
        let total = body.exprs.len() + body.stmts.len() + body.blocks.len() + body.pats.len();
        let mut eng = Engine::new(&mut body);
        let mut popped = 0;
        while let Some(_) = eng.pop() {
            popped += 1;
        }
        assert_eq!(popped, total);
    }
}
