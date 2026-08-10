//! Type names still in the form the source wrote them.
//!
//! A type name in Wado is module-relative: `Box_` means whatever the module
//! that wrote it says it means, so a head with no module answers no identity
//! question. [`WrittenHead`] is the only way from an [`ast::Type`] to its head,
//! and it carries that module.
//!
//! Transitional. WEP 2026-08-10 puts the answer on the reference site instead —
//! `Resolutions: AstId -> DeclRef`, one pass, and identity rather than a name as
//! what every query takes — and its stage D deletes this module. Until then this
//! keeps a new vantage-free derivation from being written. [`binder_of`] is the
//! part that survives, as the binder arm of that resolution.

use std::fmt;

use crate::ast;
use crate::module_source::ModuleSource;
use crate::name::RefKind;
use crate::tir::TypeTable;

/// A type name as written in one module's source.
///
/// It answers no identity question on its own, and the type says so: no
/// `PartialEq`, no `Hash`, no `Ord`, no conversion to `String`. Nothing can key
/// a registry by one, and `a != b` over two of them does not compile. The ways
/// out are [`Self::resolve_with`], which hands a resolver the spelling and the
/// vantage together, and `Display`, for diagnostics.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WrittenHead<'a> {
    spelling: &'a str,
    /// The reference kind, kept as the typed fact rather than re-read from the
    /// `&` / `&mut` spelling. A reference declares nothing, so its identity is
    /// its kind, and the kind is the same in every module.
    ref_kind: Option<RefKind>,
    vantage: &'a ModuleSource,
}

impl<'a> WrittenHead<'a> {
    /// The head of `ty` as written in `vantage`.
    ///
    /// Shapes no module spells by name — the unit type, a tuple, a reference —
    /// take their canonical head, which is the same in every module.
    pub(crate) fn of(ty: &'a ast::Type, vantage: &'a ModuleSource) -> Self {
        let ref_kind = RefKind::from_ast(ty);
        let spelling = match ty {
            ast::Type::Named(named) if named.name == "()" => TypeTable::UNIT_TYPE_NAME,
            ast::Type::Named(named) => named.name.as_str(),
            ast::Type::Generic(generic) => generic.name.as_str(),
            ast::Type::Reference(_) | ast::Type::MutReference(_) => {
                ref_kind.expect("Reference/MutReference classify").prefix()
            }
            ast::Type::Tuple(elems) if elems.is_empty() => TypeTable::UNIT_TYPE_NAME,
            ast::Type::Tuple(_) => TypeTable::TUPLE_TYPE_NAME,
            ast::Type::NamespacedGeneric(_)
            | ast::Type::TypePackSpread(..)
            | ast::Type::Function(_)
            | ast::Type::Infer(_)
            | ast::Type::Error(_) => Self::UNKNOWN,
        };
        Self {
            spelling,
            ref_kind,
            vantage,
        }
    }

    /// The head of a type the compiler has no name for.
    const UNKNOWN: &'static str = "Unknown";

    /// The reference kind this head spells, or `None` for a non-reference.
    pub(crate) fn ref_kind(&self) -> Option<RefKind> {
        self.ref_kind
    }

    /// Hand a resolver the spelling and the vantage at once, so the two cannot
    /// be paired wrongly. Every identity derived from written syntax goes
    /// through here.
    pub(crate) fn resolve_with<T>(
        &self,
        resolve: impl FnOnce(&'a ModuleSource, &'a str) -> T,
    ) -> T {
        resolve(self.vantage, self.spelling)
    }

    /// The type parameter this head binds to, if the surrounding item declares
    /// one by that name. A binder question, not an identity one: a binder is
    /// scoped to the item that wrote it, so the vantage is shared by
    /// construction and no resolution is involved.
    pub(crate) fn binder_in<'p>(
        &self,
        type_params: &'p [ast::GenericParam],
    ) -> Option<&'p ast::GenericParam> {
        type_params.iter().find(|p| p.name == self.spelling)
    }

    /// The bare spelling, compared against another bare spelling.
    ///
    /// Unsound as an identity — two modules' `Widget` compare equal. It marks
    /// the dispatch paths that still key a receiver by name; WEP 2026-08-10's
    /// stage C converts them to `DeclRef` and this method goes with them. Do
    /// not add call sites.
    pub(crate) fn spelling_pending_migration(&self) -> &'a str {
        self.spelling
    }
}

impl fmt::Display for WrittenHead<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.spelling)
    }
}

/// The type parameter `ty`'s head binds to, if the item declares one by that
/// name. The vantage-free form of [`WrittenHead::binder_in`], for a caller
/// asking only about the item's own binders.
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
