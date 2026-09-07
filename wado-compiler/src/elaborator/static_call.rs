//! The one resolution behind every `Type::method(...)` spelling: what it names,
//! and that declaration's parameters, return type and slots.
//! See `docs/wep-2026-09-06-static-call-resolution.md`.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::defs::DefId;
use crate::hashmap::IndexSet;
use crate::name::FqTraitName;
use crate::tir::{TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::callee::StaticMethodRef;
use super::sem::types::CalleeParams;
use super::sig::MethodSig;
use super::synth::ArgClass;
use super::trait_env::{ImplHeader, ImplTargetKey};
use super::types::TypeError;

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

/// Which kind of declaration a candidate is. What shadows it and which
/// argument selects it both follow from this.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CandidateKind {
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

/// Where the body a candidate names is written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CandidateOrigin {
    /// The block writes the body.
    Written,
    /// The block leaves the trait's default to answer.
    Inherited,
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

/// One declaration a block on the receiver supplies for the name: the facts the
/// rules read, and no decision of its own. Every rung produces these and none
/// applies a rule, so a rung added later cannot miss one.
struct Candidate {
    impl_def: DefId,
    trait_decl: DefId,
    /// The block's own trait reference, arguments included — the form the body
    /// is emitted under, keeping `Conv<A>` and `Conv<B>` on one receiver apart.
    trait_name: FqTraitName,
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
    /// Resolve what `receiver_name::method_name` names, once, for every site
    /// that used to ask its own question.
    ///
    /// `receiver_key` is the key the caller's own reference site resolved to;
    /// `arg_types` are its arguments, which is what separates several
    /// impls of one trait. Both are the call's one vantage — resolving without
    /// them and mangling with them gives two answers for one call.
    pub(super) fn resolve_static_callee(
        &mut self,
        site: Option<ast::AstId>,
        receiver_name: &str,
        receiver_key: Option<&ImplTargetKey>,
        method_name: &str,
        arg_types: &[TypeId],
        receiver_type: Option<TypeId>,
    ) -> StaticLookup {
        // One vantage: the key the caller resolved, else the one its reference
        // site answered for, else the name's own. Deriving a second key when
        // the first misses makes the order a silent tiebreak.
        let key = match receiver_key {
            Some(key) => key.clone(),
            None => self.impl_target_at(site, receiver_name),
        };

        // The receiver's own declarations, which shadow every inherited one as
        // dot syntax does. In the order they shadow, each falling through where
        // it cannot resolve: its inherent declaration, its resource statics,
        // then that resource's chain — the index holds only a resource's own
        // methods, so one it inherits is reached by walking it.
        let own = [
            self.unshadowed_inherent(&key, method_name),
            self.resource_static_declaration(&key, method_name),
            match &key {
                ImplTargetKey::Decl(def) => self
                    .resource_instance_method(*def, method_name)
                    .map(|(_, sig)| sig.def),
                _ => None,
            },
        ];
        for def in own.into_iter().flatten() {
            if let Some(resolved) =
                self.callee_of_own_declaration(receiver_name, method_name, def, receiver_type)
            {
                return resolved;
            }
        }

        // A case or member the receiver declares is written on the type, and
        // shadows a static it only inherits — the rule the rungs above apply to
        // an inherent declaration. Its own arm builds the constructor.
        if self.declares_case_named(&key, method_name) {
            return StaticLookup::NotStatic;
        }

        // `receiver_key` as the caller gave it, not the key derived above: a
        // derived one narrows the search to a declaration the impls may not be
        // indexed under, putting a primitive's `impl FromStr for f32` out of
        // reach.
        let candidates =
            self.trait_candidates(receiver_name, method_name, receiver_key, receiver_type);
        // Keyed where the candidates were found. Keyed any other way the check
        // answers for a different receiver, hiding a declaration it never saw
        // or admitting one it should have hidden.
        let shadow_key = self.static_receiver_key(receiver_name, receiver_key);
        let through_traits = match self.select_candidate(candidates, arg_types, |kind| {
            self.inherent_shadows(&shadow_key, method_name, kind == CandidateKind::Instance)
        }) {
            Selection::One(candidate) => {
                self.callee_of_candidate(receiver_name, method_name, &candidate, receiver_type)
            }
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
        if let Some(resolved) = through_traits {
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
        // up on the base, so the spelling resolves there too.
        match self.newtype_base(receiver_name) {
            Some((_, base_name)) => self.resolve_static_callee(
                None,
                &base_name,
                None,
                method_name,
                arg_types,
                receiver_type,
            ),
            None => StaticLookup::NotStatic,
        }
    }

    /// The receiver's own declaration of the name, where it is the only one of
    /// its kind. Several are an overload no argument has separated yet, and a
    /// trait impl's falls to the selection, which names the trait its call is
    /// mangled with.
    fn unshadowed_inherent(&self, key: &ImplTargetKey, method_name: &str) -> Option<DefId> {
        let mut entries = self.impl_method_entries(key, method_name);
        let first = entries.next()?;
        let only_of_kind = !entries.any(|other| other.has_self == first.has_self);
        (first.is_inherent() && only_of_kind).then_some(first.method_id)
    }

    /// The static a `resource` declares, which it makes like any other
    /// declaration.
    fn resource_static_declaration(&self, key: &ImplTargetKey, method_name: &str) -> Option<DefId> {
        let (name, _, decl, _) = self.tysys.trait_env.resource_static(key, method_name)?;
        Some(self.tysys.signatures.resource_method_sig(*decl, name)?.def)
    }

    /// The resolution for the declaration the rules picked. The block's module
    /// either way: the body is emitted for the block, so that is what the call
    /// names, whether the block wrote it or inherited it.
    fn callee_of_candidate(
        &mut self,
        receiver_name: &str,
        method_name: &str,
        candidate: &Candidate,
        receiver_type: Option<TypeId>,
    ) -> Option<StaticLookup> {
        let module = self
            .tysys
            .resolutions
            .defs()
            .module(candidate.impl_def)
            .clone();
        let method_ref = StaticMethodRef::new(
            module,
            receiver_name,
            method_name,
            Some(candidate.trait_name.clone()),
            Some(candidate.method_id),
        );
        match candidate.origin {
            CandidateOrigin::Written => {
                self.callee_of_declaration(candidate.method_id, method_ref, receiver_type)
            }
            // An inherited method is declared nowhere but the trait, so that is
            // where its signature is read from — in the trait's frame, which
            // the block's arguments fill.
            CandidateOrigin::Inherited => {
                // Resolved here rather than asked of every caller: the name
                // costs a scope to resolve, and this is the one rung that needs
                // it.
                let receiver_type = receiver_type.or_else(|| {
                    let mut scope = self.enter_inherited_type_param_scope();
                    let resolved = scope.resolve_unsited_type_name(receiver_name, Span::default());
                    drop(scope);
                    Some(resolved)
                });
                self.callee_of_trait_declaration(
                    candidate.trait_decl,
                    candidate.impl_def,
                    method_name,
                    method_ref,
                    receiver_type,
                )
                .map(|callee| StaticLookup::Found(Box::new(callee)))
            }
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
        receiver_name: &str,
        method_name: &str,
        receiver_key: Option<&ImplTargetKey>,
        arg_types: &[TypeId],
        span: Span,
    ) -> Result<StaticTraitRef, Reported> {
        let lookup = self.resolve_static_callee(
            None,
            receiver_name,
            receiver_key,
            method_name,
            arg_types,
            None,
        );
        let return_type = lookup.return_type();
        let selected = lookup
            .found()
            .map(|callee| callee.method_ref.clone())
            .filter(|method_ref| method_ref.trait_name.is_some());
        // The report names the argument the survey reads, which is a static's
        // first — the same one it compares its candidates on.
        let first_arg = arg_types
            .first()
            .map(|&arg| self.tysys.type_table.borrow().type_name(arg));
        if selected.is_none()
            && let Some(arg_type) = first_arg.as_deref()
            && !self.has_inherent_static_method(receiver_name, method_name, receiver_key)
            && self.report_unmatched_static_arg(
                receiver_name,
                method_name,
                arg_type,
                span,
                receiver_key,
            )
        {
            return Err(Reported);
        }
        Ok(StaticTraitRef {
            selected,
            return_type,
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
    fn select_candidate(
        &self,
        mut candidates: Vec<Candidate>,
        arg_types: &[TypeId],
        shadowed: impl Fn(CandidateKind) -> bool,
    ) -> Selection {
        // An inherent declaration shadows an inherited one of the same kind: a
        // receiver-less declaration beside an instance one is no alternative,
        // since different argument lists reach them.
        candidates.retain(|c| !shadowed(c.kind));
        // A receiver-less declaration answers before a receiver-taking one, so
        // `Type::method(x)` is a static's call before it is a UFCS receiver.
        prefer(&mut candidates, |c| c.kind == CandidateKind::Static);
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
        // Among what the argument admits, a written body outranks an inherited
        // one.
        prefer(&mut candidates, |c| c.origin == CandidateOrigin::Written);
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
        let distinct: IndexSet<DefId> = candidates.iter().map(|c| c.trait_decl).collect();
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
                    impl_def,
                    trait_decl,
                    trait_name,
                    method_id,
                    kind,
                    origin,
                    selector,
                };
                match header.methods.iter().find(|m| m.name == method_name) {
                    Some(method) => Some(self.written_candidate(header, method.def, build)),
                    None => self.inherited_candidate(
                        trait_decl,
                        impl_def,
                        method_name,
                        receiver_type,
                        build,
                    ),
                }
            })
            .collect()
    }

    /// The candidate for a body the block writes. Its parameter is already in
    /// the block's own frame, so a slot the block fills is a blanket and
    /// anything else is what the argument is compared against.
    fn written_candidate(
        &self,
        header: &ImplHeader,
        method_id: DefId,
        build: impl FnOnce(DefId, CandidateKind, CandidateOrigin, Selector) -> Candidate,
    ) -> Candidate {
        let sig = self
            .tysys
            .signatures
            .method_sig(method_id)
            .expect("the decl pass records every impl-declared method's signature");
        let selector = self.written_selector(header, sig);
        build(
            method_id,
            CandidateKind::of(sig.self_kind),
            CandidateOrigin::Written,
            selector,
        )
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
    fn inherited_candidate(
        &self,
        trait_decl: DefId,
        impl_def: DefId,
        method_name: &str,
        receiver_type: Option<TypeId>,
        build: impl FnOnce(DefId, CandidateKind, CandidateOrigin, Selector) -> Candidate,
    ) -> Option<Candidate> {
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
        Some(build(
            declared.sig.def,
            CandidateKind::of(declared.sig.self_kind),
            CandidateOrigin::Inherited,
            match instantiated.param_types.get(
                declared
                    .sig
                    .first_value_param()
                    .min(instantiated.param_types.len())..,
            ) {
                Some([]) | None => Selector::Absent,
                Some(params) => Selector::Params(params.to_vec()),
            },
        ))
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

    /// [`Self::callee_of_declaration`] for a declaration the receiver makes
    /// itself, which no trait names and whose module is its own.
    fn callee_of_own_declaration(
        &mut self,
        receiver_name: &str,
        method_name: &str,
        def: DefId,
        receiver_type: Option<TypeId>,
    ) -> Option<StaticLookup> {
        let module = self.tysys.resolutions.defs().module(def).clone();
        let method_ref = StaticMethodRef::new(module, receiver_name, method_name, None, Some(def));
        self.callee_of_declaration(def, method_ref, receiver_type)
    }

    /// The resolution for a declaration already picked: its signature, read at
    /// the receiver.
    fn callee_of_declaration(
        &mut self,
        def: DefId,
        method_ref: StaticMethodRef,
        receiver_type: Option<TypeId>,
    ) -> Option<StaticLookup> {
        // `None` falls through to the next rung rather than ending the walk: a
        // declaration the signature table does not answer for is one this rung
        // cannot resolve, not a spelling that names nothing.
        let sig = self.tysys.signatures.method_sig(def).cloned()?;
        let mut params = CalleeParams::of_signature(Some(&sig));
        let mut return_type = sig.decl.return_type.unwrap_or(TypeTable::UNIT);
        // A trait's own declaration is written in the trait's frame, where
        // `Self` leads the slots, so it is read at the receiver. A caller asking
        // only whether the spelling resolves brings none, and reads neither.
        if sig.declaring_impl.is_none()
            && let Some(receiver_type) = receiver_type
        {
            let instantiated = sig.instantiate_call(&self.tysys.type_table, &[receiver_type], &[]);
            params.param_types = instantiated.param_types;
            return_type = instantiated.return_type;
        }
        // A generic block's slots are the receiver's arguments — `ByteList`
        // fills `List<T>`'s `T` with `u8`. Where the receiver brings none they
        // stay as the block wrote them, and the call site checks past them.
        if let Some(impl_def) = sig.declaring_impl
            && let Some(declaring) = self.tysys.signatures.impl_sig(impl_def).cloned()
            && !declaring.target_type_args.is_empty()
            && let Some(args) = receiver_type.and_then(|ty| {
                self.tysys
                    .type_table
                    .borrow()
                    .nominal_type_args(self.tysys.get_base_type(ty))
            })
        {
            let instantiated =
                sig.instantiate_call_with(&self.tysys.type_table, Some(&declaring), &args, &[]);
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
