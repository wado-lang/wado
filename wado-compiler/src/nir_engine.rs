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

    /// Edit API: allocate a fresh expression node. Sets the parent of its id
    /// children, registers a `Local` mention, and enqueues it (and its
    /// children) so a freshly built subtree is visited.
    pub fn alloc_expr(&mut self, kind: ExprKind, type_id: TypeId, span: Span) -> ExprId {
        let id = self.body.exprs.push(ExprNode {
            kind,
            type_id,
            span,
        });
        self.expr_parent.push(None);
        let mut children = Vec::new();
        self.body
            .for_each_child(NodeRef::Expr(id), |c| children.push(c));
        for c in children {
            self.set_parent(c, Some(NodeRef::Expr(id)));
        }
        if let ExprKind::Local { index, .. } = &self.body.exprs[id].kind {
            let index = *index;
            self.uses.entry(index).or_default().reads.push(id);
        }
        self.enqueue(NodeRef::Expr(id));
        id
    }

    /// Edit API: allocate a fresh statement node.
    pub fn alloc_stmt(&mut self, kind: StmtKind, span: Span) -> StmtId {
        let id = self.body.stmts.push(StmtNode { kind, span });
        self.stmt_parent.push(None);
        let mut children = Vec::new();
        self.body
            .for_each_child(NodeRef::Stmt(id), |c| children.push(c));
        for c in children {
            self.set_parent(c, Some(NodeRef::Stmt(id)));
        }
        if let StmtKind::Let { local_index, .. } = &self.body.stmts[id].kind {
            let index = *local_index;
            self.uses.entry(index).or_default().def = Some(id);
        }
        self.enqueue(NodeRef::Stmt(id));
        id
    }

    /// Edit API: allocate a fresh block node from a statement list.
    pub fn alloc_block(&mut self, stmts: Vec<StmtId>, span: Span) -> BlockId {
        let id = self.body.blocks.push(BlockNode { stmts, span });
        self.block_parent.push(None);
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
        self.pat_parent.push(None);
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
        let lit = |n: u64| NirExprKind::IntLiteral {
            value: n,
            repr: n.to_string(),
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
            let mut eng = Engine::new(&mut body);
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
