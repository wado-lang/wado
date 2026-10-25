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

/// Every expression id [`Body::for_each_reachable_node`] reaches, in walk order.
///
/// Reachability is what separates the nodes a pass may act on from the ones an
/// earlier in-place rewrite left behind: the arena keeps every displaced node,
/// and one nothing refers to never runs.
pub(crate) fn reachable_exprs(body: &Body) -> Vec<ExprId> {
    let mut out = Vec::new();
    body.for_each_reachable_node(|node| {
        if let NodeRef::Expr(e) = node {
            out.push(e);
        }
    });
    out
}

/// Every expression id reachable from `node`, in walk order — the region a
/// pass walks when one region answers its question.
pub(crate) fn exprs_under(body: &Body, node: NodeRef) -> Vec<ExprId> {
    let mut out = Vec::new();
    body.for_each_node_under(node, |n| {
        if let NodeRef::Expr(e) = n {
            out.push(e);
        }
    });
    out
}
