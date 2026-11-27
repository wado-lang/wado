//! The spellings of a static call (`Type::method(…)`, `Type::<T>::method(…)`,
//! `T::method(…)`), each resolved to the function it dispatches to.

use std::ops::ControlFlow;

use super::Elaborator;
use super::call::ArgSite;
use super::call::ArgsSite;
use super::call::CaseSite;
use super::call::GivenSlots;
use super::call::InheritedStatic;
use super::call::PathCall;
use super::call::ResolvedArgs;
use super::call::SigChoice;
use super::call::StaticArgsReceiver;
use super::call::StaticCallee;
use super::call::StaticTypeArgs;
use super::call::bind_nearer;
use super::callee::StaticMethodRef;
use super::coercion::ExpectedReturn;
use super::instantiate::InstanceKind;
use super::instantiate::Instantiated;
use super::instantiate::Instantiation;
use super::method_call::PreselectedArg;
use super::method_call::QualifiedAnswer;
use super::method_call::QualifiedReceiver;
use super::method_call::Qualifier;
use super::method_call::StaticReceiver;
use super::method_call::nominal_receiver;
use super::method_call::primitive_receiver;
use super::method_call::primitive_receiver_type;
use super::method_call::static_call_symbol_name;
use super::method_lookup::settled_impl_args;
use super::pending_selection::PendingStatic;
use super::sem::types::CalleeParams;
use super::sem::types::StaticMethodDispatch;
use super::sig::MethodSig;
use super::static_call::ReceiverId;
use super::static_call::StaticLookup;
use super::static_call::StaticQuery;
use super::trait_env::ImplTargetKey;
use super::trait_env::written_arg_nodes;
use super::types::FunctionContext;
use super::types::TypeError;
use crate::Span;
use crate::ast;
use crate::ast::AstId;
use crate::compiler_host::CompilerHost;
use crate::defs::DefId;
use crate::elaborator::SelfBinding;
use crate::elaborator::expr::MemberOwner;
use crate::elaborator::method_lookup::MethodInferenceInput;
use crate::elaborator::sig;
use crate::elaborator::synth::ArgProbe;
use crate::elaborator::synth::ArgSource;
use crate::elaborator::types::ImplMemberKind;
use crate::hashmap::IndexSet;
use crate::name::FqTypeName;
use crate::name::LocalMethodName;
use crate::name::MethodName;
use crate::name::RefKind;
use crate::name::unalias_namespace_member;
use crate::tir::FunctionRef;
use crate::tir::MonomorphInfo;
use crate::tir::ResolvedType;
use crate::tir::TypeId;
use crate::token;
use crate::type_table::TypeTable;

impl<H: CompilerHost> Elaborator<'_, H> {
    /// `Type::method(…)` or `ns::Type::method(…)` where `receiver`, of type
    /// `receiver_type`, has a static method named `suffix`.
    pub(super) fn resolve_static_method_path_call(
        &mut self,
        site: PathCall<'_>,
        receiver: &ReceiverId,
        receiver_type: TypeId,
        suffix: &str,
        resolved_args: &mut ResolvedArgs,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let PathCall {
            call,
            ident,
            expected_type,
            ..
        } = site;
        let ResolvedArgs {
            args,
            type_args,
            callee_slots,
            slot_instantiation,
            ..
        } = resolved_args;
        // At the receiver's type, since no blanket is asked of a receiver
        // without one: a blanket whose bound holds there owns the call, and
        // the static a newtype inherits a link below is not the one named.
        let given = slot_instantiation.as_ref().map(|inst| GivenSlots {
            slots: callee_slots,
            inst,
        });
        let receiver_args = self.expected_receiver_args(receiver, suffix, expected_type);
        let typed = receiver.at_type(Some(receiver_type));
        let resolved = self.resolve_static_callee(StaticQuery {
            arg_types: args,
            receiver_args: &receiver_args,
            expected_return: expected_type,
            ..StaticQuery::of(&typed, suffix)
        });
        // The blanket resolver instantiates it and records the reference.
        // Before the turbofish is counted, since it names the blanket's
        // parameters and not the inherited static's.
        if matches!(resolved, StaticLookup::Blanket(_))
            && let Some(return_type) = self.resolve_named_type_blanket_static(
                &typed,
                suffix,
                call.id,
                ident.segments.last().map(|segment| segment.id),
                type_args,
                given,
                args,
                &call.args,
                expected_type,
                call.span,
                ctx,
            )
        {
            return return_type;
        }
        // Record the method segment (suffix) as a reference to the
        // declaration this call resolves to. The impl selection knows
        // which one answered — two conversion impls on a type declare
        // the same `from`, and only the argument's type separates
        // them. It covers trait impls only; an inherent static has no
        // selection and reaches the index instead.
        //
        // The same resolution the call itself uses, not a second one:
        // two resolutions of one call disagree, which is what the edge
        // then records.
        let selection = resolved
            .found()
            .and_then(|callee| callee.method_ref.method_id);
        let method_def = selection.or_else(|| {
            let owner = self.static_owner_key(receiver, suffix);
            self.qualified_method_decl_id(&owner, suffix)
        });
        let own_from = method_def.is_some_and(|def| self.declares_own_from(def));
        if let Some(suffix_seg) = ident.segments.last()
            && let Some(method_def) = method_def
            && self.record_reference_to_decl(suffix_seg.id, method_def, suffix_seg.span)
        {
            return TypeTable::ERROR;
        }
        // An omitted turbofish infers both levels; a partial one keeps
        // what it named and infers only its `_` slots. The call's own
        // `type_args` stay as written.
        let callee = StaticCallee {
            receiver,
            method_name: suffix,
        };
        let Some(mut static_args) = self.static_call_type_args(
            callee,
            &call.args,
            args,
            expected_type,
            call.span,
            type_args.clone(),
            given,
        ) else {
            return TypeTable::ERROR;
        };
        // Before the record, which then holds what stands in for a slot
        // deferred.
        self.defer_uninferred_static_method_type_args(
            callee,
            &mut static_args,
            Some(receiver_type),
            call.span,
        );
        // Record `[impl_args, method_args]` — the same order
        // `lookup_static_method_param_types` substitutes in — since reify
        // needs both halves to rebuild the mangled `__<Type>__<method>`.
        // The instance type is UNKNOWN: a static call anchors no
        // instance type, so reify reads `expression_types` instead.
        {
            let mut combined = static_args.impl_args.clone();
            combined.extend_from_slice(&static_args.method_args);
            self.record_generic_instantiation(call.id, combined, TypeTable::UNKNOWN);
        }
        self.enforce_static_method_bounds(&static_args, Some(receiver_type), call.span);
        // A generic receiver spelled without its arguments names a block only
        // by the ones the call inferred, else the ones its expected type holds.
        if self.receiver_generic_arity(receiver).is_some() {
            let inferred = self.apply_infer_holes_each(&static_args.impl_args);
            let instance = if inferred.is_empty() {
                &receiver_args
            } else {
                &inferred
            };
            self.check_static_call_visibility(receiver, instance, suffix, Some(call.id), call.span);
        }
        let StaticTypeArgs {
            impl_args: impl_type_args_inferred,
            method_args: method_type_args,
            ..
        } = static_args;
        // Impls of one trait neither the arguments nor the receiver tell
        // apart may be told apart by the instance a later use settles. A
        // receiver spelled without its arguments is the instance at the
        // ones the call inferred, each a variable where none did.
        let at_variables = (matches!(resolved, StaticLookup::Overloaded { .. })
            && impl_type_args_inferred.is_empty())
        .then(|| self.declared_type_at_variables(receiver.key()))
        .flatten();
        let (instance, instance_args) =
            at_variables.unwrap_or_else(|| (receiver_type, impl_type_args_inferred.clone()));
        if matches!(resolved, StaticLookup::Overloaded { .. })
            && method_type_args.is_empty()
            && let Some(info) = self.static_waits(instance, suffix, args.len())
        {
            for (i, (&arg, &param)) in args.iter().zip(&info.param_types).enumerate() {
                self.typecheck_arg(arg, param, self.arg_site(call, i));
            }
            self.wait_for_static(PendingStatic {
                receiver: receiver.clone(),
                receiver_type,
                instance,
                method_name: suffix.to_string(),
                args: args.clone(),
                impl_args: instance_args,
                call: call.id,
                method_id: ident.segments.last().map(|segment| segment.id),
                span: call.span,
                expected: expected_type,
                return_type: info.return_type,
            });
            return info.return_type;
        }
        if suffix == "from"
            && !own_from
            && let [arg_type] = args[..]
            && let Some(converted) =
                self.convert_without_impl(call.id, receiver_type, arg_type, selection.is_none())
        {
            return converted;
        }

        // Literal args resolved against `TypeParam`/`Unknown` fell back to
        // i32/f64, so re-coerce once the substitution is known. A
        // non-generic call is checked too, or a mismatch only shows at
        // codegen, as an invalid module rather than at its own span.
        if self.report_unresolved_static(&resolved, suffix, args, call.span) {
            return TypeTable::ERROR;
        }
        // No candidate the arguments admitted — the same report the
        // static-call spelling makes. Unreported, the call is mangled
        // anyway and reaches WIR build unresolved.
        if resolved.found().is_none()
            && !args.is_empty()
            && !self.has_inherent_static_method(receiver.key(), suffix)
            && self.report_unmatched_static_arg(StaticReceiver::of(&typed), suffix, args, call.span)
        {
            return TypeTable::ERROR;
        }
        // The count and the defaults come from the declaration, so the
        // arity is enforced whether or not the receiver filled the
        // types. The types are checked per parameter below, where a
        // slot this call could not fill is skipped rather than the
        // whole list dropped.
        let (raw_param_types, optional) = match resolved.found() {
            Some(callee) => (
                callee.params.param_types.clone(),
                Some(
                    callee
                        .params
                        .param_defaults
                        .iter()
                        .filter(|(_, default)| default.is_some())
                        .count(),
                ),
            ),
            None => (Vec::new(), None),
        };
        let generic = !impl_type_args_inferred.is_empty() || !method_type_args.is_empty();
        if generic && self.report_value_for_reference(&raw_param_types, call, args) {
            return TypeTable::ERROR;
        }
        let block = resolved
            .found()
            .and_then(|callee| self.tysys.block_of(&callee.method_ref));
        // A derivation has no block: its slots are the owning link's.
        let owner = self.static_owner_key(receiver, suffix);
        let impl_owners: Vec<DefId> = block.into_iter().chain(owner.decl()).collect();
        let mut substituted = self.static_param_types_at(
            raw_param_types,
            &impl_owners,
            resolved.found().and_then(|c| c.method_ref.method_id),
            &impl_type_args_inferred,
            &method_type_args,
        );
        let inherited = self.inherited_static(receiver.key(), &owner, &impl_type_args_inferred);
        self.retype_inherited(inherited, &mut substituted);
        self.answer_frame_projections_in(&mut substituted);
        self.recoerce_literal_args(&call.args, args, &substituted);
        self.record_call_param_types(call.id, substituted.clone());
        // Per-argument checking alone passes a call of the wrong length:
        // the loop below reaches neither a missing argument nor a
        // surplus one, and the call reaches codegen as an invalid
        // module. `Self::arg_count_fits` is the same rule the other
        // static spellings check.
        //
        // From the same answer `raw_param_types` came from, so the two
        // cannot disagree: an overloaded name yields no count to check
        // — the overload path picks the impl by argument, and reports
        // its own mismatch.
        if let Some(optional) = optional
            && !Self::arg_count_fits(args.len(), substituted.len(), optional)
        {
            let _ = self.emit(TypeError::ArgumentCountMismatch {
                expected: substituted.len(),
                found: args.len(),
                span: call.span,
            });
            return TypeTable::ERROR;
        }
        let scope = self.scope_type_param_ids();
        for (i, arg) in args.iter().enumerate() {
            // A slot neither the receiver nor the turbofish filled says
            // only "whatever this instantiation binds", which every
            // argument satisfies. Checking against it rejects the call
            // the declaration was written to accept. The caller's own
            // parameter is no such slot: `5` is not a `T`.
            if let Some(&expected) = substituted.get(i)
                && !self.is_open_slot(expected, &scope)
            {
                self.typecheck_arg(*arg, expected, self.arg_site(call, i));
            }
        }

        // WEP 2026-05-26: `resolve_static_method_call_from_qualified`
        // records the resolved `FunctionRef` under `call.id` itself
        // (`static_method_dispatch`) so reify can reproduce the same
        // `Call` shape without re-running impl lookup, mangled-name
        // construction, or monomorph-info shaping — facts reify
        // cannot reconstruct from the AST alone. It returns only a
        // typed placeholder.
        self.resolve_static_method_call_from_qualified(
            receiver,
            receiver_type,
            suffix,
            args,
            &impl_type_args_inferred,
            &method_type_args,
            inherited,
            call.id,
            call.span,
            expected_type,
            Some(ctx),
        )
    }

    /// The type the receiver of a static call is written as, a generic one
    /// bare until the call settles it: the type a rewritten `Self::` / `T::`
    /// prefix stood for, `Type` at its segment, `ns::Type` at the segment that
    /// wrote the type. `None` where `ns::X` names a trait, an interface or a
    /// function, which declares no type a static is asked at.
    pub(super) fn static_receiver_type_as_written(
        &mut self,
        site: PathCall<'_>,
        prefix: &str,
    ) -> Option<TypeId> {
        let PathCall {
            call,
            ident,
            effective_name,
            receivers,
            ..
        } = site;
        if receivers.prefix_receiver.is_some() {
            return Some(
                match (receivers.rewritten_prefix_type, receivers.prefix_site) {
                    (Some(receiver), _) => receiver,
                    (None, Some(site)) => self.resolve_named_type(site, prefix, call.span, false),
                    (None, None) => {
                        unreachable!("an unrewritten receiver is written at a segment")
                    }
                },
            );
        }
        let key = receivers.static_method_receiver.as_ref()?.key();
        if key.decl().is_some() && self.type_declared_by(Some(key)).is_none() {
            return None;
        }
        let owner = ident
            .owner_segment()
            .expect("a namespaced receiver is written at its owner segment");
        let (spelled, _) = effective_name
            .rsplit_once("::")
            .expect("a receiver comes with a qualified name");
        Some(self.resolve_named_type(owner.id, spelled, call.span, false))
    }

    /// Enforce the visibility ladder on a qualified `Type::method(...)` call.
    /// `receiver` is the key the neighbouring lookups resolved, not one
    /// re-derived from the spelling, which a splice can make mean another type.
    /// `receiver_args` are the ones the call inferred for a generic receiver
    /// spelled without them.
    pub(super) fn check_static_call_visibility(
        &mut self,
        receiver: &ReceiverId,
        receiver_args: &[TypeId],
        method_name: &str,
        node: Option<AstId>,
        span: token::Span,
    ) {
        // At the link the resolution reaches: checking the spelled receiver
        // alone found an empty bucket, so `type Q = P` passed a call the same
        // call through `P` is rejected for.
        let Some(entry) = self.reached_static_entry(receiver, receiver_args, method_name) else {
            return;
        };
        let (module, visibility) = (entry.module.clone(), entry.inherent_visibility);
        let owner = receiver.spelled(self.tysys.resolutions.defs()).to_string();
        self.check_inherent_member_visibility(
            visibility,
            Some(&module),
            MemberOwner::Named(&owner),
            method_name,
            ImplMemberKind::Method,
            node,
            span,
        );
    }

    /// Resolve a static call through a type parameter: `T::method(args)`
    /// where T is bound by a trait (e.g., `T: Constructable`). `pretyped_args`
    /// are the argument types a tagged template has already resolved.
    pub(super) fn resolve_type_param_static_call(
        &mut self,
        type_param_name: &str,
        method_name: &str,
        type_param_type_id: TypeId,
        pretyped_args: Option<&[TypeId]>,
        ident: &ast::IdentExpr,
        call: &ast::CallExpr,
        expected_type: Option<TypeId>,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let arg_count = pretyped_args.map_or(call.args.len(), <[TypeId]>::len);
        let bounds = self.carried_bounds(type_param_type_id);

        if let Some((found_trait, method_info_result)) = {
            self.find_method_in_trait_bounds(
                Some(call.id),
                false,
                &bounds,
                method_name,
                type_param_type_id,
                call.span,
                None,
                ArgSource::Exprs(&mut ArgProbe::new(&call.args, ctx, expected_type)),
            )
        } {
            if let Some(def) = method_info_result.method_def
                && self.report_unavailable(def, call.span)
            {
                return self.resolve_args_without_callee(&call.args, ctx);
            }
            let receiver = usize::from(method_info_result.self_kind != ast::SelfKind::None);
            let expected = receiver + method_info_result.param_types.len();
            let omitted_have_defaults = arg_count
                .checked_sub(receiver)
                .and_then(|given| method_info_result.param_defaults.get(given..))
                .is_some_and(|omitted| omitted.iter().all(Option::is_some));
            if arg_count > expected || !omitted_have_defaults {
                let _ = self.emit(TypeError::ArgumentCountMismatch {
                    expected,
                    found: arg_count,
                    span: call.span,
                });
                return self.resolve_args_without_callee(&call.args, ctx);
            }
            // The arguments spell the receiver, so the parameters lead with it.
            let receiver_param = match method_info_result.self_kind {
                ast::SelfKind::None => None,
                ast::SelfKind::Value => Some(type_param_type_id),
                ast::SelfKind::Ref => Some(
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_ref(type_param_type_id),
                ),
                ast::SelfKind::MutRef => Some(
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_mut_ref(type_param_type_id),
                ),
            };
            let param_types: Vec<TypeId> = receiver_param
                .into_iter()
                .chain(method_info_result.param_types.iter().copied())
                .collect();
            let method_slots = method_info_result.method_type_param_ids.clone();
            let own_params = method_info_result.method_own_params.clone();
            let declared_return = method_info_result.return_type;
            let trait_decl = found_trait.canonical();

            // The method's own slots are solved as an instance call's are: from
            // the arguments, the type expected of the call, the bounds'
            // equalities, or a later use the call's value meets. `Self`, the
            // receiver's param, is settled in this body, and monomorphization
            // substitutes it.
            let mut written = self.resolve_turbofish_args(&call.type_args);
            if self.group_variadic_type_args_of(method_name, &own_params, &mut written, call.span) {
                return TypeTable::ERROR;
            }
            let qualified = format!("{type_param_name}::{method_name}");
            let of = Instantiation {
                kind: InstanceKind::Method,
                name: &qualified,
                span: call.span,
                type_args: &written,
                self_binding: Some(SelfBinding {
                    type_id: type_param_type_id,
                    declaring_trait: trait_decl,
                }),
                impl_args: &[],
            };
            let (mut args, inst) = match pretyped_args {
                Some(args) => {
                    let inst =
                        (!method_slots.is_empty()).then(|| self.instantiate(&method_slots, &of));
                    (args.to_vec(), inst)
                }
                None => self.resolve_args_through_slots(
                    ctx,
                    &call.args,
                    &param_types,
                    &method_slots,
                    &own_params,
                    &of,
                    expected_type.map(|expected| ExpectedReturn {
                        declared: declared_return,
                        expected,
                    }),
                ),
            };
            // The method's parameters exclude the receiver, so each pairs with
            // the argument after it.
            let (method_type_args, subst) = self.bind_method_type_args(
                written,
                MethodInferenceInput {
                    slots: &method_slots,
                    own_params: &own_params,
                    param_types: &method_info_result.param_types,
                    args: &args[receiver..],
                    raw_args: call.args.get(receiver..).unwrap_or_default(),
                    decl_return_type: declared_return,
                    expected_return_type: expected_type,
                    self_binding: SelfBinding::of_static(type_param_type_id, trait_decl),
                    declaring_module: found_trait.module().cloned(),
                    impl_args: &[],
                    inst: inst.as_ref(),
                    span: call.span,
                },
            );
            let param_types: Vec<TypeId> = param_types
                .iter()
                .map(|&t| self.substitute_map_in_frame(&subst, t))
                .collect();
            self.recoerce_literal_args(&call.args, &mut args, &param_types);
            for (i, (&arg, &param)) in args.iter().zip(&param_types).enumerate() {
                self.typecheck_arg(arg, param, self.arg_site(call, i));
            }
            let final_return_type = self.substitute_map_in_frame(&subst, declared_return);

            let method_type_arg_names: Vec<FqTypeName> = method_type_args
                .iter()
                .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
                .collect();
            // The defaults below are the trait's, so they resolve in its own
            // module. The `FunctionRef` names the caller's, because the mangled
            // callee is minted per instantiation.
            let trait_module = found_trait.module().cloned();
            let mut method_info = LocalMethodName::new(
                self.tysys
                    .type_table
                    .borrow()
                    .fq_type_name(type_param_type_id),
                Some(found_trait),
                method_name.to_string(),
            );
            method_info.is_type_param_receiver = true;
            method_info.receiver_param = self
                .tysys
                .type_table
                .borrow()
                .substitution_slot(type_param_type_id);
            method_info
                .trait_type_args
                .clone_from(&method_info_result.trait_args);
            if !method_type_arg_names.is_empty() {
                method_info.method_type_args = method_type_arg_names;
            }

            let mangled_name = method_info.to_mangled_name();
            let method_type_args_for_defaults = method_type_args.clone();

            // Build monomorph_info for method-level type args so the
            // monomorphizer can substitute TypeParam type args and
            // queue instantiations.
            let monomorph_info = if method_type_args.is_empty() {
                None
            } else {
                Some(MonomorphInfo {
                    generic_name: mangled_name.clone(),
                    impl_type_args: vec![],
                    method_type_args,
                    is_blanket: false,
                })
            };

            let func_ref = FunctionRef {
                module_source: self.current_module_source.clone(),
                name: mangled_name,
                // The receiver is a parameter: the instance's dispatch picks.
                template: None,
                monomorph_info,
                method_info: Some(method_info),
            };

            // Record the resolved abstract `T::method(...)` dispatch so
            // reify can replay the same `Call` without re-running
            // trait-bound method lookup (reify has no `trait_ctx`). Keyed
            // by the `CallExpr`'s AstId — distinct from any
            // `StaticMethodCallExpr` id, so reusing `static_method_dispatch`
            // is collision-free. Args here carry no `is_mut` (the
            // production builder below uses all-false `CallArg`s). Carry the
            // trait-declared parameter defaults so reify pads omitted trailing
            // arguments. The lists lead with the receiver, as every dispatch
            // with `self_in_args` does.
            let param_defaults: Vec<(String, Option<ast::Expr>)> = receiver_param
                .map(|_| ("self".to_string(), None))
                .into_iter()
                .chain(
                    method_info_result
                        .param_names
                        .iter()
                        .cloned()
                        .zip(method_info_result.param_defaults.iter().cloned()),
                )
                .collect();
            let defaults_module = trait_module.unwrap_or_else(|| func_ref.module_source.clone());
            // The method's own slots, as the turbofish spelled them or the
            // solve above settled them. The receiver is the bound's own
            // parameter, which the monomorphizer binds.
            let self_binding = SelfBinding::of_static(type_param_type_id, trait_decl);
            let type_bindings = self.own_slot_default_bindings(
                &param_defaults,
                &own_params,
                &method_slots,
                self_binding,
                method_type_args_for_defaults,
                Some(defaults_module.clone()),
            );
            self.record_default_walk(
                call.id,
                call.span,
                &args,
                &param_types,
                &param_defaults,
                defaults_module.clone(),
                Some(self_binding),
                &type_bindings,
                ctx,
            );
            let key = call.id;
            self.sem.types.static_method_dispatch.insert(
                key,
                StaticMethodDispatch {
                    method_def: method_info_result.method_def,
                    defaults_module,
                    function_ref: func_ref,
                    param_is_mut: vec![false; args.len()],
                    type_args: vec![],
                    param_defaults,
                    param_types,
                    self_in_args: receiver_param.is_some(),
                },
            );

            // Reify rebuilds the `T::method(...)` `Call` from the
            // recorded `static_method_dispatch`; project only the result type.
            return final_return_type;
        }

        // No bound declares it, but a blanket over what the bounds admit may.
        let receiver = self
            .receiver_of(type_param_type_id)
            .written_as(type_param_name);
        let signature = self.blanket_static_signature(&receiver, method_name);
        let qualified = format!("{type_param_name}::{method_name}");
        let resolved = match self.resolve_args_against(
            signature,
            ArgsSite {
                call,
                name: &qualified,
                expected_type,
                receiver: StaticArgsReceiver::Param,
                self_type: Some(type_param_type_id),
            },
            pretyped_args.map(<[TypeId]>::to_vec),
            ctx,
        ) {
            ControlFlow::Continue(resolved) => resolved,
            ControlFlow::Break(reported) => return reported,
        };
        self.blanket_static_or_unknown(
            &receiver,
            method_name,
            ident,
            call,
            resolved.given_slots(),
            &resolved.args,
            expected_type,
            ctx,
        )
    }

    /// Resolve a static method call: `List::<i32>::with_capacity(100)` or `Point::origin()`
    pub(super) fn resolve_static_method_call(
        &mut self,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        self.resolve_static_method_call_restricted_to_trait(
            static_call,
            (None, &[]),
            ctx,
            expected_type,
        )
    }

    /// [`Self::resolve_static_method_call`] restricted to one trait's impls,
    /// which is what a `Trait::<T>::method(…)` spelling names.
    pub(super) fn resolve_static_method_call_restricted_to_trait(
        &mut self,
        static_call: &ast::StaticMethodCallExpr,
        // The trait a qualified spelling names, and the arguments it writes
        // on it.
        (required_trait, required_args): (Option<DefId>, &[TypeId]),
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // A reflection trait is a trait, not a type, so `target_type` would not
        // resolve. It is the only spelling — a bare `T::members()` never
        // resolves, so type namespaces stay the author's.
        let head = self.written_head(&static_call.target_type);
        if let Some(head) = &head
            && let Some(dispatch) = self.reflect_dispatch_of(head.site, &static_call.method)
        {
            return self.resolve_reflection_trait_call(head, dispatch, static_call, ctx);
        }

        // Resolve the target type first to get struct name for parameter type
        // lookup. A `_` in its turbofish is filled once the arguments answer it.
        if let Some(head) = &head {
            self.reject_unresolved_args(head.args);
        }
        let mut target_type_id =
            self.with_writable_holes(|e| e.resolve_type(&static_call.target_type));

        // `Tag::<Point>::tag()` where `Tag` is a trait resolves to no type;
        // unreported it types `unknown` and lowering builds an invalid module.
        if target_type_id == TypeTable::UNKNOWN
            && let Some(head) = &head
            && self
                .decl_key_at(Some(head.site))
                .is_some_and(|key| self.tysys.trait_env.declares_trait(&key))
        {
            return self.resolve_trait_headed_call(head, static_call, ctx, expected_type);
        }

        if static_call.self_type.is_some() {
            let _ = self.emit(TypeError::SelfBindingOnType {
                name: head.map_or_else(
                    || self.tysys.type_id_to_string(target_type_id),
                    |head| head.name,
                ),
                span: static_call.span,
            });
            return self.resolve_args_without_callee(&static_call.args, ctx);
        }

        // The name and canonical decl key the parameter types are looked up
        // at: the link of a newtype chain that owns the static. The canonical
        // key disambiguates two modules' same-named structs whose methods both
        // live in the global `ImplMethodIndex`.
        let owner = self.newtype_static_owner(target_type_id, &static_call.method, required_trait);
        let inherited = owner
            .as_ref()
            .is_none_or(|(link, _)| *link != target_type_id);
        let declaring = owner.filter(|(link, _)| self.is_declaring_link(*link));
        let declaring_type = declaring.as_ref().map_or(target_type_id, |(link, _)| *link);
        let declaring_key = declaring.map(|(_, key)| key);
        let key = declaring_key.or_else(|| self.tysys.static_receiver_key(target_type_id));
        // The head as written names the receiver only where the key is the
        // target's own; a link below it has no spelling of its own.
        let own_key = self.receiver_of(target_type_id);
        let lookup_receiver = key.map(|key| {
            let receiver = ReceiverId::keyed(key, None);
            match &head {
                Some(head) if receiver.key() == own_key.key() => {
                    receiver.written_as(&unalias_namespace_member(&head.name))
                }
                _ => receiver,
            }
        });
        let static_receiver = lookup_receiver.as_ref().map(ReceiverId::key);

        let spelled = own_key.at_type(Some(target_type_id));
        // A default body's `Self::m(self)` names its trait, as `Tr::m(self)` does.
        let qualifier = match required_trait {
            Some(decl)
                if self
                    .tysys
                    .trait_declares_method(Some(decl), &static_call.method, |kind| {
                        kind != ast::SelfKind::None
                    }) =>
            {
                Some(Qualifier::Trait(self.tysys.required_trait(decl), None))
            }
            Some(_) => None,
            None => match self.qualified_answer(&spelled, &static_call.method) {
                QualifiedAnswer::Static => None,
                QualifiedAnswer::Method => Some(Qualifier::Type(&spelled)),
                QualifiedAnswer::Ambiguous(traits) => {
                    let _ = self.emit(TypeError::AmbiguousTraitMethod {
                        method: static_call.method.clone(),
                        traits,
                        span: static_call.span,
                    });
                    return self.resolve_args_without_callee(&static_call.args, ctx);
                }
            },
        };
        if let Some(qualifier) = qualifier {
            let method_type_args = self.resolve_turbofish_args(&static_call.type_args);
            return self.resolve_qualified_call_parts(
                qualifier,
                &static_call.method,
                &static_call.args,
                method_type_args,
                static_call.id,
                Some(static_call.method_id),
                static_call.span,
                expected_type,
                ctx,
            );
        }

        // `Type::<T>::method()` parses as a static-method call and never
        // reaches `resolve_call`, which checks the bare spelling. The receiver
        // key comes from the resolved target type, as every lookup below does.
        if let Some(receiver) = &lookup_receiver {
            self.check_static_call_visibility(
                &receiver.at_type(Some(declaring_type)),
                &[],
                &static_call.method,
                Some(static_call.id),
                static_call.span,
            );
        }

        // `Result::<i32, String>::Ok(42)`: the head's arguments are the
        // turbofish of `Result::Ok`. A spelling naming a trait asks for no case.
        if required_trait.is_none()
            && let Some(head) = &head
            && let Some(def) = self.decl_key_at(Some(head.site))
            && let Some(owner) = self.case_owner_of_decl(def)
            && let Some(variant) = self.type_lookup().variant_cases_of(owner.def).cloned()
            && let Some((_, case_data)) = variant.case_named(&static_call.method)
        {
            let spelled = unalias_namespace_member(&head.name);
            if !static_call.type_args.is_empty() {
                let _ = self.emit(TypeError::CaseTurbofishOnBoth {
                    type_name: head.name.clone(),
                    case: static_call.method.clone(),
                    span: static_call.span,
                });
                return self.resolve_args_without_callee(&static_call.args, ctx);
            }
            self.record_case_owner(static_call.id, owner.def);
            self.unwrite_holes_in(target_type_id);
            let Some(written) = self.case_written(
                &owner,
                [&spelled, &static_call.method],
                (head.args, Self::resolve_turbofish_args),
                static_call.span,
            ) else {
                return self.resolve_args_without_callee(&static_call.args, ctx);
            };
            let case = CaseSite {
                variant: &variant,
                case: case_data,
                written: &written,
                owner: &spelled,
                site: static_call.id,
                span: static_call.span,
            };
            return self.construct_through_case_owner(
                &owner,
                &spelled,
                static_call.span,
                expected_type,
                |e, expected| {
                    e.resolve_case_construction(&case, &static_call.args, None, expected, ctx)
                },
            );
        }

        // Literal preselect for a static call (WEP 2026-07-31 phase 4): choose
        // the impl before the arguments are elaborated, so their expected types
        // come from the selected impl instead of whichever the name-keyed index
        // returns first — the circular ordering this WEP diagnoses. It runs
        // *before* the resolution below and keys it, so the parameter list and
        // the mangled name come from one answer.
        let receiver_holes = match &head {
            Some(head) => self.mint_receiver_holes(
                &mut target_type_id,
                &head.name,
                &static_call.method,
                static_call.span,
            ),
            None => Vec::new(),
        };
        let typed_receiver = lookup_receiver
            .as_ref()
            .map(|receiver| receiver.at_type(Some(target_type_id)));
        let preselected = match &typed_receiver {
            Some(receiver) => self.preselect_static_args(
                StaticReceiver {
                    required_trait,
                    required_args,
                    expected_return: expected_type,
                    ..StaticReceiver::of(receiver)
                },
                &static_call.method,
                &static_call.args,
                static_call.span,
                ctx,
            ),
            None => PreselectedArg::Undecided,
        };
        if matches!(preselected, PreselectedArg::Reported) {
            return TypeTable::ERROR;
        }
        let preselected = preselected.picked();
        // The selected impl's parameters stand in for the arguments the
        // resolution has not elaborated yet. Empty where nothing was picked,
        // which admits every candidate.
        let arg_types: Vec<TypeId> = preselected.clone().unwrap_or_default();

        let resolved = match &typed_receiver {
            Some(receiver) => self.static_callee_params(
                receiver,
                &static_call.method,
                &arg_types,
                (required_trait, required_args),
                expected_type,
            ),
            None => StaticLookup::NotStatic,
        };
        if self.report_unanswered_static(&resolved, &static_call.method, static_call.span) {
            return TypeTable::ERROR;
        }
        // A key naming no declaration holds blocks of several shapes (`&A` and
        // `&B`, `[A, i32]` and `[A, String]`), so it names no one signature:
        // the block the resolution selected does.
        let callee_sig = match static_receiver {
            Some(key) if key.decl().is_none() => resolved
                .found()
                .and_then(|callee| self.selected_static_sig(&callee.method_ref)),
            Some(key) => self
                .unique_qualified_method_sig(key, &static_call.method)
                .or_else(|| {
                    self.inherited_default_method_sig(key, &static_call.method, SigChoice::Unique)
                }),
            None => None,
        };
        // A blanket's resolver checks and pads the call itself; what the
        // arguments are typed against is still its parameters.
        let blanket_params = resolved.blanket().map(|blanket| {
            self.picked_blanket_static_at(blanket, &static_call.method, target_type_id)
                .0
        });
        let (mut callee_params, declares_params) = resolved.params();
        if let Some(param_types) = blanket_params {
            callee_params.param_types = param_types;
        }
        let CalleeParams {
            param_is_mut,
            param_defaults: static_method_defaults,
            mut param_types,
            self_in_args,
            defaults_module,
        } = callee_params;

        if let Some(picked) = &preselected {
            PreselectedArg::shape(&mut param_types, picked);
        }

        // The module those defaults were written in, so their bodies answer to
        // it rather than to this call site. The signature answers for a default
        // its own declaration did not write — a trait's, on an impl's method.
        let static_method_module = defaults_module.or_else(|| {
            static_receiver.and_then(|receiver| {
                self.static_method_entry(receiver, &static_call.method)
                    .map(|e| e.module.clone())
            })
        });

        // Resolve method-level type arguments
        let mut method_type_args: Vec<TypeId> = self.resolve_turbofish_args(&static_call.type_args);
        // Before anything counts slots, since a pack's arguments are one per
        // element until they are grouped.
        if let Some(sig) = callee_sig.as_ref() {
            let own_params = sig.own_params.clone();
            if self.group_variadic_type_args_of(
                &static_call.method,
                &own_params,
                &mut method_type_args,
                static_call.span,
            ) {
                return TypeTable::ERROR;
            }
        }

        // `ns::Wrapper<T>` supplies the target's arguments as `Wrapper<T>`
        // does; the namespace is the head's question, not the list's. A `_`
        // among them is a variable the arguments answer, as an omitted
        // turbofish's slot is, so a generic instance's are read off the
        // target, which holds the variables its `_`s became.
        let mut declaring_args: Vec<TypeId> = if self.applies_written_args(target_type_id) {
            self.tysys
                .type_table
                .borrow()
                .declared_type_args(target_type_id)
                .expect("a generic instance has arguments")
        } else {
            written_arg_nodes(&static_call.target_type)
                .iter()
                .map(|t| self.resolve_type(t))
                .collect()
        };
        let callee_args = self.args_at_callee(target_type_id, &declaring_args, callee_sig.as_ref());
        self.instantiate_static_params(
            callee_sig.as_ref(),
            &callee_args,
            &method_type_args,
            self_in_args,
            inherited.then_some(target_type_id),
            &mut param_types,
        );

        // The newtype whose base supplies the static, which then solves there.
        let newtype = match self.tysys.type_table.borrow().get(target_type_id) {
            ResolvedType::Newtype { def, .. } if inherited => Some(*def),
            _ => None,
        };
        let declaring_link = static_receiver.and_then(ImplTargetKey::decl);
        // Resolve arguments with expected types for coercion. `arg_sites` runs
        // parallel to `args` so a diagnostic still lands on the argument that
        // caused it rather than on the whole call.
        let written_vars: IndexSet<TypeId> = receiver_holes
            .iter()
            .copied()
            .chain(self.vars_holding_written_holes(&method_type_args))
            .collect();
        let open_own_slots = callee_sig.as_ref().filter(|sig| {
            receiver_holes.is_empty()
                && lookup_receiver.is_some()
                && !sig.own_params.is_empty()
                && self.turbofish_leaves_slot(&method_type_args, sig.own_params.len())
        });
        // The expected type answers an open slot or a `_` before a literal
        // under it takes its default. A newtype's inherited static is read at
        // its base.
        let ret = if open_own_slots.is_some() || !written_vars.is_empty() {
            let (_, expected_at_base) =
                self.at_inherited_base(newtype, declaring_link, &[], expected_type);
            match (&callee_sig, expected_at_base) {
                (Some(sig), Some(expected)) => {
                    let declaring = sig
                        .declaring_impl
                        .map(|id| self.tysys.signatures.impl_sig(id));
                    let declared = sig
                        .instantiate_call_with(
                            &self.tysys.type_table,
                            declaring,
                            &callee_args,
                            &method_type_args,
                        )
                        .return_type;
                    Some(ExpectedReturn { declared, expected })
                }
                (None, _) | (Some(_), None) => None,
            }
        } else {
            None
        };
        // The static's own slots the spelling left open, as variables the
        // arguments were typed against: a literal among them waits for what
        // the slots' bounds say, as an instance call's does.
        let mut own_inst = None;
        let args: Vec<TypeId> = if let Some(sig) = open_own_slots {
            let (_, impl_bindings) = self.static_impl_bindings(target_type_id, sig.declaring_impl);
            let impl_args = settled_impl_args(&impl_bindings);
            let declaring_trait = self.tysys.signatures.declaring_trait(sig);
            let (args, inst) = self.resolve_args_through_slots(
                ctx,
                &static_call.args,
                &param_types,
                &sig.own_type_param_ids(),
                &sig.own_params,
                &Instantiation {
                    kind: InstanceKind::Method,
                    name: &static_call.method,
                    span: static_call.span,
                    type_args: &method_type_args,
                    self_binding: Some(SelfBinding::of_static(target_type_id, declaring_trait)),
                    impl_args: &impl_args,
                },
                ret,
            );
            own_inst = inst;
            args
        } else if written_vars.is_empty() {
            static_call
                .args
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let expected_type = param_types.get(i).copied();
                    self.resolve_expr(a, ctx, expected_type)
                })
                .collect()
        } else {
            let inst = Instantiated::of_vars(written_vars.into_iter().collect());
            self.resolve_args_against_params(&static_call.args, ctx, &param_types, Some(&inst), ret)
        };
        let mut args: Vec<TypeId> = if receiver_holes.is_empty() {
            args
        } else {
            let (at_base, expected_at_base) =
                self.at_inherited_base(newtype, declaring_link, &args, expected_type);
            // A `_` is answered as the bare spelling answers the slot it
            // leaves out, by the one inference both spellings share.
            let inferred = lookup_receiver.as_ref().map(|receiver| {
                self.infer_static_method_type_args(
                    receiver,
                    &static_call.method,
                    &static_call.args,
                    &at_base,
                    expected_at_base,
                    static_call.span,
                    None,
                )
                .0
            });
            // The one block declaring the callee fixes a slot its target
            // writes concretely: `impl Wrap<i32>` answers `Wrap::<_>` itself.
            let block_target: Vec<TypeId> = callee_sig
                .as_ref()
                .and_then(|sig| sig.declaring_impl)
                .map(|id| self.tysys.type_table.borrow().impl_target_args(id).to_vec())
                .unwrap_or_default();
            // Both answer at the callee's link of a newtype chain, so each is
            // matched against the `_`s as that link spells them.
            let vars = &receiver_holes;
            let answers: Vec<Option<TypeId>> = (0..callee_args.len())
                .map(|j| {
                    inferred
                        .as_ref()
                        .and_then(|impl_args| impl_args.get(j).copied())
                        .or_else(|| block_target.get(j).copied())
                })
                .collect();
            let solved = self.answer_receiver_vars(vars, &callee_args, &answers);
            for (&var, answer) in vars.iter().zip(solved) {
                if answer != var {
                    self.equate(var, answer);
                }
            }
            declaring_args = self.apply_infer_holes_each(&declaring_args);
            target_type_id = self.with_written_args(target_type_id, &declaring_args);
            let callee_args =
                self.args_at_callee(target_type_id, &declaring_args, callee_sig.as_ref());
            self.instantiate_static_params(
                callee_sig.as_ref(),
                &callee_args,
                &method_type_args,
                self_in_args,
                inherited.then_some(target_type_id),
                &mut param_types,
            );
            let mut args = args;
            self.recoerce_literal_args(&static_call.args, &mut args, &param_types);
            args
        };
        let mut arg_sites: Vec<ArgSite> = static_call.args.iter().map(ArgSite::written).collect();

        let declaring_impl = callee_sig.as_ref().and_then(|sig| sig.declaring_impl);
        let (declaring_args, impl_bindings) =
            self.static_impl_bindings(target_type_id, declaring_impl);
        let declaring_trait = callee_sig
            .as_ref()
            .and_then(|sig| self.tysys.signatures.declaring_trait(sig));
        let own_type_param_ids = callee_sig
            .as_ref()
            .map(MethodSig::own_type_param_ids)
            .unwrap_or_default();
        let own_params = callee_sig
            .as_ref()
            .map(|sig| sig.own_params.clone())
            .unwrap_or_default();

        // A static's own slots, where the spelling wrote none: solved as an
        // instance call's are, so that what binds them reaches reify rather
        // than leaving an abstract `U::default()` to be rejected at the
        // trait-bound check over WIR.
        let own_from = callee_sig
            .as_ref()
            .is_some_and(|sig| self.own_from_sig(sig));
        if let Some(sig) = callee_sig
            && self.turbofish_leaves_slot(&method_type_args, sig.own_params.len())
            && !sig.own_params.is_empty()
            && let Some(receiver) = &lookup_receiver
        {
            let receiver = receiver.spelled(self.tysys.resolutions.defs()).to_string();
            let own_ids = sig.own_type_param_ids();
            let declaring = sig
                .declaring_impl
                .map(|id| self.tysys.signatures.impl_sig(id));
            let declared_return = sig
                .instantiate_call_with(
                    &self.tysys.type_table,
                    declaring,
                    &declaring_args,
                    &method_type_args,
                )
                .return_type;
            let declared_return = self.answer_frame_projections(declared_return);
            // A bound on the method may name the block's parameters, which
            // the receiver settles.
            let impl_args = settled_impl_args(&impl_bindings);
            let qualified = format!("{receiver}::{}", static_call.method);
            let (args_at_base, expected_at_base) =
                self.at_inherited_base(newtype, declaring_link, &args, expected_type);
            let input = MethodInferenceInput {
                slots: &own_ids,
                own_params: &sig.own_params,
                param_types: &param_types,
                args: &args_at_base,
                raw_args: &static_call.args,
                decl_return_type: declared_return,
                expected_return_type: expected_at_base,
                self_binding: SelfBinding::of_static(target_type_id, declaring_trait),
                declaring_module: Some(sig.defaults_module.clone()),
                impl_args: &impl_args,
                inst: own_inst.as_ref(),
                span: static_call.span,
            };
            (method_type_args, _) = if input.inst.is_some() {
                self.bind_method_type_args(method_type_args, input)
            } else {
                self.bind_static_type_args(method_type_args, &qualified, input)
            };
            self.instantiate_static_params(
                Some(&sig),
                &declaring_args,
                &method_type_args,
                self_in_args,
                inherited.then_some(target_type_id),
                &mut param_types,
            );
            self.recoerce_literal_args(&static_call.args, &mut args, &param_types);
        }

        // Pad omitted trailing arguments with declared parameter defaults.
        // Variant / flags constructors carry no defaults, so the arg-count
        // checks below are unaffected.
        //
        // The declaring block's type parameters stand for the receiver's type
        // arguments, so a default naming one (`v: T = T::default()`) resolves
        // against what `Type::<i32>::method()` spelled.
        let mut static_type_bindings = impl_bindings;
        // The static's own slots, as the turbofish spelled them, as the block
        // above solved them, or as their own type defaults settle them. That
        // last source is the one the block above cannot reach: it runs only
        // where the declaring block has slots of its own, and a block with none
        // leaves the method's unbound rather than wrong.
        let self_binding = SelfBinding::of_static(target_type_id, declaring_trait);
        let own = self.own_slot_default_bindings(
            &static_method_defaults,
            &own_params,
            &own_type_param_ids,
            self_binding,
            method_type_args.clone(),
            static_method_module.clone(),
        );
        bind_nearer(&mut static_type_bindings, own);
        self.fill_trailing_defaults(
            &mut args,
            &param_types,
            &static_method_defaults,
            static_method_module.clone(),
            Some(self_binding),
            &static_type_bindings,
            Some(static_call.id),
            ctx,
            |_, _, default_expr, _| arg_sites.push(ArgSite::written(default_expr)),
        );

        // A declared static is checked against its signature here, where the
        // spelled `Type::<T>::method(…)` call would otherwise reach codegen
        // with its arguments dropped.
        if declares_params
            && !self.check_static_call_args(
                &param_types,
                &args,
                &arg_sites,
                &static_method_defaults,
                static_call.span,
            )
        {
            return TypeTable::ERROR;
        }

        // A case, a flags member and a variant constructor are the receiver's
        // own declarations. A spelling that names a trait asks for none of
        // them — the same rule the resolution applies to its candidates.
        let builds_own_case = required_trait.is_none();

        // A flags type's `none()` and `all()`. The receiver's own declaration,
        // not its head resolved again. Only a `flags` declaration has members,
        // so this guards the kind.
        if builds_own_case
            && matches!(static_call.method.as_str(), "none" | "all")
            && let Some(flags_info) = self
                .tysys
                .type_table
                .borrow()
                .nominal_def(target_type_id)
                .and_then(|def| self.type_lookup().flags_members_of(def))
                .cloned()
        {
            if !self.flags_constant_call_is_bare(
                &static_call_symbol_name(static_call),
                args.len(),
                static_call.type_args.len(),
                static_call.span,
            ) {
                return TypeTable::ERROR;
            }
            return flags_info.type_id;
        }

        if static_call.method == "from"
            && !own_from
            && let [arg_type] = args[..]
            && let Some(converted) =
                self.convert_without_impl(static_call.id, target_type_id, arg_type, false)
        {
            return converted;
        }

        let Some((_, struct_module, mangled_struct_name, struct_type_args)) =
            self.static_receiver_parts(target_type_id, &static_call.method, required_trait)
        else {
            // Resolving the target reported why it failed.
            if target_type_id != TypeTable::ERROR {
                // The target names no struct-like type: a trait, an undeclared
                // name.
                let _ = self.emit(TypeError::UnknownFunction {
                    name: static_call_symbol_name(static_call),
                    span: static_call.span,
                });
            }
            return TypeTable::ERROR;
        };

        // A trait impl's static is mangled with its trait, so WIR resolves it.
        let receiver = self.receiver_of(target_type_id);
        let Ok(resolution) = self.static_trait_ref(
            StaticQuery {
                arg_types: &args,
                method_args: &method_type_args,
                required_trait,
                required_args,
                expected_return: expected_type,
                ..StaticQuery::of(&receiver, &static_call.method)
            },
            static_call.span,
        ) else {
            return TypeTable::ERROR;
        };
        let selected = resolution.selected;
        let declaration = resolution.declaration;
        let trait_name_opt = selected.as_ref().and_then(|r| r.trait_name.clone());

        let mangled_func_name = MethodName::format_local(
            &mangled_struct_name,
            trait_name_opt.as_ref(),
            &static_call.method,
        );

        let mut return_type = resolution.return_type;

        // A value blanket indexes statics under its receiver *param* name, so
        // the concrete receiver's own bucket misses.
        if return_type == TypeTable::UNKNOWN
            && let Some(blanket) = resolution.blanket
            && let Some(resolved) = self.resolve_blanket_static_method(
                target_type_id,
                &static_call.method,
                blanket,
                static_call.id,
                Some(static_call.method_id),
                &method_type_args,
                None,
                &static_call.args,
                &args,
                &arg_sites,
                expected_type,
                static_call.span,
                ctx,
            )
        {
            return resolved;
        }

        // Emit a compile error if the static method was not found anywhere.
        // Where one is declared and a `_` is still open, the bare spelling
        // would have asked for it, and so does this one.
        if return_type == TypeTable::UNKNOWN {
            let open: Vec<TypeId> = receiver_holes
                .iter()
                .copied()
                .chain(required_args.iter().copied())
                .filter(|&var| {
                    let answered = self.apply_infer_holes(var);
                    self.tysys.type_table.borrow().is_infer_var(answered)
                })
                .collect();
            if !open.is_empty()
                && self.static_may_resolve(
                    target_type_id,
                    &static_call.method,
                    required_trait,
                    required_args,
                )
            {
                self.close_uninferred(&open);
                return TypeTable::ERROR;
            }
            // The unknown name is the whole fault: no answer to an open `_`
            // could make it exist.
            for var in open {
                self.close_infer_var(var);
            }
            let _ = self.emit(TypeError::UnknownFunction {
                name: static_call_symbol_name(static_call),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        }

        // The receiver as the match above resolved it, which carries the
        // declaring module. Re-deriving it from `struct_name` asks the call
        // site's own scope, where `geo::Wrapper::<i64>::make(…)` has no bare
        // `Wrapper` to find and mints a shape head under the wrong module.
        let receiver_name = mangled_struct_name.head_only();

        let generic_name =
            MethodName::format_local(&receiver_name, trait_name_opt.as_ref(), &static_call.method);
        let (block, template) = match &selected {
            Some(r) => (
                self.tysys.block_of(r),
                self.tysys.static_template(r, &receiver_name),
            ),
            None => (
                declaration.and_then(|def| self.tysys.declaring_block(def)),
                declaration.map(|def| self.tysys.signatures.declared_template(def)),
            ),
        };
        let instance_args = self.static_instance_args(
            block,
            &struct_type_args,
            &mut param_types,
            &args,
            &mut return_type,
            static_call.span,
        );
        let monomorph_info = if instance_args.is_empty() && method_type_args.is_empty() {
            None
        } else {
            Some(MonomorphInfo {
                generic_name,
                impl_type_args: instance_args,
                method_type_args: method_type_args.clone(),
                is_blanket: false,
            })
        };

        let method_type_arg_names: Vec<FqTypeName> = method_type_args
            .iter()
            .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
            .collect();
        let impl_only_type_arg_names: Vec<FqTypeName> = struct_type_args
            .iter()
            .map(|t| self.tysys.type_table.borrow().fq_type_name_solved(*t))
            .collect();

        // Build method_info with base struct name and trait name (if applicable)
        let mut method_info =
            LocalMethodName::new(receiver_name, trait_name_opt, static_call.method.clone())
                .with_type_args(&impl_only_type_arg_names, &method_type_arg_names);

        // The `#[cm("...")]` import the callee binds, off the signature this
        // call resolved to, at the receiver it resolved at.
        method_info.cm_name =
            match declaration.and_then(|def| self.tysys.signatures.method_sig(def)) {
                Some(sig) => sig.cm_name.clone(),
                None => static_receiver
                    .and_then(|key| self.qualified_method_sig(key, &static_call.method))
                    .and_then(|sig| sig.cm_name),
            };

        if let Some(method_def) = declaration.or_else(|| {
            let owner = self.static_owner_key(&receiver, &static_call.method);
            self.qualified_method_decl_id(&owner, &static_call.method)
        }) && self.record_reference_to_decl(static_call.method_id, method_def, static_call.span)
        {
            return TypeTable::ERROR;
        }

        // A concrete block hosts its own function; a generic one's instance is
        // materialised in the receiver's module (`FuncInstState::impl_module`).
        // Naming the receiver's for both minted an extern stub beside a
        // definition of that name, where the two modules differ.
        let func_ref = FunctionRef {
            module_source: self
                .tysys
                .concrete_impl_module_of(selected.as_ref())
                .unwrap_or(struct_module),
            name: mangled_func_name,
            template,
            monomorph_info,
            method_info: Some(method_info),
        };

        // WEP 2026-05-26: record the resolved static-method
        // call so reify reproduces the same `FunctionRef` (mangled name,
        // monomorph info, and `cm_name` for CM binding synthesis) without
        // re-resolving the target type — reify's from-scratch resolution
        // fails on imported / CM generic targets (`Future::<T>::new`,
        // `Result::<…>::Ok`), yielding an empty struct name. Keyed on the
        // `StaticMethodCallExpr`'s own `AstId`; variant-ctor turbofish
        // shapes are handled by reify before this fact is consulted.
        //
        // Every path out of this function reaches the walk, not only the one
        // that filled the arguments above: reify pads from what it leaves.
        let defaults_module =
            static_method_module.unwrap_or_else(|| func_ref.module_source.clone());
        self.record_default_walk(
            static_call.id,
            static_call.span,
            &args,
            &param_types,
            &static_method_defaults,
            defaults_module.clone(),
            Some(self_binding),
            &static_type_bindings,
            ctx,
        );
        let key = static_call.id;
        self.sem.types.static_method_dispatch.insert(
            key,
            StaticMethodDispatch {
                method_def: declaration,
                // The scope annotate resolved these defaults in, so reify
                // resolves them in the same one.
                defaults_module,
                function_ref: func_ref,
                param_is_mut,
                type_args: method_type_args,
                param_defaults: static_method_defaults,
                param_types: param_types.clone(),
                self_in_args,
            },
        );

        return_type
    }

    /// Resolve a static method call from a qualified name like `Point::origin()`
    pub(super) fn resolve_static_method_call_from_qualified(
        &mut self,
        call_receiver: &ReceiverId,
        receiver_type: TypeId,
        method_name: &str,
        args: &[TypeId],
        impl_type_args: &[TypeId],
        method_type_args: &[TypeId],
        inherited: Option<InheritedStatic>,
        call_id: AstId,
        span: Span,
        expected_type: Option<TypeId>,
        ctx: Option<&mut FunctionContext>,
    ) -> TypeId {
        let struct_name = call_receiver
            .name(self.tysys.resolutions.defs())
            .to_string();
        let qualified_struct_name = self.qualified_receiver_name(&struct_name, receiver_type);
        let mangled_func_name_owned =
            MethodName::format_local(&qualified_struct_name, None, method_name);
        let mangled_func_name = mangled_func_name_owned.as_str();
        // A newtype inheriting the static dispatches to the link of its chain
        // that owns it.
        let mut newtype_dispatch: Option<(TypeId, TypeId, Vec<TypeId>)> = None;
        // The instance the call settled, where a bare generic newtype spelled
        // none.
        let receiver = inherited.map_or(receiver_type, |inherited| inherited.newtype);
        let owner = self
            .newtype_static_owner(receiver, method_name, None)
            .and_then(|(link, _)| (link != receiver).then_some((receiver, link)));
        // The written name keys the impl indices; the fq form names the method.
        let (actual_struct_name, actual_struct_fq, actual_mangled_name) = match owner {
            Some((newtype, link)) => {
                let (link_name, link_fq) = self.link_receiver_names(link);
                let mangled = MethodName::format_local(&link_fq, None, method_name);
                let link_args = self
                    .tysys
                    .type_table
                    .borrow()
                    .nominal_type_args(link)
                    .unwrap_or_default();
                newtype_dispatch = Some((newtype, link, link_args));
                (link_name, link_fq, mangled)
            }
            None => (
                struct_name,
                qualified_struct_name,
                mangled_func_name.to_string(),
            ),
        };

        let impl_type_args_owned: Vec<TypeId> = match &newtype_dispatch {
            Some((_, _, base_args)) if impl_type_args.is_empty() && !base_args.is_empty() => {
                base_args.clone()
            }
            _ => impl_type_args.to_vec(),
        };
        let impl_type_args = impl_type_args_owned.as_slice();

        // The argument separates a user-defined `impl From<MyType> for i32`
        // from the primitive's, and `impl Conv<A>` from `impl Conv<B>`. A
        // newtype's static call dispatches to its base, whose name is not the
        // caller's to resolve — that frame can hold a same-named declaration of
        // its own.
        let (link_receiver, link_type) = match &newtype_dispatch {
            Some((_, base_type_id, _)) => (
                ReceiverId::keyed(self.receiver_of(*base_type_id).key().clone(), None),
                *base_type_id,
            ),
            None => (call_receiver.clone(), receiver_type),
        };
        // The resolution walks from the newtype to its base itself, so it reads
        // the arguments at the base and answers in the newtype. It reads the
        // signature at `impl_type_args`, the only place they are substituted.
        let query_receiver = match &newtype_dispatch {
            Some((newtype, _, _)) => self.receiver_of(*newtype),
            None => call_receiver.clone(),
        };
        let Ok(resolution) = self.static_trait_ref(
            StaticQuery {
                arg_types: args,
                receiver_args: impl_type_args,
                method_args: method_type_args,
                expected_return: expected_type,
                ..StaticQuery::of(&query_receiver, method_name)
            },
            span,
        ) else {
            return TypeTable::ERROR;
        };
        let declaration = resolution.declaration;
        if declaration.is_none()
            && resolution.return_type == TypeTable::UNKNOWN
            && (self.derived_for_no_instance(&link_receiver, method_name, impl_type_args)
                || self.declared_by_no_reaching_block(&link_receiver, method_name, impl_type_args))
        {
            let receiver = link_receiver.spelled(self.tysys.resolutions.defs());
            let _ = self.emit(TypeError::UnknownFunction {
                name: format!("{receiver}::{method_name}"),
                span,
            });
            return TypeTable::ERROR;
        }

        // An inherent impl may live in any module of the package that owns the
        // type, and its methods are registered under that module. So the
        // fallback takes the impl's module where one is indexed, and the type's
        // home only where none is (`cross_module_inherent_static.wado`).
        let method_ref = resolution.selected.unwrap_or_else(|| {
            let module = declaration
                .map(|def| self.tysys.resolutions.defs().module(def).clone())
                .or_else(|| {
                    self.static_method_entries(link_receiver.key(), method_name)
                        .find(|e| e.is_inherent())
                        .map(|e| e.module.clone())
                })
                .unwrap_or_else(|| self.declaring_module_of(link_type));
            StaticMethodRef::new(module, &actual_struct_name, method_name, None, declaration)
        });

        // A concrete block hosts its function under the head it wrote:
        // `impl … for Cell<i32>` emits `Cell<i32>::wrap`, so a call spelling
        // the receiver `Cell` names that. A generic block's instance is
        // monomorphized under the receiver's own name, and keeps it.
        let concrete_head = self.tysys.concrete_impl_head_of(Some(&method_ref));
        let final_mangled_name = if concrete_head.is_some() || method_ref.trait_name.is_some() {
            let receiver = concrete_head.as_ref().unwrap_or(&actual_struct_fq);
            MethodName::format_local(receiver, method_ref.trait_name.as_ref(), method_name)
        } else {
            actual_mangled_name
        };
        let receiver_fq = concrete_head.unwrap_or(actual_struct_fq);
        let block = self.tysys.block_of(&method_ref);
        let mut return_type = resolution.return_type;
        let mut callee_params = resolution.params;
        let instance_args = self.static_instance_args(
            block,
            impl_type_args,
            &mut callee_params.param_types,
            args,
            &mut return_type,
            span,
        );
        self.retype_inherited(inherited, std::slice::from_mut(&mut return_type));

        let template = self.tysys.static_template(&method_ref, &receiver_fq);
        let monomorph_info = if instance_args.is_empty() && method_type_args.is_empty() {
            None
        } else {
            Some(MonomorphInfo {
                generic_name: final_mangled_name.clone(),
                impl_type_args: instance_args,
                method_type_args: method_type_args.to_vec(),
                is_blanket: false,
            })
        };

        // The signature the spelling names, whichever kind of method it is: the
        // `#[cm("...")]` import the callee binds is read off it. A static-only
        // index answers for one kind of method, and an instance one reached
        // qualified then lost its binding and left reify emitting a call to a
        // name nothing declares.
        let callee_sig = declaration
            .and_then(|def| self.tysys.signatures.method_sig(def).cloned())
            .or_else(|| {
                self.static_call_sig_or_inherited(
                    link_receiver.key(),
                    method_name,
                    SigChoice::Any,
                    impl_type_args,
                )
            });
        let cm_name = callee_sig.as_ref().and_then(|sig| sig.cm_name.clone());

        // No signature answered: there is no default to resolve, and the
        // declaring module stands in.
        let defaults_module = callee_params
            .defaults_module
            .clone()
            .unwrap_or_else(|| method_ref.module.clone());

        // Walk the defaults this spelling left out, so a default naming the
        // method's own type parameter (`u: U = U::default()`) resolves against
        // what the call settled that parameter to. Reify pads from this walk's
        // overlay; without one it replays the declaration's, where the
        // parameter is still abstract and the trait-bound check over WIR
        // rejects it.
        // A signature answers for the method's own slots; a spelling no index
        // has one for — a trait's default body, reached on a type whose `impl`
        // restates nothing — binds no name, and the walk still has to happen.
        let declaring_trait = callee_sig
            .as_ref()
            .and_then(|sig| self.tysys.signatures.declaring_trait(sig));
        // A newtype inheriting the static is `Self` in its defaults: the
        // parameters are retyped to it, so a `Self::make()` default has to
        // answer in it too.
        let self_binding = match &newtype_dispatch {
            Some((newtype, _, _)) => SelfBinding::of_static(*newtype, declaring_trait),
            None => self.tysys.base_self_binding(receiver_type, declaring_trait),
        };
        let type_bindings = match &callee_sig {
            Some(sig) => {
                let own_ids = sig.own_type_param_ids();
                let own_params = sig.own_params.clone();
                self.value_default_slot_bindings(
                    &own_params,
                    &own_ids,
                    self_binding,
                    method_type_args.to_vec(),
                    Some(defaults_module.clone()),
                )
            }
            None => Vec::new(),
        };
        match ctx {
            Some(ctx) => self.record_default_walk(
                call_id,
                span,
                args,
                &callee_params.param_types,
                &callee_params.param_defaults,
                defaults_module,
                Some(self_binding),
                &type_bindings,
                ctx,
            ),
            None => assert_eq!(
                args.len(),
                callee_params.param_types.len(),
                "a call resolved past its body omits no default"
            ),
        }

        let StaticMethodRef {
            module: struct_module,
            trait_name: trait_name_opt,
            ..
        } = method_ref;

        let func_ref = FunctionRef {
            module_source: struct_module,
            name: final_mangled_name,
            template,
            monomorph_info,
            method_info: Some({
                // The same head the name was built from: mono looks the concrete
                // block's module up by this very spelling.
                let mut m =
                    LocalMethodName::new(receiver_fq, trait_name_opt, method_name.to_string());
                m.cm_name = cm_name;
                m
            }),
        };

        // Record the static-method dispatch decision so reify can reproduce
        // the same `Call` shape without re-running impl lookup, mangled-name
        // construction, or monomorph-info shaping. Recorded for every call this
        // path resolves: reify rebuilds the `Call` from this fact alone, so a
        // callee whose signature no lookup answers still needs the shape.
        self.sem.types.static_method_dispatch.insert(
            call_id,
            StaticMethodDispatch::of_params(method_ref.method_id, func_ref, vec![], callee_params),
        );

        return_type
    }

    /// The receiver a static call on `target` names, `None` where the target
    /// names no type a block can be written for. A newtype names the link of
    /// its chain that owns `method`; a reference names itself, hosted where
    /// what it refers to is.
    fn static_receiver_parts(
        &mut self,
        target: TypeId,
        method: &str,
        required_trait: Option<DefId>,
    ) -> Option<QualifiedReceiver> {
        let resolved = self.tysys.type_table.borrow().get(target).clone();
        match resolved {
            ResolvedType::Newtype { .. } => {
                Some(self.newtype_static_receiver(target, method, required_trait))
            }
            ResolvedType::Ref(pointee) | ResolvedType::MutRef(pointee) => {
                let kind = RefKind::from_resolved(&resolved).expect("matched a reference");
                let (_, module, _, args) = self.receiver_shape(pointee)?;
                let fq = self.tysys.type_table.borrow().fq_type_name(target);
                Some((kind.prefix().to_string(), module, fq, args))
            }
            _ => self.receiver_shape(target),
        }
    }

    /// The receiver a qualified call on a newtype names: the link of its chain
    /// that owns `method`.
    fn newtype_static_receiver(
        &mut self,
        newtype: TypeId,
        method: &str,
        required_trait: Option<DefId>,
    ) -> QualifiedReceiver {
        let (link, _) = self
            .newtype_static_owner(newtype, method, required_trait)
            .expect("a newtype's static has an owning link");
        let declares = self.is_declaring_link(link);
        let type_table = self.tysys.type_table.borrow();
        if declares || type_table.instance_parts(link).is_some() {
            return nominal_receiver(&type_table, link);
        }
        match type_table.get(link) {
            ResolvedType::Primitive(prim) => primitive_receiver(*prim),
            ResolvedType::Struct { .. } => nominal_receiver(&type_table, link),
            _ => nominal_receiver(&type_table, newtype),
        }
    }

    /// Whether some declaration may offer `target` a static `method_name` at
    /// an instance of its open variables: a block on its head, or an impl of
    /// a trait in scope whose target and bounds may hold. Where none may, no
    /// answer to an open `_` could make the call resolve.
    fn static_may_resolve(
        &self,
        target: TypeId,
        method_name: &str,
        required_trait: Option<DefId>,
        required_args: &[TypeId],
    ) -> bool {
        let inherent = required_trait.is_none()
            && self.tysys.static_receiver_key(target).is_some_and(|key| {
                self.static_method_entries(&key, method_name)
                    .next()
                    .is_some()
            });
        inherent
            || self.tysys.solver.as_ref().is_some_and(|bridge| {
                bridge.borrow().may_answer(
                    &self.tysys,
                    &self.annotate_ctx,
                    self.frame_module(),
                    target,
                    method_name,
                    required_trait,
                    required_args,
                )
            })
    }

    /// The signature of the declaration a static resolution selected, read in
    /// the block supplying it: a trait default the block inherits is read
    /// there as [`Self::inherited_sig_in_block`] reads it.
    fn selected_static_sig(&mut self, selected: &StaticMethodRef) -> Option<sig::MethodSig> {
        let Some(block) = selected.supplying_block else {
            return self
                .tysys
                .signatures
                .method_sig(selected.method_id?)
                .cloned();
        };
        let trait_decl = self.tysys.trait_env.impl_headers[&block].trait_def()?;
        let inherited = self
            .tysys
            .trait_sig_of(&trait_decl)?
            .method(&selected.method_name)?
            .sig
            .clone();
        self.inherited_sig_in_block(block, inherited)
    }

    /// The type a static call's receiver names: the call's own, else the one
    /// its declaration declares, since a namespaced `dep::Pt` leaves the
    /// caller's frame no `Pt`, or another one. `None` for a declaration that
    /// registers no type, such as a local `variant`.
    pub(super) fn static_receiver_type(&mut self, receiver: &ReceiverId) -> Option<TypeId> {
        receiver
            .ty()
            .or_else(|| primitive_receiver_type(receiver.key()))
            .or_else(|| self.type_declared_by(Some(receiver.key())))
    }

    /// Whether `receiver::method_name` names a declaration at all — the
    /// resolution [`Elaborator::resolve_static_callee`] performs, asked for its
    /// outcome alone.
    pub(super) fn is_static_method_at(&mut self, receiver: &ReceiverId, method_name: &str) -> bool {
        self.resolve_static_callee(StaticQuery::of(receiver, method_name))
            .resolves()
    }
}
