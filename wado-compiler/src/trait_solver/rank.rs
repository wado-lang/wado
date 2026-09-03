//! Ranks 0-3 of `docs/wep-2026-09-01-trait-resolution.md`, over candidates
//! that already exist. Where an impl was written is not read.

use super::program::{ImplId, SolverType, TraitDeclId};

/// One impl that could answer a call, reduced to what the order reads.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Candidate {
    pub impl_: ImplId,
    pub trait_: TraitDeclId,
    /// The trait's arguments at the receiver, which is what the overload set
    /// groups on.
    pub trait_args: Vec<SolverType>,
    /// The level of the receiver's chain this candidate was selected at: 0 for
    /// the receiver itself, 1 for what it dereferences or newtype-unwraps to,
    /// and so on.
    pub depth: u32,
    /// Whether the impl covers the general case rather than being written for
    /// the receiver: its target mentions a type parameter, so it answers for
    /// every type that parameter can take.
    pub is_blanket: bool,
    /// Whether the impl's target is a bare pack (`impl<..T> Tr for [..T]`).
    pub is_variadic: bool,
}

/// What the order says about a candidate set. Every variant but [`Self::One`]
/// names the candidates it is about, so the caller can report them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Selection {
    /// No candidate at all.
    None,
    /// Exactly one answers, by its index in the slice.
    One(usize),
    /// Several trait declarations declare the method. They share no contract,
    /// so nothing selects and the call must name one.
    AmbiguousTraits(Vec<usize>),
    /// Several impls of one trait at one argument list, none written for the
    /// receiver. A blanket has no name, so only an impl written for the
    /// receiver settles it (rank 2).
    AmbiguousBlankets(Vec<usize>),
    /// One trait declaration at several argument lists. The call's arguments
    /// choose (WEP 2026-07-31), which is not this function's question.
    Overloaded(Vec<usize>),
    /// Several impls of one `(Trait, Type)` pair, which coherence rejects where
    /// they are written; ranking has nothing to say about them.
    Duplicated(Vec<usize>),
}

/// Apply the order to `candidates`.
#[must_use]
pub fn rank(candidates: &[Candidate]) -> Selection {
    let mut live: Vec<usize> = (0..candidates.len()).collect();
    drop_variadic_where_non_variadic_exists(candidates, &mut live);
    keep_shallowest(candidates, &mut live);
    drop_blankets_where_concrete_exists(candidates, &mut live);
    classify(candidates, live)
}

/// Rank 0. Within one trait at one argument list, a variadic impl yields to a
/// non-variadic one.
fn drop_variadic_where_non_variadic_exists(candidates: &[Candidate], live: &mut Vec<usize>) {
    let covered: Vec<(TraitDeclId, &[SolverType])> = live
        .iter()
        .map(|&i| &candidates[i])
        .filter(|c| !c.is_variadic)
        .map(|c| (c.trait_, c.trait_args.as_slice()))
        .collect();
    live.retain(|&i| {
        let c = &candidates[i];
        !c.is_variadic
            || !covered
                .iter()
                .any(|(trait_, args)| *trait_ == c.trait_ && *args == c.trait_args.as_slice())
    });
}

/// Rank 1. The search stops at the first level of the newtype chain that
/// answers, so nothing written for the base competes with the newtype's own.
fn keep_shallowest(candidates: &[Candidate], live: &mut Vec<usize>) {
    let Some(shallowest) = live.iter().map(|&i| candidates[i].depth).min() else {
        return;
    };
    live.retain(|&i| candidates[i].depth == shallowest);
}

/// Rank 2. Within one level, an impl written for the receiver defines the exact
/// function the call names and a blanket only covers the general case.
fn drop_blankets_where_concrete_exists(candidates: &[Candidate], live: &mut Vec<usize>) {
    if live.iter().any(|&i| !candidates[i].is_blanket) {
        live.retain(|&i| !candidates[i].is_blanket);
    }
}

/// Rank 3. What survives is one answer, or one of the shapes the caller
/// reports; two traits never form an overload set, whatever their arguments.
fn classify(candidates: &[Candidate], live: Vec<usize>) -> Selection {
    match live.as_slice() {
        [] => return Selection::None,
        [only] => return Selection::One(*only),
        [_, _, ..] => {}
    }
    let first = &candidates[live[0]];
    if live.iter().any(|&i| candidates[i].trait_ != first.trait_) {
        return Selection::AmbiguousTraits(live);
    }
    if live
        .iter()
        .any(|&i| candidates[i].trait_args != first.trait_args)
    {
        return Selection::Overloaded(live);
    }
    if live.iter().all(|&i| candidates[i].is_blanket) {
        return Selection::AmbiguousBlankets(live);
    }
    Selection::Duplicated(live)
}

#[cfg(test)]
mod tests {
    use super::super::program::TypeDeclId;
    use super::super::testing::decl as arg;
    use super::*;

    const TR: TraitDeclId = TraitDeclId(0);
    const OTHER: TraitDeclId = TraitDeclId(1);
    const I32: TypeDeclId = TypeDeclId(0);
    const STRING: TypeDeclId = TypeDeclId(1);

    struct Build(Vec<Candidate>);

    impl Build {
        fn new() -> Self {
            Self(Vec::new())
        }

        fn add(
            mut self,
            trait_: TraitDeclId,
            depth: u32,
            is_blanket: bool,
            is_variadic: bool,
        ) -> Self {
            let impl_ = ImplId(u32::try_from(self.0.len()).expect("test candidate count"));
            self.0.push(Candidate {
                impl_,
                trait_,
                trait_args: vec![],
                depth,
                is_blanket,
                is_variadic,
            });
            self
        }

        fn concrete(self, trait_: TraitDeclId, depth: u32) -> Self {
            self.add(trait_, depth, false, false)
        }

        fn blanket(self, trait_: TraitDeclId, depth: u32) -> Self {
            self.add(trait_, depth, true, false)
        }

        fn variadic(self, trait_: TraitDeclId) -> Self {
            self.add(trait_, 0, false, true)
        }

        fn with_args(mut self, args: Vec<SolverType>) -> Self {
            self.0.last_mut().expect("a candidate to arm").trait_args = args;
            self
        }

        fn done(self) -> Vec<Candidate> {
            self.0
        }
    }

    #[test]
    fn no_candidate_selects_nothing() {
        assert_eq!(rank(&[]), Selection::None);
    }

    #[test]
    fn one_candidate_answers() {
        assert_eq!(
            rank(&Build::new().concrete(TR, 0).done()),
            Selection::One(0)
        );
    }

    #[test]
    fn rank0_drops_a_variadic_impl_beside_a_non_variadic_one() {
        assert_eq!(
            rank(&Build::new().variadic(TR).concrete(TR, 0).done()),
            Selection::One(1)
        );
    }

    /// Rank 0 stays inside one trait: a non-variadic impl of another trait must
    /// not displace a variadic one.
    #[test]
    fn rank0_does_not_reach_across_traits() {
        assert_eq!(
            rank(&Build::new().variadic(TR).concrete(OTHER, 0).done()),
            Selection::AmbiguousTraits(vec![0, 1])
        );
    }

    #[test]
    fn rank1_stops_at_the_shallowest_level() {
        assert_eq!(
            rank(&Build::new().concrete(TR, 1).concrete(TR, 0).done()),
            Selection::One(1)
        );
    }

    /// Rank 1 runs before rank 2, so a blanket holding at the newtype answers
    /// before an impl written for the base.
    #[test]
    fn rank1_outranks_rank2() {
        assert_eq!(
            rank(&Build::new().concrete(TR, 1).blanket(TR, 0).done()),
            Selection::One(1)
        );
    }

    #[test]
    fn rank2_prefers_a_concrete_impl_within_one_level() {
        assert_eq!(
            rank(&Build::new().blanket(TR, 0).concrete(TR, 0).done()),
            Selection::One(1)
        );
    }

    /// Where an impl was written is not a rank, so two blankets holding at the
    /// same level are ambiguous however the caller's module relates to them.
    #[test]
    fn two_blankets_at_one_level_are_ambiguous() {
        assert_eq!(
            rank(&Build::new().blanket(TR, 0).blanket(TR, 0).done()),
            Selection::AmbiguousBlankets(vec![0, 1])
        );
    }

    /// Blankets join the cross-trait collision like any other candidate.
    #[test]
    fn two_traits_blankets_are_the_cross_trait_ambiguity() {
        assert_eq!(
            rank(&Build::new().blanket(TR, 0).blanket(OTHER, 0).done()),
            Selection::AmbiguousTraits(vec![0, 1])
        );
    }

    #[test]
    fn two_traits_concrete_impls_are_ambiguous() {
        assert_eq!(
            rank(&Build::new().concrete(TR, 0).concrete(OTHER, 0).done()),
            Selection::AmbiguousTraits(vec![0, 1])
        );
    }

    /// Two traits never form an overload set, whatever their arguments.
    #[test]
    fn distinct_traits_beat_the_argument_reading() {
        let candidates = Build::new()
            .concrete(TR, 0)
            .with_args(vec![arg(I32)])
            .concrete(OTHER, 0)
            .with_args(vec![arg(STRING)])
            .done();
        assert_eq!(rank(&candidates), Selection::AmbiguousTraits(vec![0, 1]));
    }

    /// One declaration at two argument lists is the overload set the call's
    /// arguments choose from — not an ambiguity of the order.
    #[test]
    fn one_trait_at_two_argument_lists_is_an_overload_set() {
        let candidates = Build::new()
            .concrete(TR, 0)
            .with_args(vec![arg(I32)])
            .concrete(TR, 0)
            .with_args(vec![arg(STRING)])
            .done();
        assert_eq!(rank(&candidates), Selection::Overloaded(vec![0, 1]));
    }

    /// Rank 0 separates argument lists too: a variadic impl of `Conv<i32>` is
    /// not covered by a non-variadic `Conv<String>`.
    #[test]
    fn rank0_separates_argument_lists() {
        let candidates = Build::new()
            .variadic(TR)
            .with_args(vec![arg(I32)])
            .concrete(TR, 0)
            .with_args(vec![arg(STRING)])
            .done();
        assert_eq!(rank(&candidates), Selection::Overloaded(vec![0, 1]));
    }

    /// Two impls of one pair are what coherence rejects where they are written.
    #[test]
    fn two_impls_of_one_pair_are_reported_as_the_duplicate_they_are() {
        assert_eq!(
            rank(&Build::new().concrete(TR, 0).concrete(TR, 0).done()),
            Selection::Duplicated(vec![0, 1])
        );
    }

    /// Rank 1 runs before the ambiguity rules, so a deeper collision never
    /// reports over an answer the newtype's own level already gave.
    #[test]
    fn a_deeper_collision_does_not_reach_the_ambiguity_rules() {
        let candidates = Build::new()
            .concrete(TR, 1)
            .concrete(OTHER, 1)
            .concrete(TR, 0)
            .done();
        assert_eq!(rank(&candidates), Selection::One(2));
    }

    /// Rank 2 the same way: a blanket collision at one level is settled by an
    /// impl written for the receiver rather than reported.
    #[test]
    fn a_concrete_impl_settles_a_blanket_collision() {
        let candidates = Build::new()
            .blanket(TR, 0)
            .blanket(OTHER, 0)
            .concrete(TR, 0)
            .done();
        assert_eq!(rank(&candidates), Selection::One(2));
    }
}
