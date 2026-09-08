//! The one resolution behind every `Type::method(...)` spelling: what it names,
//! and that declaration's parameters, return type and slots.
//! See `docs/wep-2026-09-06-static-call-resolution.md`.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::defs::DefId;
use crate::hashmap::IndexSet;
use crate::name::{DeclName, FqTraitName};
use crate::tir::{TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::callee::StaticMethodRef;
use super::method_call::StaticReceiver;
use super::sem::types::CalleeParams;
use super::sig::MethodSig;
use super::synth::ArgClass;
use super::trait_env::{ImplHeader, ImplTargetKey};
use super::types::TypeError;

/// One `Type::method(...)` spelling as the resolution reads it: what it names,
/// and what the call site already knows about it. Carried as one value, so a
/// fact a rung needs reaches it whatever site asked.
pub(super) struct StaticQuery<'a> {
    /// The reference site of the receiver's name, where the caller has one.
    pub(super) site: Option<ast::AstId>,
    pub(super) receiver_name: &'a str,
    /// The key the caller's own reference site resolved to. This and
    /// [`Self::arg_types`] are the call's one vantage — resolving without them
    /// and mangling with them gives two answers for one call.
    pub(super) receiver_key: Option<&'a ImplTargetKey>,
    pub(super) method_name: &'a str,
    /// The call's arguments, which is what separates several impls of one trait.
    pub(super) arg_types: &'a [TypeId],
    pub(super) receiver_type: Option<TypeId>,
    /// The receiver's type arguments, which fill the declaring block's slots.
    /// A receiver spelled as a bare name has a list without a type to read it
    /// off — the qualified spelling infers one at the call — so the fact is
    /// carried rather than re-derived from [`Self::receiver_type`].
    pub(super) receiver_args: &'a [TypeId],
    /// The trait a qualified spelling names (`Tagged::<V>::tag(5)`). Only its
    /// impls answer: the receiver's own declaration of the name is a different
    /// method, and a case it declares shadows nothing the trait supplies.
    pub(super) required_trait: Option<DefId>,
}

impl<'a> StaticQuery<'a> {
    /// The spelling alone. Every other fact is what a call site adds.
    pub(super) fn of(receiver_name: &'a str, method_name: &'a str) -> Self {
        Self {
            site: None,
            receiver_name,
            receiver_key: None,
            method_name,
            arg_types: &[],
            receiver_type: None,
            receiver_args: &[],
            required_trait: None,
        }
    }
}

/// What one `Type::method(...)` spelling names.
pub(super) struct StaticCallee {
    /// Where the declaration lives, the trait it came through, and the identity
    /// a call is mangled and a use→def edge recorded from.
    pub(super) method_ref: StaticMethodRef,
    /// The lists the call checks and pads against. Read from the declaration,
    /// so arity and defaults never wait on the receiver filling a slot.
    pub(super) params: CalleeParams,
    /// The method's own slots as its declaration wrote them.
    pub(super) own_params: Vec<ast::GenericParam>,
    /// What the call evaluates to, read at the receiver as `params` is.
    pub(super) return_type: TypeId,
}

/// The resolution's outcome. `NotStatic` is an answer, not a failure: the
/// spelling belongs to a variant case or a flags member, whose own arm counts
/// its arguments.
pub(super) enum StaticLookup {
    Found(Box<StaticCallee>),
    /// One trait, several impls, and no argument to separate them. Nothing is
    /// picked, so there is no identity to mangle and no list to check; the
    /// return type still answers where every candidate agrees.
    Overloaded {
        return_type: TypeId,
    },
    /// Several traits supply the name, which no argument can separate. The
    /// spelling names none of them and the call site reports it.
    Ambiguous(Vec<String>),
    NotStatic,
}

/// What one `impl` block offers for a method name: the declaration that
/// answers, and the facts the rules and the argument survey each read off it.
pub(super) struct StaticOffer {
    pub(super) method_id: DefId,
    pub(super) kind: CandidateKind,
    pub(super) origin: CandidateOrigin,
    pub(super) selector: Selector,
}

/// Which kind of declaration a candidate is. What shadows it and which
/// argument selects it both follow from this.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CandidateKind {
    /// Receiver-less, so argument zero is its first parameter.
    Static,
    /// Receiver-taking, reached as `Type::method(&recv, …)`, so argument zero
    /// is the receiver and argument one is its first parameter.
    Instance,
}

impl CandidateKind {
    fn of(self_kind: ast::SelfKind) -> Self {
        match self_kind {
            ast::SelfKind::None => Self::Static,
            ast::SelfKind::Value | ast::SelfKind::Ref | ast::SelfKind::MutRef => Self::Instance,
        }
    }
}

/// Where the body a candidate names is written, in the order that outranks.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum CandidateOrigin {
    /// The receiver's own declaration — its inherent impl, a resource static,
    /// or one along that resource's chain. It shadows every trait's of its
    /// kind, as dot syntax resolves it.
    Own,
    /// The block writes the body.
    Written,
    /// The block leaves the trait's default to answer.
    Inherited,
    /// A value blanket (`impl<T: Bound> Trait for T`) covers the receiver
    /// through its bound rather than naming it. Last, so anything written for
    /// the receiver itself outranks it.
    Blanket,
}

/// A diagnostic was emitted and the caller has nothing left to build.
pub(super) struct Reported;

/// What a call about to mangle a name needs, from one resolution: the
/// declaration a trait supplies, and what the call evaluates to.
pub(super) struct StaticTraitRef {
    /// `None` where no trait supplies the name, which an inherent declaration
    /// answers with a trait-less one.
    pub(super) selected: Option<StaticMethodRef>,
    pub(super) return_type: TypeId,
    /// The lists this same resolution read, so a site that mangles from
    /// `selected` and records from these cannot describe two declarations.
    pub(super) params: CalleeParams,
}

/// What the rules made of the candidates.
enum Selection {
    One(Box<Candidate>),
    /// Several traits supply the name, which no argument separates: impls of
    /// different traits share no contract.
    Ambiguous(Vec<String>),
    /// One trait, several impls, and no argument to tell them apart.
    Overloaded,
    None,
}

/// What the call's arguments are checked against.
pub(super) enum Selector {
    /// The declaration takes no arguments, so every call reaches it.
    Absent,
    /// Only an argument can fill one of these parameters — a blanket. Its
    /// unsubstituted spelling must not be mangled, so the candidate declines
    /// and the blanket resolver instantiates it instead.
    Blanket,
    /// The declaration's value parameters, as the block resolved them and with
    /// its trait arguments substituted. Compared by `TypeId`: two distinct
    /// types printing one name are two candidates, not one.
    Params(Vec<TypeId>),
}

/// The trait a candidate comes through, and the block that supplies it.
struct TraitSupply {
    impl_def: DefId,
    trait_decl: DefId,
    /// The block's own trait reference, arguments included — the form the body
    /// is emitted under, keeping `Conv<A>` and `Conv<B>` on one receiver apart.
    trait_name: FqTraitName,
}

/// One declaration the receiver supplies for the name: the facts the rules
/// read, and no decision of its own. Every rung produces these and none applies
/// a rule, so a rung added later cannot miss one.
struct Candidate {
    /// `None` for the receiver's own declaration, which no trait names and
    /// whose call is mangled without a trait segment.
    supply: Option<TraitSupply>,
    method_id: DefId,
    kind: CandidateKind,
    origin: CandidateOrigin,
    selector: Selector,
}

/// Narrow to the candidates the rule prefers, where any of them qualifies.
/// A rule that no candidate satisfies decides nothing and leaves the field.
fn prefer(candidates: &mut Vec<Candidate>, preferred: impl Fn(&Candidate) -> bool) {
    if candidates.iter().any(&preferred) {
        candidates.retain(&preferred);
    }
}

impl StaticLookup {
    /// Whether the spelling names a static at all. An ambiguity does: the call
    /// site reports it, where a `NotStatic` would read as a missing function.
    pub(super) fn resolves(&self) -> bool {
        !matches!(self, Self::NotStatic)
    }

    pub(super) fn found(&self) -> Option<&StaticCallee> {
        match self {
            Self::Found(callee) => Some(callee),
            Self::Overloaded { .. } | Self::Ambiguous(_) | Self::NotStatic => None,
        }
    }

    /// What the call evaluates to. An overload answers where its candidates
    /// agree, which is what they do when each returns the receiver.
    pub(super) fn return_type(&self) -> TypeId {
        match self {
            Self::Found(callee) => callee.return_type,
            Self::Overloaded { return_type } => *return_type,
            Self::Ambiguous(_) | Self::NotStatic => TypeTable::UNKNOWN,
        }
    }

    /// The parameters to check and pad against, and whether a declaration
    /// answered at all. A callee no declaration names — a variant case, a flags
    /// member — counts its own arguments in its own arm.
    pub(super) fn params(self) -> (CalleeParams, bool) {
        match self {
            Self::Found(callee) => (callee.params, true),
            Self::Overloaded { .. } | Self::Ambiguous(_) | Self::NotStatic => {
                (CalleeParams::default(), false)
            }
        }
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Resolve what `query`'s spelling names, once, for every site that used to
    /// ask its own question.
    pub(super) fn resolve_static_callee(&mut self, query: StaticQuery<'_>) -> StaticLookup {
        let StaticQuery {
            site,
            receiver_name,
            receiver_key,
            method_name,
            arg_types,
            receiver_type,
            receiver_args,
            required_trait,
        } = query;
        // One vantage: the key the caller resolved, else the one its reference
        // site answered for, else the name's own. Deriving a second key when
        // the first misses makes the order a silent tiebreak.
        let key = match receiver_key {
            Some(key) => key.clone(),
            None => self.impl_target_at(site, receiver_name),
        };

        // Every declaration the receiver supplies, its own and its traits'.
        // `receiver_key` as the caller gave it for the traits: a key derived
        // from the name narrows the search to a declaration the impls may not
        // be indexed under, putting a primitive's `impl FromStr for f32` out of
        // reach.
        //
        // A qualified spelling names a trait, so the receiver's own declaration
        // is not what it asks for and no case of the name shadows the answer.
        let mut candidates = match required_trait {
            Some(_) => Vec::new(),
            None => self.own_candidates(&key, method_name),
        };

        // A case or member the receiver declares is written on the type, and
        // shadows a static it only *inherits*: its own arm builds the
        // constructor, so the spelling is not a call at all. It does not shadow
        // the receiver's own declaration — that is a type declaring one name
        // twice, and answering with the case would hide it.
        if required_trait.is_none()
            && candidates.is_empty()
            && self.declares_case_named(&key, method_name)
        {
            return StaticLookup::NotStatic;
        }

        candidates.extend(self.trait_candidates(
            receiver_name,
            method_name,
            receiver_key,
            receiver_type,
        ));
        candidates.extend(self.blanket_candidates(method_name, receiver_type));
        if let Some(required) = required_trait {
            candidates.retain(|c| c.supply.as_ref().is_some_and(|s| s.trait_decl == required));
        }
        let resolved = match self.select_candidate(candidates, arg_types) {
            Selection::One(candidate) => self.callee_of_candidate(
                receiver_name,
                method_name,
                &candidate,
                receiver_type,
                receiver_args,
            ),
            Selection::Ambiguous(alternatives) => {
                return StaticLookup::Ambiguous(alternatives);
            }
            // Nothing picked a declaration, so there is none to mangle and no
            // list to check. `i32::from(b)` names one `From` impl per source
            // type, and the call site's own preselect is what reads the
            // argument.
            Selection::Overloaded => return self.overloaded_callee(&key, method_name),
            Selection::None => None,
        };
        if let Some(resolved) = resolved {
            return resolved;
        }

        // The auto-derived `Default`: synthesis emits the body, so no
        // declaration backs it. It takes no arguments and answers with the
        // receiver's own type.
        if let Some(method_ref) = self.auto_derived_default_ref(receiver_name, method_name) {
            return StaticLookup::Found(Box::new(StaticCallee {
                params: CalleeParams::default(),
                own_params: Vec::new(),
                return_type: self
                    .tysys
                    .auto_derive_default_struct_type(&self.type_lookup(), receiver_name)
                    .unwrap_or(TypeTable::UNKNOWN),
                method_ref,
            }));
        }

        // A newtype and a `flags` reach what they wrap: their impls are looked
        // up on the base, so the spelling resolves there too. Read from the
        // alias's declaration, since a namespaced `lib::Q::twice()` leaves the
        // caller's frame no `Q` to look one up by; the name answers only where
        // the key reaches no declaration to read.
        match self
            .newtype_base_of(&key)
            .or_else(|| self.newtype_base(receiver_name))
        {
            Some((base, base_name)) => {
                let base_key = self.impl_target_of(base, &DeclName::new(&base_name));
                self.resolve_static_callee(StaticQuery {
                    receiver_key: Some(&base_key),
                    arg_types,
                    receiver_type,
                    receiver_args,
                    required_trait,
                    ..StaticQuery::of(&base_name, method_name)
                })
            }
            None => StaticLookup::NotStatic,
        }
    }

    /// The declarations the receiver makes itself: its inherent impl, the
    /// statics a `resource` declares, and one it inherits along its chain —
    /// the index holds only a resource's own, so the chain is walked.
    fn own_candidates(&self, key: &ImplTargetKey, method_name: &str) -> Vec<Candidate> {
        let inherent = self
            .impl_method_entries(key, method_name)
            .filter(|entry| entry.is_inherent())
            .map(|entry| entry.method_id)
            .collect::<Vec<_>>();
        let resource_static = self
            .tysys
            .trait_env
            .resource_static(key, method_name)
            .and_then(|(name, _, decl, _)| {
                Some(self.tysys.signatures.resource_method_sig(*decl, name)?.def)
            });
        let inherited = match key {
            ImplTargetKey::Decl(def) => self
                .resource_instance_method(*def, method_name)
                .map(|(_, sig)| sig.def),
            _ => None,
        };
        inherent
            .into_iter()
            .chain(resource_static)
            .chain(inherited)
            .filter_map(|method_id| self.own_candidate(method_id))
            .collect()
    }

    /// One own declaration as a candidate. `None` where no signature answers
    /// for it, which is a rung that cannot resolve rather than a spelling that
    /// names nothing — the others still answer.
    ///
    /// Its selector is `Absent`: the arguments choose *among impls*, and the
    /// receiver's own declaration has none to be chosen against. Reading them
    /// here would drop it on a mismatch, where the call site has an argument
    /// type error to report against the one declaration the spelling names.
    fn own_candidate(&self, method_id: DefId) -> Option<Candidate> {
        let sig = self.tysys.signatures.method_sig(method_id)?;
        Some(Candidate {
            supply: None,
            method_id,
            kind: CandidateKind::of(sig.self_kind),
            origin: CandidateOrigin::Own,
            selector: Selector::Absent,
        })
    }

    /// The resolution for the declaration the rules picked. Its module is the
    /// block's where a trait supplies it — the body is emitted for the block,
    /// whether it wrote the body or inherited it — and the declaration's own
    /// otherwise.
    fn callee_of_candidate(
        &mut self,
        receiver_name: &str,
        method_name: &str,
        candidate: &Candidate,
        receiver_type: Option<TypeId>,
        receiver_args: &[TypeId],
    ) -> Option<StaticLookup> {
        let defs = self.tysys.resolutions.defs();
        let module = match &candidate.supply {
            Some(supply) => defs.module(supply.impl_def).clone(),
            None => defs.module(candidate.method_id).clone(),
        };
        let method_ref = StaticMethodRef::new(
            module,
            receiver_name,
            method_name,
            candidate.supply.as_ref().map(|s| s.trait_name.clone()),
            Some(candidate.method_id),
        );
        match (candidate.origin, &candidate.supply) {
            // A blanket's template is written against its receiver parameter,
            // which no name at a call site reaches. The rules have done their
            // part in picking it; instantiating it at the receiver, and
            // mangling it as a blanket, is the blanket resolver's.
            (CandidateOrigin::Blanket, _) => None,
            // An inherited method is declared nowhere but the trait, so that is
            // where its signature is read from — in the trait's frame, which
            // the block's arguments fill.
            (CandidateOrigin::Inherited, Some(supply)) => {
                // Resolved here rather than asked of every caller: the name
                // costs a scope to resolve, and this is the one rung that needs
                // it.
                let receiver_type = receiver_type.or_else(|| {
                    let mut scope = self.enter_inherited_type_param_scope();
                    let resolved = scope.resolve_unsited_type_name(receiver_name, Span::default());
                    drop(scope);
                    Some(resolved)
                });
                let (trait_decl, impl_def) = (supply.trait_decl, supply.impl_def);
                self.callee_of_trait_declaration(
                    trait_decl,
                    impl_def,
                    method_name,
                    method_ref,
                    receiver_type,
                )
                .map(|callee| StaticLookup::Found(Box::new(callee)))
            }
            _ => self.callee_of_declaration(
                candidate.method_id,
                method_ref,
                receiver_type,
                receiver_args,
            ),
        }
    }

    /// The resolution for a method a block inherits with the trait's default
    /// body. Its signature is written in the trait's frame, which numbers
    /// `Self` as slot 0 and the trait's own parameters after it, so the block's
    /// trait arguments follow the receiver. Reading it at the receiver alone
    /// leaves them open, and every argument then reaches every block.
    ///
    /// The defaults are the trait's and resolve where it wrote them, while the
    /// body is emitted for the block, which `method_ref` already names.
    fn callee_of_trait_declaration(
        &mut self,
        trait_decl: DefId,
        impl_def: DefId,
        method_name: &str,
        method_ref: StaticMethodRef,
        receiver_type: Option<TypeId>,
    ) -> Option<StaticCallee> {
        let declaring = self.tysys.signatures.trait_sig(trait_decl)?;
        let sig = declaring.method(method_name)?.sig.clone();
        let mut params = CalleeParams::of_signature(Some(&sig));
        params.defaults_module = Some(declaring.module.clone());
        let mut return_type = sig.decl.return_type.unwrap_or(TypeTable::UNIT);
        if let Some(receiver_type) = receiver_type {
            let frame: Vec<TypeId> = std::iter::once(receiver_type)
                .chain(self.trait_args_of_impl(impl_def))
                .collect();
            let instantiated = sig.instantiate_call(&self.tysys.type_table, &frame, &[]);
            params.param_types = instantiated.param_types;
            return_type = instantiated.return_type;
        }
        Some(StaticCallee {
            params,
            own_params: sig.own_params,
            return_type,
            method_ref,
        })
    }

    /// What a call about to mangle a name needs, resolved once so the identity
    /// and the return type cannot name different declarations.
    ///
    /// A spelling no trait answers and no inherent declaration either mangles
    /// without a trait segment and reaches WIR build unresolved. Where an
    /// argument was there to select and matched nothing, that is reported here
    /// rather than left to ICE there, and the caller stops.
    pub(super) fn static_trait_ref(
        &mut self,
        query: StaticQuery<'_>,
        span: Span,
    ) -> Result<StaticTraitRef, Reported> {
        let (receiver_name, method_name) = (query.receiver_name, query.method_name);
        let (receiver_key, arg_types) = (query.receiver_key, query.arg_types);
        let recv = StaticReceiver {
            key: receiver_key,
            ty: query.receiver_type,
            required_trait: query.required_trait,
            ..StaticReceiver::of(receiver_name)
        };
        let lookup = self.resolve_static_callee(query);
        let return_type = lookup.return_type();
        let selected = lookup
            .found()
            .map(|callee| callee.method_ref.clone())
            .filter(|method_ref| method_ref.trait_name.is_some());
        if selected.is_none()
            && !arg_types.is_empty()
            && !self.has_inherent_static_method(receiver_name, method_name, receiver_key)
            && self.report_unmatched_static_arg(recv, method_name, arg_types, span)
        {
            return Err(Reported);
        }
        let (params, _) = lookup.params();
        Ok(StaticTraitRef {
            selected,
            return_type,
            params,
        })
    }

    /// Report a spelling several traits answer, which names none of them.
    /// Every site that mangles a name has to consume this, or it builds a name
    /// with no trait segment and WIR resolves it to nothing.
    pub(super) fn report_ambiguous_static(
        &mut self,
        lookup: &StaticLookup,
        method_name: &str,
        span: Span,
    ) -> bool {
        let StaticLookup::Ambiguous(traits) = lookup else {
            return false;
        };
        let _ = self.emit(TypeError::AmbiguousTraitMethod {
            method: method_name.to_string(),
            traits: traits.clone(),
            span,
        });
        true
    }

    /// The rules, applied once to every candidate whatever rung produced it.
    /// Their order is the design, not an implementation detail.
    fn select_candidate(&self, mut candidates: Vec<Candidate>, arg_types: &[TypeId]) -> Selection {
        // The receiver's own declaration shadows an inherited one of the same
        // kind: a receiver-less declaration beside an instance one is no
        // alternative, since different argument lists reach them.
        let own_kinds: Vec<CandidateKind> = candidates
            .iter()
            .filter(|c| c.origin == CandidateOrigin::Own)
            .map(|c| c.kind)
            .collect();
        candidates.retain(|c| c.origin == CandidateOrigin::Own || !own_kinds.contains(&c.kind));
        // A receiver-less declaration answers before a receiver-taking one, so
        // `Type::method(x)` is a static's call before it is a UFCS receiver.
        prefer(&mut candidates, |c| c.kind == CandidateKind::Static);
        // A block naming the receiver answers before a blanket covering it
        // through a bound, so a blanket is no alternative to report against.
        // Before the ambiguity rule, or the trait a blanket names would be
        // named as one.
        prefer(&mut candidates, |c| c.origin != CandidateOrigin::Blanket);
        if let Some(alternatives) = self.ambiguous_alternatives(&candidates) {
            return Selection::Ambiguous(alternatives);
        }
        // The arguments pick among the declarations, read against the parameters
        // the impl declares rather than the trait it names. Where they start
        // follows from the kind: a receiver-taking declaration has the receiver
        // at argument zero, so its own parameters begin at argument one.
        candidates.retain(|c| {
            let args = match c.kind {
                CandidateKind::Static => arg_types,
                CandidateKind::Instance => arg_types.get(1..).unwrap_or_default(),
            };
            self.selector_admits(&c.selector, args)
        });
        // Among what the arguments admit, the earlier origin outranks: the
        // receiver's own before a written body, a written body before an
        // inherited one.
        if let Some(best) = candidates.iter().map(|c| c.origin).min() {
            candidates.retain(|c| c.origin == best);
        }
        match candidates.len() {
            0 => Selection::None,
            1 => Selection::One(Box::new(candidates.remove(0))),
            // One trait, several impls, and no argument that separated them.
            _ => Selection::Overloaded,
        }
    }

    /// Whether the arguments reach these parameters, each against the one it is
    /// written for. A call supplying fewer than the declaration takes is checked
    /// as far as it goes: the rest are defaults, and arity is the call site's.
    ///
    /// [`Elaborator::class_admits`] answers per parameter, so an open slot, an
    /// unresolved parameter and `&mut T` against `&T` mean here what they mean
    /// to every other argument check.
    fn selector_admits(&self, selector: &Selector, args: &[TypeId]) -> bool {
        match selector {
            Selector::Absent => true,
            // Nothing to select on leaves every declaration of the name, and
            // several of them are the overload the call site settles.
            Selector::Blanket => args.is_empty(),
            Selector::Params(params) => params
                .iter()
                .zip(args)
                .all(|(&param, &arg)| self.class_admits(param, &ArgClass::Exact(arg))),
        }
    }

    /// The traits to name when several supply one name, in the order the blocks
    /// were written. `None` where one trait answers, however many times it is
    /// implemented: naming it as both alternatives is a remedy nobody can write.
    fn ambiguous_alternatives(&self, candidates: &[Candidate]) -> Option<Vec<String>> {
        let distinct: IndexSet<DefId> = candidates
            .iter()
            .filter_map(|c| c.supply.as_ref().map(|s| s.trait_decl))
            .collect();
        (distinct.len() > 1).then(|| {
            distinct
                .into_iter()
                .map(|decl| self.tysys.resolutions.defs().name(decl).to_string())
                .collect()
        })
    }

    /// The resolution for a name several declarations answer to, which only an
    /// argument separates. No list comes with it, and the return type only
    /// where every candidate agrees.
    fn overloaded_callee(&self, key: &ImplTargetKey, method_name: &str) -> StaticLookup {
        let mut returns = self
            .qualified_method_decl_ids(key, method_name)
            .filter_map(|def| self.tysys.signatures.method_sig(def))
            .map(|sig| sig.decl.return_type.unwrap_or(TypeTable::UNIT));
        let agreed = returns
            .next()
            .filter(|&first| returns.all(|r| r == first))
            .unwrap_or(TypeTable::UNKNOWN);
        StaticLookup::Overloaded {
            return_type: agreed,
        }
    }

    /// Every declaration the receiver's trait blocks supply for `method_name`,
    /// in the order the blocks were written — the body a block writes, else the
    /// trait's default where it left one. Facts only: what to do with several
    /// is [`Self::select_candidate`]'s.
    fn trait_candidates(
        &self,
        receiver_name: &str,
        method_name: &str,
        target_hint: Option<&ImplTargetKey>,
        receiver_type: Option<TypeId>,
    ) -> Vec<Candidate> {
        self.trait_impls_for_receiver(receiver_name, target_hint)
            .into_iter()
            .filter_map(|impl_def| {
                let header = &self.tysys.trait_env.impl_headers[&impl_def];
                let trait_name = self
                    .tysys
                    .trait_env
                    .fq_trait_of_impl(header, &self.tysys.resolutions)?;
                let trait_decl = self.tysys.signatures.impl_sig(impl_def)?.trait_decl?;
                let build = |method_id, kind, origin, selector| Candidate {
                    supply: Some(TraitSupply {
                        impl_def,
                        trait_decl,
                        trait_name,
                    }),
                    method_id,
                    kind,
                    origin,
                    selector,
                };
                let offer = self.impl_static_offer(
                    header,
                    impl_def,
                    trait_decl,
                    method_name,
                    receiver_type,
                )?;
                Some(build(
                    offer.method_id,
                    offer.kind,
                    offer.origin,
                    offer.selector,
                ))
            })
            .collect()
    }

    /// Every value blanket (`impl<T: Bound> Trait for T`) whose bound the
    /// receiver satisfies. Its bucket is keyed by the receiver *parameter*, so
    /// the receiver's own never holds it and the rungs above miss it entirely.
    ///
    /// Its selector is `Blanket`: the template is written against a parameter,
    /// and instantiating it at the receiver is the blanket resolver's, not a
    /// candidate's. What the candidate is here for is the rules — which trait
    /// supplies the name, and that anything written for the receiver itself
    /// outranks a blanket covering it.
    fn blanket_candidates(
        &mut self,
        method_name: &str,
        receiver_type: Option<TypeId>,
    ) -> Vec<Candidate> {
        let Some(receiver_type) = receiver_type else {
            return Vec::new();
        };
        self.applicable_blanket_statics(receiver_type, method_name)
            .into_iter()
            .filter_map(|blanket| {
                let header = self.tysys.trait_env.impl_headers.get(&blanket.def)?;
                let method_id = header.methods.iter().find(|m| m.name == method_name)?.def;
                let trait_decl = self.tysys.signatures.impl_sig(blanket.def)?.trait_decl?;
                Some(Candidate {
                    supply: Some(TraitSupply {
                        impl_def: blanket.def,
                        trait_decl,
                        trait_name: blanket.trait_name,
                    }),
                    method_id,
                    kind: CandidateKind::Static,
                    origin: CandidateOrigin::Blanket,
                    selector: Selector::Blanket,
                })
            })
            .collect()
    }

    /// What an `impl` block offers for `method_name`: the body it wrote, else
    /// the trait default it leaves to answer. One walk, so the argument survey
    /// and the rules cannot see different candidate sets.
    ///
    /// `None` where the block offers nothing — it declares another name, or the
    /// trait left the method required, which is reported where the two are
    /// compared.
    pub(super) fn impl_static_offer(
        &self,
        header: &ImplHeader,
        impl_def: DefId,
        trait_decl: DefId,
        method_name: &str,
        receiver_type: Option<TypeId>,
    ) -> Option<StaticOffer> {
        if let Some(method) = header.methods.iter().find(|m| m.name == method_name) {
            let sig = self
                .tysys
                .signatures
                .method_sig(method.def)
                .expect("the decl pass records every impl-declared method's signature");
            return Some(StaticOffer {
                method_id: method.def,
                kind: CandidateKind::of(sig.self_kind),
                origin: CandidateOrigin::Written,
                selector: self.written_selector(header, sig),
            });
        }
        self.inherited_offer(trait_decl, impl_def, method_name, receiver_type)
    }

    /// What the argument is checked against for a body the block wrote: the
    /// declaration's first parameter past any receiver, unless only the
    /// argument can fill it.
    pub(super) fn written_selector(&self, header: &ImplHeader, sig: &MethodSig) -> Selector {
        let first = sig.first_value_param().min(sig.decl.param_types.len());
        let params = &sig.decl.param_types[first..];
        if params.is_empty() {
            return Selector::Absent;
        }
        if params
            .iter()
            .any(|&param| self.param_filled_by_block(header, sig, param))
        {
            return Selector::Blanket;
        }
        Selector::Params(params.to_vec())
    }

    /// The candidate for the trait's default body, where the block wrote none.
    /// The trait's frame numbers `Self` as slot 0 and the trait's own parameters
    /// after it, so the block's trait arguments follow the receiver: reading the
    /// default at the receiver alone leaves them open, and every argument then
    /// reaches every block.
    ///
    /// `None` where the trait left the method required, which is the block's own
    /// error and reported where the two are compared.
    fn inherited_offer(
        &self,
        trait_decl: DefId,
        impl_def: DefId,
        method_name: &str,
        receiver_type: Option<TypeId>,
    ) -> Option<StaticOffer> {
        let declared = self
            .tysys
            .signatures
            .trait_sig(trait_decl)?
            .method(method_name)?;
        declared.default_body.as_ref()?;
        let frame: Vec<TypeId> = std::iter::once(receiver_type.unwrap_or(TypeTable::UNKNOWN))
            .chain(self.trait_args_of_impl(impl_def))
            .collect();
        let instantiated = declared
            .sig
            .instantiate_call(&self.tysys.type_table, &frame, &[]);
        Some(StaticOffer {
            method_id: declared.sig.def,
            kind: CandidateKind::of(declared.sig.self_kind),
            origin: CandidateOrigin::Inherited,
            selector: match instantiated.param_types.get(
                declared
                    .sig
                    .first_value_param()
                    .min(instantiated.param_types.len())..,
            ) {
                Some([]) | None => Selector::Absent,
                Some(params) => Selector::Params(params.to_vec()),
            },
        })
    }

    /// The block's trait-reference arguments, which fill the trait's frame past
    /// `Self`.
    fn trait_args_of_impl(&self, impl_def: DefId) -> Vec<TypeId> {
        self.tysys
            .signatures
            .impl_sig(impl_def)
            .map(|impl_sig| impl_sig.trait_type_args.clone())
            .unwrap_or_default()
    }

    /// Whether the receiver declares a `variant` case, an `enum` case or a
    /// `flags` member of this name — a constructor the spelling names, not a
    /// call.
    fn declares_case_named(&self, key: &ImplTargetKey, name: &str) -> bool {
        let ImplTargetKey::Decl(def) = key else {
            return false;
        };
        let lookup = self.type_lookup();
        lookup
            .variant_cases_of(*def)
            .is_some_and(|info| info.cases.iter().any(|case| case.name == name))
            || lookup
                .enum_cases_of(*def)
                .is_some_and(|info| info.cases.iter().any(|case| case.name == name))
            || lookup
                .flags_members_of(*def)
                .is_some_and(|info| info.members.iter().any(|member| member.name == name))
    }

    /// The resolution for a declaration already picked: its signature, read at
    /// the receiver.
    /// The receiver's type arguments: the ones a call carries, else the ones
    /// its type holds. `None` where it brings neither.
    pub(super) fn receiver_declaring_args(
        &self,
        receiver_type: Option<TypeId>,
        receiver_args: &[TypeId],
    ) -> Option<Vec<TypeId>> {
        if !receiver_args.is_empty() {
            return Some(receiver_args.to_vec());
        }
        let args = receiver_type.and_then(|ty| {
            self.tysys
                .type_table
                .borrow()
                .nominal_type_args(self.tysys.get_base_type(ty))
        })?;
        (!args.is_empty()).then_some(args)
    }

    fn callee_of_declaration(
        &mut self,
        def: DefId,
        method_ref: StaticMethodRef,
        receiver_type: Option<TypeId>,
        receiver_args: &[TypeId],
    ) -> Option<StaticLookup> {
        // `None` falls through to the next rung rather than ending the walk: a
        // declaration the signature table does not answer for is one this rung
        // cannot resolve, not a spelling that names nothing.
        let sig = self.tysys.signatures.method_sig(def).cloned()?;
        let mut params = CalleeParams::of_signature(Some(&sig));
        let mut return_type = sig.decl.return_type.unwrap_or(TypeTable::UNIT);
        // The declaring slots are the receiver's *arguments* — `ByteList` fills
        // `List<T>`'s `T` with `u8`. An `impl` block aligns them, since its head
        // may reorder or fix some; a `resource` or `interface` numbers its own
        // by position. Where the receiver brings none they stay as the
        // declaration wrote them, and the call site checks past them.
        //
        // Never the receiver *type* itself: only a trait's frame leads with
        // `Self`, and that frame is read in `callee_of_trait_declaration`.
        // Binding slot zero to the receiver here made `Stream::<u8>::new()`
        // return a `StreamWritable<Stream<u8>>`.
        if sig.declaring_slot_count > 0
            && let Some(args) = self.receiver_declaring_args(receiver_type, receiver_args)
        {
            let declaring = sig
                .declaring_impl
                .and_then(|impl_def| self.tysys.signatures.impl_sig(impl_def).cloned());
            let instantiated =
                sig.instantiate_call_with(&self.tysys.type_table, declaring.as_ref(), &args, &[]);
            params.param_types = instantiated.param_types;
            return_type = instantiated.return_type;
        }
        Some(StaticLookup::Found(Box::new(StaticCallee {
            params,
            own_params: sig.own_params,
            return_type,
            method_ref,
        })))
    }
}
