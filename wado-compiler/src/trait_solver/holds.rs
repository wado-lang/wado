//! Does this type satisfy this trait? Every other question is asked on top of
//! this one, and every step it takes is through an impl's bounds.

use super::program::{
    AssocId, DerivationRequest, Env, ImplDef, ImplId, ImplOrigin, ModuleId, Program, RefRule,
    SolverType, TraitDeclId,
};

/// A bound that holds, and the bodies its answer owes.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Holds {
    pub requests: Vec<DerivationRequest>,
    /// The associated types the answering impl binds, at the receiver; empty
    /// where no impl answered.
    pub assoc: Vec<(AssocId, SolverType)>,
}

/// Whether `ty` satisfies `trait_`, asked from `scope`. `None` is "does not
/// hold", which is also the answer to a question reaching itself through bounds.
#[must_use]
pub fn holds(
    program: &Program,
    env: &Env,
    ty: &SolverType,
    trait_: TraitDeclId,
    scope: ModuleId,
) -> Option<Holds> {
    Query {
        program,
        env,
        scope,
        asking: Vec::new(),
        at_itself: None,
    }
    .holds(ty, trait_)
}

/// One question and the questions open under it.
struct Query<'a> {
    program: &'a Program,
    env: &'a Env,
    scope: ModuleId,
    asking: Vec<(SolverType, TraitDeclId)>,
    /// A type this query answers about at the type itself, without inheriting
    /// its newtype base's impls. Selection asks this way so that a blanket
    /// whose bound only the base carries is a candidate at the base's level and
    /// not at the newtype's — rank 1's question
    /// (`docs/wep-2026-09-01-trait-resolution.md`). The subject stays the same
    /// through a chained blanket's bounds, so the restriction travels with it.
    at_itself: Option<SolverType>,
}

impl Query<'_> {
    fn holds(&mut self, ty: &SolverType, trait_: TraitDeclId) -> Option<Holds> {
        if self.asking.iter().any(|(t, tr)| t == ty && *tr == trait_) {
            return None;
        }
        let program = self.program;
        let trait_def = program.traits.get(&trait_);
        if trait_def.is_some_and(|def| def.holds_for_all) {
            return Some(Holds::default());
        }
        let on_ref = trait_def.map_or(RefRule::default(), |def| def.on_ref);
        if matches!(ty, SolverType::Ref { .. }) && on_ref == RefRule::Always {
            return Some(Holds::default());
        }
        if let SolverType::Param(index) = ty
            && let Some(bounds) = self.env.param_bounds.get(*index as usize)
            && bounds
                .iter()
                .any(|&bound| program.bound_reaches(bound, trait_))
        {
            return Some(Holds::default());
        }
        if let SolverType::Decl(head, _) = ty
            && let Some(fact) = program.facts.get(&(*head, trait_))
            && fact
                .visible_from
                .as_ref()
                .is_none_or(|modules| modules.contains(&self.scope))
        {
            return Some(Holds {
                requests: vec![DerivationRequest {
                    ty: ty.clone(),
                    trait_,
                }],
                ..Holds::default()
            });
        }

        self.asking.push((ty.clone(), trait_));
        let answer = program
            .impls
            .iter()
            .find_map(|(&id, def)| {
                // A bound spells no arguments, so only an impl at the trait's
                // defaults answers one. Selection asks without this gate.
                let implemented = def.trait_?;
                if !program.bound_reaches(implemented, trait_) || !restates_defaults(program, def) {
                    return None;
                }
                Some(self.impl_answers(id, def, ty)?.holds)
            })
            .or_else(|| {
                if self.at_itself.as_ref() == Some(ty) {
                    return None;
                }
                let base = newtype_base(program, ty)?;
                self.holds(&base, trait_)
            })
            .or_else(|| {
                let SolverType::Ref { inner, .. } = ty else {
                    return None;
                };
                match on_ref {
                    RefRule::Inherits => self.holds(inner, trait_),
                    RefRule::Always | RefRule::Never => None,
                }
            });
        self.asking.pop();
        answer
    }

    /// Whether one impl applies to `ty`: its target matches, and every bound
    /// holds of what the match bound.
    fn impl_answers(&mut self, id: ImplId, def: &ImplDef, ty: &SolverType) -> Option<Answer> {
        let program = self.program;
        let implemented = def.trait_?;
        // A value blanket answers no reference; `&T` reaches it through the
        // pointee.
        if matches!(def.target, SolverType::Param(_)) && matches!(ty, SolverType::Ref { .. }) {
            return None;
        }
        let mut bindings: Vec<Option<Binding>> = vec![None; def.params.len()];
        if !match_target(&def.target, ty, &mut bindings) {
            return None;
        }
        let bound_to = |ty: &SolverType| {
            ty.map_params(&|i| bindings.get(i as usize)?.as_ref().map(Binding::as_type))
        };
        let mut requests = match def.origin {
            ImplOrigin::Written => Vec::new(),
            ImplOrigin::Derived | ImplOrigin::Marker => vec![DerivationRequest {
                ty: ty.clone(),
                trait_: implemented,
            }],
        };
        let pinned = |index: u32| {
            def.params
                .iter()
                .flat_map(|p| &p.pins)
                .any(|pin| pin.ty.mentions_param(index))
        };
        for (index, param) in def.params.iter().enumerate() {
            // A parameter only a pin supplies is bound at monomorphization,
            // and so is its bound (WEP 2026-03-14).
            let Some(binding) = bindings[index].as_ref() else {
                if param.bounds.is_empty() || pinned(index as u32) {
                    continue;
                }
                return None;
            };
            let elements: Vec<&SolverType> = match binding {
                Binding::One(ty) => vec![ty],
                Binding::Pack(elems) => elems.iter().collect(),
            };
            for &bound in &param.bounds {
                for element in &elements {
                    let answer = self.holds(element, bound)?;
                    // An impl binding the pinned assoc otherwise is refuted;
                    // one binding nothing is not.
                    for pin in param.pins.iter().filter(|pin| pin.trait_ == bound) {
                        let Some(expected) = bound_to(&pin.ty) else {
                            continue;
                        };
                        let actual = answer.assoc.iter().find(|(assoc, _)| *assoc == pin.assoc);
                        if actual.is_some_and(|(_, actual)| *actual != expected) {
                            return None;
                        }
                    }
                    requests.extend(answer.requests);
                }
            }
        }
        let assoc = program
            .assoc_bindings
            .get(&id)
            .into_iter()
            .flatten()
            .filter_map(|(assoc, binding)| Some((*assoc, bound_to(binding)?)))
            .collect();
        Some(Answer {
            holds: Holds { requests, assoc },
            trait_args: def
                .trait_args
                .iter()
                .map(|arg| bound_to(arg).unwrap_or_else(|| arg.clone()))
                .collect(),
        })
    }
}

/// One impl applying to one type. `holds` reads the bound it answers; selection
/// reads the arguments it answers at.
struct Answer {
    holds: Holds,
    /// The impl's trait arguments at the type it matched, so two impls of one
    /// trait compare on what the call would get rather than on how each spelled
    /// it.
    trait_args: Vec<SolverType>,
}

/// The trait arguments `def` applies to `ty` at, or `None` where it does not
/// apply. This is selection's question, and it differs from a bound's twice: it
/// names the trait, so it accepts an impl at any argument list
/// (WEP 2026-07-31); and it asks about `ty` at the type itself, so a blanket
/// whose bound only `ty`'s newtype base carries does not apply here — the chain
/// reaches that blanket at the base's own level, which is the depth rank 1
/// wants.
pub(super) fn impl_applies(
    program: &Program,
    env: &Env,
    scope: ModuleId,
    id: ImplId,
    def: &ImplDef,
    ty: &SolverType,
) -> Option<Vec<SolverType>> {
    Some(
        Query {
            program,
            env,
            scope,
            asking: Vec::new(),
            at_itself: Some(ty.clone()),
        }
        .impl_answers(id, def, ty)?
        .trait_args,
    )
}

/// What a target parameter matched: a pack takes every element past the
/// tuple's fixed prefix, and its bound holds of each.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Binding {
    One(SolverType),
    Pack(Vec<SolverType>),
}

impl Binding {
    fn as_type(&self) -> SolverType {
        match self {
            Self::One(ty) => ty.clone(),
            Self::Pack(elems) => SolverType::Tuple(elems.clone()),
        }
    }
}

/// The base a newtype receiver inherits from, at the receiver's own type
/// arguments.
fn newtype_base(program: &Program, ty: &SolverType) -> Option<SolverType> {
    let SolverType::Decl(head, args) = ty else {
        return None;
    };
    let base = program.types.get(head)?.newtype_base.as_ref()?;
    Some(
        base.map_params(&|i| args.get(i as usize).cloned())
            .unwrap_or_else(|| panic!("{base:?} mentions a parameter {ty:?} has no argument for")),
    )
}

/// Whether the impl's written trait arguments say what the trait's defaults
/// do: a bound spells no arguments, so `impl Mul<Inch> for Cm` answers no
/// `T: Mul`.
fn restates_defaults(program: &Program, def: &ImplDef) -> bool {
    def.trait_args.iter().enumerate().all(|(i, arg)| {
        program
            .default_arg(def, i)
            .is_none_or(|default| default == *arg)
    })
}

/// Match an impl target against a type, binding the target's parameters by
/// position.
fn match_target(target: &SolverType, ty: &SolverType, bindings: &mut [Option<Binding>]) -> bool {
    match (target, ty) {
        (SolverType::Param(index), _) => {
            let slot = &mut bindings[*index as usize];
            if let Some(bound) = slot {
                return *bound == Binding::One(ty.clone());
            }
            *slot = Some(Binding::One(ty.clone()));
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
        (SolverType::Tuple(elems), SolverType::Tuple(ty_elems)) => match elems.split_last() {
            Some((SolverType::Pack(index), prefix)) => {
                ty_elems.len() >= prefix.len()
                    && prefix
                        .iter()
                        .zip(ty_elems)
                        .all(|(a, b)| match_target(a, b, bindings))
                    && {
                        bindings[*index as usize] =
                            Some(Binding::Pack(ty_elems[prefix.len()..].to_vec()));
                        true
                    }
            }
            _ => {
                elems.len() == ty_elems.len()
                    && elems
                        .iter()
                        .zip(ty_elems)
                        .all(|(a, b)| match_target(a, b, bindings))
            }
        },
        (SolverType::Pack(index), SolverType::Tuple(ty_elems)) => {
            bindings[*index as usize] = Some(Binding::Pack(ty_elems.clone()));
            true
        }
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
    use super::super::program::{ArgDefault, Fact, ParamDef, Pin, TraitDef, TypeDeclId, TypeDef};
    use super::super::testing::{Builder, concrete, decl, ref_to};
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

    fn list_of(inner: SolverType) -> SolverType {
        SolverType::Decl(LIST, vec![inner])
    }

    fn env(bounds: Vec<Vec<TraitDeclId>>) -> Env {
        Env {
            param_bounds: bounds,
        }
    }

    #[test]
    fn nothing_holds_in_an_empty_program() {
        let p = Program::default();
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
        let p = Program::default();
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
    /// and a fact's answer owes the body, as a derived impl's does.
    #[test]
    fn a_fact_answers_and_owes_the_body() {
        let p = Builder::default()
            .fact(POINT, EQ, Fact { visible_from: None })
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), EQ, HERE),
            Some(Holds {
                requests: vec![DerivationRequest {
                    ty: decl(POINT),
                    trait_: EQ,
                }],
                ..Holds::default()
            })
        );
        assert_eq!(holds(&p, &Env::default(), &decl(I32), EQ, HERE), None);
    }

    /// A fact is stated of a declaration and answers for every instance:
    /// `Pair<String>` is a struct because `Pair` is.
    #[test]
    fn a_fact_answers_for_every_instance_of_its_declaration() {
        let p = Builder::default()
            .fact(LIST, ALPHA, Fact { visible_from: None })
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &list_of(decl(I32)), ALPHA, HERE),
            Some(Holds {
                requests: vec![DerivationRequest {
                    ty: list_of(decl(I32)),
                    trait_: ALPHA,
                }],
                ..Holds::default()
            })
        );
    }

    /// A `Reflect*` bound holds only where the receiver's members are visible,
    /// so the fact names the modules it holds in and the asking module decides.
    #[test]
    fn a_fact_restricted_to_a_module_answers_only_there() {
        let p = Builder::default()
            .fact(
                POINT,
                ALPHA,
                Fact {
                    visible_from: Some(vec![HERE]),
                },
            )
            .build();
        assert!(holds(&p, &Env::default(), &decl(POINT), ALPHA, HERE).is_some());
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), ALPHA, ELSEWHERE),
            None
        );
    }

    /// `&T` holds a bound `T` holds, by auto-deref at the call — unless the
    /// trait says otherwise: `Eq` holds of every reference by identity, and
    /// `Ord` of none. A reference's own impl is asked first.
    #[test]
    fn a_reference_answers_by_the_trait_s_reference_rule() {
        let mut p = Builder::default()
            .concrete(ALPHA, decl(POINT))
            .concrete(BETA, decl(POINT))
            .concrete(EQ, decl(POINT))
            .concrete(SUB, ref_to(decl(I32)))
            .build();
        p.traits.insert(
            BETA,
            TraitDef {
                on_ref: RefRule::Never,
                ..TraitDef::default()
            },
        );
        p.traits.insert(
            EQ,
            TraitDef {
                on_ref: RefRule::Always,
                ..TraitDef::default()
            },
        );
        let ask =
            |ty: SolverType, trait_: TraitDeclId| holds(&p, &Env::default(), &ty, trait_, HERE);
        assert_eq!(ask(ref_to(decl(POINT)), ALPHA), Some(Holds::default()));
        assert_eq!(ask(ref_to(decl(I32)), ALPHA), None);
        assert_eq!(ask(ref_to(decl(POINT)), BETA), None);
        assert_eq!(ask(ref_to(decl(I32)), EQ), Some(Holds::default()));
        assert_eq!(ask(ref_to(decl(I32)), SUB), Some(Holds::default()));
        assert_eq!(ask(decl(I32), SUB), None);
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
            .concrete(EQ, decl(I32))
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
            .concrete(EQ, decl(I32))
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &list_of(list_of(decl(I32))), EQ, HERE),
            Some(Holds::default())
        );
    }

    /// `impl<T: Alpha> Beta for T` beside `impl<T: Beta> Alpha for T` grounds
    /// neither trait; answering yes to the repeated goal would ground both.
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
            .fact(POINT, EQ, Fact { visible_from: None })
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), BETA, HERE),
            Some(Holds {
                requests: vec![request],
                ..Holds::default()
            })
        );
    }

    /// One parameter at two positions must be one type.
    #[test]
    fn a_repeated_parameter_must_match_one_type() {
        let pair = |a: SolverType, b: SolverType| SolverType::Decl(LIST, vec![a, b]);
        let p = Builder::default()
            .bounded(
                ALPHA,
                pair(SolverType::Param(0), SolverType::Param(0)),
                vec![],
            )
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
        let p = Builder::default()
            .bounded(ALPHA, ref_to(SolverType::Param(0)), vec![])
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &ref_to(decl(POINT)), ALPHA, HERE),
            Some(Holds::default())
        );
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), ALPHA, HERE), None);
        let mut_ref = SolverType::Ref {
            is_mut: true,
            inner: Box::new(decl(POINT)),
        };
        assert_eq!(holds(&p, &Env::default(), &mut_ref, ALPHA, HERE), None);
    }

    /// `impl Serialize for Handler;` is written to make the pair exist where no
    /// bound would have asked for it (WEP 2026-06-25), so it answers, and the
    /// body it asks for travels out with the answer.
    #[test]
    fn a_derivation_request_answers_and_owes_the_body() {
        let p = Builder::default()
            .impl_(ImplDef {
                origin: ImplOrigin::Marker,
                ..concrete(EQ, decl(POINT))
            })
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), EQ, HERE),
            Some(Holds {
                requests: vec![DerivationRequest {
                    ty: decl(POINT),
                    trait_: EQ,
                }],
                ..Holds::default()
            })
        );
    }

    /// `Inspect` holds of everything before any body exists. The unbounded
    /// blanket that would say so is rejected, so the trait says it itself.
    #[test]
    fn a_trait_that_holds_for_all_answers_for_anything() {
        let mut p = Program::default();
        p.traits.insert(
            ALPHA,
            TraitDef {
                holds_for_all: true,
                ..TraitDef::default()
            },
        );
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), ALPHA, HERE),
            Some(Holds::default())
        );
        assert_eq!(
            holds(&p, &Env::default(), &SolverType::Param(3), ALPHA, HERE),
            Some(Holds::default())
        );
    }

    /// `type Duration = u64`: the newtype has no impl of its own, so its base
    /// answers (WEP 2026-01-29).
    #[test]
    fn a_newtype_inherits_its_base_impls() {
        const DURATION: TypeDeclId = TypeDeclId(9);
        let mut p = Builder::default().concrete(ALPHA, decl(I32)).build();
        p.types.insert(
            DURATION,
            TypeDef {
                newtype_base: Some(decl(I32)),
            },
        );
        assert_eq!(
            holds(&p, &Env::default(), &decl(DURATION), ALPHA, HERE),
            Some(Holds::default())
        );
    }

    /// `type MyList<T> = List<T>` inherits at its own arguments.
    #[test]
    fn a_generic_newtype_inherits_at_its_arguments() {
        const MY_LIST: TypeDeclId = TypeDeclId(9);
        let mut p = Builder::default()
            .bounded(EQ, list_of(SolverType::Param(0)), vec![EQ])
            .concrete(EQ, decl(I32))
            .build();
        p.types.insert(
            MY_LIST,
            TypeDef {
                newtype_base: Some(list_of(SolverType::Param(0))),
            },
        );
        assert_eq!(
            holds(
                &p,
                &Env::default(),
                &SolverType::Decl(MY_LIST, vec![decl(I32)]),
                EQ,
                HERE
            ),
            Some(Holds::default())
        );
        assert_eq!(
            holds(
                &p,
                &Env::default(),
                &SolverType::Decl(MY_LIST, vec![decl(POINT)]),
                EQ,
                HERE
            ),
            None
        );
    }

    /// The newtype's own impl answers before the base's is asked.
    #[test]
    fn a_newtype_own_impl_answers_first() {
        const DURATION: TypeDeclId = TypeDeclId(9);
        let mut p = Builder::default()
            .concrete(ALPHA, decl(I32))
            .impl_(ImplDef {
                origin: ImplOrigin::Marker,
                ..concrete(ALPHA, decl(DURATION))
            })
            .build();
        p.types.insert(
            DURATION,
            TypeDef {
                newtype_base: Some(decl(I32)),
            },
        );
        // Answered through the marker, so it owes the body; through the base
        // it would owe nothing.
        assert_eq!(
            holds(&p, &Env::default(), &decl(DURATION), ALPHA, HERE),
            Some(Holds {
                requests: vec![DerivationRequest {
                    ty: decl(DURATION),
                    trait_: ALPHA,
                }],
                ..Holds::default()
            })
        );
    }

    /// `impl<..T> Alpha for (..T)` answers for every tuple, the empty tuple
    /// `[]` included: the pack takes whatever the tuple has.
    #[test]
    fn a_pack_target_matches_a_tuple_of_any_arity() {
        let p = Builder::default()
            .bounded(ALPHA, SolverType::Tuple(vec![SolverType::Pack(0)]), vec![])
            .build();
        for tuple in [
            SolverType::Tuple(vec![]),
            SolverType::Tuple(vec![decl(I32)]),
            SolverType::Tuple(vec![decl(I32), decl(POINT)]),
        ] {
            assert_eq!(
                holds(&p, &Env::default(), &tuple, ALPHA, HERE),
                Some(Holds::default()),
                "{tuple:?}"
            );
        }
        assert_eq!(holds(&p, &Env::default(), &decl(I32), ALPHA, HERE), None);
    }

    /// `impl<A, ..T> Alpha for (A, ..T)`: the fixed prefix must be there, and
    /// the pack takes the rest.
    #[test]
    fn a_pack_after_a_prefix_needs_the_prefix() {
        let p = Builder::default()
            .impl_(ImplDef {
                params: vec![ParamDef::bounded(vec![BETA]), ParamDef::default()],
                ..concrete(
                    ALPHA,
                    SolverType::Tuple(vec![SolverType::Param(0), SolverType::Pack(1)]),
                )
            })
            .concrete(BETA, decl(I32))
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &SolverType::Tuple(vec![]), ALPHA, HERE),
            None
        );
        assert_eq!(
            holds(
                &p,
                &Env::default(),
                &SolverType::Tuple(vec![decl(I32)]),
                ALPHA,
                HERE
            ),
            Some(Holds::default())
        );
        assert_eq!(
            holds(
                &p,
                &Env::default(),
                &SolverType::Tuple(vec![decl(I32), decl(POINT), decl(POINT)]),
                ALPHA,
                HERE
            ),
            Some(Holds::default())
        );
        // The prefix parameter's bound is checked: `Point` is not `Beta`.
        assert_eq!(
            holds(
                &p,
                &Env::default(),
                &SolverType::Tuple(vec![decl(POINT)]),
                ALPHA,
                HERE
            ),
            None
        );
    }

    /// `impl<..T: Beta> Alpha for (..T)`: a bound on the pack holds of each
    /// element the pack took, so one element that is not `Beta` refuses the
    /// tuple, and the empty tuple has nothing to refuse it.
    #[test]
    fn a_bound_on_a_pack_holds_of_every_element() {
        let p = Builder::default()
            .bounded(
                ALPHA,
                SolverType::Tuple(vec![SolverType::Pack(0)]),
                vec![BETA],
            )
            .concrete(BETA, decl(I32))
            .build();
        let ask = |elems: Vec<SolverType>| {
            holds(&p, &Env::default(), &SolverType::Tuple(elems), ALPHA, HERE)
        };
        assert_eq!(ask(vec![]), Some(Holds::default()));
        assert_eq!(ask(vec![decl(I32), decl(I32)]), Some(Holds::default()));
        assert_eq!(ask(vec![decl(I32), decl(POINT)]), None);
    }

    /// `trait Mul<Rhs = Self>`: a bound `T: Mul` asks for `Mul<T>`. An impl
    /// restating the default answers, written or elided; one of another
    /// instantiation does not (`error_bound_needs_default_instantiation`).
    #[test]
    fn an_impl_answers_a_bare_bound_only_at_the_trait_s_defaults() {
        const MUL: TraitDeclId = TraitDeclId(9);
        const CM: TypeDeclId = TypeDeclId(10);
        const INCH: TypeDeclId = TypeDeclId(11);
        let mul = |target: TypeDeclId, args: Vec<SolverType>| ImplDef {
            trait_args: args,
            ..concrete(MUL, decl(target))
        };
        let mut p = Builder::default()
            .impl_(mul(CM, vec![decl(INCH)]))
            .impl_(mul(INCH, vec![decl(INCH)]))
            .impl_(mul(POINT, vec![]))
            .build();
        p.traits.insert(
            MUL,
            TraitDef {
                arg_defaults: vec![Some(ArgDefault::SelfType)],
                ..TraitDef::default()
            },
        );
        assert_eq!(holds(&p, &Env::default(), &decl(CM), MUL, HERE), None);
        assert_eq!(
            holds(&p, &Env::default(), &decl(INCH), MUL, HERE),
            Some(Holds::default())
        );
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), MUL, HERE),
            Some(Holds::default())
        );
    }

    /// `impl<T: Mul<Output = T>> Product for T`: the impl answering `T: Mul`
    /// must bind `Output` to `T` itself. One binding it to something else
    /// refuses the blanket; one binding nothing is not refuted.
    #[test]
    fn a_pin_on_a_bound_needs_the_answering_impl_to_bind_it_so() {
        const MUL: TraitDeclId = TraitDeclId(9);
        const PRODUCT: TraitDeclId = TraitDeclId(10);
        const OUTPUT: AssocId = AssocId(0);
        const CM: TypeDeclId = TypeDeclId(10);
        const AREA: TypeDeclId = TypeDeclId(11);
        const INCH: TypeDeclId = TypeDeclId(12);
        let mut p = Builder::default()
            .impl_(ImplDef {
                params: vec![ParamDef {
                    bounds: vec![MUL],
                    pins: vec![Pin {
                        trait_: MUL,
                        assoc: OUTPUT,
                        ty: SolverType::Param(0),
                    }],
                }],
                ..concrete(PRODUCT, SolverType::Param(0))
            })
            .concrete(MUL, decl(CM))
            .concrete(MUL, decl(INCH))
            .concrete(MUL, decl(POINT))
            .build();
        p.assoc_bindings
            .insert(ImplId(1), vec![(OUTPUT, decl(AREA))]);
        p.assoc_bindings
            .insert(ImplId(2), vec![(OUTPUT, decl(INCH))]);
        assert_eq!(holds(&p, &Env::default(), &decl(CM), PRODUCT, HERE), None);
        assert_eq!(
            holds(&p, &Env::default(), &decl(INCH), PRODUCT, HERE),
            Some(Holds::default())
        );
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), PRODUCT, HERE),
            Some(Holds::default())
        );
        // The answer reports what the impl binds, at the receiver.
        assert_eq!(
            holds(&p, &Env::default(), &decl(CM), MUL, HERE),
            Some(Holds {
                assoc: vec![(OUTPUT, decl(AREA))],
                ..Holds::default()
            })
        );
    }

    /// `impl<T: Mul<Output = U>, U> Product for T`: `U` is read off the
    /// projection, not checked against it, so the blanket answers whatever
    /// `Output` is.
    #[test]
    fn a_pin_naming_an_unbound_parameter_reads_the_projection() {
        const MUL: TraitDeclId = TraitDeclId(9);
        const PRODUCT: TraitDeclId = TraitDeclId(10);
        const OUTPUT: AssocId = AssocId(0);
        const CM: TypeDeclId = TypeDeclId(10);
        const AREA: TypeDeclId = TypeDeclId(11);
        let mut p = Builder::default()
            .impl_(ImplDef {
                params: vec![
                    ParamDef {
                        bounds: vec![MUL],
                        pins: vec![Pin {
                            trait_: MUL,
                            assoc: OUTPUT,
                            ty: SolverType::Param(1),
                        }],
                    },
                    ParamDef::default(),
                ],
                ..concrete(PRODUCT, SolverType::Param(0))
            })
            .concrete(MUL, decl(CM))
            .build();
        p.assoc_bindings
            .insert(ImplId(1), vec![(OUTPUT, decl(AREA))]);
        assert_eq!(
            holds(&p, &Env::default(), &decl(CM), PRODUCT, HERE),
            Some(Holds::default())
        );
    }

    /// A derived `impl Sub for Point` answering `Point: Base` owes the `Sub`
    /// body, so the request names the impl's trait rather than the bound's.
    #[test]
    fn a_request_names_the_answering_impl_s_trait() {
        let p = Builder::default()
            .supertrait(SUB, BASE)
            .impl_(ImplDef {
                origin: ImplOrigin::Derived,
                ..concrete(SUB, decl(POINT))
            })
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), BASE, HERE),
            Some(Holds {
                requests: vec![DerivationRequest {
                    ty: decl(POINT),
                    trait_: SUB,
                }],
                ..Holds::default()
            })
        );
    }

    /// `impl<T, U: Alpha> Beta for List<T>`: nothing supplies `U`, so its bound
    /// can never be checked and the impl answers for no receiver.
    #[test]
    fn a_bound_on_a_parameter_nothing_supplies_answers_for_no_one() {
        let unsupplied = |bounds: Vec<TraitDeclId>| ImplDef {
            params: vec![ParamDef::default(), ParamDef::bounded(bounds)],
            ..concrete(BETA, list_of(SolverType::Param(0)))
        };
        let p = Builder::default()
            .impl_(unsupplied(vec![ALPHA]))
            .concrete(ALPHA, decl(I32))
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &list_of(decl(I32)), BETA, HERE),
            None
        );
        let p = Builder::default().impl_(unsupplied(vec![])).build();
        assert_eq!(
            holds(&p, &Env::default(), &list_of(decl(I32)), BETA, HERE),
            Some(Holds::default())
        );
    }

    /// `impl<S: ReflectStruct<Fields = [..F]>, ..F: Arbitrary> Arbitrary for S`:
    /// the pack is read off the projection and its bound waits for
    /// monomorphization, so the blanket answers for every struct.
    #[test]
    fn a_bound_on_a_parameter_the_target_never_mentions_is_deferred() {
        const ARBITRARY: TraitDeclId = TraitDeclId(9);
        const FIELDS: AssocId = AssocId(0);
        let p = Builder::default()
            .impl_(ImplDef {
                params: vec![
                    ParamDef {
                        bounds: vec![ALPHA],
                        pins: vec![Pin {
                            trait_: ALPHA,
                            assoc: FIELDS,
                            ty: SolverType::Tuple(vec![SolverType::Pack(1)]),
                        }],
                    },
                    ParamDef::bounded(vec![ARBITRARY]),
                ],
                ..concrete(ARBITRARY, SolverType::Param(0))
            })
            .fact(POINT, ALPHA, Fact { visible_from: None })
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), ARBITRARY, HERE),
            Some(Holds {
                requests: vec![DerivationRequest {
                    ty: decl(POINT),
                    trait_: ALPHA,
                }],
                ..Holds::default()
            })
        );
        assert_eq!(
            holds(&p, &Env::default(), &decl(I32), ARBITRARY, HERE),
            None
        );
    }

    /// `impl<T: Alpha> Beta for T` answers no reference of its own: `&Point`
    /// is `Beta` through `Point` only where `Beta` lets a reference inherit.
    #[test]
    fn a_value_blanket_does_not_answer_for_a_reference() {
        let mut p = Builder::default()
            .bounded(BETA, SolverType::Param(0), vec![ALPHA])
            .concrete(ALPHA, decl(POINT))
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &ref_to(decl(POINT)), BETA, HERE),
            Some(Holds::default())
        );
        p.traits.insert(
            BETA,
            TraitDef {
                on_ref: RefRule::Never,
                ..TraitDef::default()
            },
        );
        assert_eq!(
            holds(&p, &Env::default(), &ref_to(decl(POINT)), BETA, HERE),
            None
        );
    }
}
