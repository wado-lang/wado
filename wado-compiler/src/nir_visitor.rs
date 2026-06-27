//! Immutable visitor over NIR arena nodes. Descent delegates to
//! [`Body::for_each_child`] so passes don't re-spell the child structure.
//! The arena counterpart of [`crate::wir_visitor`].

use crate::nir_arena::{Body, NodeRef};

pub trait NirRefVisitor {
    fn visit_node(&mut self, body: &Body, node: NodeRef) {
        self.walk_node(body, node);
    }

    fn walk_node(&mut self, body: &Body, node: NodeRef) {
        body.for_each_child(node, |child| self.visit_node(body, child));
    }
}
