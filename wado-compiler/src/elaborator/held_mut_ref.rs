//! A `&mut` of a type that is not `RefMut` lasts for a scope: a call, a
//! pattern's arm or a loop's turn (WEP: Writable References). Such a type has
//! no object a reference could name, so the scope copies the value in and
//! stores it back, and a `&mut` held past the scope writes a copy nothing
//! stores. A parameter's own type is the one place one is held, and the
//! caller's scope bounds it.

use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::tir::{ResolvedType, TypeId};
use crate::token::Span;

use super::Elaborator;
use super::types::TypeError;

/// Where a type is written.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Holder {
    /// A parameter, of a function or of a function type: a `&mut` at its top
    /// is the scoped one a caller lends.
    Param,
    /// Anything that keeps the value: a variable, a field, a payload, an
    /// element, a returned value.
    Held(&'static str),
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Report a `&mut` of a type that is not `RefMut` held by `type_id`,
    /// written where `holder` says.
    pub(super) fn refuse_held_mut_ref(&self, type_id: TypeId, holder: Holder, span: Span) {
        let Some((pointee, what)) = self.held_mut_ref(type_id, holder) else {
            return;
        };
        let pointee_name = self.tysys.type_table.borrow().name(pointee);
        let rewrite = if self.tysys.type_table.borrow().contains_rigid_param(pointee) {
            format!("bound `{pointee_name}: RefMut`, or hold a `Shared<{pointee_name}>` instead")
        } else {
            format!("hold a `Shared<{pointee_name}>` instead")
        };
        let _ = self.emit(TypeError::CannotMutate {
            message: format!(
                "{what} cannot hold a `&mut {pointee_name}`: a `&mut` of a type that is not \
                 `RefMut` lasts only for a call, a pattern's arm or a loop's turn; {rewrite}"
            ),
            span,
        });
    }

    /// The pointee of the first `&mut` `type_id` holds past its scope, and
    /// what holds it.
    fn held_mut_ref(&self, type_id: TypeId, holder: Holder) -> Option<(TypeId, &'static str)> {
        let resolved = self.tysys.type_table.borrow().get(type_id).clone();
        match resolved {
            ResolvedType::MutRef(pointee) => match holder {
                Holder::Held(what) if !self.borrows_in_place(pointee) => Some((pointee, what)),
                _ => self.held_mut_ref(pointee, holder),
            },
            ResolvedType::Ref(pointee) => self.held_mut_ref(pointee, holder),
            // `<&mut I>::Item` names a type the base declares, not a value
            // holding the base.
            ResolvedType::AssocTypeProjection { .. } => None,
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => params
                .iter()
                .find_map(|&param| self.held_mut_ref(param, Holder::Param))
                .or_else(|| self.held_mut_ref(return_type, Holder::Held("a returned value"))),
            _ => {
                let what = match holder {
                    Holder::Held(what) => what,
                    Holder::Param => "a parameter",
                };
                // Collected first: the bound query below writes the table.
                let mut members = Vec::new();
                self.tysys
                    .type_table
                    .borrow()
                    .any_constituent(type_id, &mut |member| {
                        members.push(member);
                        false
                    });
                members
                    .into_iter()
                    .find_map(|member| self.held_mut_ref(member, Holder::Held(what)))
            }
        }
    }

    /// Whether a `&mut` of `pointee` names an object of its own, which it can
    /// be held across: a `RefMut` type, a `&mut`, or an affine value.
    fn borrows_in_place(&self, pointee: TypeId) -> bool {
        let table = self.tysys.type_table.borrow();
        if table.contains_undecided(pointee)
            || matches!(table.get(pointee), ResolvedType::MutRef(_))
        {
            return true;
        }
        let ref_mut = table.compiler_trait_fq(CompilerItem::RefMut);
        drop(table);
        // An `affine T` parameter admits a copyable argument, so only a type
        // affine in every instantiation counts.
        self.tysys.is_affine(pointee, &[])
            || self
                .tysys
                .type_implements_trait(&self.annotate_ctx, pointee, &ref_mut)
    }
}
