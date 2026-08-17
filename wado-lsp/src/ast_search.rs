//! First-match searches over a module's AST.
//!
//! [`AstVisitor`] walks to completion, so a visitor after one node stops itself.

use wado_compiler::ast::{AstVisitor, Module};

/// A visitor that answers with the first node it matches.
pub(crate) trait FirstMatch: AstVisitor {
    type Output;

    /// Whether the answer is in hand. Overridden `visit_*` methods check this
    /// on entry too: `walk_*` keeps descending into a subtree regardless.
    fn found(&self) -> bool;

    fn take(self) -> Option<Self::Output>;
}

/// Visit `module`'s items with `visitor`, stopping at the first answer.
pub(crate) fn find_in_module<V: FirstMatch>(module: &Module, mut visitor: V) -> Option<V::Output> {
    for item in &module.items {
        visitor.visit_item(item);
        if visitor.found() {
            break;
        }
    }
    visitor.take()
}
