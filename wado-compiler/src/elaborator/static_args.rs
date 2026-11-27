//! A static call's arguments: the impl they preselect, the type arguments
//! they settle, and the check of each against the callee's parameters.

use super::Elaborator;
use super::call::ArgSite;
use super::call::CallSolve;
use super::call::DefaultTypeBinding;
use super::call::GivenSlots;
use super::call::InheritedStatic;
use super::call::SigChoice;
use super::call::StaticCallee;
use super::call::StaticTypeArgs;
use super::infer_hole::BoundRows;
use super::instantiate::InstanceKind;
use super::instantiate::Instantiation;
use super::instantiate::static_slot_owner;
use super::method_call::ArgCandidate;
use super::method_call::ArgPreselect;
use super::method_call::PreselectedArg;
use super::method_call::StaticArgSurvey;
use super::method_call::StaticReceiver;
use super::method_call::render_type_list;
use super::method_call::render_type_list_qualified;
use super::method_lookup::MethodInferenceInput;
use super::sig::MethodSig;
use super::static_call::ReceiverId;
use super::static_call::Selector;
use super::static_call::StaticLookup;
use super::static_call::StaticQuery;
use super::synth::ArgClass;
use super::trait_env::ImplTargetKey;
use super::trait_query::Unsettled;
use super::types::FunctionContext;
use super::types::TypeError;
use crate::Span;
use crate::ast;
use crate::ast::Expr;
use crate::compiler_host::CompilerHost;
use crate::defs::DefId;
use crate::elaborator::SelfBinding;
use crate::elaborator::call::slot_type_bindings;
use crate::tir::EffectRows;
use crate::tir::TypeArgMap;
use crate::tir::TypeId;
use crate::token;
use crate::type_table::TypeTable;
use std::sync::Arc;

impl<H: CompilerHost> Elaborator<'_, H> {
    /// The type arguments a `Type::method(..)` call dispatches with: the impl
    /// level inferred from the receiver, and the `written` turbofish with
    /// inference filling each `_`. One function, so the bare and namespaced
    /// spellings cannot drift apart. `None` once a pack's grouping is reported.
    #[expect(clippy::too_many_arguments)]
    pub(super) fn static_call_type_args(
        &mut self,
        callee: StaticCallee<'_>,
        raw_args: &[Expr],
        args: &[TypeId],
        expected_type: Option<TypeId>,
        span: token::Span,
        mut written: Vec<TypeId>,
        given: Option<GivenSlots<'_>>,
    ) -> Option<StaticTypeArgs> {
        // The method's own parameters, in the dense space its type arguments
        // are indexed by — an effect or `fn`-bound parameter holds no slot in
        // one.
        let params: Vec<ast::GenericParam> = self
            .lookup_static_method_type_params(callee.receiver, callee.method_name)
            .into_iter()
            .filter(ast::GenericParam::is_real_type_param)
            .collect();
        // Before anything counts slots, since a pack's arguments are one per
        // element until they are grouped.
        if self.group_variadic_type_args_of(callee.method_name, &params, &mut written, span) {
            return None;
        }
        // An unwritten turbofish leaves the impl level to infer as well, which
        // the method's own slot count says nothing about.
        let expected_args =
            self.expected_receiver_args(callee.receiver, callee.method_name, expected_type);
        let (method_def, [declaring_slots, own_slots], effect_rows) = self
            .owner_static_sig(
                callee.receiver,
                callee.method_name,
                SigChoice::Any,
                &expected_args,
            )
            .map_or((None, [0, 0], EffectRows::default()), |sig| {
                (
                    Some(sig.def),
                    [
                        sig.declaring_type_params().len(),
                        sig.own_type_params().len(),
                    ],
                    self.tysys
                        .type_table
                        .borrow()
                        .call_effect_binding(&sig.decl.param_types, args),
                )
            });
        let names_own = !written.is_empty() && !self.turbofish_leaves_slot(&written, own_slots);
        // A turbofish names only the method's own slots, so the receiver's are
        // inferred wherever it declares any.
        let (impl_args, mut method_args) = if names_own && declaring_slots == 0 {
            (Vec::new(), written)
        } else {
            let (impl_args, inferred) = self.infer_static_method_type_args(
                callee.receiver,
                callee.method_name,
                raw_args,
                args,
                expected_type,
                span,
                given,
            );
            if !names_own {
                self.merge_turbofish_type_args(&mut written, &inferred);
            }
            (impl_args, written)
        };
        // A method-level parameter bound only through another's associated
        // type (`..V` off `Holes`) is projected once the owner is inferred, as
        // the free-function path does; the bound check then sees it concrete.
        if !method_args.is_empty() {
            self.project_assoc_bound_args(method_def, &params, &mut method_args);
        }
        Some(StaticTypeArgs {
            params,
            impl_args,
            method_args,
            effect_rows,
        })
    }

    /// Holds a static call's own type arguments to the method's bounds.
    /// `receiver` is what `Self` means at the call, so a bound written
    /// `Assoc = Self::Item` projects off it as it does on a method call.
    pub(super) fn enforce_static_method_bounds(
        &mut self,
        type_args: &StaticTypeArgs,
        receiver: Option<TypeId>,
        span: token::Span,
    ) {
        let self_binding = receiver.map(|receiver| SelfBinding::of_static(receiver, None));
        self.enforce_type_arg_bounds(
            &type_args.params,
            &type_args.method_args,
            &[],
            self_binding,
            &type_args.effect_rows,
            span,
        );
    }

    /// Infer a generic static method's type arguments in the call's own
    /// variables: `given` where the call instantiated this signature's slots
    /// before its arguments resolved, fresh ones otherwise. Returns
    /// `(declaring_args, method_args)` — the first from `impl Container<T>`, the
    /// second from `fn make<U>()` — either possibly empty. A slot nothing
    /// answers comes back as the declaration's own parameter.
    #[expect(clippy::too_many_arguments)]
    pub(super) fn infer_static_method_type_args(
        &mut self,
        receiver: &ReceiverId,
        method_name: &str,
        raw_args: &[Expr],
        args: &[TypeId],
        expected_type: Option<TypeId>,
        span: token::Span,
        given: Option<GivenSlots<'_>>,
    ) -> (Vec<TypeId>, Vec<TypeId>) {
        let expected_args = self.expected_receiver_args(receiver, method_name, expected_type);
        // The resolution reads these arguments and finds nothing to call.
        if self.declared_by_no_reaching_block(receiver, method_name, &expected_args) {
            return (expected_args, vec![]);
        }
        let key = receiver.key();
        if let Some(owner) = self.newtype_static_target(receiver, method_name) {
            return self.infer_through_newtype(
                key,
                owner,
                method_name,
                raw_args,
                args,
                expected_type,
                span,
                given,
            );
        }
        let Some(sig) =
            self.static_call_sig_or_inherited(key, method_name, SigChoice::Any, &expected_args)
        else {
            return (self.derived_impl_args(key, method_name, args), vec![]);
        };
        if sig.decl.type_params.is_empty() {
            return (vec![], vec![]);
        }

        let all_param_ids = sig.decl.type_param_ids();
        let decl_return_type = sig.decl.return_type;

        // `args` are the arguments as written, so they begin with the receiver
        // when the method declares one. Dropping it here would slide every pair
        // one position left and solve each slot from the wrong argument.
        let (names, owners) = self.sig_slots(&sig);
        let fresh;
        let given = given.filter(|given| given.slots == all_param_ids);
        let inst = if let Some(given) = given {
            given.inst
        } else {
            fresh = self.instantiate(
                &all_param_ids,
                &Instantiation {
                    kind: InstanceKind::Method,
                    name: method_name,
                    span,
                    type_args: &[],
                    self_binding: None,
                    impl_args: &[],
                },
            );
            &fresh
        };
        let param_types = self.instantiate_types(&sig.decl.param_types, inst);
        let ret = decl_return_type.map(|declared| self.instantiate_type(declared, inst));
        let solved = self.solve_call_in_context(CallSolve {
            slots: &inst.vars,
            param_types: &param_types,
            args,
            raw_args,
            ret: ret.zip(expected_type),
        });
        // A given slot still open stays the call's own variable, which the
        // arguments were typed by and a literal among them may wait at.
        let answered_any = given.is_some() || solved != inst.vars;
        let mut inferred: Vec<TypeId> = if given.is_some() {
            solved
        } else {
            // The site reports the slot each parameter below stands for.
            self.abandon(inst);
            solved
                .into_iter()
                .zip(&all_param_ids)
                .map(|(answer, &param)| {
                    if self.tysys.type_table.borrow().is_infer_var(answer) {
                        param
                    } else {
                        answer
                    }
                })
                .collect()
        };
        let split = sig.declaring_split();

        // A method-level slot the arguments do not pin takes the default its
        // declaration wrote (WEP 2026-04-11), as the instance spelling does.
        let defaulted = self.fill_static_default_type_args(
            &sig,
            &receiver.at_type(None),
            &mut inferred[split..],
        );
        if !defaulted && !answered_any {
            return (vec![], vec![]);
        }
        // A slot the arguments reach only through a bound's associated type
        // (`impl<T> W<T> { fn of<S: AsSlice<Elem = T>>(s: &S) }`, or the
        // block's own `impl<T: Tr<Out = U>, U>`) is projected off the owner
        // the arguments did settle.
        self.resolve_assoc_bounds_in(&owners, 0, &names, &mut inferred);
        // The arguments were typed by the given variables, which a default or
        // a projection above answers too.
        if given.is_some() {
            self.record_instantiation(inst, &inferred);
        }
        (
            self.declaring_slot_values(&sig, &inferred[..split]),
            inferred[split..].to_vec(),
        )
    }

    /// [`Self::infer_static_method_type_args`] on a newtype declaring nothing of
    /// the name, which calls its base's at `base`: solved there, then held to
    /// the shape the newtype gives the base's arguments, so
    /// `type Wl<T> = Wrap<List<T>>` takes a `List<i32>` for `Wrap`'s slot and
    /// never an `i32`. A slot that shape leaves open comes back as a bare
    /// variable nothing answers, which the uninferred report then names. An
    /// expected instance of the newtype is read at the base, as the arguments
    /// are.
    #[expect(clippy::too_many_arguments)]
    fn infer_through_newtype(
        &mut self,
        key: &ImplTargetKey,
        base_key: ImplTargetKey,
        method_name: &str,
        raw_args: &[Expr],
        args: &[TypeId],
        expected_type: Option<TypeId>,
        span: token::Span,
        given: Option<GivenSlots<'_>>,
    ) -> (Vec<TypeId>, Vec<TypeId>) {
        let newtype = key
            .decl()
            .expect("a newtype receiver names its declaration");
        let (base_args, expected_at_base) =
            self.at_inherited_base(Some(newtype), base_key.decl(), args, expected_type);
        let base = ReceiverId::keyed(base_key.clone(), None);
        let (inferred, method_args) = self.infer_static_method_type_args(
            &base,
            method_name,
            raw_args,
            &base_args,
            expected_at_base,
            span,
            given,
        );
        match self.newtype_at_base_args(key, base_key.decl(), &inferred) {
            Some((_, shaped)) => {
                // The base's slots are the call's own variables, which the
                // arguments were typed by: a shape the newtype fixes answers
                // them. One it leaves open is reported as the newtype's.
                for (&slot, &answer) in inferred.iter().zip(&shaped) {
                    if !self.tysys.type_table.borrow().contains_infer_var(answer) {
                        self.equate(slot, answer);
                    }
                }
                (shaped, method_args)
            }
            None => (inferred, method_args),
        }
    }

    /// Defer the slots a static call left unanswered to the end of the body,
    /// as [`Self::defer_or_report_uninferred_fn_type_args`] does a free
    /// function's: a slot left as the callee's own binder would read as
    /// whatever the caller declares under that name. `receiver` is what `Self`
    /// means at the call.
    pub(super) fn defer_uninferred_static_method_type_args(
        &mut self,
        callee: StaticCallee,
        type_args: &mut StaticTypeArgs,
        receiver: Option<TypeId>,
        span: token::Span,
    ) {
        let StaticCallee {
            receiver: static_receiver,
            method_name: suffix,
        } = callee;
        let prefix = static_receiver
            .spelled(self.tysys.resolutions.defs())
            .to_string();
        let StaticTypeArgs {
            impl_args: impl_type_args,
            method_args: method_type_args,
            ..
        } = type_args;
        // This report runs before any resolution, so where several impls
        // declare the name it has no pick to read: complaining about one of
        // their slots names a declaration the arguments may not even select.
        let sig = self.owner_static_sig(static_receiver, suffix, SigChoice::Unique, &[]);
        let Some(sig) = sig else {
            self.report_uninferred_derived_type_args(static_receiver, suffix, impl_type_args, span);
            return;
        };
        let (declaring_slots, method_slots) = (sig.declaring_type_params(), sig.own_type_params());
        if declaring_slots.is_empty() && method_slots.is_empty() {
            return;
        }

        let scope_params = self.scope_type_param_ids();
        let unresolved = |this: &Self, slot: Option<&TypeId>| -> bool {
            slot.is_none_or(|&t| this.slot_unanswered(t, &scope_params))
        };

        let impl_unresolved =
            self.receiver_slots_where(&sig, impl_type_args, |this, slot| unresolved(this, slot));
        let method_unresolved: Vec<(usize, String)> = method_slots
            .iter()
            .enumerate()
            .filter(|&(i, _)| unresolved(self, method_type_args.get(i)))
            .map(|(i, (name, _))| (i, name.clone()))
            .collect();
        if impl_unresolved.is_empty() && method_unresolved.is_empty() {
            return;
        }
        let owner = static_slot_owner(&prefix, suffix, !impl_unresolved.is_empty());
        let trait_env = Arc::clone(&self.tysys.trait_env);
        let impl_params: &[ast::GenericParam] = sig
            .declaring_impl
            .map_or(&[], |def| &trait_env.impl_headers[&def].type_params);
        for (type_args, unresolved, declaring) in [
            (&mut *impl_type_args, &impl_unresolved, true),
            (&mut *method_type_args, &method_unresolved, false),
        ] {
            for (slot, name) in unresolved {
                let inferred = type_args.get(*slot).copied();
                if type_args.len() <= *slot {
                    type_args.resize(slot + 1, TypeTable::ERROR);
                }
                let param = if declaring {
                    impl_params.iter().find(|p| &p.name == name)
                } else {
                    sig.own_params.get(*slot)
                };
                let bounds = param.map_or_else(Vec::new, |p| {
                    self.declared_bounds(p, None, &[], BoundRows::Unbound)
                });
                type_args[*slot] =
                    self.defer_slot(inferred, span, owner.clone(), name.clone(), bounds);
            }
        }
        let (names, owners) = self.sig_slots(&sig);
        let split = sig.declaring_split();
        let args: Vec<TypeId> = sig
            .decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, &(_, id))| {
                if i < split {
                    impl_type_args[self.declared_slot(id) as usize]
                } else {
                    method_type_args[i - split]
                }
            })
            .collect();
        let self_binding = receiver.map(|receiver| SelfBinding::of_static(receiver, None));
        self.link_deferred_assoc_equalities(&owners, &names, &args, self_binding, span);
    }

    /// The newtype a static call's receiver is, where it inherits the static
    /// from the link `owner` below it, at the arguments `owner_args` give that
    /// link. The resolution retypes a newtype declaring no parameters itself;
    /// one declaring some is solved from `owner_args` here, as far as they
    /// answer it. `None` where the receiver owns the static.
    pub(super) fn inherited_static(
        &mut self,
        receiver: &ImplTargetKey,
        owner: &ImplTargetKey,
        owner_args: &[TypeId],
    ) -> Option<InheritedStatic> {
        if owner == receiver {
            return None;
        }
        let link = owner.decl();
        let (newtype, _) = self.newtype_at_base_args(receiver, link, owner_args)?;
        let base = self.tysys.type_table.borrow().inherited_base(newtype, link);
        Some(InheritedStatic { newtype, base })
    }

    /// The signature a static call on the receiver `key` names.
    pub(super) fn static_call_sig(
        &self,
        key: &ImplTargetKey,
        method_name: &str,
        choice: SigChoice,
        receiver_args: &[TypeId],
    ) -> Option<MethodSig> {
        match choice {
            SigChoice::Any => self.qualified_method_sig_reaching(key, method_name, receiver_args),
            SigChoice::Unique => self.unique_qualified_method_sig(key, method_name),
        }
    }

    /// [`Self::static_call_sig`], or the trait default the receiver inherits
    /// without overriding it, which no impl block lists.
    pub(super) fn static_call_sig_or_inherited(
        &mut self,
        key: &ImplTargetKey,
        method_name: &str,
        choice: SigChoice,
        receiver_args: &[TypeId],
    ) -> Option<MethodSig> {
        self.static_call_sig(key, method_name, choice, receiver_args)
            .or_else(|| self.inherited_default_method_sig(key, method_name, choice))
    }

    /// [`Self::static_call_sig_or_inherited`] at the link of a newtype chain
    /// that owns the static, which [`Self::infer_through_newtype`] solves at.
    /// A link below the receiver is read at any arguments: `receiver_args`
    /// spell the receiver.
    fn owner_static_sig(
        &mut self,
        receiver: &ReceiverId,
        method_name: &str,
        choice: SigChoice,
        receiver_args: &[TypeId],
    ) -> Option<MethodSig> {
        let key = receiver.key();
        match self.newtype_static_target(receiver, method_name) {
            Some(owner) => self.static_call_sig_or_inherited(&owner, method_name, choice, &[]),
            None => self.static_call_sig_or_inherited(key, method_name, choice, receiver_args),
        }
    }

    /// `param_types` read at the receiver's arguments `declaring_args` and the
    /// method's `method_type_args`, where either list writes any, and as the
    /// newtype `inheriting` reads its base's static, and as the caller's
    /// bounds answer a projection off its own parameter.
    pub(super) fn instantiate_static_params(
        &mut self,
        sig: Option<&MethodSig>,
        declaring_args: &[TypeId],
        method_type_args: &[TypeId],
        self_in_args: bool,
        inheriting: Option<TypeId>,
        param_types: &mut [TypeId],
    ) {
        if let Some(sig) = sig
            && !(declaring_args.is_empty() && method_type_args.is_empty())
            && !param_types.is_empty()
        {
            // `TreeMap::<String, i32>` spells the *target's* arguments;
            // `impl … for TreeMap<String, V>` numbers only `V`. The declaring
            // block is what aligns the two.
            let declaring = sig
                .declaring_impl
                .map(|id| self.tysys.signatures.impl_sig(id));
            let instantiated = sig.instantiate_call_with(
                &self.tysys.type_table,
                declaring,
                declaring_args,
                method_type_args,
            );
            // `param_types` leads with the receiver exactly where the spelling
            // wrote one, so the instantiated list must start at the same
            // parameter.
            let skip = if self_in_args {
                0
            } else {
                sig.first_value_param().min(instantiated.param_types.len())
            };
            for (param_type, &instantiated_type) in param_types
                .iter_mut()
                .zip(&instantiated.param_types[skip..])
            {
                *param_type = instantiated_type;
            }
        }
        if let Some(newtype) = inheriting {
            let declared_on = sig
                .and_then(|sig| sig.declaring_impl)
                .and_then(|block| self.tysys.type_table.borrow().impl_target_decl(block));
            let base = self
                .tysys
                .type_table
                .borrow()
                .inherited_base(newtype, declared_on);
            for ty in param_types.iter_mut() {
                *ty = self.tysys.substitute_newtype_in_type(*ty, base, newtype);
            }
        }
        self.answer_frame_projections_in(param_types);
    }

    /// The type parameters of the block `declaring_impl` as the static's
    /// receiver `target` settles them: the arguments, then the bindings.
    pub(super) fn static_impl_bindings(
        &self,
        target: TypeId,
        declaring_impl: Option<DefId>,
    ) -> (Vec<TypeId>, Vec<DefaultTypeBinding>) {
        let declaring_args = self
            .tysys
            .receiver_args_at_impl(Some(target), &[], declaring_impl)
            .unwrap_or_default();
        let impl_bindings = declaring_impl
            .map(|id| {
                let table = &self.tysys.type_table;
                slot_type_bindings(table, table.borrow().impl_target_args(id), &declaring_args)
            })
            .unwrap_or_default();
        (declaring_args, impl_bindings)
    }

    /// Report an argument list a static method's signature cannot accept,
    /// returning `false` once a diagnostic was emitted. Shared by the three
    /// spellings that reach one: `Type::method(…)`, `Type::<T>::method(…)` and
    /// `ns::Type::method(…)`. A parameter still carrying a type param the
    /// caller does not declare belongs to a blanket's pack, which the call
    /// site cannot pin: it counts toward arity but its type is not compared.
    pub(super) fn check_static_call_args(
        &mut self,
        param_types: &[TypeId],
        args: &[TypeId],
        arg_sites: &[ArgSite],
        static_method_defaults: &[(String, Option<ast::Expr>)],
        span: Span,
    ) -> bool {
        assert_eq!(
            args.len(),
            arg_sites.len(),
            "every resolved argument carries the span it was written at"
        );
        let optional = static_method_defaults
            .iter()
            .filter(|(_, d)| d.is_some())
            .count();
        if !Self::arg_count_fits(args.len(), param_types.len(), optional) {
            let _ = self.emit(TypeError::ArgumentCountMismatch {
                expected: param_types.len(),
                found: args.len(),
                span,
            });
            return false;
        }
        let scope = self.scope_type_param_ids();
        for (i, (arg, &expected)) in args.iter().zip(param_types).enumerate() {
            if !self
                .tysys
                .type_table
                .borrow()
                .type_params_all_in(expected, scope.ids())
            {
                continue;
            }
            self.typecheck_arg(*arg, expected, arg_sites[i]);
        }
        true
    }

    /// Report why a static call's arguments matched no impl, when the
    /// receiver's impls explain it: a blanket impl this path cannot
    /// instantiate, or concrete impls none of which accept the arguments'
    /// types. Returns whether an error was emitted — the caller then stops
    /// instead of building an unresolvable mangled name (an ICE at WIR build).
    pub(super) fn report_unmatched_static_arg(
        &mut self,
        recv: StaticReceiver<'_>,
        method_name: &str,
        arg_types: &[TypeId],
        span: Span,
    ) -> bool {
        // A newtype inheriting the static answers with the impls of the link
        // that owns it, as the resolution does, so the survey reads them there.
        if let Some(owner_key) = self.newtype_static_target(recv.receiver, method_name) {
            let (owner_type, owner_arg_types) =
                self.at_owner_link(recv.receiver.ty(), &owner_key, arg_types);
            let owner = ReceiverId::keyed(owner_key, owner_type);
            let owner_recv = StaticReceiver {
                receiver: &owner,
                ..recv
            };
            return self.report_unmatched_static_arg(
                owner_recv,
                method_name,
                &owner_arg_types,
                span,
            );
        }
        let struct_name = recv
            .receiver
            .spelled(self.tysys.resolutions.defs())
            .to_string();
        let survey = self.static_arg_survey(recv, method_name);
        let spelled = render_type_list(&self.tysys.type_table.borrow(), arg_types);
        // The trait the first candidate names. Where several traits supply the
        // name the call is an overload, reported before it reaches here.
        let Some(trait_name) = survey.candidates.first().map(|c| c.trait_name.clone()) else {
            return false;
        };
        // An impl the first argument matches is one the selection kept, so the
        // call did not fail on that argument: it failed on the list. Reporting
        // the unmatched case here named the argument as both unaccepted and
        // available.
        if survey
            .candidates
            .iter()
            .any(|c| c.params.first() == arg_types.first())
        {
            let _ = self.emit(TypeError::NoMatchingArgumentList {
                trait_name,
                receiver: struct_name,
                method: method_name.to_string(),
                span,
            });
            return true;
        }
        let mut candidates: Vec<String> = survey
            .candidates
            .iter()
            .map(|c| c.spelling.clone())
            .collect();
        let mut arg_type = spelled;
        // Two modules' same-named types spell alike, and the message would
        // name the argument as both unaccepted and available.
        if candidates.contains(&arg_type) {
            let table = self.tysys.type_table.borrow();
            arg_type = render_type_list_qualified(&table, arg_types);
            candidates = survey
                .candidates
                .iter()
                .map(|c| render_type_list_qualified(&table, &c.params))
                .collect();
        }
        let _ = self.emit(TypeError::NoMatchingTraitArgument {
            trait_name,
            receiver: struct_name,
            method: method_name.to_string(),
            arg_type,
            candidates,
            span,
        });
        true
    }

    /// The shared preselect entry for a static call (`Wrapper::from(42)`, in
    /// either its static-call or plain-call spelling). It runs before the
    /// callee is resolved, and its answer is what the resolution keys on:
    /// resolving without the arguments and mangling with them is two answers
    /// for one call.
    pub(super) fn preselect_static_args(
        &mut self,
        recv: StaticReceiver<'_>,
        method_name: &str,
        args: &[ast::Expr],
        span: Span,
        ctx: &FunctionContext,
    ) -> PreselectedArg {
        if args.is_empty() || self.has_inherent_static_method(recv.receiver.key(), method_name) {
            return PreselectedArg::Undecided;
        }
        let classes: Vec<ArgClass> = args
            .iter()
            .map(|arg| self.synthesize_arg_class(arg, ctx))
            .collect();
        // A trait's default body reads `Self` off the receiver's type. Resolved
        // here rather than asked of every caller: it costs a scope, and the
        // returns above are the calls that never survey (WEP: "Resolving is not
        // free").
        let receiver_type = match recv.receiver.ty() {
            Some(ty) => Some(ty),
            None => self
                .declared_type_at_variables(recv.receiver.key())
                .map(|(instance, _)| instance)
                .or_else(|| self.static_receiver_type(recv.receiver)),
        };
        let Some(receiver_type) = receiver_type else {
            return PreselectedArg::Undecided;
        };
        let typed = recv.receiver.at_type(Some(receiver_type));
        let recv = StaticReceiver {
            receiver: &typed,
            ..recv
        };
        match self.static_arg_preselect(recv, method_name, &classes) {
            ArgPreselect::Selected(params) => PreselectedArg::Types(params),
            ArgPreselect::Ambiguous(candidates) => {
                let _ = self.emit(TypeError::AmbiguousStaticArgument {
                    receiver: recv
                        .receiver
                        .spelled(self.tysys.resolutions.defs())
                        .to_string(),
                    method: method_name.to_string(),
                    candidates,
                    span,
                });
                PreselectedArg::Reported
            }
            ArgPreselect::Pass => PreselectedArg::Undecided,
        }
    }

    /// Whether an inherent impl (`impl Type { … }`) declares a no-self method
    /// of this name. A conversion-call guard needs the distinction: a trait
    /// lookup returning `None` is a failure only when no inherent static can
    /// answer instead.
    pub(super) fn has_inherent_static_method(
        &self,
        target: &ImplTargetKey,
        method_name: &str,
    ) -> bool {
        self.tysys.inherent_shadows(target, method_name, false)
    }

    /// The argument preselect over a receiver's impls: `Selected` and
    /// `Ambiguous` short-circuit resolution, so it decides calls. It must run
    /// *before* the argument is elaborated — the expected type shaping a literal
    /// comes from the selected impl. Admissibility is [`Elaborator::class_admits`]
    /// over each impl's *resolved* parameter type, since spelling under-admits.
    pub(super) fn static_arg_preselect(
        &self,
        recv: StaticReceiver<'_>,
        method_name: &str,
        classes: &[ArgClass],
    ) -> ArgPreselect {
        // An opaque argument admits every parameter, so the rest decide. Every
        // argument opaque selects nothing, and shapes the arguments only where
        // the impls all declare one parameter list.
        let all_opaque = classes.iter().all(|c| matches!(c, ArgClass::Opaque(_)));
        let mut admitted: Vec<ArgCandidate> = self
            .static_arg_survey(recv, method_name)
            .candidates
            .into_iter()
            .filter(|c| self.params_admit(&c.params, classes))
            .collect();
        if let Some(expected) = recv.expected_return {
            self.narrow_to_expected_return(&mut admitted, expected, |c| c.return_type);
        }
        // An impl leaving a slot to the call is told from another by the
        // arguments' types, which the resolution reads once they are
        // elaborated; only a numeric literal, whose width it would choose,
        // leaves that question open.
        if let [_, _, ..] = admitted.as_slice()
            && !classes.iter().any(ArgClass::leaves_width_open)
            && admitted.iter().any(|c| {
                self.tysys
                    .type_table
                    .borrow()
                    .impl_trait_sets_params(c.block)
            })
        {
            return ArgPreselect::Pass;
        }
        match admitted.as_slice() {
            [] => ArgPreselect::Pass,
            [only] => ArgPreselect::Selected(only.params.clone()),
            // A `Head` names a family, not a type — `Pair { a: 5 }` is a
            // `Pair` of something — so several same-head impls are the expected
            // answer, not a tie. Only classes each denoting one type may call
            // two candidates ambiguous; elaborating the arguments decides the
            // rest.
            _ if all_opaque || classes.iter().any(|c| matches!(c, ArgClass::Head(_))) => {
                ArgPreselect::Pass
            }
            _ => ArgPreselect::Ambiguous(admitted.into_iter().map(|c| c.spelling).collect()),
        }
    }

    /// The first-parameter types the receiver's trait impls declare for
    /// `method_name` (`From<String>`'s `String` beside `From<i64>`'s `i64`,
    /// `Enc<A>`'s `A` beside `Enc<B>`'s `B`), in candidate order, plus the
    /// trait of any blanket among them. The parameter is read from the
    /// declaration rather than off the trait reference: a conversion trait's
    /// source type is also its trait argument, but no other trait's is.
    ///
    /// It walks the impls directly rather than reading the resolution, because
    /// its consumers need every candidate and the resolution keeps one.
    pub(super) fn static_arg_survey(
        &self,
        recv: StaticReceiver<'_>,
        method_name: &str,
    ) -> StaticArgSurvey {
        let mut survey = StaticArgSurvey::default();
        let receiver = recv.receiver;
        let receiver_args = self
            .tysys
            .receiver_declaring_args(receiver.ty(), recv.args, receiver.key().decl())
            .unwrap_or_default();
        for impl_def in self.trait_impls_for_receiver(receiver, &receiver_args) {
            let Some(trait_decl) = self.tysys.signatures.impl_trait(impl_def) else {
                continue;
            };
            // A qualified spelling names a trait, so another trait's impl is
            // not a candidate to weigh against: the same rule the resolution
            // applies, asked where the arguments are surveyed.
            let answers = match recv.required_trait {
                Some(required) => required == trait_decl,
                // A trait's declarations answer only where the trait is in
                // scope, as the resolution reads them.
                None => self
                    .tysys
                    .resolutions
                    .decl_in_scope(self.frame_module(), trait_decl),
            };
            if !answers {
                continue;
            }
            if !recv.required_args.is_empty()
                && !self
                    .tysys
                    .impl_answers_trait_args(impl_def, receiver.ty(), recv.required_args)
            {
                continue;
            }
            let header = &self.tysys.trait_env.impl_headers[&impl_def];
            // The same walk the rules read, so a body the block inherits is a
            // candidate here too.
            let Some(offer) =
                self.tysys
                    .impl_static_offer(header, impl_def, trait_decl, method_name)
            else {
                continue;
            };
            let trait_name = || {
                header
                    .trait_head_name()
                    .expect("trait_impls_for_receiver yields trait impls alone")
                    .to_string()
            };
            // The selection's own question, asked through the selection's own
            // answer: a blanket accepts a family rather than a type, so the
            // blanket resolver answers such a call and it is never an unmatched
            // alternative worth listing. Asking it a second way here is what
            // let the two disagree.
            let params = match offer.selector {
                Selector::Absent => continue,
                Selector::Blanket(_) => {
                    unreachable!("a block offers the body it wrote or inherits, never a blanket's")
                }
                Selector::Params(params) => params,
            };
            // By the types, as `Selector` is: two distinct types printing one
            // name are two candidates, not one.
            // The parameters as the impl's own frame resolved them, so a
            // private or aliased name means what the impl wrote.
            let spelling = render_type_list(&self.tysys.type_table.borrow(), &params);
            // At the receiver, so a literal argument is shaped by what the
            // receiver places in the impl's slots rather than by the slots.
            let at_receiver = |ty: TypeId| match receiver.ty() {
                Some(receiver) => self.tysys.at_receiver(impl_def, receiver, ty),
                None => ty,
            };
            let params: Vec<TypeId> = params.into_iter().map(at_receiver).collect();
            let return_type = at_receiver(offer.return_type);
            // The expected type selects among candidates by their result, so
            // two taking the same parameters are one only where they return
            // the same too.
            if survey
                .candidates
                .iter()
                .any(|c| c.params == params && c.return_type == return_type)
            {
                continue;
            }
            survey.candidates.push(ArgCandidate {
                spelling,
                params,
                return_type,
                trait_name: trait_name(),
                block: impl_def,
            });
        }
        survey
    }

    /// Enforce the bounds of the impl block a static call selected at the
    /// receiver arguments it names, which no lookup filtered the block by:
    /// now where they are decided, and once settled where they are open.
    fn enforce_static_block_bounds(
        &mut self,
        block: Option<DefId>,
        receiver_args: &[TypeId],
        span: Span,
    ) {
        let Some(block) = block else {
            return;
        };
        self.require_impl_bounds_at(block, receiver_args, span);
        let unmet = self
            .tysys
            .unmet_impl_bound(
                &self.annotate_ctx,
                block,
                None,
                Some(receiver_args),
                Unsettled::MayHold,
            )
            .or_else(|| {
                // A list past the receiver's positions spells the slots only
                // the trait arguments name, whose bounds no position reaches.
                let mut table = self.tysys.type_table.borrow_mut();
                if !table.impl_trait_sets_params(block) {
                    return None;
                }
                let slots = table.impl_spelled_slots(block, receiver_args);
                drop(table);
                self.tysys
                    .unmet_slot_bound(&self.annotate_ctx, block, &slots, Unsettled::MayHold)
            });
        if let Some(unmet) = unmet {
            self.report_unmet(unmet, span);
        }
    }

    /// The key of the instance a static call reaches in `block` at the
    /// receiver's `positions`, its bounds enforced there, and the slots only
    /// the trait arguments name settled by the call's `args`
    /// ([`Self::slot_instance_args`]). `return_type` comes back with the
    /// caller's projections answered, as the parameters already are.
    pub(super) fn static_instance_args(
        &mut self,
        block: Option<DefId>,
        positions: &[TypeId],
        params: &mut [TypeId],
        args: &[TypeId],
        return_type: &mut TypeId,
        span: Span,
    ) -> Vec<TypeId> {
        self.enforce_static_block_bounds(block, positions, span);
        let instance_args = block
            .and_then(|block| {
                let given = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .impl_spelled_slots(block, positions);
                self.slot_instance_args(block, given, params, args, return_type, span)
            })
            .unwrap_or_else(|| positions.to_vec());
        *return_type = self.answer_frame_projections(*return_type);
        instance_args
    }

    /// [`Self::fill_defaulted_method_type_args`] for a static's own slots,
    /// reading the trait and the declaring module off its signature.
    pub(super) fn fill_static_default_type_args(
        &mut self,
        sig: &MethodSig,
        receiver: &ReceiverId,
        inferred: &mut [TypeId],
    ) -> bool {
        if !sig.own_params.iter().any(|p| p.default.is_some()) {
            return false;
        }
        let Some(receiver_type) = self.static_receiver_type(receiver) else {
            return false;
        };
        let declaring_module = Some(sig.defaults_module.clone());
        let trait_decl = self.tysys.signatures.declaring_trait(sig);
        self.fill_defaulted_method_type_args(
            &sig.own_params,
            SelfBinding::of_static(receiver_type, trait_decl),
            &sig.own_type_param_ids(),
            declaring_module,
            inferred,
        )
    }

    /// [`Self::bind_method_type_args`] for a static whose arguments resolved
    /// before its own slots were instantiated: they are instantiated here, so
    /// an unsolved one is blamed on `name`.
    pub(super) fn bind_static_type_args(
        &mut self,
        explicit: Vec<TypeId>,
        name: &str,
        input: MethodInferenceInput<'_>,
    ) -> (Vec<TypeId>, TypeArgMap) {
        assert!(
            input.inst.is_none(),
            "the static's own slots are instantiated here"
        );
        let inst = (!input.slots.is_empty()).then(|| {
            self.instantiate(
                input.slots,
                &Instantiation {
                    kind: InstanceKind::Method,
                    name,
                    span: input.span,
                    type_args: &explicit,
                    self_binding: Some(input.self_binding),
                    impl_args: input.impl_args,
                },
            )
        });
        self.bind_method_type_args(
            explicit,
            MethodInferenceInput {
                inst: inst.as_ref(),
                ..input
            },
        )
    }

    /// The lists a `Type::method(...)` call checks and pads against. Every
    /// static spelling asks this one question, so none of them differs on the
    /// arity it checks.
    pub(super) fn static_callee_params(
        &mut self,
        receiver: &ReceiverId,
        method_name: &str,
        arg_types: &[TypeId],
        (required_trait, required_args): (Option<DefId>, &[TypeId]),
        expected_return: Option<TypeId>,
    ) -> StaticLookup {
        self.resolve_static_callee(StaticQuery {
            arg_types,
            required_trait,
            required_args,
            expected_return,
            ..StaticQuery::of(receiver, method_name)
        })
    }

    /// The method's own slots as its declaration wrote them, read off the
    /// declaration [`Elaborator::resolve_static_callee`] picked: a trait-`impl`
    /// method reports the trait's bounds and defaults, which the block itself
    /// does not restate.
    pub(super) fn lookup_static_method_type_params(
        &mut self,
        receiver: &ReceiverId,
        method_name: &str,
    ) -> Vec<ast::GenericParam> {
        self.resolve_static_callee(StaticQuery::of(receiver, method_name))
            .found()
            .map(|callee| callee.own_params.clone())
            .unwrap_or_default()
    }
}
