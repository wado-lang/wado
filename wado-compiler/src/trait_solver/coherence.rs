//! The checks that read the program alone — no receiver, no bounds in force.

use super::program::{ImplDef, ImplId, Program, SolverType};
use crate::hashmap::IndexMap;

/// What a coherence check found. It names impls; the caller turns an id into a
/// span and a message, because only the caller knows what anything is called.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CoherenceError {
    /// Two impls of one `(Trait, Type)` pair. `first` is the one that keeps the
    /// pair, in program order.
    DuplicateImpl { first: ImplId, second: ImplId },
    /// `impl<T> Tr for T` — a value blanket whose receiver parameter carries no
    /// bound, so nothing could ever select it.
    UnboundedValueBlanket { impl_: ImplId },
}

/// Every coherence violation in `program`, in program order.
///
/// Two of the four rules `docs/wep-2026-09-01-trait-resolution.md` gives this
/// function. The orphan rule and variadic overlap still run over the AST in
/// `elaborator::trait_env`, and move here as the lowering grows to carry what
/// they read.
#[must_use]
pub fn coherence_errors(program: &Program) -> Vec<CoherenceError> {
    let mut errors = Vec::new();
    let mut seen: IndexMap<ImplKey<'_>, ImplId> = IndexMap::default();
    for (&id, def) in &program.impls {
        if let Some(impl_) = unbounded_value_blanket(id, def) {
            errors.push(impl_);
        }
        let Some(key) = impl_key(def) else {
            continue;
        };
        match seen.get(&key) {
            Some(&first) => errors.push(CoherenceError::DuplicateImpl { first, second: id }),
            None => {
                seen.insert(key, id);
            }
        }
    }
    errors
}

/// What makes two impls the same pair: the trait, its own arguments, the
/// target, and the bounds its parameters carry.
///
/// The bounds belong in the key because a parameter in the target stands for a
/// condition, not a type. `impl<T: A> Tr for T` beside `impl<T: B> Tr for T`
/// covers a common receiver only where one type satisfies both, and whether any
/// does is not decidable where the impls are written — another module may
/// implement `A` for a type carrying `B` at any time. Selection reports that
/// overlap at the use site, which is the one place the question has an answer.
///
/// An inherent impl has no pair to duplicate — several are how a type spreads
/// its methods across modules — so it has no key. Neither has a derivation
/// request: it asks for a body rather than providing one, so it is redundant
/// beside a real impl of the pair, not a second of them.
type ImplKey<'a> = (
    super::program::TraitDeclId,
    &'a [SolverType],
    &'a SolverType,
    &'a [super::program::ParamDef],
);

fn impl_key(def: &ImplDef) -> Option<ImplKey<'_>> {
    if def.is_derivation_request {
        return None;
    }
    Some((
        def.trait_?,
        def.trait_args.as_slice(),
        &def.target,
        def.params.as_slice(),
    ))
}

/// A value blanket targets a bare parameter, and that parameter states the
/// condition selection reads. Without one the impl names no condition at all.
fn unbounded_value_blanket(id: ImplId, def: &ImplDef) -> Option<CoherenceError> {
    def.trait_?;
    let SolverType::Param(index) = def.target else {
        return None;
    };
    let param = def
        .params
        .get(index as usize)
        .unwrap_or_else(|| panic!("{id:?} targets parameter {index}, which it does not declare"));
    param
        .bounds
        .is_empty()
        .then_some(CoherenceError::UnboundedValueBlanket { impl_: id })
}

#[cfg(test)]
mod tests {
    use super::super::program::{ParamDef, TraitDeclId, TypeDeclId};
    use super::*;

    const TR: TraitDeclId = TraitDeclId(0);
    const OTHER_TR: TraitDeclId = TraitDeclId(1);
    const POINT: TypeDeclId = TypeDeclId(0);
    const BOX: TypeDeclId = TypeDeclId(1);
    const I32: TypeDeclId = TypeDeclId(2);
    const LIMIT: TraitDeclId = TraitDeclId(2);

    fn point() -> SolverType {
        SolverType::Decl(POINT, vec![])
    }

    fn concrete(trait_: TraitDeclId, target: SolverType) -> ImplDef {
        ImplDef {
            trait_: Some(trait_),
            trait_args: vec![],
            target,
            params: vec![],
            is_derivation_request: false,
        }
    }

    fn program(impls: impl IntoIterator<Item = ImplDef>) -> Program {
        let mut program = Program::new();
        for (i, def) in impls.into_iter().enumerate() {
            program.add_impl(ImplId(u32::try_from(i).expect("test impl count")), def);
        }
        program
    }

    #[test]
    fn one_impl_per_pair_is_no_error() {
        let p = program([
            concrete(TR, point()),
            concrete(OTHER_TR, point()),
            concrete(TR, SolverType::Decl(BOX, vec![])),
        ]);
        assert_eq!(coherence_errors(&p), vec![]);
    }

    #[test]
    fn two_impls_of_one_pair_report_the_second() {
        let p = program([concrete(TR, point()), concrete(TR, point())]);
        assert_eq!(
            coherence_errors(&p),
            vec![CoherenceError::DuplicateImpl {
                first: ImplId(0),
                second: ImplId(1),
            }]
        );
    }

    #[test]
    fn three_impls_of_one_pair_report_each_after_the_first() {
        let p = program([
            concrete(TR, point()),
            concrete(TR, point()),
            concrete(TR, point()),
        ]);
        assert_eq!(
            coherence_errors(&p),
            vec![
                CoherenceError::DuplicateImpl {
                    first: ImplId(0),
                    second: ImplId(1),
                },
                CoherenceError::DuplicateImpl {
                    first: ImplId(0),
                    second: ImplId(2),
                },
            ]
        );
    }

    /// `impl Conv<i32> for X` beside `impl Conv<String> for X` are two traits
    /// instantiation-wise, so the pair is not duplicated.
    #[test]
    fn one_trait_at_two_argument_lists_is_no_duplicate() {
        let mut a = concrete(TR, point());
        a.trait_args = vec![SolverType::Decl(I32, vec![])];
        let mut b = concrete(TR, point());
        b.trait_args = vec![SolverType::Decl(BOX, vec![])];
        assert_eq!(coherence_errors(&program([a, b])), vec![]);
    }

    /// The specific-impls-win pair: `impl Tag for Box_<i32>` names one
    /// instantiation and `impl<T> Tag for Box_<T>` the head, so the two targets
    /// differ and selection ranks them rather than coherence rejecting them.
    #[test]
    fn a_specific_instantiation_beside_a_generic_head_is_no_duplicate() {
        let specific = concrete(
            TR,
            SolverType::Decl(BOX, vec![SolverType::Decl(I32, vec![])]),
        );
        let general = ImplDef {
            trait_: Some(TR),
            trait_args: vec![],
            target: SolverType::Decl(BOX, vec![SolverType::Param(0)]),
            params: vec![ParamDef::default()],
            is_derivation_request: false,
        };
        assert_eq!(coherence_errors(&program([specific, general])), vec![]);
    }

    /// A parameter is its position, so the same generic impl written twice
    /// under different letters is one pair.
    #[test]
    fn a_generic_head_written_twice_is_a_duplicate() {
        let head = || ImplDef {
            trait_: Some(TR),
            trait_args: vec![],
            target: SolverType::Decl(BOX, vec![SolverType::Param(0)]),
            params: vec![ParamDef::default()],
            is_derivation_request: false,
        };
        assert_eq!(
            coherence_errors(&program([head(), head()])),
            vec![CoherenceError::DuplicateImpl {
                first: ImplId(0),
                second: ImplId(1),
            }]
        );
    }

    /// Several inherent impls are how a type spreads its methods across
    /// modules; they have no `(Trait, Type)` pair to duplicate.
    #[test]
    fn inherent_impls_never_duplicate() {
        let inherent = || ImplDef {
            trait_: None,
            trait_args: vec![],
            target: point(),
            params: vec![],
            is_derivation_request: false,
        };
        assert_eq!(coherence_errors(&program([inherent(), inherent()])), vec![]);
    }

    /// A blanket names a condition, not a type, so two of them are one pair
    /// only when they state the same condition. Whether two different bounds
    /// can both hold is what an open world cannot decide, which is why the
    /// overlap is a use-site report rather than a definition-time error.
    #[test]
    fn two_blankets_of_one_trait_at_different_bounds_are_no_duplicate() {
        let blanket = |bound: TraitDeclId| ImplDef {
            trait_: Some(TR),
            trait_args: vec![],
            target: SolverType::Param(0),
            params: vec![ParamDef {
                bounds: vec![bound],
            }],
            is_derivation_request: false,
        };
        assert_eq!(
            coherence_errors(&program([blanket(LIMIT), blanket(OTHER_TR)])),
            vec![]
        );
    }

    #[test]
    fn two_blankets_of_one_trait_at_one_bound_are_a_duplicate() {
        let blanket = || ImplDef {
            trait_: Some(TR),
            trait_args: vec![],
            target: SolverType::Param(0),
            params: vec![ParamDef {
                bounds: vec![LIMIT],
            }],
            is_derivation_request: false,
        };
        assert_eq!(
            coherence_errors(&program([blanket(), blanket()])),
            vec![CoherenceError::DuplicateImpl {
                first: ImplId(0),
                second: ImplId(1),
            }]
        );
    }

    /// The same reading one level in: `impl<T: A> Tr for Box_<T>` beside
    /// `impl<T: B> Tr for Box_<T>` covers `Box_<X>` from both sides only for an
    /// `X` satisfying both, which is again undecidable where they are written.
    #[test]
    fn two_bounded_head_impls_at_different_bounds_are_no_duplicate() {
        let boxed = |bound: TraitDeclId| ImplDef {
            trait_: Some(TR),
            trait_args: vec![],
            target: SolverType::Decl(BOX, vec![SolverType::Param(0)]),
            params: vec![ParamDef {
                bounds: vec![bound],
            }],
            is_derivation_request: false,
        };
        assert_eq!(
            coherence_errors(&program([boxed(LIMIT), boxed(OTHER_TR)])),
            vec![]
        );
    }

    /// `impl Serialize for Handler;` beside a hand-written `impl Serialize for
    /// Handler { … }` asks for the derived body and checks conformance; the
    /// real impl answers it. Two requests for one pair are redundant the same
    /// way (WEP 2026-06-25).
    #[test]
    fn a_derivation_request_never_duplicates() {
        let request = || ImplDef {
            trait_: Some(TR),
            trait_args: vec![],
            target: point(),
            params: vec![],
            is_derivation_request: true,
        };
        assert_eq!(
            coherence_errors(&program([concrete(TR, point()), request(), request()])),
            vec![]
        );
    }

    #[test]
    fn a_bounded_value_blanket_is_accepted() {
        let p = program([ImplDef {
            trait_: Some(TR),
            trait_args: vec![],
            target: SolverType::Param(0),
            params: vec![ParamDef {
                bounds: vec![LIMIT],
            }],
            is_derivation_request: false,
        }]);
        assert_eq!(coherence_errors(&p), vec![]);
    }

    #[test]
    fn an_unbounded_value_blanket_is_rejected() {
        let p = program([ImplDef {
            trait_: Some(TR),
            trait_args: vec![],
            target: SolverType::Param(0),
            params: vec![ParamDef::default()],
            is_derivation_request: false,
        }]);
        assert_eq!(
            coherence_errors(&p),
            vec![CoherenceError::UnboundedValueBlanket { impl_: ImplId(0) }]
        );
    }

    /// A reference blanket binds the parameter under a `&`, so the bound is not
    /// what selects it and an unbounded one is a different question.
    #[test]
    fn an_unbounded_reference_blanket_is_not_this_error() {
        let p = program([ImplDef {
            trait_: Some(TR),
            trait_args: vec![],
            target: SolverType::Ref {
                is_mut: false,
                inner: Box::new(SolverType::Param(0)),
            },
            params: vec![ParamDef::default()],
            is_derivation_request: false,
        }]);
        assert_eq!(coherence_errors(&p), vec![]);
    }

    /// An unbounded blanket is still one impl of its pair, so the two checks do
    /// not hide each other.
    #[test]
    fn both_errors_are_reported_for_one_impl_set() {
        let blanket = || ImplDef {
            trait_: Some(TR),
            trait_args: vec![],
            target: SolverType::Param(0),
            params: vec![ParamDef::default()],
            is_derivation_request: false,
        };
        assert_eq!(
            coherence_errors(&program([blanket(), blanket()])),
            vec![
                CoherenceError::UnboundedValueBlanket { impl_: ImplId(0) },
                CoherenceError::UnboundedValueBlanket { impl_: ImplId(1) },
                CoherenceError::DuplicateImpl {
                    first: ImplId(0),
                    second: ImplId(1),
                },
            ]
        );
    }
}
