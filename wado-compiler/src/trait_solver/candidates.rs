//! Which impls could answer `recv.m(args)`, and at what depth
//! (`docs/wep-2026-09-01-trait-resolution.md`, "The candidates"). What picks
//! between them is [`rank`](super::rank).

use super::holds::impl_applies;
use super::program::{Env, MethodId, ModuleId, Program, SolverType, TraitDeclId};
use super::rank::{Candidate, Generality};

/// The candidates at one call site, and what the diagnostic needs where there
/// are none.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Candidates {
    /// What [`rank`](super::rank::rank) orders: the impls of a trait the call
    /// site can name.
    pub in_scope: Vec<Candidate>,
    /// The traits that would have answered had the call site imported them,
    /// collected only where `in_scope` came out empty. The caller turns one
    /// into the "not imported here" message rather than a candidate.
    pub out_of_scope: Vec<TraitDeclId>,
}

/// Every impl that could answer a call of `method` on `receiver` made in
/// `scope`.
#[must_use]
pub fn candidates(
    program: &Program,
    env: &Env,
    receiver: &SolverType,
    method: MethodId,
    scope: ModuleId,
) -> Candidates {
    let in_scope: &[TraitDeclId] = program
        .scopes
        .get(&scope)
        .map_or(&[], |module| &module.traits_in_scope);
    let mut found = Candidates::default();
    for (depth, ty) in chain(program, receiver).iter().enumerate() {
        let depth = u32::try_from(depth).expect("a chain shorter than 2^32");
        for (&impl_, def) in &program.impls {
            let Some(trait_) = def.trait_ else {
                continue;
            };
            if !program
                .traits
                .get(&trait_)
                .is_some_and(|decl| decl.methods.contains(&method))
            {
                continue;
            }
            let Some(trait_args) = impl_applies(program, env, scope, impl_, def, ty) else {
                continue;
            };
            if !in_scope.contains(&trait_) {
                if !found.out_of_scope.contains(&trait_) {
                    found.out_of_scope.push(trait_);
                }
                continue;
            }
            found.in_scope.push(Candidate {
                impl_,
                trait_,
                trait_args,
                depth,
                generality: generality(&def.target),
                is_variadic: is_variadic(&def.target),
            });
        }
    }
    if !found.in_scope.is_empty() {
        found.out_of_scope.clear();
    }
    found
}

/// The receiver, then what it dereferences or newtype-unwraps to, and so on.
/// The index of a type here is the depth rank 1 reads: a reference's own impls
/// are one level above its pointee's, and a newtype's above its base's.
fn chain(program: &Program, receiver: &SolverType) -> Vec<SolverType> {
    let mut chain = vec![receiver.clone()];
    while let Some(next) = peel(program, chain.last().expect("a chain starts non-empty")) {
        // A newtype cycle is rejected where it is declared; refusing to walk one
        // twice keeps a malformed program from hanging the query.
        if chain.contains(&next) {
            break;
        }
        chain.push(next);
    }
    chain
}

/// One level down: a reference reaches its pointee, and a newtype the base it
/// inherits impls from, at the newtype's own type arguments.
fn peel(program: &Program, ty: &SolverType) -> Option<SolverType> {
    match ty {
        SolverType::Ref { inner, .. } => Some((**inner).clone()),
        SolverType::Decl(head, args) => {
            let base = program.types.get(head)?.newtype_base.as_ref()?;
            Some(
                base.map_params(&|i| args.get(i as usize).cloned())
                    .unwrap_or_else(|| {
                        panic!("{base:?} mentions a parameter {ty:?} has no argument for")
                    }),
            )
        }
        SolverType::Param(_) | SolverType::Pack(_) | SolverType::Tuple(_) => None,
    }
}

/// How much of the general case a target covers: rank 2's question, read off
/// the target alone.
fn generality(target: &SolverType) -> Generality {
    match target {
        SolverType::Param(_) | SolverType::Pack(_) => Generality::Any,
        SolverType::Decl(..) | SolverType::Ref { .. } | SolverType::Tuple(_) => {
            if target.mentions(&|_| true) {
                Generality::Head
            } else {
                Generality::Exact
            }
        }
    }
}

/// Whether the target is the bare `[..T]` a variadic impl is written for.
fn is_variadic(target: &SolverType) -> bool {
    matches!(target, SolverType::Tuple(elems) if matches!(elems.as_slice(), [SolverType::Pack(_)]))
}

#[cfg(test)]
mod tests {
    use super::super::program::{
        AssocId, ImplDef, ImplId, ImplOrigin, ModuleScope, ParamDef, Pin, TypeDeclId, TypeDef,
    };
    use super::super::rank::{Selection, rank};
    use super::super::testing::{Builder, concrete, decl, generic, ref_to};
    use super::*;

    const M: MethodId = MethodId(0);
    const OTHER_M: MethodId = MethodId(1);
    const TR: TraitDeclId = TraitDeclId(0);
    const OTHER: TraitDeclId = TraitDeclId(1);
    const LIMIT: TraitDeclId = TraitDeclId(2);
    const POINT: TypeDeclId = TypeDeclId(0);
    const WRAPPER: TypeDeclId = TypeDeclId(1);
    const BOX: TypeDeclId = TypeDeclId(2);
    const I32: TypeDeclId = TypeDeclId(3);
    const HERE: ModuleId = ModuleId(0);
    const ELSEWHERE: ModuleId = ModuleId(1);

    /// A program where `TR` and `OTHER` declare `M`, `LIMIT` declares nothing
    /// and only bounds a blanket, and all three are in scope in `HERE`. A test
    /// then states only what it is about.
    fn program(build: Builder) -> Program {
        let mut program = build.build();
        for trait_ in [TR, OTHER] {
            program.traits.entry(trait_).or_default().methods = vec![M];
        }
        program.traits.entry(LIMIT).or_default();
        program.scopes.insert(
            HERE,
            ModuleScope {
                traits_in_scope: vec![TR, OTHER, LIMIT],
            },
        );
        program
    }

    fn ask(program: &Program, receiver: &SolverType) -> Candidates {
        candidates(program, &Env::default(), receiver, M, HERE)
    }

    /// The winner's impl, so a test states the answer rather than an index.
    fn selected(found: &Candidates) -> Option<ImplId> {
        match rank(&found.in_scope) {
            Selection::One(index) => Some(found.in_scope[index].impl_),
            Selection::None
            | Selection::AmbiguousTraits(_)
            | Selection::AmbiguousBlankets(_)
            | Selection::Overloaded(_)
            | Selection::Duplicated(_) => None,
        }
    }

    #[test]
    fn nothing_is_a_candidate_in_an_empty_program() {
        assert_eq!(
            ask(&program(Builder::default()), &decl(POINT)),
            Candidates::default()
        );
    }

    #[test]
    fn an_impl_written_for_the_receiver_is_a_candidate() {
        let p = program(Builder::default().concrete(TR, decl(POINT)));
        let found = ask(&p, &decl(POINT));
        assert_eq!(selected(&found), Some(ImplId(0)));
        assert_eq!(found.in_scope[0].generality, Generality::Exact);
        assert_eq!(found.in_scope[0].depth, 0);
    }

    #[test]
    fn an_impl_for_another_type_is_not_a_candidate() {
        let p = program(Builder::default().concrete(TR, decl(POINT)));
        assert_eq!(ask(&p, &decl(BOX)), Candidates::default());
    }

    /// The method name is what a call site matches on, so a trait declaring
    /// another name contributes nothing however well its impl matches.
    #[test]
    fn a_trait_that_declares_another_method_contributes_nothing() {
        let mut p = program(Builder::default().concrete(TR, decl(POINT)));
        p.traits.entry(TR).or_default().methods = vec![OTHER_M];
        assert_eq!(ask(&p, &decl(POINT)), Candidates::default());
    }

    /// A value blanket is a candidate for every receiver its bound holds of,
    /// and rank 2 places it under an impl written for the receiver.
    #[test]
    fn a_value_blanket_is_a_candidate_where_its_bound_holds() {
        let p = program(Builder::default().concrete(LIMIT, decl(POINT)).bounded(
            TR,
            SolverType::Param(0),
            vec![LIMIT],
        ));
        let found = ask(&p, &decl(POINT));
        assert_eq!(selected(&found), Some(ImplId(1)));
        assert_eq!(found.in_scope[0].generality, Generality::Any);
    }

    #[test]
    fn a_value_blanket_whose_bound_fails_is_no_candidate() {
        let p = program(Builder::default().bounded(TR, SolverType::Param(0), vec![LIMIT]));
        assert_eq!(ask(&p, &decl(POINT)), Candidates::default());
    }

    /// The WEP's third list. A reference blanket answers a reference receiver,
    /// which no dispatch path reached before.
    #[test]
    fn a_reference_blanket_is_a_candidate_for_a_reference_receiver() {
        let p = program(Builder::default().concrete(LIMIT, decl(POINT)).bounded(
            TR,
            ref_to(SolverType::Param(0)),
            vec![LIMIT],
        ));
        let found = ask(&p, &ref_to(decl(POINT)));
        assert_eq!(selected(&found), Some(ImplId(1)));
        assert_eq!(found.in_scope[0].depth, 0);
        assert_eq!(found.in_scope[0].generality, Generality::Head);
    }

    /// It ranks below a concrete `&T` impl.
    #[test]
    fn a_concrete_reference_impl_beats_a_reference_blanket() {
        let p = program(
            Builder::default()
                .concrete(LIMIT, decl(POINT))
                .bounded(TR, ref_to(SolverType::Param(0)), vec![LIMIT])
                .concrete(TR, ref_to(decl(POINT))),
        );
        assert_eq!(selected(&ask(&p, &ref_to(decl(POINT)))), Some(ImplId(2)));
    }

    /// And above any impl on the base type, which sits one level down.
    #[test]
    fn a_reference_blanket_beats_an_impl_on_the_pointee() {
        let p = program(
            Builder::default()
                .concrete(LIMIT, decl(POINT))
                .bounded(TR, ref_to(SolverType::Param(0)), vec![LIMIT])
                .concrete(TR, decl(POINT)),
        );
        let found = ask(&p, &ref_to(decl(POINT)));
        assert_eq!(found.in_scope[1].depth, 1);
        assert_eq!(selected(&found), Some(ImplId(1)));
    }

    /// Rank 1's question. A newtype's own impl sits at 0 and its base's at 1,
    /// so the newtype answers whatever the base carries.
    #[test]
    fn a_newtype_own_impl_outranks_its_base_s() {
        let mut p = program(
            Builder::default()
                .concrete(TR, decl(POINT))
                .concrete(TR, decl(WRAPPER)),
        );
        p.types.insert(
            WRAPPER,
            TypeDef {
                newtype_base: Some(decl(POINT)),
            },
        );
        let found = ask(&p, &decl(WRAPPER));
        assert_eq!(selected(&found), Some(ImplId(1)));
        assert_eq!(found.in_scope[0].depth, 0);
        assert_eq!(found.in_scope[1].depth, 1);
    }

    /// The gap the WEP records closed: depth is read off an impl's target too,
    /// so a blanket holding at the newtype outranks a concrete impl on the base
    /// rather than losing to it.
    #[test]
    fn a_blanket_at_the_newtype_outranks_a_concrete_impl_on_the_base() {
        let mut p = program(
            Builder::default()
                .concrete(LIMIT, decl(WRAPPER))
                .bounded(TR, SolverType::Param(0), vec![LIMIT])
                .concrete(TR, decl(POINT)),
        );
        p.types.insert(
            WRAPPER,
            TypeDef {
                newtype_base: Some(decl(POINT)),
            },
        );
        assert_eq!(selected(&ask(&p, &decl(WRAPPER))), Some(ImplId(1)));
    }

    /// `spec.md`'s "Specific Impls Win", assembled end to end.
    #[test]
    fn an_impl_for_one_instantiation_outranks_the_head_impl() {
        let p = program(
            Builder::default()
                .impl_(generic(
                    1,
                    concrete(TR, SolverType::Decl(BOX, vec![SolverType::Param(0)])),
                ))
                .concrete(TR, SolverType::Decl(BOX, vec![decl(I32)])),
        );
        let found = ask(&p, &SolverType::Decl(BOX, vec![decl(I32)]));
        assert_eq!(found.in_scope[0].generality, Generality::Head);
        assert_eq!(selected(&found), Some(ImplId(1)));
    }

    #[test]
    fn the_head_impl_answers_every_other_instantiation() {
        let p = program(
            Builder::default()
                .impl_(generic(
                    1,
                    concrete(TR, SolverType::Decl(BOX, vec![SolverType::Param(0)])),
                ))
                .concrete(TR, SolverType::Decl(BOX, vec![decl(I32)])),
        );
        assert_eq!(
            selected(&ask(&p, &SolverType::Decl(BOX, vec![decl(POINT)]))),
            Some(ImplId(0))
        );
    }

    /// The prelude's shape, assembled end to end: an impl written for the
    /// receiver's head, beside a value blanket whose bound the receiver also
    /// satisfies. This is `IntoIterator` over a range — `RangeExclusive<T>`
    /// implements `Iterator`, so `impl<I: Iterator> IntoIterator for I` applies
    /// to it beside the `impl<T: Step + Ord> IntoIterator for RangeExclusive<T>`
    /// the prelude means, and rank 2 is what separates them.
    #[test]
    fn the_head_impl_outranks_a_value_blanket_the_receiver_also_answers() {
        let boxed = |arg| SolverType::Decl(BOX, vec![arg]);
        let p = program(
            Builder::default()
                .concrete(LIMIT, boxed(decl(I32)))
                .impl_(generic(1, concrete(TR, boxed(SolverType::Param(0)))))
                .bounded(TR, SolverType::Param(0), vec![LIMIT]),
        );
        let found = ask(&p, &boxed(decl(I32)));
        assert_eq!(found.in_scope[0].generality, Generality::Head);
        assert_eq!(found.in_scope[1].generality, Generality::Any);
        assert_eq!(selected(&found), Some(ImplId(1)));
    }

    /// Selection names the trait, so unlike a bound it takes an impl at any
    /// argument list — the two form the overload set the call's arguments
    /// choose from (WEP 2026-07-31).
    #[test]
    fn one_trait_at_two_argument_lists_yields_both_candidates() {
        let mut p = program(Builder::default());
        p.traits.entry(TR).or_default().arg_defaults = vec![None];
        for arg in [decl(I32), decl(POINT)] {
            p.push_impl(ImplDef {
                trait_args: vec![arg],
                ..concrete(TR, decl(BOX))
            });
        }
        let found = ask(&p, &decl(BOX));
        assert_eq!(
            found
                .in_scope
                .iter()
                .map(|c| &c.trait_args)
                .collect::<Vec<_>>(),
            vec![&vec![decl(I32)], &vec![decl(POINT)]]
        );
        assert_eq!(rank(&found.in_scope), Selection::Overloaded(vec![0, 1]));
    }

    /// The arguments a candidate reports are the trait's at the receiver, not
    /// as the impl spelled them, so two impls compare on what the call gets.
    #[test]
    fn a_candidate_reports_its_trait_arguments_at_the_receiver() {
        let mut p = program(Builder::default());
        p.traits.entry(TR).or_default().arg_defaults = vec![None];
        p.push_impl(ImplDef {
            trait_args: vec![SolverType::Param(0)],
            params: vec![ParamDef::default()],
            ..concrete(TR, SolverType::Decl(BOX, vec![SolverType::Param(0)]))
        });
        let found = ask(&p, &SolverType::Decl(BOX, vec![decl(I32)]));
        assert_eq!(found.in_scope[0].trait_args, vec![decl(I32)]);
    }

    /// Two traits declaring one method name both contribute, which is what
    /// makes the collision reportable.
    #[test]
    fn two_traits_declaring_the_method_both_contribute() {
        let p = program(
            Builder::default()
                .concrete(TR, decl(POINT))
                .concrete(OTHER, decl(POINT)),
        );
        assert_eq!(
            rank(&ask(&p, &decl(POINT)).in_scope),
            Selection::AmbiguousTraits(vec![0, 1])
        );
    }

    /// The scope gate: a trait the calling module never imported answers no
    /// call there, wherever its impl was written.
    #[test]
    fn a_trait_out_of_scope_contributes_no_candidate() {
        let p = program(Builder::default().concrete(TR, decl(POINT)));
        let found = candidates(&p, &Env::default(), &decl(POINT), M, ELSEWHERE);
        assert_eq!(found.in_scope, vec![]);
        assert_eq!(found.out_of_scope, vec![TR]);
    }

    /// The recovery path: the unscoped search runs for the diagnostic alone, so
    /// it reports nothing once a scoped candidate exists.
    #[test]
    fn the_unscoped_search_reports_only_where_nothing_was_in_scope() {
        let mut p = program(
            Builder::default()
                .concrete(TR, decl(POINT))
                .concrete(OTHER, decl(POINT)),
        );
        p.scopes.insert(
            HERE,
            ModuleScope {
                traits_in_scope: vec![TR],
            },
        );
        let found = ask(&p, &decl(POINT));
        assert_eq!(selected(&found), Some(ImplId(0)));
        assert_eq!(found.out_of_scope, vec![]);
    }

    /// A module that imported nothing sees nothing, and the recovery path names
    /// every trait that would have answered.
    #[test]
    fn a_module_with_no_imports_sees_no_trait() {
        let p = program(
            Builder::default()
                .concrete(TR, decl(POINT))
                .concrete(OTHER, decl(POINT)),
        );
        let found = candidates(&p, &Env::default(), &decl(POINT), M, ModuleId(7));
        assert_eq!(found.in_scope, vec![]);
        assert_eq!(found.out_of_scope, vec![TR, OTHER]);
    }

    /// A variadic impl's target is the bare `[..T]`, which rank 0 reads.
    #[test]
    fn a_variadic_impl_is_marked_as_one() {
        let p = program(Builder::default().impl_(generic(
            1,
            concrete(TR, SolverType::Tuple(vec![SolverType::Pack(0)])),
        )));
        let found = ask(&p, &SolverType::Tuple(vec![decl(I32), decl(I32)]));
        assert!(found.in_scope[0].is_variadic);
    }

    #[test]
    fn a_non_variadic_tuple_impl_is_not_marked_variadic() {
        let p = program(Builder::default().concrete(TR, SolverType::Tuple(vec![decl(I32)])));
        let found = ask(&p, &SolverType::Tuple(vec![decl(I32)]));
        assert!(!found.in_scope[0].is_variadic);
    }

    /// An impl whose bound the environment supplies answers for a type
    /// parameter, so a generic body's call resolves by its own signature.
    #[test]
    fn a_bound_in_force_makes_a_blanket_a_candidate_for_a_parameter() {
        let p = program(Builder::default().bounded(TR, SolverType::Param(0), vec![LIMIT]));
        let env = Env {
            param_bounds: vec![vec![LIMIT]],
        };
        let found = candidates(&p, &env, &SolverType::Param(0), M, HERE);
        assert_eq!(found.in_scope.len(), 1);
    }

    /// A pin on the blanket's bound is checked against what the answering impl
    /// binds, so an impl binding it otherwise supplies no candidate.
    #[test]
    fn a_blanket_whose_pin_the_receiver_fails_is_no_candidate() {
        let mut p = program(
            Builder::default()
                .concrete(LIMIT, decl(POINT))
                .impl_(ImplDef {
                    params: vec![ParamDef {
                        bounds: vec![LIMIT],
                        pins: vec![Pin {
                            trait_: LIMIT,
                            assoc: AssocId(0),
                            ty: decl(I32),
                        }],
                    }],
                    ..concrete(TR, SolverType::Param(0))
                }),
        );
        p.assoc_bindings
            .insert(ImplId(0), vec![(AssocId(0), decl(POINT))]);
        assert_eq!(ask(&p, &decl(POINT)).in_scope, vec![]);
    }

    /// A derived impl is a candidate like any other: what it owes the caller is
    /// the body, which `holds` reports, not a place in the order.
    #[test]
    fn a_derived_impl_is_a_candidate() {
        let p = program(Builder::default().impl_(ImplDef {
            origin: ImplOrigin::Derived,
            ..concrete(TR, decl(POINT))
        }));
        assert_eq!(selected(&ask(&p, &decl(POINT))), Some(ImplId(0)));
    }

    /// An inherent impl declares no trait, so it never enters the trait order —
    /// the step above it in method lookup already shadowed the call.
    #[test]
    fn an_inherent_impl_is_no_trait_candidate() {
        let p = program(Builder::default().impl_(ImplDef {
            trait_: None,
            ..concrete(TR, decl(POINT))
        }));
        assert_eq!(ask(&p, &decl(POINT)), Candidates::default());
    }

    /// A newtype chain that closes on itself is rejected where it is declared;
    /// the walk refuses to hang on one.
    #[test]
    fn a_newtype_cycle_does_not_hang_the_walk() {
        let mut p = program(Builder::default().concrete(TR, decl(POINT)));
        p.types.insert(
            WRAPPER,
            TypeDef {
                newtype_base: Some(decl(WRAPPER)),
            },
        );
        assert_eq!(ask(&p, &decl(WRAPPER)), Candidates::default());
    }

    /// A trait's own declaration answers its supertrait's bound, but not its
    /// supertrait's methods: an implementor writes an impl of each.
    #[test]
    fn an_impl_of_a_subtrait_does_not_supply_the_supertrait_s_method() {
        let mut p = program(
            Builder::default()
                .supertrait(OTHER, TR)
                .concrete(OTHER, decl(POINT)),
        );
        p.traits.entry(TR).or_default().methods = vec![M];
        p.traits.entry(OTHER).or_default().methods = vec![OTHER_M];
        assert_eq!(ask(&p, &decl(POINT)), Candidates::default());
    }
}
