//! Whether a type holds a resource, answered by one walk for every phase that
//! asks: the move check, a by-value `self`, and the cleanup pass. Each phase
//! supplies what a type holds one level down, typed at its instance; the walk
//! here decides the rest, so the phases cannot disagree on an aggregate.

use crate::defs::DefId;
use crate::tir::TypeId;
use crate::type_table::TypeKey;

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

    /// `type_id`'s identity: many ids can name one type.
    fn key(&self, type_id: TypeId) -> TypeKey;
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
    /// hold a resource, and the arguments.
    open: Vec<Open>,
}

struct Open {
    decl: DefId,
    holding: Vec<bool>,
    args: Vec<TypeId>,
}

impl<T: HeldTypes> Walk<'_, T> {
    /// Whether `type_id` is one of `args`, or is inside one of them.
    fn within(&self, type_id: TypeId, args: &[TypeId]) -> bool {
        let key = self.types.key(type_id);
        args.iter().any(|&arg| {
            self.types.key(arg) == key
                || matches!(self.types.held(arg), Held::Members { args, .. } if self.within(type_id, &args))
        })
    }

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
                // `Nested<List<T>>` and no type repeats. A type inside the
                // open one's arguments is no recursion but a part of it, as
                // `Option<Fields>` is of `Option<Option<Fields>>`: the walk
                // descends into it, which ends where the type does.
                let holding: Vec<bool> = args.iter().map(|&arg| self.holds(arg)).collect();
                let recurs = self.open.iter().any(|open| {
                    open.decl == decl
                        && open.holding == holding
                        && !self.within(type_id, &open.args)
                });
                if recurs {
                    return false;
                }
                self.open.push(Open {
                    decl,
                    holding,
                    args,
                });
                let held = members.into_iter().any(|member| self.holds(member));
                self.open.pop();
                held
            }
        }
    }
}
