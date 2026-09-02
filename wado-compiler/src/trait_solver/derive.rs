//! Derivation as impl generation.
//!
//! A structural trait — `Eq`, `Ord`, `Default`, serde — is not special to
//! [`holds`]: it is answered by impls like any other. What is special is where
//! some of those impls come from. A declaration whose members all satisfy the
//! trait derives it, and deriving it means contributing
//! `impl<Pi: Tr, …> Tr for D<P1..Pn>`, bounded on the parameters its members
//! mention.
//!
//! Whether a declaration derives is decided once, here, over the declarations
//! — finite, and known before any body is elaborated — rather than at each
//! bound over each receiver. The decision assumes every declaration derives,
//! then refutes: a declaration whose members fail under everyone's tentative
//! impl is removed, and the check repeats until none is. Assuming first is what
//! makes a recursive type derive.

use super::holds::holds;
use super::program::{
    Declaration, Env, ImplDef, ImplId, ImplOrigin, ParamDef, Program, SolverType, TraitDeclId,
};

/// What derivation found besides the impls.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DeriveError {
    /// `impl Tr for D;` demands a derivation `D`'s members do not support.
    MarkerNotDerivable { impl_: ImplId },
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Derived {
    /// The impls the declarations derive, in declaration order. Each is a
    /// [`ImplOrigin::Derived`] impl whose target is the declaration at its own
    /// parameters.
    pub impls: Vec<ImplDef>,
    pub errors: Vec<DeriveError>,
}

/// Which of `declarations` derive `trait_` given `program`, and the impls that
/// says.
///
/// A declaration with an impl of `trait_` already — written, or a marker
/// demanding the derivation — derives nothing: the impl it has answers for it.
/// A marker is checked instead, and reported where the members would not
/// support the body it demands.
#[must_use]
pub fn derive(program: &Program, trait_: TraitDeclId, declarations: &[Declaration]) -> Derived {
    let (undecided, markers): (Vec<&Declaration>, Vec<(&Declaration, ImplId)>) = {
        let mut undecided = Vec::new();
        let mut markers = Vec::new();
        for decl in declarations {
            match impl_on_head(program, trait_, decl.id) {
                None => undecided.push(decl),
                Some((id, ImplOrigin::Marker)) => markers.push((decl, id)),
                Some((_, ImplOrigin::Written | ImplOrigin::Derived)) => {}
            }
        }
        (undecided, markers)
    };

    // Assume, then refute. Every undecided declaration gets its tentative impl,
    // and one whose members fail against the rest loses it; the loop ends when
    // a pass removes none.
    let mut tentative = program.clone();
    let mut standing: Vec<(&Declaration, ImplId)> = Vec::with_capacity(undecided.len());
    for decl in undecided {
        let id = tentative.next_impl_id();
        tentative.add_impl(id, derived_impl(trait_, decl));
        standing.push((decl, id));
    }
    loop {
        let before = standing.len();
        let mut kept = Vec::with_capacity(before);
        for (decl, id) in standing {
            if members_satisfy(&tentative, trait_, decl) {
                kept.push((decl, id));
            } else {
                tentative.impls.shift_remove(&id);
            }
        }
        standing = kept;
        if standing.len() == before {
            break;
        }
    }

    Derived {
        impls: standing
            .iter()
            .map(|(_, id)| tentative.impls[id].clone())
            .collect(),
        errors: markers
            .into_iter()
            .filter(|(decl, _)| !members_satisfy(&tentative, trait_, decl))
            .map(|(_, impl_)| DeriveError::MarkerNotDerivable { impl_ })
            .collect(),
    }
}

/// The impl `decl` derives: its own head at its own parameters, each parameter
/// its members mention bounded by the trait.
fn derived_impl(trait_: TraitDeclId, decl: &Declaration) -> ImplDef {
    ImplDef {
        trait_: Some(trait_),
        trait_args: Vec::new(),
        target: SolverType::Decl(decl.id, (0..decl.params).map(SolverType::Param).collect()),
        params: (0..decl.params)
            .map(|index| {
                ParamDef::bounded(if mentions_param(&decl.members, index) {
                    vec![trait_]
                } else {
                    Vec::new()
                })
            })
            .collect(),
        origin: ImplOrigin::Derived,
    }
}

/// Whether every member satisfies `trait_` under the bounds the derived impl
/// would carry. The environment is what makes `struct W<T> { inner: List<T> }`
/// derive: `List<T>: Eq` reaches the prelude's blanket, whose `T: Eq` the
/// environment answers.
fn members_satisfy(program: &Program, trait_: TraitDeclId, decl: &Declaration) -> bool {
    let env = Env {
        param_bounds: (0..decl.params)
            .map(|index| {
                if mentions_param(&decl.members, index) {
                    vec![trait_]
                } else {
                    Vec::new()
                }
            })
            .collect(),
    };
    decl.members
        .iter()
        .all(|member| holds(program, &env, member, trait_, decl.module).is_some())
}

/// The impl of `trait_` whose target head is `head`, if any, with its origin.
/// A specific instantiation counts: an impl written for `W<i32>` is the
/// program's word on `W`, and a derived `W<T>` beside it would overlap.
fn impl_on_head(
    program: &Program,
    trait_: TraitDeclId,
    head: super::program::TypeDeclId,
) -> Option<(ImplId, ImplOrigin)> {
    program.impls.iter().find_map(|(&id, def)| {
        (def.trait_ == Some(trait_)
            && matches!(&def.target, SolverType::Decl(target_head, _) if *target_head == head))
        .then_some((id, def.origin))
    })
}

fn mentions_param(members: &[SolverType], index: u32) -> bool {
    members.iter().any(|m| type_mentions_param(m, index))
}

fn type_mentions_param(ty: &SolverType, index: u32) -> bool {
    match ty {
        SolverType::Param(i) | SolverType::Pack(i) => *i == index,
        SolverType::Decl(_, args) | SolverType::Tuple(args) => {
            args.iter().any(|a| type_mentions_param(a, index))
        }
        SolverType::Ref { inner, .. } => type_mentions_param(inner, index),
    }
}

#[cfg(test)]
mod tests {
    use super::super::program::{ModuleId, TypeDeclId};
    use super::*;

    const EQ: TraitDeclId = TraitDeclId(0);
    const POINT: TypeDeclId = TypeDeclId(0);
    const WRAPPER: TypeDeclId = TypeDeclId(1);
    const LIST: TypeDeclId = TypeDeclId(2);
    const I32: TypeDeclId = TypeDeclId(3);
    const OPAQUE: TypeDeclId = TypeDeclId(4);
    const NODE: TypeDeclId = TypeDeclId(5);
    const OPTION: TypeDeclId = TypeDeclId(6);
    const HERE: ModuleId = ModuleId(0);

    fn decl(id: TypeDeclId) -> SolverType {
        SolverType::Decl(id, vec![])
    }

    fn declaration(id: TypeDeclId, params: u32, members: Vec<SolverType>) -> Declaration {
        Declaration {
            id,
            params,
            members,
            module: HERE,
        }
    }

    /// A program where `i32: Eq` and the prelude's `impl<T: Eq> Eq for List<T>`
    /// and `impl<T: Eq> Eq for Option<T>` exist.
    fn prelude() -> Program {
        let mut p = Program::new();
        p.add_impl(
            ImplId(0),
            ImplDef {
                trait_: Some(EQ),
                trait_args: vec![],
                target: decl(I32),
                params: vec![],
                origin: ImplOrigin::Written,
            },
        );
        for (id, head) in [(ImplId(1), LIST), (ImplId(2), OPTION)] {
            p.add_impl(
                id,
                ImplDef {
                    trait_: Some(EQ),
                    trait_args: vec![],
                    target: SolverType::Decl(head, vec![SolverType::Param(0)]),
                    params: vec![ParamDef::bounded(vec![EQ])],
                    origin: ImplOrigin::Written,
                },
            );
        }
        p
    }

    fn derived_targets(d: &Derived) -> Vec<SolverType> {
        d.impls.iter().map(|i| i.target.clone()).collect()
    }

    #[test]
    fn a_struct_of_eq_members_derives() {
        let d = derive(&prelude(), EQ, &[declaration(POINT, 0, vec![decl(I32)])]);
        assert_eq!(derived_targets(&d), vec![decl(POINT)]);
        assert_eq!(d.impls[0].origin, ImplOrigin::Derived);
        assert_eq!(d.errors, vec![]);
    }

    #[test]
    fn a_struct_with_a_member_that_is_not_eq_does_not_derive() {
        let d = derive(&prelude(), EQ, &[declaration(POINT, 0, vec![decl(OPAQUE)])]);
        assert_eq!(d.impls, vec![]);
    }

    /// A plain `enum` or `flags` has no members and derives unconditionally.
    #[test]
    fn a_declaration_with_no_members_derives() {
        let d = derive(&prelude(), EQ, &[declaration(POINT, 0, vec![])]);
        assert_eq!(derived_targets(&d), vec![decl(POINT)]);
    }

    /// `struct Wrapper { inner: List<Point> }`: the member reaches the
    /// prelude's blanket, whose bound `Point: Eq` is answered by `Point`'s own
    /// tentative impl. Derivation and impl search meet inside one query.
    #[test]
    fn a_member_that_routes_through_a_blanket_derives() {
        let d = derive(
            &prelude(),
            EQ,
            &[
                declaration(POINT, 0, vec![decl(I32)]),
                declaration(WRAPPER, 0, vec![SolverType::Decl(LIST, vec![decl(POINT)])]),
            ],
        );
        assert_eq!(derived_targets(&d), vec![decl(POINT), decl(WRAPPER)]);
    }

    /// The refutation propagates: `Wrapper` loses its impl once `Point` does.
    #[test]
    fn a_member_that_stops_deriving_takes_its_container_with_it() {
        let d = derive(
            &prelude(),
            EQ,
            &[
                declaration(WRAPPER, 0, vec![SolverType::Decl(LIST, vec![decl(POINT)])]),
                declaration(POINT, 0, vec![decl(OPAQUE)]),
            ],
        );
        assert_eq!(d.impls, vec![]);
    }

    /// `struct Wrapper<T> { inner: List<T> }` derives
    /// `impl<T: Eq> Eq for Wrapper<T>`: the bound on `T` is what answers the
    /// blanket's own bound.
    #[test]
    fn a_generic_struct_derives_bounded_on_the_parameters_its_members_mention() {
        let d = derive(
            &prelude(),
            EQ,
            &[declaration(
                WRAPPER,
                1,
                vec![SolverType::Decl(LIST, vec![SolverType::Param(0)])],
            )],
        );
        assert_eq!(
            d.impls,
            vec![ImplDef {
                trait_: Some(EQ),
                trait_args: vec![],
                target: SolverType::Decl(WRAPPER, vec![SolverType::Param(0)]),
                params: vec![ParamDef::bounded(vec![EQ])],
                origin: ImplOrigin::Derived,
            }]
        );
    }

    /// A parameter no member mentions carries no bound: `struct H<T> { n: i32 }`
    /// is `Eq` whatever `T` is.
    #[test]
    fn a_parameter_no_member_mentions_is_unbounded() {
        let d = derive(&prelude(), EQ, &[declaration(WRAPPER, 1, vec![decl(I32)])]);
        assert_eq!(d.impls[0].params, vec![ParamDef::default()]);
    }

    /// `struct Node { next: Option<Node> }` reaches itself through a member.
    /// Assuming first is what lets it derive; refuting first would not.
    #[test]
    fn a_recursive_type_derives() {
        let d = derive(
            &prelude(),
            EQ,
            &[declaration(
                NODE,
                0,
                vec![SolverType::Decl(OPTION, vec![decl(NODE)])],
            )],
        );
        assert_eq!(derived_targets(&d), vec![decl(NODE)]);
    }

    /// Two declarations reaching each other derive together, and fall together
    /// when one of them carries a member that is not `Eq`.
    #[test]
    fn mutually_recursive_types_derive_or_fall_together() {
        let a_of = |members: Vec<SolverType>| declaration(POINT, 0, members);
        let b_of = |members: Vec<SolverType>| declaration(NODE, 0, members);
        let both = derive(
            &prelude(),
            EQ,
            &[
                a_of(vec![SolverType::Decl(OPTION, vec![decl(NODE)])]),
                b_of(vec![SolverType::Decl(OPTION, vec![decl(POINT)])]),
            ],
        );
        assert_eq!(derived_targets(&both), vec![decl(POINT), decl(NODE)]);

        let neither = derive(
            &prelude(),
            EQ,
            &[
                a_of(vec![SolverType::Decl(OPTION, vec![decl(NODE)])]),
                b_of(vec![
                    SolverType::Decl(OPTION, vec![decl(POINT)]),
                    decl(OPAQUE),
                ]),
            ],
        );
        assert_eq!(neither.impls, vec![]);
    }

    /// A written impl is the program's word on the pair; nothing is derived
    /// beside it, even where the members would allow it.
    #[test]
    fn a_written_impl_blocks_derivation() {
        let mut p = prelude();
        p.add_impl(
            ImplId(3),
            ImplDef {
                trait_: Some(EQ),
                trait_args: vec![],
                target: decl(POINT),
                params: vec![],
                origin: ImplOrigin::Written,
            },
        );
        let d = derive(&p, EQ, &[declaration(POINT, 0, vec![decl(I32)])]);
        assert_eq!(d.impls, vec![]);
    }

    /// A marker demands the derivation and answers for it; it is checked, not
    /// duplicated, and reported where the members would not support the body.
    #[test]
    fn a_marker_is_checked_rather_than_derived_beside() {
        let marker = |p: &mut Program| {
            p.add_impl(
                ImplId(3),
                ImplDef {
                    trait_: Some(EQ),
                    trait_args: vec![],
                    target: decl(POINT),
                    params: vec![],
                    origin: ImplOrigin::Marker,
                },
            );
        };
        let mut ok = prelude();
        marker(&mut ok);
        let d = derive(&ok, EQ, &[declaration(POINT, 0, vec![decl(I32)])]);
        assert_eq!(d.impls, vec![]);
        assert_eq!(d.errors, vec![]);

        let mut bad = prelude();
        marker(&mut bad);
        let d = derive(&bad, EQ, &[declaration(POINT, 0, vec![decl(OPAQUE)])]);
        assert_eq!(
            d.errors,
            vec![DeriveError::MarkerNotDerivable { impl_: ImplId(3) }]
        );
    }

    /// A member that reaches a declaration through the marker's impl derives:
    /// the marker answers the bound the same as a written impl would.
    #[test]
    fn a_marker_answers_for_a_container_that_mentions_it() {
        let mut p = prelude();
        p.add_impl(
            ImplId(3),
            ImplDef {
                trait_: Some(EQ),
                trait_args: vec![],
                target: decl(POINT),
                params: vec![],
                origin: ImplOrigin::Marker,
            },
        );
        let d = derive(
            &p,
            EQ,
            &[
                declaration(POINT, 0, vec![decl(I32)]),
                declaration(WRAPPER, 0, vec![decl(POINT)]),
            ],
        );
        assert_eq!(derived_targets(&d), vec![decl(WRAPPER)]);
    }
}
