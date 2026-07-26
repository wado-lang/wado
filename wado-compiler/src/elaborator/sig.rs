//! Canonical declaration signatures and the one way to instantiate them.

use std::cell::RefCell;

use crate::hashmap::IndexMap;
use crate::tir::{TypeId, TypeTable};

/// A declaration's parameter and return types, resolved once in its
/// declaring frame and abstract over the positional slots in
/// [`Self::type_params`] (WEP 2026-07-10).
///
/// Slot `i` is a `ResolvedType::TypeParam` (or `TypePack`) whose index is
/// `i`, so a use site's type arguments fill the slots positionally.
/// Effect parameters and `<F: fn(…)>` bounds consume no slot — they are
/// scope entries, not substitution targets — so this list is dense.
///
/// A use site that knows its type arguments reads the signature through
/// [`Self::instantiate`]. Inference, which solves *for* those arguments,
/// is the one consumer that reads the canonical types directly.
#[derive(Clone, Debug, Default)]
pub(crate) struct DeclSig {
    pub(crate) type_params: Vec<(String, TypeId)>,
    pub(crate) param_types: Vec<TypeId>,
    /// `None` when the declaration declares no return type.
    pub(crate) return_type: Option<TypeId>,
}

/// An impl method's canonical signature: the frame, plus the receiver shape
/// that dispatch needs before it can adjust a receiver expression. The shape
/// is not part of [`DeclSig`] because it is not a type and nothing
/// substitutes into it.
#[derive(Clone, Debug)]
pub(crate) struct MethodSig {
    pub(crate) decl: DeclSig,
    pub(crate) self_kind: crate::ast::SelfKind,
    /// The non-receiver parameters, in order. `decl.param_types` includes
    /// the receiver at index 0 when there is one, so these are offset by
    /// [`Self::first_value_param`].
    pub(crate) params: Vec<Param>,
}

/// What a declaration says about one parameter beyond its type. One record
/// per parameter rather than a vector per attribute: callers read them
/// together, and parallel vectors can disagree in length or order.
#[derive(Clone, Debug)]
pub(crate) struct Param {
    pub(crate) name: String,
    pub(crate) is_mut: bool,
    /// Irreducibly AST — re-resolved per call site under the callee's scope
    /// (WEP 2026-04-11).
    pub(crate) default: Option<crate::ast::Expr>,
}

impl Param {
    pub(crate) fn names(params: &[Self]) -> Vec<String> {
        params.iter().map(|p| p.name.clone()).collect()
    }

    pub(crate) fn is_mut_flags(params: &[Self]) -> Vec<bool> {
        params.iter().map(|p| p.is_mut).collect()
    }

    pub(crate) fn named_defaults(params: &[Self]) -> Vec<(String, Option<crate::ast::Expr>)> {
        params
            .iter()
            .map(|p| (p.name.clone(), p.default.clone()))
            .collect()
    }
}

impl MethodSig {
    /// Index of the first non-receiver parameter in `decl.param_types`.
    pub(crate) fn first_value_param(&self) -> usize {
        usize::from(self.self_kind != crate::ast::SelfKind::None)
    }
}

/// A [`DeclSig`] with its slots filled by a use site's type arguments.
#[derive(Clone, Debug)]
pub(crate) struct InstantiatedSig {
    pub(crate) param_types: Vec<TypeId>,
    pub(crate) return_type: TypeId,
}

impl DeclSig {
    /// Fill the signature's slots with `type_args`, positionally.
    ///
    /// Arity is the caller's contract: it owns the diagnostic for a
    /// mismatch, and passing fewer arguments than slots is how a
    /// partially-inferred call site instantiates what it knows, leaving
    /// the trailing slots abstract. Passing none substitutes nothing.
    pub(crate) fn instantiate(
        &self,
        type_table: &RefCell<TypeTable>,
        type_args: &[TypeId],
    ) -> InstantiatedSig {
        let substitution: IndexMap<u32, TypeId> = type_args
            .iter()
            .enumerate()
            .map(|(i, &t)| (i as u32, t))
            .collect();
        self.instantiate_slots(type_table, &substitution)
    }

    /// Fill slots by index rather than by position, for a caller that already
    /// holds a slot map. Equivalent to [`Self::instantiate`] when the map is
    /// dense from zero; the two differ only when the caller knows a
    /// non-contiguous subset, which positional arguments cannot express.
    pub(crate) fn instantiate_slots(
        &self,
        type_table: &RefCell<TypeTable>,
        substitution: &IndexMap<u32, TypeId>,
    ) -> InstantiatedSig {
        let mut table = type_table.borrow_mut();
        InstantiatedSig {
            param_types: self
                .param_types
                .iter()
                .map(|&p| table.substitute_type_params(p, substitution))
                .collect(),
            return_type: table
                .substitute_type_params(self.return_type.unwrap_or(TypeTable::UNIT), substitution),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `fn id<T>(x: T) -> T` instantiated at `T = i32`.
    fn generic_identity(table: &RefCell<TypeTable>) -> DeclSig {
        let t = table.borrow_mut().make_type_param("T".to_string(), 0);
        DeclSig {
            type_params: vec![("T".to_string(), t)],
            param_types: vec![t],
            return_type: Some(t),
        }
    }

    #[test]
    fn instantiate_fills_slots_positionally() {
        let table = RefCell::new(TypeTable::new());
        let sig = generic_identity(&table);

        let inst = sig.instantiate(&table, &[TypeTable::I32]);

        assert_eq!(inst.param_types, vec![TypeTable::I32]);
        assert_eq!(inst.return_type, TypeTable::I32);
    }

    #[test]
    fn instantiate_without_args_keeps_the_canonical_form() {
        let table = RefCell::new(TypeTable::new());
        let sig = generic_identity(&table);
        let canonical = sig.param_types.clone();

        let inst = sig.instantiate(&table, &[]);

        assert_eq!(inst.param_types, canonical);
        assert_eq!(inst.return_type, canonical[0]);
    }

    #[test]
    fn instantiate_substitutes_inside_containers() {
        let table = RefCell::new(TypeTable::new());
        let t = table.borrow_mut().make_type_param("T".to_string(), 0);
        let list_of_t = table.borrow_mut().make_ref(t);
        let sig = DeclSig {
            type_params: vec![("T".to_string(), t)],
            param_types: vec![list_of_t],
            return_type: None,
        };

        let inst = sig.instantiate(&table, &[TypeTable::I32]);

        let expected = table.borrow_mut().make_ref(TypeTable::I32);
        assert_eq!(inst.param_types, vec![expected]);
        assert_eq!(inst.return_type, TypeTable::UNIT);
    }

    #[test]
    fn a_partial_instantiation_leaves_trailing_slots_abstract() {
        let table = RefCell::new(TypeTable::new());
        let t = table.borrow_mut().make_type_param("T".to_string(), 0);
        let u = table.borrow_mut().make_type_param("U".to_string(), 1);
        let sig = DeclSig {
            type_params: vec![("T".to_string(), t), ("U".to_string(), u)],
            param_types: vec![t, u],
            return_type: Some(u),
        };

        let inst = sig.instantiate(&table, &[TypeTable::I32]);

        assert_eq!(inst.param_types, vec![TypeTable::I32, u]);
        assert_eq!(inst.return_type, u);
    }

    #[test]
    fn a_slotless_signature_is_unchanged_by_instantiation() {
        let table = RefCell::new(TypeTable::new());
        let sig = DeclSig {
            type_params: Vec::new(),
            param_types: vec![TypeTable::I32],
            return_type: Some(TypeTable::BOOL),
        };

        let inst = sig.instantiate(&table, &[]);

        assert_eq!(inst.param_types, vec![TypeTable::I32]);
        assert_eq!(inst.return_type, TypeTable::BOOL);
    }
}
