//! Binder questions over written syntax. Whether a written head names one of the
//! enclosing item's own type parameters is a question about that item, not about
//! which declaration a name refers to, so no module scope is consulted and no
//! identity produced. WEP 2026-08-10 makes the identity question a table keyed
//! by reference site; this is the binder arm of that resolution.

use crate::ast;

/// The type parameter `ty`'s head binds to, if the item declares one by that
/// name.
pub(crate) fn binder_of<'p>(
    ty: &ast::Type,
    type_params: &'p [ast::GenericParam],
) -> Option<&'p ast::GenericParam> {
    let head = match ty {
        ast::Type::Named(named) => named.name.as_str(),
        ast::Type::Generic(generic) => generic.name.as_str(),
        _ => return None,
    };
    type_params.iter().find(|p| p.name == head)
}
