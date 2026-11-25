//! Does this type satisfy this trait? Every other question is asked on top of
//! this one, and every step it takes is through an impl's bounds.

use super::program::{
    AssocId, DerivationRequest, Env, ImplDef, ImplId, ImplOrigin, ParamBound, Pin, Program,
    RefRule, SolverType, Source, TraitDeclId, TypeDeclId, Var,
};
use crate::hashmap::IndexSet;
use std::mem;

/// A bound that holds, and the bodies its answer owes.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Holds {
    pub requests: Vec<DerivationRequest>,
    /// The associated types the answering impl binds; empty where no impl
    /// answered.
    assoc: Vec<(AssocId, Member)>,
}

impl Holds {
    /// What the answering impl binds `assoc` to at the receiver, a family's
    /// member at `args`. `None` where it names an impl parameter the match
    /// left unbound.
    fn assoc_at(&self, assoc: AssocId, args: &[SolverType]) -> Option<SolverType> {
        let (_, member) = self.assoc.iter().find(|(a, _)| *a == assoc)?;
        member.at(args)
    }
}

/// An associated type as one impl binds it, in that impl's parameter space,
/// with what the match bound each parameter to. A family's own parameters
/// follow the impl's, and stay open for each projection's arguments: they
/// share no space with the receiver's parameters the match bound.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Member {
    binding: SolverType,
    matched: Vec<Option<SolverType>>,
}

impl Member {
    fn at(&self, args: &[SolverType]) -> Option<SolverType> {
        let own = self.matched.len();
        self.binding
            .map_params(&|i| match (i as usize).checked_sub(own) {
                None => self.matched[i as usize].clone(),
                Some(j) => args.get(j).cloned(),
            })
    }
}

/// Whether a type satisfies a trait, at every instance its inference variables
/// may become.
#[must_use]
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Verdict<T = Holds> {
    Holds(T),
    /// At no instance, and the answer to a question reaching itself through
    /// bounds.
    Fails,
    /// At some instance and not at another.
    Unknown,
}

impl<T> Verdict<T> {
    /// The answer to a question holding no inference variable, which is
    /// always decided.
    pub fn settled(self) -> Option<T> {
        match self {
            Self::Holds(held) => Some(held),
            Self::Fails => None,
            Self::Unknown => unreachable!("a question holding no inference variable is decided"),
        }
    }

    fn map<U>(self, f: impl FnOnce(T) -> U) -> Verdict<U> {
        match self {
            Self::Holds(held) => Verdict::Holds(f(held)),
            Self::Fails => Verdict::Fails,
            Self::Unknown => Verdict::Unknown,
        }
    }

    /// This answer, or the next rule's where this one fails.
    fn or_else(self, next: impl FnOnce() -> Self) -> Self {
        match self {
            Self::Fails => next(),
            Self::Holds(_) | Self::Unknown => self,
        }
    }
}

/// What a check failing after a bound not yet known answers: it may have
/// failed only for what that bound would have bound, so it refutes nothing.
fn refuted<T>(after_unknown: bool) -> Verdict<T> {
    if after_unknown {
        Verdict::Unknown
    } else {
        Verdict::Fails
    }
}

/// Whether `ty` satisfies `trait_`.
pub fn holds(program: &Program, env: &Env, ty: &SolverType, trait_: TraitDeclId) -> Verdict {
    holds_with_args(program, env, ty, trait_, &[])
}

/// [`holds`] where the asking bound writes arguments for the trait's own
/// parameters; an empty list asks for the declared defaults.
pub fn holds_with_args(
    program: &Program,
    env: &Env,
    ty: &SolverType,
    trait_: TraitDeclId,
    args: &[SolverType],
) -> Verdict {
    Query {
        program,
        env,
        asking: Vec::new(),
        member_next: false,
        at_itself: None,
    }
    .holds(ty, trait_, args)
}

/// `requests` with every body they owe in turn: a structural body calls its
/// trait on each member, so it owes each member's body at the instance, and an
/// impl owes one of each supertrait beside it, as `Ord` owes `Eq`. A recursive
/// type owes its own body once.
#[must_use]
pub fn owed(
    program: &Program,
    env: &Env,
    mut requests: Vec<DerivationRequest>,
) -> Vec<DerivationRequest> {
    let mut seen = IndexSet::default();
    let mut owed = Vec::new();
    while let Some(request) = requests.pop() {
        if !seen.insert((request.ty.clone(), request.trait_)) {
            continue;
        }
        if request.structural
            && let SolverType::Decl(head, args) = &request.ty
            && let Some(decl) = program.declarations.get(head)
        {
            for member in decl.members_at(args) {
                // A marker's bounds are written, so a member may not hold;
                // that is the marker's error, reported where it is written.
                if let Some(held) = holds(program, env, &member, request.trait_).settled() {
                    requests.extend(held.requests);
                }
            }
        }
        if let Some(def) = program.traits.get(&request.trait_) {
            // A derived body answers its trait at the defaults.
            let args = program.per_param(Some(request.trait_), &[]);
            for clause in &def.supertraits {
                if let Some(clause) = program.clause_at(clause, request.trait_, &args, &request.ty)
                    && let Some(held) =
                        holds_with_args(program, env, &request.ty, clause.trait_, &clause.args)
                            .settled()
                {
                    requests.extend(held.requests);
                }
            }
        }
        owed.push(request);
    }
    owed
}

/// Whether a bound in force on `subject` answers `trait_` at `wanted`,
/// comparing what the walk to the trait writes for each parameter against
/// `wanted`, defaulted where it writes none.
fn bound_answers(
    program: &Program,
    subject: &SolverType,
    bound: &ParamBound,
    trait_: TraitDeclId,
    wanted: &[SolverType],
) -> Verdict<()> {
    let wanted = program.per_param(Some(trait_), wanted);
    any_of(
        program
            .args_reaching(bound, trait_, subject)
            .iter()
            .map(|args| {
                let args = program.args_at(trait_, args, subject);
                let mut pairs = Vec::new();
                for (i, want) in wanted.iter().enumerate() {
                    let Some(want) = want else {
                        continue;
                    };
                    let Some(arg) = args.get(i).and_then(Option::as_ref) else {
                        return Verdict::Fails;
                    };
                    pairs.push((arg.clone(), want.clone()));
                }
                same(&pairs)
            }),
    )
}

/// A question open on the stack.
struct Open {
    ty: SolverType,
    trait_: TraitDeclId,
    args: Vec<SolverType>,
    through_member: bool,
}

/// One question and the questions open under it.
struct Query<'a> {
    program: &'a Program,
    env: &'a Env,
    asking: Vec<Open>,
    /// Whether the next question asks about a member of the type a pin
    /// bound it from, rather than about a bound's subject.
    member_next: bool,
    /// A type this query answers about at the type itself, without inheriting
    /// its newtype base's impls. Selection asks this way so that a blanket
    /// whose bound only the base carries is a candidate at the base's level and
    /// not at the newtype's — rank 1's question
    /// (`docs/wep-2026-09-01-trait-resolution.md`). The subject stays the same
    /// through a chained blanket's bounds, so the restriction travels with it.
    at_itself: Option<SolverType>,
}

impl Query<'_> {
    fn holds(&mut self, ty: &SolverType, trait_: TraitDeclId, args: &[SolverType]) -> Verdict {
        self.holds_at(ty, trait_, args, None)
    }

    /// [`Self::holds`] carrying the type the question was first asked at: a
    /// newtype inherits its base's impl with itself put in for `Self`.
    fn holds_at(
        &mut self,
        ty: &SolverType,
        trait_: TraitDeclId,
        args: &[SolverType],
        subject: Option<&SolverType>,
    ) -> Verdict {
        let through_member = std::mem::take(&mut self.member_next);
        // Keyed by the arguments too: the same trait asked at two
        // instantiations is two questions, and only one of them may hold.
        if let Some(at) = self
            .asking
            .iter()
            .position(|open| open.ty == *ty && open.trait_ == trait_ && open.args == args)
        {
            // Reached through a member, it is the type's own recursion, which
            // holds as a derived `Eq` holds of it; through bounds alone it
            // grounds nothing.
            let recursive =
                through_member || self.asking[at + 1..].iter().any(|open| open.through_member);
            return if recursive {
                Verdict::Holds(Holds::default())
            } else {
                Verdict::Fails
            };
        }
        // The same question at fresh variables: each step down mints new ones
        // and asks it again, so descending decides nothing.
        if self.asking.iter().any(|open| {
            let mut renaming = Vec::new();
            open.trait_ == trait_
                && open.ty.alike(ty, &mut renaming)
                && open.args.len() == args.len()
                && open
                    .args
                    .iter()
                    .zip(args)
                    .all(|(a, b)| a.alike(b, &mut renaming))
        }) {
            return Verdict::Unknown;
        }
        self.asking.push(Open {
            ty: ty.clone(),
            trait_,
            args: args.to_vec(),
            through_member,
        });
        let answer = self.decide(ty, trait_, args, subject);
        self.asking.pop();
        answer
    }

    /// [`Self::holds_at`] once the question is open: every step that asks
    /// another, a bound's projection normalized included, finds it asked.
    fn decide(
        &mut self,
        ty: &SolverType,
        trait_: TraitDeclId,
        args: &[SolverType],
        subject: Option<&SolverType>,
    ) -> Verdict {
        let program = self.program;
        let trait_def = program.traits.get(&trait_);
        if trait_def.is_some_and(|def| def.holds_for_all) {
            return Verdict::Holds(Holds::default());
        }
        // Where a rule holds at some instance of the variables, no answer
        // fails; where one fails at some instance, none holds.
        let mut may_hold = is_opaque(ty);
        let mut may_fail = false;
        match withholds(program, trait_, ty) {
            Verdict::Holds(()) => return Verdict::Fails,
            Verdict::Unknown => may_fail = true,
            Verdict::Fails => {}
        }
        let answer = self.decide_by_rules(ty, trait_, args, subject, &mut may_hold, &mut may_fail);
        match answer {
            Verdict::Holds(_) if may_fail => Verdict::Unknown,
            Verdict::Fails if may_hold => Verdict::Unknown,
            Verdict::Holds(_) | Verdict::Fails | Verdict::Unknown => answer,
        }
    }

    /// [`Self::decide`]'s rules in order, the first to answer deciding.
    fn decide_by_rules(
        &mut self,
        ty: &SolverType,
        trait_: TraitDeclId,
        args: &[SolverType],
        subject: Option<&SolverType>,
        may_hold: &mut bool,
        may_fail: &mut bool,
    ) -> Verdict {
        let program = self.program;
        let trait_def = program.traits.get(&trait_);
        let on_ref = trait_def.map_or(RefRule::default(), |def| def.on_ref);
        if matches!(ty, SolverType::Ref { .. }) && on_ref == RefRule::Always {
            return Verdict::Holds(Holds::default());
        }
        if let Some(holds) = never_holds(program, ty, trait_) {
            return Verdict::Holds(holds);
        }
        // A pack's bound holds of each element, so an element in a variadic
        // body answers from the pack's slot. A bound reads a projection the
        // program answers as what it binds: `U: Conv<Self::Item>` in
        // `impl Ints` is `U: Conv<i32>`.
        let env = self.env;
        if let SolverType::Param(index) | SolverType::Pack(index) = ty
            && let Some(bounds) = env.param_bounds.get(*index as usize)
        {
            let answered = any_of(bounds.iter().map(|bound| {
                let bound = ParamBound {
                    trait_: bound.trait_,
                    args: bound.args.iter().map(|arg| self.normalized(arg)).collect(),
                };
                bound_answers(program, ty, &bound, trait_, args)
            }));
            match answered {
                Verdict::Holds(()) => return Verdict::Holds(Holds::default()),
                Verdict::Unknown => *may_hold = true,
                Verdict::Fails => {}
            }
        }
        // A projection satisfies what its trait declares of the associated
        // type, by the declaration alone.
        if let SolverType::Projection {
            trait_: of, assoc, ..
        } = ty
            && program
                .traits
                .get(of)
                .and_then(|def| def.assoc_bounds.get(assoc))
                .is_some_and(|bounds| {
                    bounds
                        .iter()
                        .any(|&bound| program.bound_reaches(bound, trait_, ty))
                })
        {
            return Verdict::Holds(Holds::default());
        }
        if let Some(head) = fact_head(program, ty)
            && let Some(fact) = program.facts.get(&(head, trait_))
        {
            let matched = || -> Vec<Option<SolverType>> {
                let SolverType::Decl(_, args) = ty else {
                    unreachable!("only a declaration's fact binds an associated type");
                };
                args.iter().cloned().map(Some).collect()
            };
            return Verdict::Holds(Holds {
                requests: vec![DerivationRequest {
                    ty: ty.clone(),
                    trait_,
                    structural: false,
                }],
                assoc: fact
                    .assoc
                    .iter()
                    .map(|(assoc, binding)| {
                        let member = Member {
                            binding: binding.clone(),
                            matched: matched(),
                        };
                        (*assoc, member)
                    })
                    .collect(),
            });
        }
        // Only an impl of `trait_` itself answers. One of a subtrait owes an
        // impl of `trait_` beside it, and where that is missing the subtrait's
        // impl is the error, not this answer.
        // The first impl not refuted answers, and one not yet known leaves which
        // impl answers, and so what it binds, unknown.
        let mut answer = Verdict::Fails;
        for (id, def) in program.impls_for(trait_, ty) {
            if subject.is_some() && program.not_inherited.contains(&id) {
                continue;
            }
            // A default reads `Self` as the type asked about, which a newtype
            // inheriting this impl is.
            let asked = program.args_at(trait_, args, subject.unwrap_or(ty));
            answer = match self.impl_answers(id, def, ty, &asked) {
                // The bound's arguments are what the impl must answer at.
                // Selection asks without this gate.
                Verdict::Holds(answer) => {
                    answers_args(program, def, ty, subject, &answer.trait_args, args)
                        .map(|()| answer.holds)
                }
                Verdict::Fails => Verdict::Fails,
                Verdict::Unknown => Verdict::Unknown,
            };
            if !matches!(answer, Verdict::Fails) {
                break;
            }
        }
        answer
            .or_else(|| {
                if self.at_itself.as_ref() == Some(ty) {
                    return Verdict::Fails;
                }
                let Some(base) = newtype_base(program, ty) else {
                    return Verdict::Fails;
                };
                if program.hash == Some(trait_) {
                    match written_comparison_reaches(program, ty) {
                        Verdict::Holds(()) => return Verdict::Fails,
                        Verdict::Unknown => *may_fail = true,
                        Verdict::Fails => {}
                    }
                }
                self.holds_at(&base, trait_, args, subject.or(Some(ty)))
            })
            .or_else(|| {
                let SolverType::Ref { inner, .. } = ty else {
                    return Verdict::Fails;
                };
                match on_ref {
                    RefRule::Inherits => self.holds(inner, trait_, args),
                    RefRule::Always | RefRule::Never => Verdict::Fails,
                }
            })
    }

    /// Whether one impl applies to `ty`: its target and its trait reference
    /// match, and every bound holds of what the match bound. `asked` is what
    /// the question writes at each of the trait's parameters, `None` where it
    /// leaves one open. A parameter only the trait reference names, as `G` in
    /// `impl<G: Back> Shown<G> for G::T`, is bound by the arguments asked.
    fn impl_answers(
        &mut self,
        id: ImplId,
        def: &ImplDef,
        ty: &SolverType,
        asked: &[Option<SolverType>],
    ) -> Verdict<Answer> {
        let program = self.program;
        let Some(implemented) = def.trait_ else {
            return Verdict::Fails;
        };
        // A value blanket answers no reference; `&T` reaches it through the
        // pointee.
        let value_blanket = matches!(def.target, SolverType::Param(_));
        if value_blanket && matches!(ty, SolverType::Ref { .. }) {
            return Verdict::Fails;
        }
        let depth = u32::try_from(self.asking.len()).expect("fewer than 2^32 open questions");
        let mut unifier = Unifier::new(depth);
        // A variable may yet be a reference.
        unifier.undecided = value_blanket && matches!(ty, SolverType::Var(_));
        let mut bindings: Vec<Option<Binding>> = vec![None; def.params.len()];
        let mut deferred = Vec::new();
        if !match_target(&def.target, ty, &mut bindings, &mut deferred, &mut unifier) {
            return Verdict::Fails;
        }
        // A parameter the trait reference alone determines, `impl<U> Tr<U>
        // for X`, is what the bound asks at its position.
        let offered = program.per_param(def.trait_, &def.trait_args);
        let mut left_to_the_call = vec![false; def.params.len()];
        for (at, offer) in offered.iter().enumerate() {
            let Some(offer) = offer else {
                continue;
            };
            match asked.get(at).and_then(Option::as_ref) {
                Some(ask) => {
                    if !match_target(offer, ask, &mut bindings, &mut deferred, &mut unifier) {
                        return Verdict::Fails;
                    }
                }
                // A selection asking no argument there leaves what only that
                // argument names to the call's own inference.
                None => {
                    for (index, open) in left_to_the_call.iter_mut().enumerate() {
                        *open |= offer.mentions_param(index as u32);
                    }
                }
            }
        }
        let through = self.bound_through_projections(&mut deferred, &mut bindings, &mut unifier);
        // A check failing where the match left a part undecided may have failed
        // for that part alone, and so may one asked without the binding a pin
        // of an answer not yet known would supply. Any other failure fails at
        // every instance, whatever else is not yet known.
        let mut after_unknown = unifier.undecided;
        let mut refutation_assumes = unifier.undecided;
        if !through || self.projections_refuted(&deferred, &bindings, &mut unifier) {
            return refuted(refutation_assumes);
        }
        let yielded = if def.origin == ImplOrigin::Written || program.hash != Some(implemented) {
            Verdict::Fails
        } else {
            written_comparison_reaches(program, ty)
        };
        match any_of([
            reached_by(program, implemented, ty, def.origin.yields_to()),
            yielded,
        ]) {
            Verdict::Holds(()) => return refuted(refutation_assumes),
            Verdict::Unknown => after_unknown = true,
            Verdict::Fails => {}
        }
        let mut requests = match def.origin {
            ImplOrigin::Written => Vec::new(),
            ImplOrigin::Derived | ImplOrigin::Marker | ImplOrigin::Paired => {
                vec![DerivationRequest {
                    ty: ty.clone(),
                    trait_: implemented,
                    // The `==` a written `cmp` gives calls `cmp`, a marker's
                    // included.
                    structural: program
                        .traits
                        .get(&implemented)
                        .is_some_and(|t| t.structural)
                        && match eq_from_cmp(program, implemented, ty) {
                            Verdict::Holds(()) => false,
                            Verdict::Fails => true,
                            // What the body owes is not known yet.
                            Verdict::Unknown => {
                                after_unknown = true;
                                true
                            }
                        },
                }]
            }
        };
        let pins_on = |index: usize| {
            def.params
                .iter()
                .flat_map(|p| &p.pins)
                .filter(move |pin| pin.ty.mentions_param(index as u32))
        };
        // A parameter only a pin supplies is bound by the answer to the bound
        // carrying the pin, so the ones the target binds go first.
        let from_pin: Vec<bool> = bindings.iter().map(Option::is_none).collect();
        let order: Vec<usize> = (0..def.params.len())
            .filter(|&i| !from_pin[i])
            .chain((0..def.params.len()).filter(|&i| from_pin[i]))
            .collect();
        for index in order {
            let param = &def.params[index];
            // Only a pin on a member channel descends into the type's members;
            // any other associated type is no part of it.
            let through_member = from_pin[index]
                && pins_on(index).any(|pin| program.member_channels.contains(&pin.assoc));
            // One the answers leave unbound reads a projection off a
            // parameter, and is bound at monomorphization with its bound
            // (WEP 2026-03-14).
            let Some(binding) = bindings[index].clone() else {
                if param.bounds.is_empty()
                    || pins_on(index).next().is_some()
                    || left_to_the_call[index]
                {
                    continue;
                }
                return refuted(refutation_assumes);
            };
            let elements: Vec<SolverType> = match binding {
                Binding::One(ty) => vec![unifier.resolve(&ty)],
                Binding::Pack(elems) => elems.iter().map(|e| unifier.resolve(e)).collect(),
            };
            // The marker owes no body, so its answer's requests are dropped.
            if param.copyable
                && let Some(copyable) = program.copyable
            {
                for element in &elements {
                    self.member_next = through_member;
                    match self.holds(&peel_newtypes(program, element), copyable, &[]) {
                        Verdict::Holds(_) => {}
                        Verdict::Fails => return refuted(refutation_assumes),
                        Verdict::Unknown => after_unknown = true,
                    }
                }
            }
            for bound in &param.bounds {
                for element in &elements {
                    // The bound writes its arguments in the impl's own
                    // parameter space, so `T: Eq<U>` under `U = String` asks
                    // about `Eq<String>` and not about `Eq<U>`.
                    let args: Vec<SolverType> = bound
                        .args
                        .iter()
                        .map(|arg| {
                            self.at_bindings(arg, &bindings, &unifier)
                                .unwrap_or_else(|| arg.clone())
                        })
                        .collect();
                    self.member_next = through_member;
                    match self.holds(element, bound.trait_, &args) {
                        Verdict::Holds(answer) => {
                            if !self.meets_pins(
                                &param.pins,
                                bound.trait_,
                                &answer,
                                &mut bindings,
                                &mut deferred,
                                &mut unifier,
                            ) {
                                return refuted(refutation_assumes);
                            }
                            requests.extend(answer.requests);
                        }
                        Verdict::Fails => return refuted(refutation_assumes),
                        Verdict::Unknown => {
                            after_unknown = true;
                            refutation_assumes |=
                                param.pins.iter().any(|pin| pin.trait_ == bound.trait_);
                        }
                    }
                }
            }
        }
        // No bound on the parameters answers for a member projected off one,
        // so a derivation asks each at the instance.
        if def.origin == ImplOrigin::Derived
            && let SolverType::Decl(head, args) = ty
            && let Some(decl) = program.declarations.get(head)
        {
            for member in decl.projected_members_at(args, &program.projecting) {
                let member = self.normalized(&member);
                self.member_next = true;
                match self.holds(&member, implemented, &[]) {
                    Verdict::Holds(answer) => requests.extend(answer.requests),
                    Verdict::Fails => return refuted(refutation_assumes),
                    Verdict::Unknown => after_unknown = true,
                }
            }
        }
        // A projection may name a parameter only a later pin binds, so each is
        // read in full once every answer has bound what it binds.
        if !self.projections_met(&deferred, &bindings, &mut unifier) {
            return refuted(refutation_assumes || unifier.undecided);
        }
        // An impl applying only at some instances of the question's variables
        // answers only there.
        if after_unknown || unifier.constrains() {
            return Verdict::Unknown;
        }
        let matched: Vec<Option<SolverType>> = bindings
            .iter()
            .map(|binding| {
                binding
                    .as_ref()
                    .map(|binding| unifier.resolve(&binding.as_type()))
            })
            .collect();
        let assoc = program
            .assoc_bindings
            .get(&id)
            .into_iter()
            .flatten()
            .map(|(assoc, binding)| {
                let member = Member {
                    binding: binding.clone(),
                    matched: matched.clone(),
                };
                (*assoc, member)
            })
            .collect();
        Verdict::Holds(Answer {
            holds: Holds { requests, assoc },
            trait_args: def
                .trait_args
                .iter()
                .map(|arg| {
                    self.at_bindings(arg, &bindings, &unifier)
                        .unwrap_or_else(|| arg.clone())
                })
                .collect(),
        })
    }

    /// Whether `answer`, to a bound on `trait_`, meets each of `pins` on it. An
    /// answer binding the pinned assoc otherwise is refuted; one binding
    /// nothing is not. A pin naming a parameter still unbound binds it from
    /// what the answer binds.
    fn meets_pins(
        &mut self,
        pins: &[Pin],
        trait_: TraitDeclId,
        answer: &Holds,
        bindings: &mut [Option<Binding>],
        deferred: &mut Vec<(SolverType, SolverType)>,
        unifier: &mut Unifier,
    ) -> bool {
        for pin in pins.iter().filter(|pin| pin.trait_ == trait_) {
            let Some(actual) = answer.assoc_at(pin.assoc, &[]) else {
                continue;
            };
            let actual = self.normalized(&actual);
            let met = if let Some(expected) = self.at_bindings(&pin.ty, bindings, unifier) {
                unifier.unify(&expected, &actual)
            } else {
                let from = deferred.len();
                match_target(&pin.ty, &actual, bindings, deferred, unifier)
                    && !self.projections_refuted(&deferred[from..], bindings, unifier)
            };
            if !met {
                return false;
            }
        }
        true
    }

    /// Bind the parameters only a deferred projection's arguments name, from
    /// the type it met: once its base is bound, `G::Of<E>` reads as the member
    /// it names with `E` still open (`Boxed<E>`), which that type then matches.
    /// One binding can unlock another projection's base, so this repeats until
    /// nothing binds. False where a member does not match.
    fn bound_through_projections(
        &mut self,
        deferred: &mut Vec<(SolverType, SolverType)>,
        bindings: &mut [Option<Binding>],
        unifier: &mut Unifier,
    ) -> bool {
        loop {
            let mut bound_any = false;
            for (projection, met) in mem::take(deferred) {
                match self.bound_through(&projection, &met, bindings, unifier) {
                    Through::Bound(nested) => {
                        bound_any = true;
                        deferred.extend(nested);
                    }
                    Through::Open => deferred.push((projection, met)),
                    Through::Refuted => return false,
                }
            }
            if !bound_any {
                return true;
            }
        }
    }

    /// Match the member `projection` names against `met`, binding what it
    /// leaves open. The member holds the caller's parameters through what the
    /// bindings hold, so the open ones move past every caller index, and a
    /// caller's parameter matches only itself.
    fn bound_through(
        &mut self,
        projection: &SolverType,
        met: &SolverType,
        bindings: &mut [Option<Binding>],
        unifier: &mut Unifier,
    ) -> Through {
        let SolverType::Projection { base, .. } = projection else {
            unreachable!("only a projection is deferred");
        };
        let met = self.normalized(&unifier.resolve(met));
        // A projection the normalizer leaves standing reads off a caller's
        // parameter, which names no type yet: the impl's projection of the
        // same member meets it argument for argument, its base among them.
        let off_caller = projection_parts(projection, &met).is_some();
        if (!off_caller && self.at_bindings(base, bindings, unifier).is_none())
            || self.at_bindings(projection, bindings, unifier).is_some()
        {
            return Through::Open;
        }
        for binding in bindings.iter_mut().flatten() {
            *binding = unifier.resolved(binding);
        }
        // The member's parameters move past the caller's, where no variable
        // standing for one is minted, so a match meeting a variable is left to
        // the instance.
        if met.is_open() || bindings.iter().flatten().any(|b| b.as_type().is_open()) {
            unifier.undecided = true;
            return Through::Open;
        }
        let past = bindings
            .iter()
            .flatten()
            .map(|binding| params_past(&binding.as_type()))
            .fold(
                params_past(&met).max(self.env.param_bounds.len() as u32),
                u32::max,
            );
        let open = projection.map_params(&|i| {
            Some(match bindings.get(i as usize)? {
                Some(binding) => binding.as_type(),
                None => SolverType::Param(past + i),
            })
        });
        let member = self.normalized(&open.expect("every parameter maps"));
        let mut slots: Vec<Option<Binding>> = (0..past)
            .map(|caller| Some(Binding::One(SolverType::Param(caller))))
            .chain(bindings.iter().cloned())
            .collect();
        let mut nested = Vec::new();
        // Neither side holds a variable, so the match binds none.
        let mut closed = Unifier::standalone();
        let matched = match projection_parts(&member, &met) {
            Some(parts) => parts.into_iter().all(|(ours, theirs)| {
                match_target(ours, theirs, &mut slots, &mut nested, &mut closed)
            }),
            None if matches!(member, SolverType::Projection { .. }) => return Through::Open,
            None => match_target(&member, &met, &mut slots, &mut nested, &mut closed),
        };
        if !matched {
            return Through::Refuted;
        }
        // A projection nested in the member names the impl's parameters only
        // when it mentions no caller's.
        let nested: Option<Vec<(SolverType, SolverType)>> = nested
            .into_iter()
            .map(|(projection, met)| {
                Some((
                    projection.map_params(&|i| i.checked_sub(past).map(SolverType::Param))?,
                    met,
                ))
            })
            .collect();
        let Some(nested) = nested else {
            return Through::Open;
        };
        for (binding, slot) in bindings.iter_mut().zip(slots.split_off(past as usize)) {
            *binding = slot;
        }
        Through::Bound(nested)
    }

    /// Whether a projection a match met, readable at `bindings` already, holds
    /// something other than what it was matched against.
    fn projections_refuted(
        &mut self,
        deferred: &[(SolverType, SolverType)],
        bindings: &[Option<Binding>],
        unifier: &mut Unifier,
    ) -> bool {
        deferred.iter().any(|(projection, met)| {
            self.projection_holds(projection, met, bindings, unifier) == Some(false)
        })
    }

    /// Whether each projection a match met holds what it was matched against,
    /// read at `bindings`.
    fn projections_met(
        &mut self,
        deferred: &[(SolverType, SolverType)],
        bindings: &[Option<Binding>],
        unifier: &mut Unifier,
    ) -> bool {
        deferred.iter().all(|(projection, met)| {
            self.projection_holds(projection, met, bindings, unifier) == Some(true)
        })
    }

    /// Whether `projection` read at `bindings` is `met`; `None` where it names
    /// a parameter still unbound.
    fn projection_holds(
        &mut self,
        projection: &SolverType,
        met: &SolverType,
        bindings: &[Option<Binding>],
        unifier: &mut Unifier,
    ) -> Option<bool> {
        let expected = self.at_bindings(projection, bindings, unifier)?;
        let met = self.normalized(&unifier.resolve(met));
        Some(unifier.unify(&expected, &met))
    }

    /// `ty`, written in an impl's parameter space, at what the match bound
    /// each parameter to, normalized there. `None` where it names a parameter
    /// the match left unbound.
    fn at_bindings(
        &mut self,
        ty: &SolverType,
        bindings: &[Option<Binding>],
        unifier: &Unifier,
    ) -> Option<SolverType> {
        let bound = ty.map_params(&|i| bindings.get(i as usize)?.as_ref().map(Binding::as_type))?;
        Some(self.normalized(&unifier.resolve(&bound)))
    }

    /// `ty` with each projection an impl answers replaced by what that impl
    /// binds: `<Ints as Src>::Item` is `i32` under `impl Src for Ints`, and a
    /// family's member is its binding at the projection's arguments. A
    /// projection off a parameter is what the parameter's bounds state for
    /// it, and stays as written where they state nothing, or disagree: a
    /// projection names no trait arguments, so it cannot pick between
    /// `C: Conv<i32, Out = A> + Conv<String, Out = B>`.
    fn normalized(&mut self, ty: &SolverType) -> SolverType {
        match ty {
            SolverType::Projection {
                base,
                trait_,
                assoc,
                args,
            } => {
                let base = self.normalized(base);
                if let SolverType::Param(index) = base {
                    let mut stated = self
                        .env
                        .param_assoc
                        .get(index as usize)
                        .into_iter()
                        .flatten()
                        .filter(|eq| eq.trait_ == *trait_ && eq.assoc == *assoc)
                        .map(|eq| &eq.ty);
                    if let Some(first) = stated.next()
                        && stated.all(|other| other == first)
                        // One written as itself (`Item = I::Item`) states
                        // nothing, and reading it would not end.
                        && first != ty
                    {
                        let first = first.clone();
                        return self.normalized(&first);
                    }
                }
                let args: Vec<SolverType> = args.iter().map(|a| self.normalized(a)).collect();
                let member = match self.holds(&base, *trait_, &[]) {
                    Verdict::Holds(held) => held.assoc_at(*assoc, &args),
                    Verdict::Fails | Verdict::Unknown => None,
                };
                match member {
                    Some(ty) => self.normalized(&ty),
                    None => SolverType::Projection {
                        base: Box::new(base),
                        trait_: *trait_,
                        assoc: *assoc,
                        args,
                    },
                }
            }
            SolverType::Decl(head, args) => {
                SolverType::Decl(*head, args.iter().map(|a| self.normalized(a)).collect())
            }
            SolverType::Tuple(elems) => {
                SolverType::Tuple(elems.iter().map(|e| self.normalized(e)).collect())
            }
            SolverType::Ref { is_mut, inner } => SolverType::Ref {
                is_mut: *is_mut,
                inner: Box::new(self.normalized(inner)),
            },
            SolverType::Param(_)
            | SolverType::Pack(_)
            | SolverType::OpenRow(_)
            | SolverType::Var(_) => ty.clone(),
        }
    }
}

/// Whether an impl of `trait_` at its defaults from one of `origins` names
/// `ty`'s own head and reaches it, whatever its bounds. A derived impl answers
/// only at the defaults, so only an impl there takes its place.
fn reached_by(
    program: &Program,
    trait_: TraitDeclId,
    ty: &SolverType,
    origins: &[ImplOrigin],
) -> Verdict<()> {
    if origins.is_empty() {
        return Verdict::Fails;
    }
    any_of(
        program
            .impls_for(trait_, ty)
            .filter(|(_, def)| at_self(program, def, trait_, origins))
            .map(|(_, def)| reaches_target(def, ty)),
    )
}

/// Whether `def` is an impl of `trait_` at its defaults from one of `origins`,
/// targeting a declaration.
pub(super) fn at_self(
    program: &Program,
    def: &ImplDef,
    trait_: TraitDeclId,
    origins: &[ImplOrigin],
) -> bool {
    def.trait_ == Some(trait_)
        && origins.contains(&def.origin)
        && at_defaults(program, def)
        && matches!(def.target, SolverType::Decl(..))
}

/// The trait `ty` has none of, whatever answers it: `Ord`, where a written `eq`
/// reaches it alone.
pub(super) fn withheld_at(program: &Program, ty: &SolverType) -> Verdict<TraitDeclId> {
    let Some((eq, ord)) = program.comparisons else {
        return Verdict::Fails;
    };
    row_is(program, ty, eq).map(|()| ord)
}

/// `!` and a newtype over it hold every trait, binding each associated type to
/// `!`, and no impl is written for either (spec-types.md § Trait Bounds on `!`).
fn never_holds(program: &Program, ty: &SolverType, trait_: TraitDeclId) -> Option<Holds> {
    if !peels_to_never(program, ty) {
        return None;
    }
    let never = SolverType::Decl(program.never?, Vec::new());
    let assoc = program.traits[&trait_]
        .assoc_bounds
        .keys()
        .map(|&assoc| {
            let member = Member {
                binding: never.clone(),
                matched: Vec::new(),
            };
            (assoc, member)
        })
        .collect();
    Some(Holds {
        requests: Vec::new(),
        assoc,
    })
}

/// Whether `ty` is `!` or a newtype over it.
pub(super) fn peels_to_never(program: &Program, ty: &SolverType) -> bool {
    is_never(program, &peel_newtypes(program, ty))
}

fn is_never(program: &Program, ty: &SolverType) -> bool {
    program.never.is_some_and(
        |never| matches!(ty, SolverType::Decl(id, args) if *id == never && args.is_empty()),
    )
}

/// Whether `ty` has no `trait_`, as `withheld_at` says, checking the trait
/// before the row: the row scans every impl.
fn withholds(program: &Program, trait_: TraitDeclId, ty: &SolverType) -> Verdict<()> {
    match program.comparisons {
        Some((eq, ord)) if trait_ == ord => row_is(program, ty, eq),
        Some(_) | None => Verdict::Fails,
    }
}

/// The trait only an impl written at `ty` itself answers: [`Program::hash`],
/// where a written comparison reaches `ty`. Neither a derived body nor a
/// newtype's base answers it there.
pub(super) fn written_only_at(program: &Program, ty: &SolverType) -> Verdict<TraitDeclId> {
    let Some(hash) = program.hash else {
        return Verdict::Fails;
    };
    written_comparison_reaches(program, ty).map(|()| hash)
}

/// Whether a written impl of `Eq` or `Ord` reaches `ty`.
fn written_comparison_reaches(program: &Program, ty: &SolverType) -> Verdict<()> {
    let Some((eq, ord)) = program.comparisons else {
        return Verdict::Fails;
    };
    any_of(
        [eq, ord]
            .into_iter()
            .map(|written| reached_by(program, written, ty, &[ImplOrigin::Written])),
    )
}

/// Which of `Eq` and `Ord` a written impl reaching `ty` implements, `Eq` first.
#[must_use]
pub fn written_comparison(program: &Program, ty: &SolverType) -> Option<TraitDeclId> {
    let (eq, ord) = program.comparisons?;
    [eq, ord].into_iter().find(|&written| {
        reached_by(program, written, ty, &[ImplOrigin::Written])
            .settled()
            .is_some()
    })
}

/// Whether `trait_`'s body at `ty` is the `==` a `cmp` written alone gives.
fn eq_from_cmp(program: &Program, trait_: TraitDeclId, ty: &SolverType) -> Verdict<()> {
    match program.comparisons {
        Some((eq, ord)) if trait_ == eq => row_is(program, ty, ord),
        Some(_) | None => Verdict::Fails,
    }
}

fn row_is(program: &Program, ty: &SolverType, written: TraitDeclId) -> Verdict<()> {
    match comparison_row_at(program, ty) {
        Verdict::Holds((row, _)) if row == written => Verdict::Holds(()),
        Verdict::Holds(_) | Verdict::Fails => Verdict::Fails,
        Verdict::Unknown => Verdict::Unknown,
    }
}

/// The row of the comparison table `ty` reads (spec-traits.md §Derivation
/// Policy): which of `Eq` and `Ord` a written impl reaching `ty` implements
/// without the other, with that impl.
#[must_use]
pub fn comparison_row(program: &Program, ty: &SolverType) -> Option<(TraitDeclId, ImplId)> {
    comparison_row_at(program, ty).settled()
}

/// [`comparison_row`] at every instance of `ty`'s variables. A concrete impl
/// answers before a generic one, as coherence Rule 1 orders them.
fn comparison_row_at(program: &Program, ty: &SolverType) -> Verdict<(TraitDeclId, ImplId)> {
    let Some((eq, ord)) = program.comparisons else {
        return Verdict::Fails;
    };
    let written = |trait_| -> Verdict<ImplId> {
        let mut reaching = Vec::new();
        for (id, def) in program.impls_for(trait_, ty) {
            if !at_self(program, def, trait_, &[ImplOrigin::Written]) {
                continue;
            }
            match reaches_target(def, ty) {
                Verdict::Holds(()) => reaching.push((id, def)),
                Verdict::Unknown => return Verdict::Unknown,
                Verdict::Fails => {}
            }
        }
        reaching
            .into_iter()
            .min_by_key(|(_, def)| !def.params.is_empty())
            .map_or(Verdict::Fails, |(id, _)| Verdict::Holds(id))
    };
    match (written(eq), written(ord)) {
        (Verdict::Holds(id), Verdict::Fails) => Verdict::Holds((eq, id)),
        (Verdict::Fails, Verdict::Holds(id)) => Verdict::Holds((ord, id)),
        (Verdict::Holds(_), Verdict::Holds(_)) | (Verdict::Fails, Verdict::Fails) => Verdict::Fails,
        (Verdict::Unknown, _) | (_, Verdict::Unknown) => Verdict::Unknown,
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

/// The trait arguments `def` applies to `ty` at. This is selection's question,
/// and it differs from a bound's twice: it names the trait, so it accepts an
/// impl at any argument list (WEP 2026-07-31); and it asks about `ty` at the
/// type itself, so a blanket whose bound only `ty`'s newtype base carries does
/// not apply here — the chain reaches that blanket at the base's own level,
/// which is the depth rank 1 wants.
pub(super) fn impl_applies(
    program: &Program,
    env: &Env,
    id: ImplId,
    def: &ImplDef,
    ty: &SolverType,
    asked: &[Option<SolverType>],
) -> Verdict<Vec<SolverType>> {
    Query {
        program,
        env,
        asking: Vec::new(),
        member_next: false,
        at_itself: Some(ty.clone()),
    }
    .impl_answers(id, def, ty, asked)
    .map(|answer| answer.trait_args)
}

/// Whether `def`'s target meets `ty` at every instance of `ty`'s variables, so
/// that committing to `def` settles none of them. A literal's variable may be
/// settled, to a type a literal may be: its type is a default the one impl
/// that could answer chooses.
#[must_use]
pub(super) fn settles_nothing(program: &Program, def: &ImplDef, ty: &SolverType) -> bool {
    // A variable may yet be a reference, which a value blanket answers not.
    if matches!(def.target, SolverType::Param(_)) && matches!(ty, SolverType::Var(_)) {
        return false;
    }
    let mut unifier = Unifier::new(0);
    let mut bindings: Vec<Option<Binding>> = vec![None; def.params.len()];
    let mut deferred = Vec::new();
    match_target(&def.target, ty, &mut bindings, &mut deferred, &mut unifier)
        && !unifier.undecided
        && (deferred.is_empty() || !ty.is_open())
        && unifier.subst.iter().all(|(var, value)| {
            unifier.is_own(*var)
                || matches!(var, Var::Infer { literal: true, .. }) && may_be_literal(program, value)
        })
}

/// Whether `ty` may be a numeric literal's type: one of the program's literal
/// types, a newtype over one, or a variable that may yet be one.
fn may_be_literal(program: &Program, ty: &SolverType) -> bool {
    match peel_newtypes(program, ty) {
        SolverType::Var(_) => true,
        SolverType::Decl(head, _) => program.literal_types.contains(&head),
        SolverType::Param(_)
        | SolverType::Pack(_)
        | SolverType::Ref { .. }
        | SolverType::Tuple(_)
        | SolverType::Projection { .. }
        | SolverType::OpenRow(_) => false,
    }
}

/// Whether `source`'s bounds hold of `ty`, its pins met by one binding
/// of the source's parameters.
#[must_use]
pub(super) fn admits(program: &Program, env: &Env, source: &Source, ty: &SolverType) -> bool {
    let mut query = Query {
        program,
        env,
        asking: Vec::new(),
        member_next: false,
        at_itself: None,
    };
    let mut bindings: Vec<Option<Binding>> = vec![None; source.params];
    let mut deferred = Vec::new();
    let mut unifier = Unifier::standalone();
    source.bounds.iter().all(|bound| {
        query
            .holds(ty, bound.trait_, &bound.args)
            .settled()
            .is_some_and(|answer| {
                query.meets_pins(
                    &source.pins,
                    bound.trait_,
                    &answer,
                    &mut bindings,
                    &mut deferred,
                    &mut unifier,
                )
            })
    }) && query.projections_met(&deferred, &bindings, &mut unifier)
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

/// The declaration a type instantiates, which a fact is stated of. A tuple is
/// an instance of the tuple declaration and lowers to its own shape rather than
/// to a `Decl`, because an impl spells one `[..T]`.
fn fact_head(program: &Program, ty: &SolverType) -> Option<TypeDeclId> {
    match ty {
        SolverType::Decl(head, _) => Some(*head),
        SolverType::Tuple(_) => program.tuple,
        SolverType::Param(_)
        | SolverType::Pack(_)
        | SolverType::Ref { .. }
        | SolverType::Projection { .. }
        | SolverType::OpenRow(_)
        | SolverType::Var(_) => None,
    }
}

/// Whether `ty` names no type of its own until inference settles it: a
/// variable, or a projection off one. No rule refutes a trait of such a type,
/// since `!` holds every trait.
pub(super) fn is_opaque(ty: &SolverType) -> bool {
    matches!(ty, SolverType::Var(_) | SolverType::Projection { .. }) && ty.is_open()
}

/// The base a newtype receiver inherits from, at the receiver's own type
/// arguments.
pub(super) fn newtype_base(program: &Program, ty: &SolverType) -> Option<SolverType> {
    let SolverType::Decl(head, args) = ty else {
        return None;
    };
    let base = program.types.get(head)?.newtype_base.as_ref()?;
    Some(
        base.map_params(&|i| args.get(i as usize).cloned())
            .unwrap_or_else(|| panic!("{base:?} mentions a parameter {ty:?} has no argument for")),
    )
}

/// `ty` with every newtype level peeled. A newtype copies where its base does,
/// and the subject a newtype's base is asked under ranks impls, which a kind
/// does not. A newtype cycle is reported where it is declared.
fn peel_newtypes(program: &Program, ty: &SolverType) -> SolverType {
    let mut seen = vec![ty.clone()];
    while let Some(base) = newtype_base(program, seen.last().expect("seeded"))
        && !seen.contains(&base)
    {
        seen.push(base);
    }
    seen.pop().expect("seeded")
}

/// Whether the impl answers a bound writing `args`: at every parameter each
/// side says its written argument, or the trait's default at `ty` where it
/// wrote none.
pub(super) fn answers_args(
    program: &Program,
    def: &ImplDef,
    ty: &SolverType,
    subject: Option<&SolverType>,
    written: &[SolverType],
    args: &[SolverType],
) -> Verdict<()> {
    answers_args_by(program, def, ty, subject, written, args, |ask, offer| {
        same(&[(ask.clone(), offer.clone())])
    })
}

/// Whether the impl answers a call that names `args` at an instance, as
/// [`answers_args`] answers a bound. A parameter in `args` is a binder the
/// call's own frame leaves unsettled, and fits any type.
pub(super) fn answers_named_args(
    program: &Program,
    def: &ImplDef,
    ty: &SolverType,
    subject: &SolverType,
    written: &[SolverType],
    args: &[SolverType],
) -> bool {
    answers_args_by(
        program,
        def,
        ty,
        Some(subject),
        written,
        args,
        |ask, offer| decided(fits(ask, offer)),
    )
    .settled()
    .is_some()
}

/// The answer a question holding no variable gives.
fn decided(holds: bool) -> Verdict<()> {
    if holds {
        Verdict::Holds(())
    } else {
        Verdict::Fails
    }
}

/// At every parameter, whether an argument `args` says `fits` one `written`
/// says: each side its written argument, or the trait's default at `ty` where
/// it wrote none.
fn answers_args_by(
    program: &Program,
    def: &ImplDef,
    ty: &SolverType,
    subject: Option<&SolverType>,
    written: &[SolverType],
    args: &[SolverType],
    mut fits: impl FnMut(&SolverType, &SolverType) -> Verdict<()>,
) -> Verdict<()> {
    // A `Self` default lowers to the impl's target, which the match bound to
    // `ty` — and to the newtype a peeled question was asked at, which the
    // inherited impl also spells `Self`.
    let selves: Vec<SolverType> = [Some(ty), subject].into_iter().flatten().cloned().collect();
    // Each side fills its own defaults, so one reading an earlier argument
    // (`B = A`) reads that side's.
    let said = |args: &[SolverType]| -> Vec<Vec<SolverType>> {
        let given = program.per_param(def.trait_, args);
        let filled = match def.trait_ {
            Some(trait_) => program.filled(trait_, given.clone(), &def.target),
            None => given.clone(),
        };
        given
            .iter()
            .chain(std::iter::repeat(&None))
            .zip(filled)
            .map(|(given, filled)| match (given, filled) {
                (Some(given), _) => vec![given.clone()],
                (None, Some(default)) if default == def.target => selves.clone(),
                (None, Some(default)) => vec![default],
                (None, None) => Vec::new(),
            })
            .collect()
    };
    let (asked, offered) = (said(args), said(written));
    let mut answer = Verdict::Holds(());
    for i in 0..asked.len().max(offered.len()) {
        let asks = asked.get(i).map_or(&[][..], Vec::as_slice);
        let offered = offered.get(i).map_or(&[][..], Vec::as_slice);
        // A position the bound leaves open and the trait gives no default is
        // one no bound can name, so every impl answers there.
        if asks.is_empty() {
            continue;
        }
        let pairs: Vec<Verdict<()>> = asks
            .iter()
            .flat_map(|ask| offered.iter().map(move |offer| (ask, offer)))
            .map(|(ask, offer)| fits(ask, offer))
            .collect();
        match any_of(pairs) {
            Verdict::Holds(()) => {}
            Verdict::Fails => return Verdict::Fails,
            Verdict::Unknown => answer = Verdict::Unknown,
        }
    }
    answer
}

/// Whether `offered` is `asked`, a parameter in `asked` fitting any type.
fn fits(asked: &SolverType, offered: &SolverType) -> bool {
    match (asked, offered) {
        (SolverType::Param(_) | SolverType::Pack(_), _) => true,
        (SolverType::Decl(head, args), SolverType::Decl(other, offered)) => {
            head == other && all_fit(args, offered)
        }
        (
            SolverType::Ref { is_mut, inner },
            SolverType::Ref {
                is_mut: other,
                inner: offered,
            },
        ) => is_mut == other && fits(inner, offered),
        (SolverType::Tuple(elems), SolverType::Tuple(offered)) => {
            elems.iter().any(|e| matches!(e, SolverType::Pack(_))) || all_fit(elems, offered)
        }
        _ => asked == offered,
    }
}

fn all_fit(asked: &[SolverType], offered: &[SolverType]) -> bool {
    asked.len() == offered.len() && asked.iter().zip(offered).all(|(a, o)| fits(a, o))
}

/// Whether the impl answers its trait at the declared defaults, as a derived
/// impl does: `impl Eq for D` and `impl<R> Eq<R> for D`, whose `R` the default
/// binds, but not `impl Eq<String> for D`.
pub(super) fn at_defaults(program: &Program, def: &ImplDef) -> bool {
    // The target's own parameters stand for themselves; only one the trait
    // arguments alone name is free to take the default.
    let mut bindings: Vec<Option<Binding>> = (0..def.params.len())
        .map(|index| {
            let index = u32::try_from(index).expect("fewer than 2^32 impl parameters");
            def.target
                .mentions_param(index)
                .then_some(Binding::One(SolverType::Param(index)))
        })
        .collect();
    answers_args_by(
        program,
        def,
        &def.target,
        None,
        &def.trait_args,
        &[],
        |default, written| {
            decided(match_target(
                written,
                default,
                &mut bindings,
                &mut Vec::new(),
                &mut Unifier::standalone(),
            ))
        },
    )
    .settled()
    .is_some()
}

/// What reading a deferred projection through its bound base came to.
enum Through {
    /// Its member matched, binding what it could; the projections nested in
    /// the member are deferred in its place.
    Bound(Vec<(SolverType, SolverType)>),
    /// Its base, or the member it names, is not known yet.
    Open,
    /// Its member does not match the type it met.
    Refuted,
}

/// The base and arguments of `a` and of `b`, paired, where both project one
/// associated type at as many arguments; `None` otherwise.
fn projection_parts<'t>(
    a: &'t SolverType,
    b: &'t SolverType,
) -> Option<Vec<(&'t SolverType, &'t SolverType)>> {
    let (
        SolverType::Projection {
            base,
            trait_,
            assoc,
            args,
        },
        SolverType::Projection {
            base: b_base,
            trait_: b_trait,
            assoc: b_assoc,
            args: b_args,
        },
    ) = (a, b)
    else {
        return None;
    };
    (trait_ == b_trait && assoc == b_assoc && args.len() == b_args.len()).then(|| {
        [(&**base, &**b_base)]
            .into_iter()
            .chain(args.iter().zip(b_args))
            .collect()
    })
}

/// One past the highest parameter or pack index `ty` mentions.
fn params_past(ty: &SolverType) -> u32 {
    match ty {
        SolverType::Param(index) | SolverType::Pack(index) => index + 1,
        SolverType::OpenRow(_) | SolverType::Var(_) => 0,
        SolverType::Decl(_, inner) | SolverType::Tuple(inner) => {
            inner.iter().map(params_past).max().unwrap_or(0)
        }
        SolverType::Ref { inner, .. } => params_past(inner),
        SolverType::Projection { base, args, .. } => args
            .iter()
            .map(params_past)
            .fold(params_past(base), u32::max),
    }
}

/// What one match binds the question's inference variables to. It is local to
/// the match, so asking a question changes no variable.
struct Unifier {
    /// How deep the question whose impl is matched is: a variable minted there
    /// stands for an impl parameter, and binding it constrains nothing asked.
    depth: u32,
    subst: Vec<(Var, SolverType)>,
    /// Whether a part of the match meets a variable at a type no substitution
    /// spells: a pack, or a projection off a variable.
    undecided: bool,
}

impl Unifier {
    fn new(depth: u32) -> Self {
        Self {
            depth,
            subst: Vec::new(),
            undecided: false,
        }
    }

    /// One for a match no question runs, whose bindings nothing reads after it.
    fn standalone() -> Self {
        Self::new(u32::MAX)
    }

    /// What a match that `matched` says of the question's variables.
    fn verdict(&self, matched: bool) -> Verdict<()> {
        if !matched {
            Verdict::Fails
        } else if self.constrains() {
            Verdict::Unknown
        } else {
            Verdict::Holds(())
        }
    }

    /// `ty` with every variable bound here replaced by its binding.
    fn resolve(&self, ty: &SolverType) -> SolverType {
        ty.map_vars(&|var| match self.binding(var) {
            Some(bound) => self.resolve(bound),
            None => SolverType::Var(var),
        })
    }

    fn resolved(&self, binding: &Binding) -> Binding {
        match binding {
            Binding::One(ty) => Binding::One(self.resolve(ty)),
            Binding::Pack(elems) => Binding::Pack(elems.iter().map(|e| self.resolve(e)).collect()),
        }
    }

    /// `ty`, read through the bindings of the variable it is.
    fn shallow(&self, ty: &SolverType) -> SolverType {
        match ty {
            SolverType::Var(var) => match self.binding(*var) {
                Some(bound) => self.shallow(bound),
                None => ty.clone(),
            },
            SolverType::Decl(..)
            | SolverType::Param(_)
            | SolverType::Pack(_)
            | SolverType::Ref { .. }
            | SolverType::Tuple(_)
            | SolverType::Projection { .. }
            | SolverType::OpenRow(_) => ty.clone(),
        }
    }

    fn binding(&self, var: Var) -> Option<&SolverType> {
        self.subst
            .iter()
            .find(|(bound, _)| *bound == var)
            .map(|(_, ty)| ty)
    }

    /// The variable standing for the impl's parameter `param`.
    fn mint(&self, param: u32) -> SolverType {
        SolverType::Var(Var::Local {
            depth: self.depth,
            param,
        })
    }

    fn is_own(&self, var: Var) -> bool {
        matches!(var, Var::Local { depth, .. } if depth == self.depth)
    }

    /// Whether the match holds only at some instances of the question's
    /// variables: it bound one, or left a part undecided.
    fn constrains(&self) -> bool {
        self.undecided || self.subst.iter().any(|(var, _)| !self.is_own(*var))
    }

    /// Bind `var` to `ty`, unless `ty` holds it.
    fn bind(&mut self, var: Var, ty: &SolverType) -> bool {
        let ty = self.resolve(ty);
        if ty == SolverType::Var(var) {
            return true;
        }
        if ty.mentions_var(&|v| v == var) {
            return false;
        }
        self.subst.push((var, ty));
        true
    }

    /// Make `a` and `b`, both in the question's own space, one type. A
    /// parameter is rigid there, a caller's, and meets only itself.
    fn unify(&mut self, a: &SolverType, b: &SolverType) -> bool {
        let (a, b) = (self.shallow(a), self.shallow(b));
        match (&a, &b) {
            (SolverType::Var(x), SolverType::Var(y)) if x == y => true,
            // An impl's own variable takes the other, so binding it stays its own.
            (SolverType::Var(x), SolverType::Var(y)) if self.is_own(*y) && !self.is_own(*x) => {
                self.bind(*y, &a)
            }
            (SolverType::Var(x), _) => self.bind(*x, &b),
            (_, SolverType::Var(y)) => self.bind(*y, &a),
            (SolverType::OpenRow(written), row) | (row, SolverType::OpenRow(written)) => {
                SolverType::open_row_meets(written, row)
            }
            (SolverType::Projection { .. }, _) | (_, SolverType::Projection { .. }) => {
                if a == b {
                    return true;
                }
                if a.is_open() || b.is_open() {
                    self.undecided = true;
                    return true;
                }
                false
            }
            (SolverType::Decl(head, args), SolverType::Decl(other, other_args)) => {
                head == other && self.unify_all(args, other_args)
            }
            (
                SolverType::Ref { is_mut, inner },
                SolverType::Ref {
                    is_mut: other_mut,
                    inner: other,
                },
            ) => is_mut == other_mut && self.unify(inner, other),
            (SolverType::Tuple(elems), SolverType::Tuple(others)) => self.unify_all(elems, others),
            (SolverType::Param(i), SolverType::Param(j))
            | (SolverType::Pack(i), SolverType::Pack(j)) => i == j,
            (
                SolverType::Decl(..)
                | SolverType::Ref { .. }
                | SolverType::Tuple(_)
                | SolverType::Param(_)
                | SolverType::Pack(_),
                _,
            ) => false,
        }
    }

    fn unify_all(&mut self, a: &[SolverType], b: &[SolverType]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(a, b)| self.unify(a, b))
    }
}

/// Whether each pair is one type at every instance of their variables.
fn same(pairs: &[(SolverType, SolverType)]) -> Verdict<()> {
    let mut unifier = Unifier::standalone();
    let matched = pairs.iter().all(|(a, b)| unifier.unify(a, b));
    unifier.verdict(matched)
}

/// Holds where one of `verdicts` holds, fails where each fails.
fn any_of(verdicts: impl IntoIterator<Item = Verdict<()>>) -> Verdict<()> {
    let mut answer = Verdict::Fails;
    for verdict in verdicts {
        match verdict {
            Verdict::Holds(()) => return Verdict::Holds(()),
            Verdict::Unknown => answer = Verdict::Unknown,
            Verdict::Fails => {}
        }
    }
    answer
}

/// Match an impl target against a type, binding the target's parameters by
/// position. A projection the target writes, inside an `<F: fn(...)>`
/// signature, is decided only once its base is bound, so it is left in
/// `deferred` beside the type it met. A variable the target meets is bound to
/// the target, its parameters minted as variables of their own.
fn match_target(
    target: &SolverType,
    ty: &SolverType,
    bindings: &mut [Option<Binding>],
    deferred: &mut Vec<(SolverType, SolverType)>,
    unifier: &mut Unifier,
) -> bool {
    let ty = &unifier.shallow(ty);
    let mut each = |a: &SolverType, b: &SolverType| match_target(a, b, bindings, deferred, unifier);
    match (target, ty) {
        (SolverType::Var(_), _) => unreachable!("an impl target holds no inference variable"),
        (SolverType::Param(index), _) => match &bindings[*index as usize] {
            Some(Binding::One(bound)) => {
                let bound = bound.clone();
                unifier.unify(&bound, ty)
            }
            Some(Binding::Pack(_)) => false,
            None => {
                bindings[*index as usize] = Some(Binding::One(ty.clone()));
                true
            }
        },
        (SolverType::Projection { .. }, _) => {
            deferred.push((target.clone(), ty.clone()));
            true
        }
        (SolverType::OpenRow(written), row) | (row, SolverType::OpenRow(written)) => {
            SolverType::open_row_meets(written, row)
        }
        (
            SolverType::Decl(..)
            | SolverType::Ref { .. }
            | SolverType::Tuple(_)
            | SolverType::Pack(_),
            SolverType::Var(var),
        ) => bind_to_target(target, *var, bindings, unifier),
        (
            SolverType::Decl(..)
            | SolverType::Ref { .. }
            | SolverType::Tuple(_)
            | SolverType::Pack(_),
            SolverType::Projection { .. },
        ) if ty.is_open() => {
            unifier.undecided = true;
            true
        }
        (SolverType::Decl(head, args), SolverType::Decl(ty_head, ty_args)) => {
            head == ty_head
                && args.len() == ty_args.len()
                && args.iter().zip(ty_args).all(|(a, b)| each(a, b))
        }
        (
            SolverType::Ref { is_mut, inner },
            SolverType::Ref {
                is_mut: ty_mut,
                inner: ty_inner,
            },
        ) => is_mut == ty_mut && each(inner, ty_inner),
        (SolverType::Tuple(elems), SolverType::Tuple(ty_elems)) => match elems.split_last() {
            Some((SolverType::Pack(index), prefix)) => {
                ty_elems.len() >= prefix.len()
                    && prefix.iter().zip(ty_elems).all(|(a, b)| each(a, b))
                    && {
                        bindings[*index as usize] =
                            Some(Binding::Pack(ty_elems[prefix.len()..].to_vec()));
                        true
                    }
            }
            _ => {
                elems.len() == ty_elems.len() && elems.iter().zip(ty_elems).all(|(a, b)| each(a, b))
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
            | SolverType::Tuple(_)
            | SolverType::Projection { .. },
        ) => false,
    }
}

/// Bind `var` to `target`, each of the target's parameters still unbound
/// minted as a variable of its own. A pack or a projection in the target has
/// no instance a substitution spells, so the match is left undecided there.
fn bind_to_target(
    target: &SolverType,
    var: Var,
    bindings: &mut [Option<Binding>],
    unifier: &mut Unifier,
) -> bool {
    if target.mentions(&|p| matches!(p, SolverType::Pack(_))) || has_projection(target) {
        unifier.undecided = true;
        return true;
    }
    let instance = target.map_params(&|index| match &bindings[index as usize] {
        Some(Binding::One(bound)) => Some(bound.clone()),
        Some(Binding::Pack(_)) => unreachable!("a target holding a pack is undecided"),
        None => Some(unifier.mint(index)),
    });
    for (index, slot) in bindings.iter_mut().enumerate() {
        let index = u32::try_from(index).expect("fewer than 2^32 impl parameters");
        if slot.is_none() && target.mentions_param(index) {
            *slot = Some(Binding::One(unifier.mint(index)));
        }
    }
    unifier.bind(var, &instance.expect("every parameter maps"))
}

/// Whether `ty` holds a projection.
fn has_projection(ty: &SolverType) -> bool {
    match ty {
        SolverType::Projection { .. } => true,
        SolverType::Decl(_, inner) | SolverType::Tuple(inner) => inner.iter().any(has_projection),
        SolverType::Ref { inner, .. } => has_projection(inner),
        SolverType::Param(_)
        | SolverType::Pack(_)
        | SolverType::OpenRow(_)
        | SolverType::Var(_) => false,
    }
}

/// Whether `def`'s target meets `ty` without a projection left to decide, at
/// every instance of `ty`'s variables. A projection is decided only against
/// the impl's bindings, which [`Query::impl_answers`] checks, so a target
/// writing one reaches nothing here.
fn reaches_target(def: &ImplDef, ty: &SolverType) -> Verdict<()> {
    let mut deferred = Vec::new();
    let mut unifier = Unifier::standalone();
    let matched = match_target(
        &def.target,
        ty,
        &mut vec![None; def.params.len()],
        &mut deferred,
        &mut unifier,
    );
    unifier.verdict(matched && deferred.is_empty())
}

#[cfg(test)]
mod tests {
    use super::super::program::{ArgDefault, Fact, ParamDef, Pin, TraitDef, TypeDef};
    use super::super::testing::{Builder, concrete, decl, generic, holds, holds_with_args, ref_to};
    use super::holds as verdict_of;
    use super::*;

    const ALPHA: TraitDeclId = TraitDeclId(0);
    const BETA: TraitDeclId = TraitDeclId(1);
    const BASE: TraitDeclId = TraitDeclId(2);
    const SUB: TraitDeclId = TraitDeclId(3);
    const EQ: TraitDeclId = TraitDeclId(4);
    const MARKER: TraitDeclId = TraitDeclId(5);
    const POINT: TypeDeclId = TypeDeclId(0);
    const LIST: TypeDeclId = TypeDeclId(1);
    const I32: TypeDeclId = TypeDeclId(2);

    fn list_of(inner: SolverType) -> SolverType {
        SolverType::Decl(LIST, vec![inner])
    }

    fn env(bounds: Vec<Vec<TraitDeclId>>) -> Env {
        Env {
            param_bounds: bounds
                .into_iter()
                .map(|b| b.into_iter().map(ParamBound::bare).collect())
                .collect(),
            param_assoc: Vec::new(),
        }
    }

    const PAIR: TypeDeclId = TypeDeclId(3);

    fn pair(a: SolverType, b: SolverType) -> SolverType {
        SolverType::Decl(PAIR, vec![a, b])
    }

    fn var(id: u32) -> SolverType {
        SolverType::Var(Var::Infer { id, literal: false })
    }

    fn verdict(p: &Program, ty: &SolverType, trait_: TraitDeclId) -> Verdict<()> {
        verdict_of(p, &Env::default(), ty, trait_).map(|_| ())
    }

    /// An impl written for one instance applies to a variable only where the
    /// variable becomes that instance.
    #[test]
    fn a_variable_meeting_a_written_instance_is_not_yet_known() {
        let p = Builder::default().concrete(ALPHA, decl(POINT)).build();
        assert_eq!(verdict(&p, &var(0), ALPHA), Verdict::Unknown);
        assert_eq!(
            verdict(&p, &list_of(var(0)), ALPHA),
            Verdict::Fails,
            "no instance of `List<?0>` is `Point`"
        );
    }

    /// A variable no impl answers is still not refuted: `!` holds every trait.
    #[test]
    fn a_bare_variable_is_never_refuted() {
        let p = Program::default();
        assert_eq!(verdict(&p, &var(0), ALPHA), Verdict::Unknown);
    }

    /// An impl at every instance of the head holds whatever the variable
    /// inside becomes, and a value blanket does not: the variable may yet be a
    /// reference, which a value blanket does not answer.
    #[test]
    fn an_impl_at_every_instance_holds_of_a_variable_inside() {
        let p = Builder::default()
            .impl_(generic(1, concrete(ALPHA, list_of(SolverType::Param(0)))))
            .impl_(generic(1, concrete(BETA, SolverType::Param(0))))
            .build();
        assert_eq!(verdict(&p, &list_of(var(0)), ALPHA), Verdict::Holds(()));
        assert_eq!(verdict(&p, &list_of(var(0)), BETA), Verdict::Holds(()));
        assert_eq!(verdict(&p, &var(0), BETA), Verdict::Unknown);
    }

    /// Two occurrences of one variable are one type, so a parameter the target
    /// writes twice binds once.
    #[test]
    fn a_variable_is_one_type_wherever_it_occurs() {
        let same = generic(
            1,
            concrete(ALPHA, pair(SolverType::Param(0), SolverType::Param(0))),
        );
        let p = Builder::default().impl_(same).build();
        assert_eq!(
            verdict(&p, &pair(var(0), var(0)), ALPHA),
            Verdict::Holds(())
        );
        assert_eq!(verdict(&p, &pair(var(0), var(1)), ALPHA), Verdict::Unknown);
        assert_eq!(
            verdict(&p, &pair(var(0), decl(POINT)), ALPHA),
            Verdict::Unknown
        );
        assert_eq!(
            verdict(&p, &pair(list_of(var(0)), decl(POINT)), ALPHA),
            Verdict::Fails
        );
    }

    /// A bound failing at every instance refutes the impl, whatever a bound
    /// asked before it left unknown: neither answer depends on the other.
    #[test]
    fn a_bound_failing_everywhere_refutes_past_one_not_yet_known() {
        let p = Builder::default()
            .concrete(BASE, pair(decl(POINT), decl(POINT)))
            .impl_(ImplDef {
                params: vec![ParamDef::bounded(vec![BASE, BETA])],
                ..concrete(ALPHA, SolverType::Param(0))
            })
            .build();
        assert_eq!(verdict(&p, &pair(var(0), var(1)), BASE), Verdict::Unknown);
        assert_eq!(verdict(&p, &pair(var(0), var(1)), ALPHA), Verdict::Fails);
    }

    /// A variable never becomes a type holding itself.
    #[test]
    fn a_variable_does_not_bind_to_a_type_holding_it() {
        let target = pair(SolverType::Param(0), list_of(SolverType::Param(0)));
        let p = Builder::default()
            .impl_(generic(1, concrete(ALPHA, target)))
            .build();
        assert_eq!(verdict(&p, &pair(var(0), var(0)), ALPHA), Verdict::Fails);
    }

    fn only_impl(p: &Program) -> &ImplDef {
        let [def] = p.impls().values().collect::<Vec<_>>()[..] else {
            panic!("the program writes one impl");
        };
        def
    }

    /// Committing to an impl settles a variable where its target meets the
    /// receiver only at some instances of it; a literal's may be settled, to a
    /// type a literal may be.
    #[test]
    fn an_impl_settles_a_variable_its_target_writes_a_type_at() {
        let mut written = Builder::default()
            .concrete(ALPHA, list_of(decl(I32)))
            .build();
        let literal = SolverType::Var(Var::Infer {
            id: 0,
            literal: true,
        });
        assert!(!settles_nothing(
            &written,
            only_impl(&written),
            &list_of(var(0))
        ));
        assert!(!settles_nothing(
            &written,
            only_impl(&written),
            &list_of(literal.clone())
        ));
        written.literal_types.insert(I32);
        assert!(settles_nothing(
            &written,
            only_impl(&written),
            &list_of(literal)
        ));
        let over_any = Builder::default()
            .impl_(generic(1, concrete(ALPHA, list_of(SolverType::Param(0)))))
            .build();
        assert!(settles_nothing(
            &over_any,
            only_impl(&over_any),
            &list_of(var(0))
        ));
        let twin = Builder::default()
            .impl_(generic(
                1,
                concrete(ALPHA, pair(SolverType::Param(0), SolverType::Param(0))),
            ))
            .build();
        assert!(!settles_nothing(
            &twin,
            only_impl(&twin),
            &pair(var(0), var(1))
        ));
    }

    /// `impl<T: Alpha> Alpha for List<T>` asks of `?0` what it asks of `T`,
    /// at a fresh variable each step down.
    #[test]
    fn a_question_repeating_at_fresh_variables_is_not_yet_known() {
        let p = Builder::default()
            .bounded(ALPHA, list_of(SolverType::Param(0)), vec![ALPHA])
            .build();
        assert_eq!(verdict(&p, &var(0), ALPHA), Verdict::Unknown);
    }

    /// A bound refutes the impl at the only instance it applies at.
    #[test]
    fn a_bound_refutes_at_the_instance_the_match_binds() {
        let p = Builder::default()
            .bounded(
                ALPHA,
                pair(SolverType::Param(0), SolverType::Param(0)),
                vec![BETA],
            )
            .concrete(BETA, decl(I32))
            .build();
        assert_eq!(
            verdict(&p, &pair(var(0), decl(POINT)), ALPHA),
            Verdict::Fails
        );
        assert_eq!(
            verdict(&p, &pair(var(0), decl(I32)), ALPHA),
            Verdict::Unknown
        );
    }

    /// A bound not yet known leaves the impl not yet known.
    #[test]
    fn a_bound_not_yet_known_leaves_the_impl_unknown() {
        let p = Builder::default()
            .bounded(ALPHA, list_of(SolverType::Param(0)), vec![BETA])
            .concrete(BETA, decl(I32))
            .build();
        assert_eq!(verdict(&p, &list_of(var(0)), ALPHA), Verdict::Unknown);
        assert_eq!(verdict(&p, &list_of(decl(I32)), ALPHA), Verdict::Holds(()));
    }

    /// Asking with a variable binds nothing the asker sees: the same question
    /// asked twice answers the same.
    #[test]
    fn asking_changes_no_variable() {
        let p = Builder::default().concrete(ALPHA, decl(POINT)).build();
        let ty = list_of(var(0));
        let first = verdict(&p, &ty, ALPHA);
        assert_eq!(verdict(&p, &ty, ALPHA), first);
        assert_eq!(verdict(&p, &var(0), ALPHA), Verdict::Unknown);
    }

    /// A bound writing no argument says the trait’s declared defaults, so a
    /// request writing one it does not say goes unanswered.
    #[test]
    fn a_bare_bound_does_not_answer_a_written_argument() {
        let mut p = Program::default();
        p.traits.entry(EQ).or_default().arg_defaults = vec![Some(ArgDefault::SelfType)];
        let env = Env {
            param_bounds: vec![vec![ParamBound::bare(EQ)]],
            param_assoc: Vec::new(),
        };
        assert_eq!(
            holds_with_args(&p, &env, &SolverType::Param(0), EQ, &[]),
            Some(Holds::default())
        );
        assert_eq!(
            holds_with_args(&p, &env, &SolverType::Param(0), EQ, &[decl(I32)]),
            None
        );
    }

    /// An impl leaving the argument open answers a bare bound at the default
    /// it binds: `impl<R: Alpha> Eq<R> for Point` answers `Point: Eq` where
    /// `Point: Alpha`.
    #[test]
    fn an_open_impl_answers_a_bare_bound_at_the_default() {
        let open = |target| ImplDef {
            trait_args: vec![SolverType::Param(0)],
            params: vec![ParamDef::bounded(vec![ALPHA])],
            ..concrete(EQ, target)
        };
        let mut p = Builder::default()
            .concrete(ALPHA, decl(POINT))
            .impl_(open(decl(POINT)))
            .impl_(open(decl(I32)))
            .build();
        p.traits.entry(EQ).or_default().arg_defaults = vec![Some(ArgDefault::SelfType)];
        let ask = |ty| holds_with_args(&p, &Env::default(), &decl(ty), EQ, &[]);
        assert_eq!(ask(POINT), Some(Holds::default()));
        assert_eq!(ask(I32), None);
    }

    /// The bound writing the argument answers it, and only it.
    #[test]
    fn a_written_bound_answers_its_own_argument() {
        let p = Program::default();
        let env = Env {
            param_bounds: vec![vec![ParamBound {
                trait_: EQ,
                args: vec![decl(I32)],
            }]],
            param_assoc: Vec::new(),
        };
        assert_eq!(
            holds_with_args(&p, &env, &SolverType::Param(0), EQ, &[decl(I32)]),
            Some(Holds::default())
        );
        assert_eq!(
            holds_with_args(&p, &env, &SolverType::Param(0), EQ, &[decl(POINT)]),
            None
        );
    }

    #[test]
    fn nothing_holds_in_an_empty_program() {
        let p = Program::default();
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), ALPHA), None);
    }

    #[test]
    fn an_impl_written_for_the_type_answers() {
        let p = Builder::default().concrete(ALPHA, decl(POINT)).build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), ALPHA),
            Some(Holds::default())
        );
        assert_eq!(holds(&p, &Env::default(), &decl(I32), ALPHA), None);
    }

    /// An impl of `Sub` owes one of `Base` beside it, and does not stand in
    /// for it: where that is missing, the `Sub` impl is the error.
    #[test]
    fn an_impl_of_a_subtrait_does_not_answer_its_supertrait() {
        let p = Builder::default()
            .supertrait(SUB, BASE)
            .concrete(SUB, decl(POINT))
            .build();
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), BASE), None);
    }

    /// A generic body's parameter holds by its own signature, not by any impl.
    #[test]
    fn a_bound_in_force_answers_for_a_parameter() {
        let p = Program::default();
        let e = env(vec![vec![ALPHA]]);
        assert_eq!(
            holds(&p, &e, &SolverType::Param(0), ALPHA),
            Some(Holds::default())
        );
        assert_eq!(holds(&p, &e, &SolverType::Param(0), BETA), None);
    }

    /// A pack's bound holds of each element, so inside a variadic body the
    /// pack's own tuple satisfies an impl bounding its elements.
    #[test]
    fn a_bound_in_force_answers_for_a_packs_elements() {
        let p = Builder::default()
            .bounded(
                BETA,
                SolverType::Tuple(vec![SolverType::Pack(0)]),
                vec![ALPHA],
            )
            .build();
        let own = SolverType::Tuple(vec![SolverType::Pack(0)]);
        assert_eq!(
            holds(&p, &env(vec![vec![ALPHA]]), &own, BETA),
            Some(Holds::default())
        );
        assert_eq!(holds(&p, &env(vec![vec![]]), &own, BETA), None);
    }

    #[test]
    fn a_bound_in_force_answers_for_its_supertraits() {
        let p = Builder::default().supertrait(SUB, BASE).build();
        assert_eq!(
            holds(&p, &env(vec![vec![SUB]]), &SolverType::Param(0), BASE),
            Some(Holds::default())
        );
    }

    /// A supertrait clause writing an argument answers the supertrait at that
    /// argument, and only there — `trait AsStrSlice: Eq<String>`.
    #[test]
    fn a_supertrait_clause_answers_at_the_arguments_it_writes() {
        let p = Builder::default()
            .supertrait_args(
                SUB,
                ParamBound {
                    trait_: BASE,
                    args: vec![decl(I32)],
                },
            )
            .build();
        let bound = env(vec![vec![SUB]]);
        let ask =
            |args: &[SolverType]| holds_with_args(&p, &bound, &SolverType::Param(0), BASE, args);
        assert_eq!(ask(&[]), Some(Holds::default()));
        assert_eq!(ask(&[decl(I32)]), Some(Holds::default()));
        assert_eq!(ask(&[decl(POINT)]), None);
    }

    /// A clause writing the subtrait's own parameter answers at what the bound
    /// supplies for it, through every step of a chain:
    /// `trait Top<X>: Sub<X>`, `trait Sub<Y>: Base<Y>`, and `T: Top<i32>`.
    #[test]
    fn a_supertrait_clause_answers_at_the_bounds_arguments() {
        let p = Builder::default()
            .supertrait_args(
                ALPHA,
                ParamBound {
                    trait_: SUB,
                    args: vec![SolverType::Param(0)],
                },
            )
            .supertrait_args(
                SUB,
                ParamBound {
                    trait_: BASE,
                    args: vec![SolverType::Param(0)],
                },
            )
            .build();
        let bound = Env {
            param_bounds: vec![vec![ParamBound {
                trait_: ALPHA,
                args: vec![decl(I32)],
            }]],
            param_assoc: Vec::new(),
        };
        let ask = |trait_: TraitDeclId, args: &[SolverType]| {
            holds_with_args(&p, &bound, &SolverType::Param(0), trait_, args)
        };
        assert_eq!(ask(SUB, &[decl(I32)]), Some(Holds::default()));
        assert_eq!(ask(BASE, &[decl(I32)]), Some(Holds::default()));
        assert_eq!(ask(BASE, &[decl(POINT)]), None);
    }

    /// A parameter the bound leaves at a `Self` default is the bounded type
    /// itself, down the clause: `trait Top<X = Self>: Sub<X>` and `T: Top`
    /// answer `Sub<T>`.
    #[test]
    fn a_self_default_reaches_a_clause_as_the_subject() {
        let mut p = Builder::default()
            .supertrait_args(
                ALPHA,
                ParamBound {
                    trait_: SUB,
                    args: vec![SolverType::Param(0)],
                },
            )
            .build();
        p.traits.entry(ALPHA).or_default().arg_defaults = vec![Some(ArgDefault::SelfType)];
        let bound = env(vec![vec![ALPHA]]);
        let ask =
            |args: &[SolverType]| holds_with_args(&p, &bound, &SolverType::Param(0), SUB, args);
        assert_eq!(ask(&[SolverType::Param(0)]), Some(Holds::default()));
        assert_eq!(ask(&[decl(I32)]), None);
    }

    /// A clause spreading the subtrait's pack reads the run of arguments the
    /// pack absorbs: `trait Alpha<..X>: Sub<[..X]>` and `T: Alpha<i32, Point>`
    /// answer `Sub<[i32, Point]>`.
    #[test]
    fn a_pack_spread_in_a_clause_reads_the_absorbed_arguments() {
        let mut p = Builder::default()
            .supertrait_args(
                ALPHA,
                ParamBound {
                    trait_: SUB,
                    args: vec![SolverType::Tuple(vec![SolverType::Pack(0)])],
                },
            )
            .build();
        let alpha = p.traits.entry(ALPHA).or_default();
        alpha.arg_defaults = vec![None];
        alpha.pack = Some(0);
        let bound = Env {
            param_bounds: vec![vec![ParamBound {
                trait_: ALPHA,
                args: vec![decl(I32), decl(POINT)],
            }]],
            param_assoc: Vec::new(),
        };
        let ask =
            |args: &[SolverType]| holds_with_args(&p, &bound, &SolverType::Param(0), SUB, args);
        let pair = SolverType::Tuple(vec![decl(I32), decl(POINT)]);
        assert_eq!(ask(&[pair]), Some(Holds::default()));
        assert_eq!(ask(&[SolverType::Tuple(vec![decl(I32)])]), None);
    }

    /// A pack the bound stops short of absorbs nothing and moves no default:
    /// `trait Alpha<A = i32, ..P>: Sub<A>` and `T: Alpha` answer `Sub<i32>`.
    #[test]
    fn a_pack_past_the_written_arguments_leaves_the_defaults_in_place() {
        let mut p = Builder::default()
            .supertrait_args(
                ALPHA,
                ParamBound {
                    trait_: SUB,
                    args: vec![SolverType::Param(0)],
                },
            )
            .build();
        let alpha = p.traits.entry(ALPHA).or_default();
        alpha.arg_defaults = vec![Some(ArgDefault::Type(decl(I32))), None];
        alpha.pack = Some(1);
        let bound = env(vec![vec![ALPHA]]);
        let ask =
            |args: &[SolverType]| holds_with_args(&p, &bound, &SolverType::Param(0), SUB, args);
        assert_eq!(ask(&[decl(I32)]), Some(Holds::default()));
        assert_eq!(ask(&[SolverType::Tuple(Vec::new())]), None);
    }

    /// A clause writing no argument says the supertrait's declared defaults, so
    /// `T: Ord` does not answer `Eq<i32>`.
    #[test]
    fn a_bare_supertrait_clause_does_not_answer_a_written_argument() {
        let mut p = Builder::default().supertrait(SUB, BASE).build();
        p.traits.entry(BASE).or_default().arg_defaults = vec![Some(ArgDefault::SelfType)];
        let bound = env(vec![vec![SUB]]);
        let ask =
            |args: &[SolverType]| holds_with_args(&p, &bound, &SolverType::Param(0), BASE, args);
        assert_eq!(ask(&[]), Some(Holds::default()));
        assert_eq!(ask(&[decl(I32)]), None);
    }

    /// The ways a bound holds that are properties of the type arrive as facts,
    /// and a fact's answer owes the body, as a derived impl's does.
    #[test]
    fn a_fact_answers_and_owes_the_body() {
        let p = Builder::default().fact(POINT, EQ, Fact::default()).build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), EQ),
            Some(Holds {
                requests: vec![DerivationRequest {
                    ty: decl(POINT),
                    trait_: EQ,
                    structural: false,
                }],
                ..Holds::default()
            })
        );
        assert_eq!(holds(&p, &Env::default(), &decl(I32), EQ), None);
    }

    /// A fact is stated of a declaration and answers for every instance:
    /// `Pair<String>` is a struct because `Pair` is.
    #[test]
    fn a_fact_answers_for_every_instance_of_its_declaration() {
        let p = Builder::default()
            .fact(LIST, ALPHA, Fact::default())
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &list_of(decl(I32)), ALPHA),
            Some(Holds {
                requests: vec![DerivationRequest {
                    ty: list_of(decl(I32)),
                    trait_: ALPHA,
                    structural: false,
                }],
                ..Holds::default()
            })
        );
    }

    /// `&T` holds a bound `T` holds, by auto-deref at the call — unless the
    /// trait says otherwise: the `Ref` markers hold of every reference, and
    /// `Ord` of none. A reference's own impl is asked first.
    #[test]
    fn a_reference_answers_by_the_trait_s_reference_rule() {
        let mut p = Builder::default()
            .concrete(ALPHA, decl(POINT))
            .concrete(BETA, decl(POINT))
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
            MARKER,
            TraitDef {
                on_ref: RefRule::Always,
                ..TraitDef::default()
            },
        );
        let ask = |ty: SolverType, trait_: TraitDeclId| holds(&p, &Env::default(), &ty, trait_);
        assert_eq!(ask(ref_to(decl(POINT)), ALPHA), Some(Holds::default()));
        assert_eq!(ask(ref_to(decl(I32)), ALPHA), None);
        assert_eq!(ask(ref_to(decl(POINT)), BETA), None);
        assert_eq!(ask(ref_to(decl(I32)), MARKER), Some(Holds::default()));
        assert_eq!(ask(decl(I32), MARKER), None);
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
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), BETA), None);

        let p = Builder::default()
            .bounded(BETA, SolverType::Param(0), vec![ALPHA])
            .concrete(ALPHA, decl(POINT))
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), BETA),
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
            holds(&p, &Env::default(), &list_of(decl(I32)), EQ),
            Some(Holds::default())
        );
        assert_eq!(holds(&p, &Env::default(), &list_of(decl(POINT)), EQ), None);
    }

    /// Nested, so the recursion is more than one step deep.
    #[test]
    fn a_bounded_head_impl_descends_as_far_as_it_needs() {
        let p = Builder::default()
            .bounded(EQ, list_of(SolverType::Param(0)), vec![EQ])
            .concrete(EQ, decl(I32))
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &list_of(list_of(decl(I32))), EQ),
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
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), ALPHA), None);
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), BETA), None);
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
            holds(&p, &Env::default(), &decl(POINT), BETA),
            Some(Holds::default())
        );
        assert_eq!(holds(&p, &Env::default(), &decl(I32), BETA), None);
    }

    /// A blanket keyed on the trait it implements is its own cycle.
    #[test]
    fn a_blanket_bounded_by_its_own_trait_holds_of_nothing() {
        let p = Builder::default()
            .bounded(ALPHA, SolverType::Param(0), vec![ALPHA])
            .build();
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), ALPHA), None);
    }

    /// A request found behind a blanket reaches the caller: the body is owed
    /// whether the answer came directly or through a chain.
    #[test]
    fn a_request_travels_out_through_a_blanket() {
        let request = DerivationRequest {
            ty: decl(POINT),
            trait_: EQ,
            structural: false,
        };
        let p = Builder::default()
            .bounded(BETA, SolverType::Param(0), vec![EQ])
            .fact(POINT, EQ, Fact::default())
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), BETA),
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
            holds(&p, &Env::default(), &pair(decl(I32), decl(I32)), ALPHA),
            Some(Holds::default())
        );
        assert_eq!(
            holds(&p, &Env::default(), &pair(decl(I32), decl(POINT)), ALPHA),
            None
        );
    }

    #[test]
    fn a_reference_impl_answers_only_for_a_reference() {
        let p = Builder::default()
            .bounded(ALPHA, ref_to(SolverType::Param(0)), vec![])
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &ref_to(decl(POINT)), ALPHA),
            Some(Holds::default())
        );
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), ALPHA), None);
        let mut_ref = SolverType::Ref {
            is_mut: true,
            inner: Box::new(decl(POINT)),
        };
        assert_eq!(holds(&p, &Env::default(), &mut_ref, ALPHA), None);
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
            holds(&p, &Env::default(), &decl(POINT), EQ),
            Some(Holds {
                requests: vec![DerivationRequest {
                    ty: decl(POINT),
                    trait_: EQ,
                    structural: false,
                }],
                ..Holds::default()
            })
        );
    }

    /// A derived `Sub` body owes the derived `Base` body beside it, so a
    /// generic body bound by `Sub` finds `Base` there too.
    #[test]
    fn an_owed_body_owes_its_supertraits_body() {
        let derived = |trait_| ImplDef {
            origin: ImplOrigin::Derived,
            ..concrete(trait_, decl(POINT))
        };
        let p = Builder::default()
            .supertrait(SUB, BASE)
            .impl_(derived(SUB))
            .impl_(derived(BASE))
            .build();
        let env = Env::default();
        let held = holds(&p, &env, &decl(POINT), SUB).expect("`Sub` holds");
        let owed: Vec<TraitDeclId> = owed(&p, &env, held.requests)
            .into_iter()
            .map(|request| request.trait_)
            .collect();
        assert_eq!(owed, [SUB, BASE]);
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
            holds(&p, &Env::default(), &decl(POINT), ALPHA),
            Some(Holds::default())
        );
        assert_eq!(
            holds(&p, &Env::default(), &SolverType::Param(3), ALPHA),
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
            holds(&p, &Env::default(), &decl(DURATION), ALPHA),
            Some(Holds::default())
        );
    }

    /// A newtype writing `eq` alone withholds its base's `Ord`, and only that.
    #[test]
    fn a_newtype_does_not_inherit_what_it_withholds() {
        const COARSE: TypeDeclId = TypeDeclId(9);
        let mut p = Builder::default()
            .concrete(ALPHA, decl(I32))
            .concrete(BETA, decl(I32))
            .build();
        p.types.insert(
            COARSE,
            TypeDef {
                newtype_base: Some(decl(I32)),
            },
        );
        p.comparisons = Some((EQ, ALPHA));
        p.push_impl(concrete(EQ, decl(COARSE)));
        assert_eq!(holds(&p, &Env::default(), &decl(COARSE), ALPHA), None);
        assert_eq!(
            holds(&p, &Env::default(), &decl(COARSE), BETA),
            Some(Holds::default())
        );
    }

    /// A withheld trait is not there, a marker asking for it included; a
    /// written impl is the one thing that answers it.
    #[test]
    fn only_a_written_impl_answers_what_is_withheld() {
        let mut p = Builder::default().build();
        p.comparisons = Some((EQ, ALPHA));
        p.push_impl(ImplDef {
            origin: ImplOrigin::Marker,
            ..concrete(ALPHA, decl(POINT))
        });
        assert!(holds(&p, &Env::default(), &decl(POINT), ALPHA).is_some());
        p.push_impl(concrete(EQ, decl(POINT)));
        assert_eq!(holds(&p, &Env::default(), &decl(POINT), ALPHA), None);
        p.push_impl(concrete(ALPHA, decl(POINT)));
        assert!(holds(&p, &Env::default(), &decl(POINT), ALPHA).is_some());
    }

    /// Each instance reads its own row, by the written impls reaching it; a
    /// concrete impl answers before a generic one.
    #[test]
    fn each_instance_reads_its_own_comparison_row() {
        const ORD: TraitDeclId = BETA;
        let mut p = Builder::default().build();
        p.comparisons = Some((EQ, ORD));
        let ord_for_all = p.push_impl(generic(1, concrete(ORD, list_of(SolverType::Param(0)))));
        p.push_impl(concrete(EQ, list_of(decl(I32))));
        assert_eq!(
            comparison_row(&p, &list_of(decl(POINT))),
            Some((ORD, ord_for_all))
        );
        assert_eq!(comparison_row(&p, &list_of(decl(I32))), None);
        assert_eq!(comparison_row(&p, &ref_to(list_of(decl(POINT)))), None);

        let mut p = Builder::default().build();
        p.comparisons = Some((EQ, ORD));
        p.push_impl(generic(1, concrete(EQ, list_of(SolverType::Param(0)))));
        let eq_for_i32 = p.push_impl(concrete(EQ, list_of(decl(I32))));
        assert_eq!(
            comparison_row(&p, &list_of(decl(I32))),
            Some((EQ, eq_for_i32))
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
                EQ
            ),
            Some(Holds::default())
        );
        assert_eq!(
            holds(
                &p,
                &Env::default(),
                &SolverType::Decl(MY_LIST, vec![decl(POINT)]),
                EQ
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
            holds(&p, &Env::default(), &decl(DURATION), ALPHA),
            Some(Holds {
                requests: vec![DerivationRequest {
                    ty: decl(DURATION),
                    trait_: ALPHA,
                    structural: false,
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
                holds(&p, &Env::default(), &tuple, ALPHA),
                Some(Holds::default()),
                "{tuple:?}"
            );
        }
        assert_eq!(holds(&p, &Env::default(), &decl(I32), ALPHA), None);
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
            holds(&p, &Env::default(), &SolverType::Tuple(vec![]), ALPHA),
            None
        );
        assert_eq!(
            holds(
                &p,
                &Env::default(),
                &SolverType::Tuple(vec![decl(I32)]),
                ALPHA
            ),
            Some(Holds::default())
        );
        assert_eq!(
            holds(
                &p,
                &Env::default(),
                &SolverType::Tuple(vec![decl(I32), decl(POINT), decl(POINT)]),
                ALPHA
            ),
            Some(Holds::default())
        );
        // The prefix parameter's bound is checked: `Point` is not `Beta`.
        assert_eq!(
            holds(
                &p,
                &Env::default(),
                &SolverType::Tuple(vec![decl(POINT)]),
                ALPHA
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
        let ask =
            |elems: Vec<SolverType>| holds(&p, &Env::default(), &SolverType::Tuple(elems), ALPHA);
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
        assert_eq!(holds(&p, &Env::default(), &decl(CM), MUL), None);
        assert_eq!(
            holds(&p, &Env::default(), &decl(INCH), MUL),
            Some(Holds::default())
        );
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), MUL),
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
                    bounds: vec![ParamBound::bare(MUL)],
                    pins: vec![Pin {
                        trait_: MUL,
                        assoc: OUTPUT,
                        ty: SolverType::Param(0),
                    }],
                    copyable: false,
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
        assert_eq!(holds(&p, &Env::default(), &decl(CM), PRODUCT), None);
        assert_eq!(
            holds(&p, &Env::default(), &decl(INCH), PRODUCT),
            Some(Holds::default())
        );
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), PRODUCT),
            Some(Holds::default())
        );
        // The answer reports what the impl binds, at the receiver.
        assert_eq!(
            holds(&p, &Env::default(), &decl(CM), MUL).and_then(|held| held.assoc_at(OUTPUT, &[])),
            Some(decl(AREA))
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
                        bounds: vec![ParamBound::bare(MUL)],
                        pins: vec![Pin {
                            trait_: MUL,
                            assoc: OUTPUT,
                            ty: SolverType::Param(1),
                        }],
                        copyable: false,
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
            holds(&p, &Env::default(), &decl(CM), PRODUCT),
            Some(Holds::default())
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
        assert_eq!(holds(&p, &Env::default(), &list_of(decl(I32)), BETA), None);
        let p = Builder::default().impl_(unsupplied(vec![])).build();
        assert_eq!(
            holds(&p, &Env::default(), &list_of(decl(I32)), BETA),
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
                        bounds: vec![ParamBound::bare(ALPHA)],
                        pins: vec![Pin {
                            trait_: ALPHA,
                            assoc: FIELDS,
                            ty: SolverType::Tuple(vec![SolverType::Pack(1)]),
                        }],
                        copyable: false,
                    },
                    ParamDef::bounded(vec![ARBITRARY]),
                ],
                ..concrete(ARBITRARY, SolverType::Param(0))
            })
            .fact(POINT, ALPHA, Fact::default())
            .build();
        assert_eq!(
            holds(&p, &Env::default(), &decl(POINT), ARBITRARY),
            Some(Holds {
                requests: vec![DerivationRequest {
                    ty: decl(POINT),
                    trait_: ALPHA,
                    structural: false,
                }],
                ..Holds::default()
            })
        );
        assert_eq!(holds(&p, &Env::default(), &decl(I32), ARBITRARY), None);
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
            holds(&p, &Env::default(), &ref_to(decl(POINT)), BETA),
            Some(Holds::default())
        );
        p.traits.insert(
            BETA,
            TraitDef {
                on_ref: RefRule::Never,
                ..TraitDef::default()
            },
        );
        assert_eq!(holds(&p, &Env::default(), &ref_to(decl(POINT)), BETA), None);
    }
}
