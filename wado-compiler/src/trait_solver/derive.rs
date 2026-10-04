//! Derivation as impl generation: a declaration whose members all satisfy a
//! structural trait contributes `impl<Pi: Tr, …> Tr for D<P1..Pn>`.

use super::holds::{at_defaults, at_self, holds};
use super::program::ParamBound;
use super::program::{
    Declaration, Env, ImplDef, ImplId, ImplOrigin, ParamDef, Program, SolverType, TraitDeclId,
    TypeDeclId,
};
use crate::hashmap::IndexSet;

/// Add to `program` the impls of `trait_` that `declarations` derive, in order:
/// each but those another impl reaches at every instance.
pub fn derive(program: &mut Program, trait_: TraitDeclId, declarations: &[Declaration]) {
    // An impl at other arguments (`Eq<String>`) leaves the defaults, where a
    // derived impl answers, to derive.
    let has_impl: IndexSet<TypeDeclId> = program
        .impls
        .values()
        .filter(|def| def.trait_ == Some(trait_) && at_defaults(program, def))
        .filter_map(|def| match &def.target {
            SolverType::Decl(head, _) if covers_every_instance(def) => Some(*head),
            SolverType::Decl(..)
            | SolverType::Param(_)
            | SolverType::Pack(_)
            | SolverType::Ref { .. }
            | SolverType::Tuple(_)
            | SolverType::Projection { .. } => None,
        })
        .collect();
    let mut standing: Vec<(&Declaration, ImplId, Env)> = Vec::new();
    for decl in declarations {
        if has_impl.contains(&decl.id) {
            continue;
        }
        let bounds = derived_bounds(trait_, decl);
        let id = program.push_impl(ImplDef {
            trait_: Some(trait_),
            trait_args: Vec::new(),
            target: SolverType::Decl(decl.id, decl_params(decl)),
            params: bounds.iter().cloned().map(ParamDef::bounded).collect(),
            origin: ImplOrigin::Derived,
        });
        let env = Env {
            param_bounds: bounds
                .iter()
                .map(|b| b.iter().copied().map(ParamBound::bare).collect())
                .collect(),
        };
        standing.push((decl, id, env));
    }
    loop {
        let before = standing.len();
        standing.retain(|(decl, id, env)| {
            let satisfied = decl
                .members
                .iter()
                .all(|member| holds(program, env, member, trait_, decl.module).is_some());
            if !satisfied {
                program.impls.shift_remove(id);
            }
            satisfied
        });
        if standing.len() == before {
            break;
        }
    }
}

/// The declaration's own parameters, as its derived impl's target spells them.
fn decl_params(decl: &Declaration) -> Vec<SolverType> {
    (0..decl.params)
        .map(|index| {
            if decl.variadic && index + 1 == decl.params {
                SolverType::Pack(index)
            } else {
                SolverType::Param(index)
            }
        })
        .collect()
}

/// Whether `def` reaches every instance of its target's head: the target names
/// a declaration over distinct parameters.
fn covers_every_instance(def: &ImplDef) -> bool {
    let SolverType::Decl(_, args) = &def.target else {
        return false;
    };
    let mut seen = IndexSet::default();
    args.iter()
        .all(|arg| {
            matches!(arg, SolverType::Param(index) | SolverType::Pack(index) if seen.insert(*index))
        })
}

/// State the comparison table (spec-traits.md §Derivation Policy) as impls,
/// each at the target and with the bounds of the written impl it pairs with:
/// a written `cmp` gives `==`, and a written `eq` gives no `Ord`. Which row an
/// instance reads is then the precedence between impls reaching it: a written
/// `eq` or a marker takes the place of the `==` from `cmp`, and only a written
/// `cmp` lifts the withholding. A marker of `Eq` at a written `Ord`'s target
/// asks for that `==`, so it takes the `Ord`'s bounds in place of the paired
/// impl.
pub fn pair_comparisons(program: &mut Program, eq: TraitDeclId, ord: TraitDeclId) {
    let written = |program: &Program, trait_| -> Vec<ImplDef> {
        program
            .impls
            .values()
            .filter(|def| at_self(program, def, trait_, &[ImplOrigin::Written]))
            .cloned()
            .collect()
    };
    for written_eq in written(program, eq) {
        program.push_impl(ImplDef {
            trait_: Some(ord),
            trait_args: Vec::new(),
            origin: ImplOrigin::Withheld,
            ..written_eq
        });
    }
    for written_ord in written(program, ord) {
        let paired = |origin| ImplDef {
            trait_: Some(eq),
            trait_args: Vec::new(),
            origin,
            ..written_ord.clone()
        };
        let markers: Vec<ImplId> = program
            .impls
            .iter()
            .filter(|(_, def)| {
                def.trait_ == Some(eq)
                    && def.origin == ImplOrigin::Marker
                    && def.target == written_ord.target
            })
            .map(|(&marker, _)| marker)
            .collect();
        if markers.is_empty() {
            program.push_impl(paired(ImplOrigin::Paired));
        }
        for marker in markers {
            program.impls[&marker] = paired(ImplOrigin::Marker);
        }
    }
}

/// The bound each parameter of the derived impl carries: `trait_` where a
/// member mentions it, none otherwise.
fn derived_bounds(trait_: TraitDeclId, decl: &Declaration) -> Vec<Vec<TraitDeclId>> {
    (0..decl.params)
        .map(|index| {
            let mentioned = decl.members.iter().any(|m| m.mentions_param(index));
            if mentioned { vec![trait_] } else { Vec::new() }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::program::{ArgDefault, ModuleId, TypeDef};
    use super::super::testing::{Builder, bounded, concrete, decl};
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

    fn declaration(id: TypeDeclId, params: u32, members: Vec<SolverType>) -> Declaration {
        Declaration {
            id,
            params,
            variadic: false,
            members,
            module: HERE,
        }
    }

    /// A program where `i32: Eq` and the prelude's `impl<T: Eq> Eq for List<T>`
    /// and `impl<T: Eq> Eq for Option<T>` exist.
    fn prelude() -> Program {
        let of = |head| SolverType::Decl(head, vec![SolverType::Param(0)]);
        Builder::default()
            .concrete(EQ, decl(I32))
            .bounded(EQ, of(LIST), vec![EQ])
            .bounded(EQ, of(OPTION), vec![EQ])
            .build()
    }

    fn marker(p: &mut Program) {
        p.push_impl(ImplDef {
            origin: ImplOrigin::Marker,
            ..concrete(EQ, decl(POINT))
        });
    }

    /// The impls `derive` added to `prelude()`, in order.
    fn derived(program: Program, declarations: &[Declaration]) -> Vec<ImplDef> {
        let mut p = program;
        derive(&mut p, EQ, declarations);
        p.impls
            .values()
            .filter(|def| def.origin == ImplOrigin::Derived)
            .cloned()
            .collect()
    }

    fn targets(impls: &[ImplDef]) -> Vec<SolverType> {
        impls.iter().map(|i| i.target.clone()).collect()
    }

    #[test]
    fn a_struct_of_eq_members_derives() {
        let d = derived(prelude(), &[declaration(POINT, 0, vec![decl(I32)])]);
        assert_eq!(targets(&d), vec![decl(POINT)]);
    }

    #[test]
    fn a_struct_with_a_member_that_is_not_eq_does_not_derive() {
        let d = derived(prelude(), &[declaration(POINT, 0, vec![decl(OPAQUE)])]);
        assert_eq!(d, vec![]);
    }

    /// A plain `enum` or `flags` has no members and derives unconditionally.
    #[test]
    fn a_declaration_with_no_members_derives() {
        let d = derived(prelude(), &[declaration(POINT, 0, vec![])]);
        assert_eq!(targets(&d), vec![decl(POINT)]);
    }

    /// `struct Wrapper { inner: List<Point> }`: the member reaches the
    /// prelude's blanket, whose bound `Point: Eq` is answered by `Point`'s own
    /// tentative impl. Derivation and impl search meet inside one query.
    #[test]
    fn a_member_that_routes_through_a_blanket_derives() {
        let d = derived(
            prelude(),
            &[
                declaration(POINT, 0, vec![decl(I32)]),
                declaration(WRAPPER, 0, vec![SolverType::Decl(LIST, vec![decl(POINT)])]),
            ],
        );
        assert_eq!(targets(&d), vec![decl(POINT), decl(WRAPPER)]);
    }

    /// The refutation propagates: `Wrapper` loses its impl once `Point` does.
    #[test]
    fn a_member_that_stops_deriving_takes_its_container_with_it() {
        let d = derived(
            prelude(),
            &[
                declaration(WRAPPER, 0, vec![SolverType::Decl(LIST, vec![decl(POINT)])]),
                declaration(POINT, 0, vec![decl(OPAQUE)]),
            ],
        );
        assert_eq!(d, vec![]);
    }

    /// `struct Wrapper<T> { inner: List<T> }` derives
    /// `impl<T: Eq> Eq for Wrapper<T>`: the bound on `T` is what answers the
    /// blanket's own bound.
    #[test]
    fn a_generic_struct_derives_bounded_on_the_parameters_its_members_mention() {
        let d = derived(
            prelude(),
            &[declaration(
                WRAPPER,
                1,
                vec![SolverType::Decl(LIST, vec![SolverType::Param(0)])],
            )],
        );
        assert_eq!(
            d,
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
        let d = derived(prelude(), &[declaration(WRAPPER, 1, vec![decl(I32)])]);
        assert_eq!(d[0].params, vec![ParamDef::default()]);
    }

    /// An anonymous struct's head takes its field types as one tuple, and
    /// derives `impl<..F: Eq> Eq for Anon<..F>`: a shape answers by its fields.
    #[test]
    fn a_variadic_shape_derives_over_each_element_of_its_pack() {
        let shape = Declaration {
            variadic: true,
            ..declaration(WRAPPER, 1, vec![SolverType::Pack(0)])
        };
        let mut p = prelude();
        derive(&mut p, EQ, &[shape]);
        let of = |elems| SolverType::Decl(WRAPPER, vec![SolverType::Tuple(elems)]);
        let at = |ty: &SolverType| holds(&p, &Env::default(), ty, EQ, HERE).is_some();
        assert!(at(&of(vec![decl(I32), decl(I32)])));
        assert!(at(&of(vec![])));
        assert!(!at(&of(vec![decl(I32), decl(OPAQUE)])));
    }

    /// `struct Node { next: Option<Node> }` reaches itself through a member.
    /// Assuming first is what lets it derive; refuting first would not.
    #[test]
    fn a_recursive_type_derives() {
        let d = derived(
            prelude(),
            &[declaration(
                NODE,
                0,
                vec![SolverType::Decl(OPTION, vec![decl(NODE)])],
            )],
        );
        assert_eq!(targets(&d), vec![decl(NODE)]);
    }

    /// Two declarations reaching each other derive together, and fall together
    /// when one of them carries a member that is not `Eq`.
    #[test]
    fn mutually_recursive_types_derive_or_fall_together() {
        let a_of = |members: Vec<SolverType>| declaration(POINT, 0, members);
        let b_of = |members: Vec<SolverType>| declaration(NODE, 0, members);
        let both = derived(
            prelude(),
            &[
                a_of(vec![SolverType::Decl(OPTION, vec![decl(NODE)])]),
                b_of(vec![SolverType::Decl(OPTION, vec![decl(POINT)])]),
            ],
        );
        assert_eq!(targets(&both), vec![decl(POINT), decl(NODE)]);

        let neither = derived(
            prelude(),
            &[
                a_of(vec![SolverType::Decl(OPTION, vec![decl(NODE)])]),
                b_of(vec![
                    SolverType::Decl(OPTION, vec![decl(POINT)]),
                    decl(OPAQUE),
                ]),
            ],
        );
        assert_eq!(neither, vec![]);
    }

    /// A written impl is the program's word on the pair; nothing is derived
    /// beside it, even where the members would allow it.
    #[test]
    fn a_written_impl_blocks_derivation() {
        let mut p = prelude();
        p.push_impl(concrete(EQ, decl(POINT)));
        let d = derived(p, &[declaration(POINT, 0, vec![decl(I32)])]);
        assert_eq!(d, vec![]);
    }

    /// `impl Eq for Wrapper<i32>` covers one instance: the rest still derive,
    /// and the derived impl yields where the written one reaches.
    #[test]
    fn a_written_impl_at_some_instances_leaves_the_rest_derived() {
        let wrapper_of = |arg| SolverType::Decl(WRAPPER, vec![arg]);
        let mut p = prelude();
        p.push_impl(concrete(EQ, wrapper_of(decl(I32))));
        derive(
            &mut p,
            EQ,
            &[declaration(WRAPPER, 1, vec![SolverType::Param(0)])],
        );
        let asked = |ty| holds(&p, &Env::default(), &ty, EQ, HERE).map(|h| h.requests.len());
        assert_eq!(asked(wrapper_of(decl(I32))), Some(0));
        assert_eq!(
            asked(wrapper_of(SolverType::Decl(LIST, vec![decl(I32)]))),
            Some(1)
        );
    }

    /// A marker demands the derivation and answers for it, so nothing is
    /// derived beside it either.
    #[test]
    fn a_marker_blocks_derivation() {
        let mut p = prelude();
        marker(&mut p);
        let d = derived(p, &[declaration(POINT, 0, vec![decl(I32)])]);
        assert_eq!(d, vec![]);
    }

    /// `impl Eq<i32> for Point` answers another argument than a derived impl
    /// would, so `Point` still derives `Eq<Self>`.
    #[test]
    fn an_impl_at_another_argument_leaves_the_defaults_to_derive() {
        let mut p = prelude();
        p.traits.entry(EQ).or_default().arg_defaults = vec![Some(ArgDefault::SelfType)];
        p.push_impl(ImplDef {
            trait_args: vec![decl(I32)],
            ..concrete(EQ, decl(POINT))
        });
        let d = derived(p, &[declaration(POINT, 0, vec![decl(I32)])]);
        assert_eq!(targets(&d), vec![decl(POINT)]);
    }

    const ORD: TraitDeclId = TraitDeclId(1);

    fn wrapper_of(arg: SolverType) -> SolverType {
        SolverType::Decl(WRAPPER, vec![arg])
    }

    /// `impl<T: Ord> Ord for Wrapper<T>` gives `impl<T: Ord> Eq for Wrapper<T>`,
    /// whatever `Wrapper`'s members are.
    #[test]
    fn a_written_ord_pairs_an_eq_at_its_target_and_bounds() {
        let mut p = prelude();
        let wrapper = wrapper_of(SolverType::Param(0));
        p.push_impl(bounded(ORD, wrapper.clone(), vec![ORD]));
        pair_comparisons(&mut p, EQ, ORD);
        let paired: Vec<&ImplDef> = p
            .impls
            .values()
            .filter(|def| def.origin == ImplOrigin::Paired)
            .collect();
        assert_eq!(
            paired,
            vec![&ImplDef {
                origin: ImplOrigin::Paired,
                ..bounded(EQ, wrapper, vec![ORD])
            }]
        );
    }

    /// `impl<T: Ord> Ord for Node<T, i32>` decides `==` at the instances it
    /// reaches, in place of the members: `Node<Opaque, i32>` has no `==`, as
    /// `Opaque` writes `eq` and no `cmp`, though the members would give one.
    /// Every other instance compares its members.
    #[test]
    fn a_written_ord_at_some_instances_decides_eq_at_those() {
        let node_of = |a, b| SolverType::Decl(NODE, vec![a, b]);
        let mut p = prelude();
        p.push_impl(concrete(EQ, decl(OPAQUE)));
        p.push_impl(bounded(
            ORD,
            node_of(SolverType::Param(0), decl(I32)),
            vec![ORD],
        ));
        p.push_impl(concrete(ORD, decl(I32)));
        pair_comparisons(&mut p, EQ, ORD);
        derive(
            &mut p,
            EQ,
            &[declaration(NODE, 2, vec![SolverType::Param(0)])],
        );
        let has_eq = |ty| holds(&p, &Env::default(), &ty, EQ, HERE).is_some();
        assert!(has_eq(node_of(decl(I32), decl(I32))));
        assert!(!has_eq(node_of(decl(OPAQUE), decl(I32))));
        assert!(has_eq(node_of(decl(OPAQUE), decl(OPTION))));
    }

    /// `impl Eq for Wrapper<i32>` withholds `Ord` there and nowhere else, and
    /// a newtype over `Wrapper<i32>` inherits the withholding unless it writes
    /// `cmp` itself.
    #[test]
    fn a_written_eq_withholds_ord_where_it_reaches() {
        const ALIAS: TypeDeclId = TypeDeclId(7);
        const ORDERED: TypeDeclId = TypeDeclId(8);
        let mut p = Builder::default()
            .concrete(EQ, decl(I32))
            .concrete(ORD, decl(I32))
            .build();
        for head in [ALIAS, ORDERED] {
            p.types.insert(
                head,
                TypeDef {
                    newtype_base: Some(wrapper_of(decl(I32))),
                },
            );
        }
        p.push_impl(concrete(EQ, wrapper_of(decl(I32))));
        p.push_impl(concrete(ORD, decl(ORDERED)));
        pair_comparisons(&mut p, EQ, ORD);
        derive(
            &mut p,
            ORD,
            &[declaration(WRAPPER, 1, vec![SolverType::Param(0)])],
        );
        let ordered = |ty| holds(&p, &Env::default(), &ty, ORD, HERE).is_some();
        assert!(!ordered(wrapper_of(decl(I32))));
        assert!(!ordered(decl(ALIAS)));
        assert!(ordered(decl(ORDERED)));
        assert!(ordered(wrapper_of(decl(ORDERED))));
    }

    /// `impl<T> Eq for Wrapper<T>;` beside `impl<T: Ord> Ord for Wrapper<T>`
    /// asks for the `==` that `cmp` gives, so it holds where the `Ord` does.
    #[test]
    fn a_marker_beside_a_written_ord_takes_its_bounds() {
        let mut p = prelude();
        p.push_impl(bounded(ORD, wrapper_of(SolverType::Param(0)), vec![ORD]));
        let marker = p.push_impl(ImplDef {
            origin: ImplOrigin::Marker,
            ..bounded(EQ, wrapper_of(SolverType::Param(0)), vec![])
        });
        pair_comparisons(&mut p, EQ, ORD);
        assert!(p.impls.values().all(|def| def.origin != ImplOrigin::Paired));
        assert_eq!(
            p.impls[&marker],
            ImplDef {
                origin: ImplOrigin::Marker,
                ..bounded(EQ, wrapper_of(SolverType::Param(0)), vec![ORD])
            }
        );
        assert_eq!(
            holds(&p, &Env::default(), &wrapper_of(decl(OPAQUE)), EQ, HERE),
            None
        );
    }

    /// A member that reaches a declaration through the marker's impl derives:
    /// the marker answers the bound the same as a written impl would.
    #[test]
    fn a_marker_answers_for_a_container_that_mentions_it() {
        let mut p = prelude();
        marker(&mut p);
        let d = derived(
            p,
            &[
                declaration(POINT, 0, vec![decl(I32)]),
                declaration(WRAPPER, 0, vec![decl(POINT)]),
            ],
        );
        assert_eq!(targets(&d), vec![decl(WRAPPER)]);
    }
}
