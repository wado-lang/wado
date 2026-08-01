//! Immutable visitor over NIR arena nodes. Descent delegates to
//! [`Body::for_each_child`] so passes don't re-spell the child structure.
//! The arena counterpart of [`crate::wir_visitor`].

use crate::nir_arena::{Body, ExprId, NodeRef};

pub trait NirRefVisitor {
    fn visit_node(&mut self, body: &Body, node: NodeRef) {
        self.walk_node(body, node);
    }

    fn walk_node(&mut self, body: &Body, node: NodeRef) {
        body.for_each_child(node, |child| self.visit_node(body, child));
    }
}

/// Every expression id reachable from the body root, in walk order.
///
/// Reachability is what separates the nodes a pass may act on from the ones an
/// earlier in-place rewrite left behind: the arena keeps every displaced node,
/// and one nothing refers to never runs.
///
/// A body with no block structure is a bare expression — a global initializer —
/// and everything it holds is reachable by construction.
pub(crate) fn reachable_exprs(body: &Body) -> Vec<ExprId> {
    struct Collect(Vec<ExprId>);
    impl NirRefVisitor for Collect {
        fn visit_node(&mut self, body: &Body, node: NodeRef) {
            if let NodeRef::Expr(e) = node {
                self.0.push(e);
            }
            self.walk_node(body, node);
        }
    }
    if body.blocks.is_empty() {
        return body.exprs.iter().map(|(e, _)| e).collect();
    }
    let mut collect = Collect(Vec::new());
    collect.visit_node(body, NodeRef::Block(body.root));
    collect.0
}
