//! Whether a type holds a resource, answered by one walk for every phase that
//! asks: the move check, a by-value `self`, and the cleanup pass. Each phase
//! supplies what a type holds one level down, typed at its instance; the walk
//! here decides the rest, so the phases cannot disagree on an aggregate.

use crate::defs::DefId;
use crate::tir::TypeId;

/// What a type holds, one level down.
pub(crate) enum Held {
    /// A resource the asking phase counts.
    Resource,
    /// Nothing that can hold a resource: a scalar, a reference, a type
    /// parameter.
    Nothing,
    /// An aggregate's members, typed at this instance. `decl` is the
    /// declaration it instantiates with `args`, where it has one.
    Members {
        decl: Option<DefId>,
        args: Vec<TypeId>,
        members: Vec<TypeId>,
    },
}

/// A phase's answer to what a type holds.
pub(crate) trait HeldTypes {
    fn held(&self, type_id: TypeId) -> Held;
}

/// Whether `type_id` is, or holds at any depth, a resource `types` counts.
pub(crate) fn holds_resource(types: &impl HeldTypes, type_id: TypeId) -> bool {
    Walk {
        types,
        open: Vec::new(),
    }
    .holds(type_id)
}

struct Walk<'a, T> {
    types: &'a T,
    /// The declarations the walk is inside, each with which of its arguments
    /// hold a resource.
    open: Vec<(DefId, Vec<bool>)>,
}

impl<T: HeldTypes> Walk<'_, T> {
    fn holds(&mut self, type_id: TypeId) -> bool {
        match self.types.held(type_id) {
            Held::Resource => true,
            Held::Nothing => false,
            Held::Members {
                decl: None,
                members,
                ..
            } => members.into_iter().any(|member| self.holds(member)),
            Held::Members {
                decl: Some(decl),
                args,
                members,
            } => {
                // A declaration reached again with arguments that hold alike
                // holds what the open one is still deciding, so the cut adds
                // nothing. Cutting on the type instead would never end on
                // polymorphic recursion, where `Nested<T>` holds a
                // `Nested<List<T>>` and no type repeats.
                let signature = (decl, args.iter().map(|&arg| self.holds(arg)).collect());
                if self.open.contains(&signature) {
                    return false;
                }
                self.open.push(signature);
                let held = members.into_iter().any(|member| self.holds(member));
                self.open.pop();
                held
            }
        }
    }
}
