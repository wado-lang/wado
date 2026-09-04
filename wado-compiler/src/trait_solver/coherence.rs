//! The checks that read the program alone — no receiver, no bounds in force.

use super::program::{ImplDef, ImplId, ImplOrigin, Pin, Program, SolverType, TraitDeclId};
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

/// Every coherence violation in `program`, in program order. The orphan rule
/// and variadic overlap still run over the AST in `elaborator::trait_env`.
#[must_use]
pub fn coherence_errors(program: &Program) -> Vec<CoherenceError> {
    let mut errors = Vec::new();
    let mut seen: IndexMap<ImplKey, ImplId> = IndexMap::default();
    for (&id, def) in &program.impls {
        if is_unbounded_value_blanket(def) {
            errors.push(CoherenceError::UnboundedValueBlanket { impl_: id });
        }
        let Some(key) = impl_key(program, def) else {
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

/// What makes two impls one pair: trait, arguments at defaults, target, and
/// each parameter's bounds and pins as sets.
type ImplKey = (
    TraitDeclId,
    Vec<SolverType>,
    SolverType,
    Vec<(Vec<TraitDeclId>, Vec<Pin>)>,
);

/// An inherent impl and a marker have no key: several inherent impls spread a
/// type's methods across modules, and a marker asks for a body.
fn impl_key(program: &Program, def: &ImplDef) -> Option<ImplKey> {
    if def.origin == ImplOrigin::Marker {
        return None;
    }
    let trait_ = def.trait_?;
    fn set<T: Ord + Clone>(items: &[T]) -> Vec<T> {
        let mut items = items.to_vec();
        items.sort_unstable();
        items.dedup();
        items
    }
    let conditions = def
        .params
        .iter()
        .map(|p| (set(&p.bounds), set(&p.pins)))
        .collect();
    let mut trait_args = def.trait_args.clone();
    trait_args.extend((trait_args.len()..).map_while(|i| program.default_arg(def, i)));
    Some((trait_, trait_args, def.target.clone(), conditions))
}

/// A value blanket targets a bare parameter, whose bounds are the condition
/// selection reads; without one the impl names no condition at all.
fn is_unbounded_value_blanket(def: &ImplDef) -> bool {
    match def.target {
        SolverType::Param(index) => {
            def.trait_.is_some() && def.params[index as usize].bounds.is_empty()
        }
        SolverType::Decl(..)
        | SolverType::Pack(_)
        | SolverType::Ref { .. }
        | SolverType::Tuple(_)
        | SolverType::Projection { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::program::{ArgDefault, AssocId, ParamDef, TraitDef, TypeDeclId};
    use super::super::testing::{Builder, bounded, concrete, decl, ref_to};
    use super::*;

    /// A program of these impls, at ids in order.
    fn program(impls: impl IntoIterator<Item = ImplDef>) -> Program {
        impls
            .into_iter()
            .fold(Builder::default(), Builder::impl_)
            .build()
    }

    fn boxed(inner: SolverType) -> SolverType {
        SolverType::Decl(BOX, vec![inner])
    }

    const TR: TraitDeclId = TraitDeclId(0);
    const OTHER_TR: TraitDeclId = TraitDeclId(1);
    const POINT: TypeDeclId = TypeDeclId(0);
    const BOX: TypeDeclId = TypeDeclId(1);
    const I32: TypeDeclId = TypeDeclId(2);
    const LIMIT: TraitDeclId = TraitDeclId(2);

    fn point() -> SolverType {
        decl(POINT)
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
        let specific = concrete(TR, boxed(decl(I32)));
        let general = bounded(TR, boxed(SolverType::Param(0)), vec![]);
        assert_eq!(coherence_errors(&program([specific, general])), vec![]);
    }

    /// A parameter is its position, so the same generic impl written twice
    /// under different letters is one pair.
    #[test]
    fn a_generic_head_written_twice_is_a_duplicate() {
        let head = || bounded(TR, boxed(SolverType::Param(0)), vec![]);
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
            ..concrete(TR, point())
        };
        assert_eq!(coherence_errors(&program([inherent(), inherent()])), vec![]);
    }

    /// A blanket names a condition, not a type, so two of them are one pair
    /// only when they state the same condition.
    #[test]
    fn two_blankets_of_one_trait_at_different_bounds_are_no_duplicate() {
        let blanket = |bound: TraitDeclId| bounded(TR, SolverType::Param(0), vec![bound]);
        assert_eq!(
            coherence_errors(&program([blanket(LIMIT), blanket(OTHER_TR)])),
            vec![]
        );
    }

    #[test]
    fn two_blankets_of_one_trait_at_one_bound_are_a_duplicate() {
        let blanket = || bounded(TR, SolverType::Param(0), vec![LIMIT]);
        assert_eq!(
            coherence_errors(&program([blanket(), blanket()])),
            vec![CoherenceError::DuplicateImpl {
                first: ImplId(0),
                second: ImplId(1),
            }]
        );
    }

    /// `impl<T: A + B>` and `impl<T: B + A>` state one condition.
    #[test]
    fn two_blankets_at_reordered_bounds_are_a_duplicate() {
        let blanket = |bounds: Vec<TraitDeclId>| bounded(TR, SolverType::Param(0), bounds);
        assert_eq!(
            coherence_errors(&program([
                blanket(vec![LIMIT, OTHER_TR]),
                blanket(vec![OTHER_TR, LIMIT]),
            ])),
            vec![CoherenceError::DuplicateImpl {
                first: ImplId(0),
                second: ImplId(1),
            }]
        );
    }

    /// The pins travel with the bounds: `T: A<X = i32> + B<Y = i32>` and
    /// `T: B<Y = i32> + A<X = i32>` are one condition.
    #[test]
    fn two_blankets_at_reordered_pinned_bounds_are_a_duplicate() {
        let pin = |trait_: TraitDeclId| Pin {
            trait_,
            assoc: AssocId(0),
            ty: decl(I32),
        };
        let blanket = |bounds: [TraitDeclId; 2]| ImplDef {
            params: vec![ParamDef {
                bounds: bounds.to_vec(),
                pins: bounds.map(pin).to_vec(),
            }],
            ..concrete(TR, SolverType::Param(0))
        };
        assert_eq!(
            coherence_errors(&program([
                blanket([LIMIT, OTHER_TR]),
                blanket([OTHER_TR, LIMIT]),
            ])),
            vec![CoherenceError::DuplicateImpl {
                first: ImplId(0),
                second: ImplId(1),
            }]
        );
    }

    /// `trait Mul<Rhs = Self>`: `impl Mul for Point` and
    /// `impl Mul<Point> for Point` are one pair.
    #[test]
    fn an_omitted_argument_is_its_default() {
        let mut explicit = concrete(TR, point());
        explicit.trait_args = vec![point()];
        let mut p = program([concrete(TR, point()), explicit]);
        p.traits.insert(
            TR,
            TraitDef {
                arg_defaults: vec![Some(ArgDefault::SelfType)],
                ..TraitDef::default()
            },
        );
        assert_eq!(
            coherence_errors(&p),
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
        let head = |bound: TraitDeclId| bounded(TR, boxed(SolverType::Param(0)), vec![bound]);
        assert_eq!(
            coherence_errors(&program([head(LIMIT), head(OTHER_TR)])),
            vec![]
        );
    }

    /// `impl Serialize for Handler;` beside a hand-written impl asks for the
    /// derived body, which the real impl answers (WEP 2026-06-25).
    #[test]
    fn a_derivation_request_never_duplicates() {
        let request = || ImplDef {
            origin: ImplOrigin::Marker,
            ..concrete(TR, point())
        };
        assert_eq!(
            coherence_errors(&program([concrete(TR, point()), request(), request()])),
            vec![]
        );
    }

    #[test]
    fn a_bounded_value_blanket_is_accepted() {
        let p = program([bounded(TR, SolverType::Param(0), vec![LIMIT])]);
        assert_eq!(coherence_errors(&p), vec![]);
    }

    #[test]
    fn an_unbounded_value_blanket_is_rejected() {
        let p = program([bounded(TR, SolverType::Param(0), vec![])]);
        assert_eq!(
            coherence_errors(&p),
            vec![CoherenceError::UnboundedValueBlanket { impl_: ImplId(0) }]
        );
    }

    /// A reference blanket binds the parameter under a `&`, so the bound is not
    /// what selects it and an unbounded one is a different question.
    #[test]
    fn an_unbounded_reference_blanket_is_not_this_error() {
        let p = program([bounded(TR, ref_to(SolverType::Param(0)), vec![])]);
        assert_eq!(coherence_errors(&p), vec![]);
    }

    /// An unbounded blanket is still one impl of its pair, so the two checks do
    /// not hide each other.
    #[test]
    fn both_errors_are_reported_for_one_impl_set() {
        let blanket = || bounded(TR, SolverType::Param(0), vec![]);
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
