//! Does this type satisfy this trait?
//!
//! Every other question is asked on top of this one: whether a blanket is a
//! candidate, whether a call's bound is met, whether a derived body is owed.
//!
//! The nine ways a bound holds split at the recursion. Six are properties of a
//! type — a primitive's built-in traits, a plain `enum`'s `Display`, a
//! reference identity, a structural derivation over the members, a
//! declaration's own reflection kind, a trait that holds for everything — and
//! the lowering states them as [`Fact`]s. The other three are here: a bound in
//! force, an impl written for the type, and a blanket whose own bound is asked
//! in turn.

use super::program::{DerivationRequest, Env, ImplDef, ModuleId, Program, SolverType, TraitDeclId};

/// A bound that holds, and the bodies its answer owes.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Holds {
    pub requests: Vec<DerivationRequest>,
}

/// Whether `ty` satisfies `trait_`, asked from `scope`.
///
/// `None` is "does not hold". It is also the answer to a question that reaches
/// itself: `impl<T: A> B for T` beside `impl<T: B> A for T` grounds neither
/// trait, and a walk that answered yes to the repeat would make every type
/// satisfy both. The solver recurses only through blanket bounds, so a repeated
/// `(type, trait)` pair is always that cycle — descent into a type's members,
/// where a repeat is the well-founded case, is a fact rather than a step here.
#[must_use]
pub fn holds(
    program: &Program,
    env: &Env,
    ty: &SolverType,
    trait_: TraitDeclId,
    scope: ModuleId,
) -> Option<Holds> {
    holds_within(program, env, ty, trait_, scope, &mut Vec::new())
}

fn holds_within(
    program: &Program,
    env: &Env,
    ty: &SolverType,
    trait_: TraitDeclId,
    scope: ModuleId,
    asking: &mut Vec<(SolverType, TraitDeclId)>,
) -> Option<Holds> {
    let goal = (ty.clone(), trait_);
    if asking.contains(&goal) {
        return None;
    }
    if let SolverType::Param(index) = ty
        && let Some(bounds) = env.param_bounds.get(*index as usize)
        && bounds
            .iter()
            .any(|&bound| program.bound_reaches(bound, trait_))
    {
        return Some(Holds::default());
    }
    if let Some(fact) = program.facts.get(&goal)
        && fact
            .visible_from
            .as_ref()
            .is_none_or(|modules| modules.contains(&scope))
    {
        return Some(Holds {
            requests: fact.requests.clone(),
        });
    }

    asking.push(goal);
    let answer = program
        .impls
        .values()
        .find_map(|def| impl_answers(program, env, def, ty, trait_, scope, asking));
    asking.pop();
    answer
}

/// Whether one impl answers the goal: its trait must reach `trait_`, its target
/// must match `ty`, and every bound its parameters carry must hold of what the
/// match bound them to.
fn impl_answers(
    program: &Program,
    env: &Env,
    def: &ImplDef,
    ty: &SolverType,
    trait_: TraitDeclId,
    scope: ModuleId,
    asking: &mut Vec<(SolverType, TraitDeclId)>,
) -> Option<Holds> {
    if !program.bound_reaches(def.trait_?, trait_) {
        return None;
    }
    let mut bindings: Vec<Option<SolverType>> = vec![None; def.params.len()];
    if !match_target(&def.target, ty, &mut bindings) {
        return None;
    }
    // A marker (`impl Serialize for T;`) makes the pair exist where no bound
    // would have asked for it, so it answers — and the body it asks for is owed
    // by the answer, exactly as a structural fact's is.
    let mut requests = if def.is_derivation_request {
        vec![DerivationRequest {
            ty: ty.clone(),
            trait_,
        }]
    } else {
        Vec::new()
    };
    for (index, param) in def.params.iter().enumerate() {
        // A parameter the target never mentions is unconstrained, which is an
        // error where the impl is written; it binds nothing here, so a bound on
        // it cannot be checked and the impl does not answer through it.
        let Some(bound_to) = bindings[index].as_ref() else {
            if param.bounds.is_empty() {
                continue;
            }
            return None;
        };
        for &bound in &param.bounds {
            let answer = holds_within(program, env, bound_to, bound, scope, asking)?;
            requests.extend(answer.requests);
        }
    }
    Some(Holds { requests })
}

/// Match an impl target against a type, binding the target's parameters by
/// position. A parameter matches anything; everything else matches its own
/// shape.
fn match_target(target: &SolverType, ty: &SolverType, bindings: &mut [Option<SolverType>]) -> bool {
    match (target, ty) {
        (SolverType::Param(index), _) => {
            // One parameter at two positions must be one type:
            // `impl<T> Pair<T, T>` does not match `Pair<i32, String>`.
            let slot = &mut bindings[*index as usize];
            if let Some(bound) = slot {
                return bound == ty;
            }
            *slot = Some(ty.clone());
            true
        }
        (SolverType::Decl(head, args), SolverType::Decl(ty_head, ty_args)) => {
            head == ty_head
                && args.len() == ty_args.len()
                && args
                    .iter()
                    .zip(ty_args)
                    .all(|(a, b)| match_target(a, b, bindings))
        }
        (
            SolverType::Ref { is_mut, inner },
            SolverType::Ref {
                is_mut: ty_mut,
                inner: ty_inner,
            },
        ) => is_mut == ty_mut && match_target(inner, ty_inner, bindings),
        (SolverType::Tuple(elems), SolverType::Tuple(ty_elems)) => {
            elems.len() == ty_elems.len()
                && elems
                    .iter()
                    .zip(ty_elems)
                    .all(|(a, b)| match_target(a, b, bindings))
        }
        // A pack matches a whole tuple, and only `candidates` needs what it
        // bound; a bound on a pack is checked at monomorphization
        // (WEP 2026-03-14 §5), never here.
        (SolverType::Pack(_), SolverType::Tuple(_)) => true,
        (
            SolverType::Decl(_, _)
            | SolverType::Ref { .. }
            | SolverType::Tuple(_)
            | SolverType::Pack(_),
            SolverType::Decl(_, _)
            | SolverType::Param(_)
            | SolverType::Pack(_)
            | SolverType::Ref { .. }
            | SolverType::Tuple(_),
        ) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::program::{Fact, ImplId, ParamDef, TraitDef, TypeDeclId};
    use super::*;

    const ALPHA: TraitDeclId = TraitDeclId(0);
    const BETA: TraitDeclId = TraitDeclId(1);
    const BASE: TraitDeclId = TraitDeclId(2);
    const SUB: TraitDeclId = TraitDeclId(3);
    const EQ: TraitDeclId = TraitDeclId(4);
    const POINT: TypeDeclId = TypeDeclId(0);
    const LIST: TypeDeclId = TypeDeclId(1);
    const I32: TypeDeclId = TypeDeclId(2);
    const HERE: ModuleId = ModuleId(0);
    const ELSEWHERE: ModuleId = ModuleId(1);

    fn decl(id: TypeDeclId) -> SolverType {
        SolverType::Decl(id, vec![])
    }

    fn list_of(inner: SolverType) -> SolverType {
        SolverType::Decl(LIST, vec![inner])
    }

    #[derive(Default)]
    struct Builder {
        program: Program,
        next: u32,
    }

    impl Builder {
        fn impl_(mut self, def: ImplDef) -> Self {
            self.program.add_impl(ImplId(self.next), def);
            self.next += 1;
            self
        }

        fn concrete(self, trait_: TraitDeclId, target: SolverType) -> Self {
            self.impl_(ImplDef {
                trait_: Some(trait_),
                trait_args: vec![],
                target,
                params: vec![],
                is_derivation_request: false,
            })
        }

        /// `impl<T: bound> trait_ for target`, where `target` mentions `T` as
        /// parameter 0.
        fn bounded(
            self,
            trait_: TraitDeclId,
            target: SolverType,
            bounds: Vec<TraitDeclId>,
        ) -> Self {
            self.impl_(ImplDef {
                trait_: Some(trait_),
                trait_args: vec![],
                target,
                params: vec![ParamDef { bounds }],
                is_derivation_request: false,
            })
        }

        fn fact(mut self, ty: SolverType, trait_: TraitDeclId, fact: Fact) -> Self {
            self.program.facts.insert((ty, trait_), fact);
            self
        }

        fn supertrait(mut self, sub: TraitDeclId, super_: TraitDeclId) -> Self {
            self.program.traits.insert(
                sub,
                TraitDef {
                    supertraits: vec![super_],
                },
            );
            self
        }

        fn build(self) -> Program {
            self.program
        }
    }

    fn env(bounds: Vec<Vec<TraitDeclId>>) -> Env {
        Env {
            param_bounds: bounds,
        }
    }

    #[test]
    fn nothing_holds_in_an_empty_program() {
        let p = Program::new();
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), ALPHA, HERE), None);
    }

    #[test]
    fn an_impl_written_for_the_type_answers() {
        let p = Builder::default().concrete(ALPHA, decl(POINT)).build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), ALPHA, HERE),
            Some(Holds::default())
        );
        assert_eq!(holds(&p, &Env::default(), &decl(I32), ALPHA, HERE), None);
    }

    /// A bound naming a subtrait answers for its supertraits: implementing
    /// `Sub` is implementing `Base`.
    #[test]
    fn an_impl_of_a_subtrait_answers_its_supertrait() {
        let p = Builder::default()
            .supertrait(SUB, BASE)
            .concrete(SUB, decl(POINT))
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), BASE, HERE),
            Some(Holds::default())
        );
    }

    /// A generic body's parameter holds by its own signature, not by any impl.
    #[test]
    fn a_bound_in_force_answers_for_a_parameter() {
        let p = Program::new();
        let e = env(vec![vec![ALPHA]]);
        assert_eq!(
            holds(&p, &e, &SolverType::Param(0), ALPHA, HERE),
            Some(Holds::default())
        );
        assert_eq!(holds(&p, &e, &SolverType::Param(0), BETA, HERE), None);
    }

    #[test]
    fn a_bound_in_force_answers_for_its_supertraits() {
        let p = Builder::default().supertrait(SUB, BASE).build();
        assert_eq!(
            holds(&p, &env(vec![vec![SUB]]), &SolverType::Param(0), BASE, HERE),
            Some(Holds::default())
        );
    }

    /// The ways a bound holds that are properties of the type arrive as facts,
    /// and a fact carries the body its answer owes.
    #[test]
    fn a_fact_answers_and_carries_its_request() {
        let request = DerivationRequest {
            ty: decl(POINT),
            trait_: EQ,
        };
        let p = Builder::default()
            .fact(
                decl(POINT),
                EQ,
                Fact {
                    visible_from: None,
                    requests: vec![request.clone()],
                },
            )
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), EQ, HERE),
            Some(Holds {
                requests: vec![request]
            })
        );
    }

    /// A `Reflect*` bound holds only where the receiver's members are visible,
    /// so the fact names the modules it holds in and the asking module decides.
    #[test]
    fn a_fact_restricted_to_a_module_answers_only_there() {
        let p = Builder::default()
            .fact(
                decl(POINT),
                ALPHA,
                Fact {
                    visible_from: Some(vec![HERE]),
                    requests: vec![],
                },
            )
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), ALPHA, HERE),
            Some(Holds::default())
        );
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), ALPHA, ELSEWHERE),
            None
        );
    }

    /// `impl<T: Alpha> Beta for T` answers `Point: Beta` exactly when
    /// `Point: Alpha` does.
    #[test]
    fn a_blanket_answers_through_its_bound() {
        let p = Builder::default()
            .bounded(BETA, SolverType::Param(0), vec![ALPHA])
            .build();
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), BETA, HERE), None);

        let p = Builder::default()
            .bounded(BETA, SolverType::Param(0), vec![ALPHA])
            .concrete(ALPHA, decl(POINT))
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), BETA, HERE),
            Some(Holds::default())
        );
    }

    /// `impl<T: Eq> Eq for List<T>` descends into the type argument the match
    /// bound, so the question changes subject and terminates.
    #[test]
    fn a_bounded_head_impl_asks_about_its_argument() {
        let p = Builder::default()
            .bounded(EQ, list_of(SolverType::Param(0)), vec![EQ])
            .fact(
                decl(I32),
                EQ,
                Fact {
                    visible_from: None,
                    requests: vec![],
                },
            )
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &list_of(decl(I32)), EQ, HERE),
            Some(Holds::default())
        );
        assert_eq!(
            holds(&p, &Env::default(), &list_of(decl(POINT)), EQ, HERE),
            None
        );
    }

    /// Nested, so the recursion is more than one step deep.
    #[test]
    fn a_bounded_head_impl_descends_as_far_as_it_needs() {
        let p = Builder::default()
            .bounded(EQ, list_of(SolverType::Param(0)), vec![EQ])
            .fact(
                decl(I32),
                EQ,
                Fact {
                    visible_from: None,
                    requests: vec![],
                },
            )
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &list_of(list_of(decl(I32))), EQ, HERE),
            Some(Holds::default())
        );
    }

    /// The gap this design closes: `impl<T: Alpha> Beta for T` beside
    /// `impl<T: Beta> Alpha for T` grounds neither trait. A walk that answered
    /// yes to the repeated goal would make every type in the program satisfy
    /// both.
    #[test]
    fn an_ungrounded_cycle_holds_of_nothing() {
        let p = Builder::default()
            .bounded(BETA, SolverType::Param(0), vec![ALPHA])
            .bounded(ALPHA, SolverType::Param(0), vec![BETA])
            .build();
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), ALPHA, HERE), None);
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), BETA, HERE), None);
    }

    /// A cycle grounded at one type holds there and nowhere else.
    #[test]
    fn a_cycle_grounded_by_an_impl_holds_where_it_is_grounded() {
        let p = Builder::default()
            .bounded(BETA, SolverType::Param(0), vec![ALPHA])
            .bounded(ALPHA, SolverType::Param(0), vec![BETA])
            .concrete(ALPHA, decl(POINT))
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), BETA, HERE),
            Some(Holds::default())
        );
        assert_eq!(holds(&p, &Env::default(), &decl(I32), BETA, HERE), None);
    }

    /// A blanket keyed on the trait it implements is its own cycle.
    #[test]
    fn a_blanket_bounded_by_its_own_trait_holds_of_nothing() {
        let p = Builder::default()
            .bounded(ALPHA, SolverType::Param(0), vec![ALPHA])
            .build();
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), ALPHA, HERE), None);
    }

    /// A request found behind a blanket reaches the caller: the body is owed
    /// whether the answer came directly or through a chain.
    #[test]
    fn a_request_travels_out_through_a_blanket() {
        let request = DerivationRequest {
            ty: decl(POINT),
            trait_: EQ,
        };
        let p = Builder::default()
            .bounded(BETA, SolverType::Param(0), vec![EQ])
            .fact(
                decl(POINT),
                EQ,
                Fact {
                    visible_from: None,
                    requests: vec![request.clone()],
                },
            )
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), BETA, HERE),
            Some(Holds {
                requests: vec![request]
            })
        );
    }

    /// One parameter at two positions must be one type.
    #[test]
    fn a_repeated_parameter_must_match_one_type() {
        let pair = |a: SolverType, b: SolverType| SolverType::Decl(LIST, vec![a, b]);
        let p = Builder::default()
            .impl_(ImplDef {
                trait_: Some(ALPHA),
                trait_args: vec![],
                target: pair(SolverType::Param(0), SolverType::Param(0)),
                params: vec![ParamDef::default()],
                is_derivation_request: false,
            })
            .build();
        assert_eq!(
            holds(
                &p,
                &Env::default(),
                &pair(decl(I32), decl(I32)),
                ALPHA,
                HERE
            ),
            Some(Holds::default())
        );
        assert_eq!(
            holds(
                &p,
                &Env::default(),
                &pair(decl(I32), decl(POINT)),
                ALPHA,
                HERE
            ),
            None
        );
    }

    #[test]
    fn a_reference_impl_answers_only_for_a_reference() {
        let ref_of = |inner: SolverType, is_mut: bool| SolverType::Ref {
            is_mut,
            inner: Box::new(inner),
        };
        let p = Builder::default()
            .bounded(ALPHA, ref_of(SolverType::Param(0), false), vec![])
            .build();
        assert_eq!(
            holds(
                &p,
                &Env::default(),
                &ref_of(decl(POINT), false),
                ALPHA,
                HERE
            ),
            Some(Holds::default())
        );
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), ALPHA, HERE), None);
        assert_eq!(
            holds(&p, &Env::default(), &ref_of(decl(POINT), true), ALPHA, HERE),
            None
        );
    }

    /// `impl Serialize for Handler;` is written to make the pair exist where no
    /// bound would have asked for it (WEP 2026-06-25), so it answers, and the
    /// body it asks for travels out with the answer.
    #[test]
    fn a_derivation_request_answers_and_owes_the_body() {
        let p = Builder::default()
            .impl_(ImplDef {
                trait_: Some(EQ),
                trait_args: vec![],
                target: decl(POINT),
                params: vec![],
                is_derivation_request: true,
            })
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), EQ, HERE),
            Some(Holds {
                requests: vec![DerivationRequest {
                    ty: decl(POINT),
                    trait_: EQ,
                }]
            })
        );
    }
}
