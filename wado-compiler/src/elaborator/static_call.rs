//! The one walk behind a `Type::method(...)` spelling.
//!
//! Every question a static call asks — whether the spelling names a static at
//! all, which declaration it picked, what that declaration's parameters, return
//! type and type parameters are — is answered from a single resolution. The
//! lookups this replaced each walked their own subset of the ladder in their
//! own order, so two of them could disagree about one call: the spelling
//! resolved, and its signature did not.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::defs::DefId;
use crate::tir::{TypeId, TypeTable};

use super::Elaborator;
use super::callee::StaticMethodRef;
use super::sem::types::CalleeParams;
use super::trait_env::ImplTargetKey;

/// What one `Type::method(...)` spelling names.
pub(super) struct StaticCallee {
    /// The declaration picked: where it lives, the trait it came through, and
    /// the identity a call is mangled and a use→def edge recorded from.
    ///
    /// `None` alongside a `None` `params`: a name several declarations answer
    /// to picks none of them until an argument does.
    pub(super) method_ref: Option<StaticMethodRef>,
    /// The declaration's lists, read at the receiver: what the call checks and
    /// pads against.
    ///
    /// `None` where the spelling resolves but no list it can be checked against
    /// comes with it — several declarations an argument picks among, or a
    /// generic block whose slots the receiver did not fill. The call site
    /// substitutes from its own inference and counts its own arguments, as it
    /// did before any lookup answered. The return type is known either way.
    pub(super) params: Option<CalleeParams>,
    /// The method's own slots as its declaration wrote them.
    pub(super) own_params: Vec<ast::GenericParam>,
    /// What the call evaluates to, read at the receiver as `params` is.
    /// `UNKNOWN` where the declaration names no return type.
    pub(super) return_type: TypeId,
}

/// The resolution's outcome. `NotStatic` is an answer, not a failure: the
/// spelling belongs to a variant case or a flags member, whose own arm counts
/// its arguments.
pub(super) enum StaticLookup {
    Found(Box<StaticCallee>),
    /// Several traits supply the name with a default body. No argument
    /// separates them, so the spelling names none: the call site reports it.
    Ambiguous(Vec<String>),
    NotStatic,
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
            Self::Ambiguous(_) | Self::NotStatic => None,
        }
    }

    /// The parameters to check and pad against, and whether any came with the
    /// resolution.
    pub(super) fn params(self) -> (CalleeParams, bool) {
        match self {
            Self::Found(callee) => match callee.params {
                Some(params) => (params, true),
                None => (CalleeParams::default(), false),
            },
            Self::Ambiguous(_) | Self::NotStatic => (CalleeParams::default(), false),
        }
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Resolve what `receiver_name::method_name` names, once, for every site
    /// that used to ask its own question.
    ///
    /// `receiver_key` is the key the caller's own reference site resolved to,
    /// where it has one; deriving a second key here would answer from another
    /// vantage than the call. `arg_hint` names the sole argument's type for the
    /// `From` / `TryFrom` overloads that only an argument separates.
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

        // An inherent declaration shadows every inherited one, as dot syntax
        // resolves it: selecting a trait's would mangle the call to a body the
        // spelling does not name. Both kinds of method, receiver-less first —
        // the spelling admits an instance one, writing the receiver as its
        // first argument.
        // Several declarations of one name are an overload only an argument
        // separates. Committing to the first would decide it here, so the rung
        // declines and the selection below — which reads the argument — picks.
        let (inherent, overloaded) = {
            let entries: Vec<(DefId, bool, bool)> = self
                .impl_method_entries(&key, method_name)
                .map(|entry| (entry.method_id, entry.has_self, entry.is_inherent()))
                .collect();
            let first = entries.first().copied();
            // Only the same kind counts: two receiver-less declarations of one
            // name are an overload an argument separates, while a receiver-less
            // one beside an instance one is the per-kind shadowing the index has
            // already applied — different argument lists reach them.
            let alternatives = first.map_or(0, |(_, has_self, _)| {
                entries
                    .iter()
                    .filter(|(_, kind, _)| *kind == has_self)
                    .count()
            });
            // Only a declaration the receiver makes itself: a trait impl's is
            // reached through the selection below, which names the trait its
            // call is mangled with. Named without one, the call reaches WIR
            // build as a body nothing declares.
            let inherent = first
                .filter(|&(_, _, inherent)| inherent && alternatives == 1)
                .map(|(def, _, _)| def);
            (inherent, alternatives > 1)
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

        // An overload with no argument to read cannot be picked here at all:
        // the selection below takes the first candidate, which is the wrong one
        // as often as not. The call site resolves it from its arguments, and
        // the return type still answers where every candidate agrees — each
        // `From` impl returns the receiver, so the pick cannot change it.
        if overloaded && arg_hint.is_none() {
            return self.overloaded_callee(&key, method_name);
        }

        // `receiver_key` as the caller gave it, not the key derived above: a
        // derived one narrows the search to a declaration the impls may not be
        // indexed under, and a primitive's `impl FromStr for f32` is then out
        // of reach. Where the caller resolved a key at its own reference site
        // — a namespace-qualified receiver — that key is the vantage.
        let through_traits = self.resolve_static_callee_through_traits(
            receiver_name,
            &key,
            receiver_key,
            method_name,
            arg_hint,
            receiver_type,
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
        key: &ImplTargetKey,
        written_key: Option<&ImplTargetKey>,
        method_name: &str,
        arg_hint: Option<&str>,
        receiver_type: Option<TypeId>,
    ) -> StaticLookup {
        // Two traits supplying the name leaves the spelling naming neither.
        // Unreported, the call is built to whichever impl came first and
        // reaches codegen as a module nothing validates.
        let declaring = self.traits_inheriting_static(key, method_name);
        if declaring.len() > 1 {
            return StaticLookup::Ambiguous(
                declaring.into_iter().map(|(_, _, name)| name).collect(),
            );
        }
        // The one trait's own declaration is where an inherited method's
        // signature lives: `method_sig` answers for what an `impl` block
        // declares, and this method is declared nowhere but the trait.
        if let Some((decl, impl_def)) = declaring
            .first()
            .map(|&(decl, impl_def, _)| (decl, impl_def))
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
            if let Some(callee) = self.callee_of_trait_declaration(
                receiver_name,
                method_name,
                decl,
                impl_def,
                receiver_type,
            ) {
                return StaticLookup::Found(Box::new(callee));
            }
        }

        if let Some(method_ref) =
            self.locate_static_method_impl(receiver_name, method_name, arg_hint, written_key)
        {
            let found = match method_ref.method_id {
                Some(def) => self.callee_of_declaration(def, method_ref, receiver_type),
                // The auto-derived `Default`: synthesis emits the body, so no
                // declaration backs it. It takes no arguments and answers with
                // the receiver's own type.
                None => Some(StaticLookup::Found(Box::new(StaticCallee {
                    params: Some(CalleeParams::default()),
                    own_params: Vec::new(),
                    return_type: self
                        .tysys
                        .auto_derive_default_struct_type(&self.type_lookup(), receiver_name)
                        .unwrap_or(TypeTable::UNKNOWN),
                    method_ref: Some(method_ref),
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
    fn callee_of_trait_declaration(
        &mut self,
        receiver_name: &str,
        method_name: &str,
        trait_decl: DefId,
        impl_def: DefId,
        receiver_type: Option<TypeId>,
    ) -> Option<StaticCallee> {
        let declaring = self.tysys.signatures.trait_sig(trait_decl)?;
        // Two modules, and a call needs both: the body is emitted for the block
        // that inherits it, so that is what the call names, while the defaults
        // are the trait's and resolve where it wrote them. Naming the trait's
        // for both mints an extern stub for a body the package defines.
        let module = self.tysys.resolutions.defs().module(impl_def).clone();
        let sig = declaring.method(method_name)?.sig.clone();
        let mut params = CalleeParams::of_signature(Some(&sig));
        params.defaults_module = Some(declaring.module.clone());
        let mut return_type = sig.decl.return_type.unwrap_or(TypeTable::UNIT);
        if let Some(receiver_type) = receiver_type {
            let instantiated = sig.instantiate_call(&self.tysys.type_table, &[receiver_type], &[]);
            params.param_types = instantiated.param_types;
            return_type = instantiated.return_type;
        }
        let trait_name =
            crate::name::FqTraitName::declared(self.tysys.resolutions.defs(), trait_decl);
        Some(StaticCallee {
            params: Some(params),
            own_params: sig.own_params,
            return_type,
            method_ref: Some(StaticMethodRef::new(
                module,
                receiver_name,
                method_name,
                Some(trait_name),
                Some(sig.def),
            )),
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
        StaticLookup::Found(Box::new(StaticCallee {
            params: None,
            own_params: Vec::new(),
            return_type: agreed,
            method_ref: None,
        }))
    }

    /// The traits a block on the receiver implements that supply `method_name`
    /// with a default body, none of the blocks declaring it themselves. Read
    /// off the blocks rather than resolved: resolving is what the call has not
    /// done yet, and doing it here decides an overload.
    fn traits_inheriting_static(
        &self,
        key: &ImplTargetKey,
        method_name: &str,
    ) -> Vec<(DefId, DefId, String)> {
        if self
            .qualified_method_decl_ids(key, method_name)
            .next()
            .is_some()
        {
            return Vec::new();
        }
        let Some(impls) = self.tysys.trait_env.impl_index.get(key) else {
            return Vec::new();
        };
        impls
            .iter()
            .filter_map(|&impl_def| {
                let impl_sig = self.tysys.signatures.impl_sig(impl_def)?;
                let decl = impl_sig.trait_decl?;
                let declared = self.tysys.signatures.trait_sig(decl)?.method(method_name)?;
                // A required method the block leaves undeclared is its own
                // error, reported where the two are compared.
                (declared.default_body.is_some() && declared.sig.self_kind == ast::SelfKind::None)
                    .then(|| {
                        (
                            decl,
                            impl_def,
                            self.tysys.resolutions.defs().name(decl).to_string(),
                        )
                    })
            })
            .collect()
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
        let mut listed = true;
        // A trait's own declaration — the method a block inherits with its
        // default body — is written in the trait's frame, where `Self` leads
        // the slots. Read it at the receiver, or every type it names stays a
        // slot the call site cannot check against. A caller asking only whether
        // the spelling resolves brings no receiver type and reads neither.
        if sig.declaring_impl.is_none()
            && let Some(receiver_type) = receiver_type
        {
            let instantiated = sig.instantiate_call(&self.tysys.type_table, &[receiver_type], &[]);
            params.param_types = instantiated.param_types;
            return_type = instantiated.return_type;
        }
        // A generic block's slots are the receiver's arguments — `ByteList`
        // fills `List<T>`'s `T` with `u8`. Reported unfilled, the list says the
        // call takes a `T`, which no argument is; reported empty, it says the
        // call takes nothing. Where the receiver brings no arguments, this
        // lookup has no list to offer at all.
        if let Some(impl_def) = sig.declaring_impl
            && let Some(declaring) = self.tysys.signatures.impl_sig(impl_def).cloned()
            && !declaring.target_type_args.is_empty()
        {
            let receiver_args = receiver_type.and_then(|ty| {
                self.tysys
                    .type_table
                    .borrow()
                    .nominal_type_args(self.tysys.get_base_type(ty))
            });
            match receiver_args {
                Some(args) => {
                    let instantiated = sig.instantiate_call_with(
                        &self.tysys.type_table,
                        Some(&declaring),
                        &args,
                        &[],
                    );
                    params.param_types = instantiated.param_types;
                    return_type = instantiated.return_type;
                }
                // The return type is the declaration's own, as it was before
                // any list came with it: only the list needs the slots filled.
                None => listed = false,
            }
        }
        Some(StaticLookup::Found(Box::new(StaticCallee {
            params: listed.then_some(params),
            own_params: sig.own_params,
            return_type,
            method_ref: Some(method_ref),
        })))
    }
}
