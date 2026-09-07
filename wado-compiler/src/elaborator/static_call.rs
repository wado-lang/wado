//! The one walk behind a `Type::method(...)` spelling: whether it names a
//! static, which declaration, and that declaration's parameters, return type
//! and slots, all from a single resolution.
//! See `docs/wep-2026-09-06-static-call-resolution.md`.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::defs::DefId;
use crate::module_source::ModuleSource;
use crate::tir::{TypeId, TypeTable};

use super::Elaborator;
use super::callee::StaticMethodRef;
use super::sem::types::CalleeParams;
use super::trait_env::ImplTargetKey;

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

/// One block on the receiver whose trait supplies a static of the name asked
/// for. Read off the block, before anything is resolved.
pub(super) struct TraitSupply {
    trait_decl: DefId,
    impl_def: DefId,
    /// The block declares no body of its own, so the trait's default answers.
    inherited: bool,
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
    /// `arg_hint` names its first argument's type, which is what separates
    /// several impls of one trait. Both are the call's one vantage — resolving
    /// without them and mangling with them gives two answers for one call.
    pub(super) fn resolve_static_callee(
        &mut self,
        site: Option<ast::AstId>,
        receiver_name: &str,
        receiver_key: Option<&ImplTargetKey>,
        method_name: &str,
        arg_hint: Option<&str>,
        receiver_type: Option<TypeId>,
    ) -> StaticLookup {
        // One vantage: the key the caller resolved, else the one its reference
        // site answered for, else the name's own. Deriving a second key when
        // the first misses makes the order a silent tiebreak.
        let key = match receiver_key {
            Some(key) => key.clone(),
            None => self.impl_target_at(site, receiver_name),
        };

        // The receiver's own declaration, which shadows every inherited one as
        // dot syntax does — but only of the same kind: a receiver-less
        // declaration beside an instance one is no alternative, since different
        // argument lists reach them. A trait impl's declaration falls to the
        // selection below, which names the trait its call is mangled with, and
        // several of one kind are an overload only an argument separates.
        let (inherent, overloaded) = {
            let entries: Vec<(DefId, bool, bool)> = self
                .impl_method_entries(&key, method_name)
                .map(|entry| (entry.method_id, entry.has_self, entry.is_inherent()))
                .collect();
            let first = entries.first().copied();
            let same_kind = first.map_or(0, |(_, has_self, _)| {
                entries
                    .iter()
                    .filter(|(_, kind, _)| *kind == has_self)
                    .count()
            });
            let inherent = first
                .filter(|&(_, _, inherent)| inherent && same_kind == 1)
                .map(|(def, _, _)| def);
            (inherent, same_kind > 1)
        };
        if let Some(def) = inherent
            && let Some(resolved) =
                self.callee_of_own_declaration(receiver_name, method_name, def, receiver_type)
        {
            return resolved;
        }

        // A resource declares its statics like any other declaration.
        let resource_static = self
            .tysys
            .trait_env
            .resource_static(&key, method_name)
            .map(|(name, _, decl, _)| (name.clone(), *decl))
            .and_then(|(name, decl)| {
                Some(self.tysys.signatures.resource_method_sig(decl, &name)?.def)
            });
        if let Some(def) = resource_static
            && let Some(resolved) =
                self.callee_of_own_declaration(receiver_name, method_name, def, receiver_type)
        {
            return resolved;
        }

        // The index holds only the declaring resource's own methods, so one it
        // inherits is reached by walking the chain.
        let inherited = match &key {
            ImplTargetKey::Decl(def) => self
                .resource_instance_method(*def, method_name)
                .map(|(_, sig)| sig.def),
            _ => None,
        };
        if let Some(def) = inherited
            && let Some(resolved) =
                self.callee_of_own_declaration(receiver_name, method_name, def, receiver_type)
        {
            return resolved;
        }

        // A case or member the receiver declares is written on the type, and
        // shadows a static it only inherits — the rule the rungs above apply to
        // an inherent declaration. Its own arm builds the constructor.
        if self.declares_case_named(&key, method_name) {
            return StaticLookup::NotStatic;
        }

        // Two *traits* supplying the name leaves the spelling naming neither,
        // and no argument can separate traits — impls of different traits share
        // no contract. Asked before the overload below, which is the case an
        // argument *does* separate: one trait implemented several times. Asked
        // over blocks that declare a body and blocks that inherit the trait's
        // alike, since which of the two it is decides nothing here.
        let supplies = self.traits_supplying_static(&key, method_name);
        if let Some(alternatives) = self.ambiguous_alternatives(&supplies) {
            return StaticLookup::Ambiguous(alternatives);
        }

        // With no argument to read, the selection below cannot pick among
        // several declarations of one name: it would take the first, and a call
        // site checking against that list rejects every argument the others
        // accept. `i32::from(b)` names one `From` impl per source type, and the
        // conversion preselect is what reads the argument.
        if overloaded && arg_hint.is_none() {
            return self.overloaded_callee(&key, method_name);
        }

        // `receiver_key` as the caller gave it, not the key derived above: a
        // derived one narrows the search to a declaration the impls may not be
        // indexed under, putting a primitive's `impl FromStr for f32` out of
        // reach.
        let through_traits = self.resolve_static_callee_through_traits(
            receiver_name,
            receiver_key,
            method_name,
            arg_hint,
            receiver_type,
            &supplies,
        );
        if through_traits.found().is_some() || matches!(through_traits, StaticLookup::Ambiguous(_))
        {
            return through_traits;
        }
        // The selection above reads the argument; where it could not pick, the
        // name is an overload the call site's own resolution settles.
        if overloaded {
            return self.overloaded_callee(&key, method_name);
        }

        // A newtype and a `flags` reach what they wrap: their impls are looked
        // up on the base, so the spelling resolves there too.
        match self.newtype_base(receiver_name) {
            Some((_, base_name)) => self.resolve_static_callee(
                None,
                &base_name,
                None,
                method_name,
                arg_hint,
                receiver_type,
            ),
            None => through_traits,
        }
    }

    /// The trait half of [`Self::resolve_static_callee`]: a block on the
    /// receiver that declares the method, one that inherits the trait's default
    /// body, or the auto-derived `Default`.
    fn resolve_static_callee_through_traits(
        &mut self,
        receiver_name: &str,
        written_key: Option<&ImplTargetKey>,
        method_name: &str,
        arg_hint: Option<&str>,
        receiver_type: Option<TypeId>,
        supplies: &[TraitSupply],
    ) -> StaticLookup {
        // An inherited method is declared nowhere but the trait, so that is
        // where its signature is read from. Only one trait reaches here: the
        // caller reported the ambiguity.
        if let Some((decl, impl_def)) = supplies
            .iter()
            .find(|supply| supply.inherited)
            .map(|supply| (supply.trait_decl, supply.impl_def))
        {
            // Resolved here rather than asked of every caller: the name costs a
            // scope to resolve, and this is the one rung whose frame needs it.
            let receiver_type = receiver_type.or_else(|| {
                let mut scope = self.enter_inherited_type_param_scope();
                let resolved =
                    scope.resolve_unsited_type_name(receiver_name, crate::token::Span::default());
                drop(scope);
                Some(resolved)
            });
            let module = self.tysys.resolutions.defs().module(impl_def).clone();
            // The block's own trait reference, arguments included — the form
            // the body is emitted under. Minting one from the trait declaration
            // instead drops them, and `impl Enc<A> for M` inheriting a default
            // body then names `Enc::make` where `Enc<A>::make` was emitted.
            let trait_name = {
                let header = &self.tysys.trait_env.impl_headers[&impl_def];
                self.tysys
                    .trait_env
                    .fq_trait_of_impl(header, &self.tysys.resolutions)
            };
            if let Some(callee) = self.callee_of_trait_declaration(
                receiver_name,
                method_name,
                decl,
                module,
                receiver_type,
                trait_name,
            ) {
                return StaticLookup::Found(Box::new(callee));
            }
        }

        if let Some(method_ref) =
            self.locate_static_method_impl(receiver_name, method_name, arg_hint, written_key)
        {
            let found = match method_ref.method_id {
                // The selection reaches a trait's own declaration too, where a
                // block on the receiver declares the name under another trait
                // and this one only inherits it. Two tables hold a method's
                // signature and a `DefId` does not say which: the impl-declared
                // ones are `method_sig`'s, a trait's own its `TraitSig`'s.
                Some(def) if self.tysys.signatures.method_sig(def).is_none() => method_ref
                    .trait_name
                    .as_ref()
                    .and_then(crate::name::FqTraitName::canonical)
                    .and_then(|decl| {
                        let module = method_ref.module.clone();
                        // The selection's own trait reference, not one re-minted
                        // from `decl`: it is the form the body was emitted under.
                        let trait_name = method_ref.trait_name.clone();
                        self.callee_of_trait_declaration(
                            receiver_name,
                            method_name,
                            decl,
                            module,
                            receiver_type,
                            trait_name,
                        )
                    })
                    .map(|callee| StaticLookup::Found(Box::new(callee))),
                Some(def) => self.callee_of_declaration(def, method_ref, receiver_type),
                // The auto-derived `Default`: synthesis emits the body, so no
                // declaration backs it. It takes no arguments and answers with
                // the receiver's own type.
                None => Some(StaticLookup::Found(Box::new(StaticCallee {
                    params: CalleeParams::default(),
                    own_params: Vec::new(),
                    return_type: self
                        .tysys
                        .auto_derive_default_struct_type(&self.type_lookup(), receiver_name)
                        .unwrap_or(TypeTable::UNKNOWN),
                    method_ref,
                }))),
            };
            if let Some(resolved) = found {
                return resolved;
            }
        }
        StaticLookup::NotStatic
    }

    /// The resolution for a method a block inherits with the trait's default
    /// body. Its signature is written in the trait's frame, where `Self` leads
    /// the slots, so it is read at the receiver.
    /// `module` is the block's, not the trait's: the body is emitted for the
    /// block that inherits it, so that is what the call names, while the
    /// defaults are the trait's and resolve where it wrote them.
    fn callee_of_trait_declaration(
        &mut self,
        receiver_name: &str,
        method_name: &str,
        trait_decl: DefId,
        module: ModuleSource,
        receiver_type: Option<TypeId>,
        trait_name: Option<crate::name::FqTraitName>,
    ) -> Option<StaticCallee> {
        let declaring = self.tysys.signatures.trait_sig(trait_decl)?;
        let sig = declaring.method(method_name)?.sig.clone();
        let mut params = CalleeParams::of_signature(Some(&sig));
        params.defaults_module = Some(declaring.module.clone());
        let mut return_type = sig.decl.return_type.unwrap_or(TypeTable::UNIT);
        if let Some(receiver_type) = receiver_type {
            let instantiated = sig.instantiate_call(&self.tysys.type_table, &[receiver_type], &[]);
            params.param_types = instantiated.param_types;
            return_type = instantiated.return_type;
        }
        Some(StaticCallee {
            params,
            own_params: sig.own_params,
            return_type,
            method_ref: StaticMethodRef::new(
                module,
                receiver_name,
                method_name,
                trait_name,
                Some(sig.def),
            ),
        })
    }

    /// The traits to name when several supply one static, in the order the
    /// blocks were written. `None` where one trait answers, however many times
    /// it is implemented: naming it as both alternatives is a remedy nobody can
    /// write.
    fn ambiguous_alternatives(&self, supplies: &[TraitSupply]) -> Option<Vec<String>> {
        let mut distinct: Vec<DefId> = Vec::new();
        for supply in supplies {
            if !distinct.contains(&supply.trait_decl) {
                distinct.push(supply.trait_decl);
            }
        }
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

    /// The traits a block on the receiver implements that supply `method_name`
    /// as a static, whether the block declares its own body or leaves the
    /// trait's default to answer. Read off the blocks rather than resolved:
    /// resolving is what the call has not done yet, and doing it here decides
    /// an overload.
    fn traits_supplying_static(&self, key: &ImplTargetKey, method_name: &str) -> Vec<TraitSupply> {
        let Some(impls) = self.tysys.trait_env.impl_index.get(key) else {
            return Vec::new();
        };
        impls
            .iter()
            .filter_map(|&impl_def| {
                let impl_sig = self.tysys.signatures.impl_sig(impl_def)?;
                let trait_decl = impl_sig.trait_decl?;
                let declared = self
                    .tysys
                    .signatures
                    .trait_sig(trait_decl)?
                    .method(method_name)?;
                if declared.sig.self_kind != ast::SelfKind::None {
                    return None;
                }
                let written_here = self.tysys.trait_env.impl_headers[&impl_def]
                    .methods
                    .iter()
                    .any(|m| m.name == method_name);
                // A required method the block leaves undeclared is its own
                // error, reported where the two are compared.
                (written_here || declared.default_body.is_some()).then_some(TraitSupply {
                    trait_decl,
                    impl_def,
                    inherited: !written_here,
                })
            })
            .collect()
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
