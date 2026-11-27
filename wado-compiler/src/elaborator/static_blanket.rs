//! A static call a value blanket (`impl<T: Bound> Trait for T`) answers: which
//! blankets cover a receiver, and the template instantiated at it.

use std::sync::Arc;

use super::Elaborator;
use super::call::ArgSite;
use super::call::CalleeSignature;
use super::call::GivenSlots;
use super::call::slot_names;
use super::method_call::UnappliedStatic;
use super::method_lookup::MethodInferenceInput;
use super::method_lookup::settled_impl_args;
use super::sem::types::StaticMethodDispatch;
use super::sig::MethodSig;
use super::sig::Param;
use super::static_call::ReceiverId;
use super::static_call::StaticLookup;
use super::static_call::StaticQuery;
use super::trait_env::ImplTargetKey;
use super::trait_env::TraitEnv;
use super::trait_query::SelfBinding;
use super::types::FunctionContext;
use super::types::TypeError;
use super::tysys::TypeSystem;
use crate::Span;
use crate::ast;
use crate::ast::AstId;
use crate::ast::Expr;
use crate::compiler_host::CompilerHost;
use crate::defs::DefId;
use crate::elaborator::call::slot_type_bindings;
use crate::elaborator::trait_env::BlanketImpl;
use crate::elaborator::trait_env::BlanketReceiver;
use crate::module_source::ModuleSource;
use crate::name::FqTraitName;
use crate::name::FqTypeName;
use crate::name::LocalMethodName;
use crate::name::MethodName;
use crate::tir::FunctionRef;
use crate::tir::MonomorphInfo;
use crate::tir::ResolvedType;
use crate::tir::TypeArgMap;
use crate::tir::TypeId;
use crate::type_table::TypeTable;

/// The value blanket a static call dispatches through.
pub(super) struct BlanketStatic {
    pub trait_name: FqTraitName,
    /// The receiver parameter as written (`T`) — what the static-method
    /// indices key on.
    pub param: String,
    pub binder: FqTypeName,
    pub module: ModuleSource,
    pub def: DefId,
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Every value blanket supplying a static `method_name` that the receiver's
    /// bounds admit, in the order the blocks were written. The bounds are read
    /// at the receiver itself, not through its newtype chain: each link is
    /// asked on its own. Facts only: which of several answers is
    /// [`Elaborator::resolve_static_callee`]'s to decide.
    pub(super) fn applicable_blanket_statics(
        &self,
        receiver_type_id: TypeId,
        method_name: &str,
    ) -> Vec<BlanketStatic> {
        self.tysys
            .trait_env
            .blanket_impls
            .values()
            .flatten()
            .filter(|b| b.receiver == BlanketReceiver::Value)
            .filter(|b| {
                let target = ImplTargetKey::TypeParam {
                    owner: b.def,
                    name: b.param.clone(),
                };
                self.static_method_entries(&target, method_name)
                    .next()
                    .is_some()
            })
            .filter(|b| {
                b.bounds.iter().all(|bound| {
                    bound.trait_.as_ref().is_some_and(|bound_trait| {
                        self.tysys.type_implements_trait_here(
                            &self.annotate_ctx,
                            &self.type_lookup(),
                            receiver_type_id,
                            bound_trait,
                        )
                    })
                })
            })
            .filter_map(|b| self.blanket_static(b))
            .collect()
    }

    /// `None` where the impl names a trait that does not resolve.
    fn blanket_static(&self, blanket: &BlanketImpl) -> Option<BlanketStatic> {
        // The trait comes off the impl's own header, so the blanket index's
        // bare-name key never reaches a mangled name.
        let header = &self.tysys.trait_env.impl_headers[&blanket.def];
        Some(BlanketStatic {
            trait_name: self
                .tysys
                .trait_env
                .fq_trait_of_impl(header, &self.tysys.resolutions)?,
            param: blanket.param.clone(),
            binder: blanket.receiver_binder(self.tysys.resolutions.defs()),
            module: blanket.module.clone(),
            def: blanket.def,
        })
    }

    /// The value blanket impl `blanket` the resolution picked. Its bounds held
    /// at the link that owns the call, which need not be the receiver's head:
    /// a newtype may withhold a trait its base derives.
    fn picked_blanket_static(&self, blanket: DefId) -> BlanketStatic {
        self.tysys
            .trait_env
            .blanket_of_block(blanket)
            .and_then(|candidate| self.blanket_static(candidate))
            .expect("the resolution picks a blanket the index holds")
    }

    /// The value parameters and the result of the blanket's static `method`
    /// with its receiver param bound to the concrete receiver: the template is
    /// written against that param, so `-> Self` / `-> T` lands on the receiver
    /// and the call site reads the types the instantiation will take.
    fn blanket_static_at_receiver(
        &mut self,
        blanket: DefId,
        blanket_param: &str,
        method: &str,
        receiver_type_id: TypeId,
    ) -> (Vec<TypeId>, TypeId) {
        let method_def = self
            .tysys
            .declared_method(blanket, method)
            .expect("a blanket is a candidate only where it declares the method");
        let sig = self
            .tysys
            .signatures
            .method_sig(method_def)
            .expect("the decl pass records every impl-declared method's signature");
        let template_params = sig.value_param_types();
        let template_return = sig.decl.return_type.unwrap_or(TypeTable::UNIT);
        let block_slots = self.blanket_block_slots(blanket, blanket_param, receiver_type_id);
        let param_types = template_params
            .iter()
            .map(|&pt| self.substitute_map_in_frame(&block_slots, pt))
            .collect();
        (
            param_types,
            self.substitute_map_in_frame(&block_slots, template_return),
        )
    }

    /// The blanket's block parameters at `receiver_type_id`: its own is the
    /// receiver, which settles the others through its bounds, `A` in
    /// `impl<A, T: Holder<Item = A>> Trait<A> for T`.
    fn blanket_block_slots(
        &mut self,
        blanket: DefId,
        blanket_param: &str,
        receiver_type_id: TypeId,
    ) -> TypeArgMap {
        let blanket_slot = self.tysys.blanket_param_slot(blanket, blanket_param);
        let mut table = self.tysys.type_table.borrow_mut();
        let mut slots = table.bind_params(&[blanket_slot], &[receiver_type_id]);
        table.project_impl_slots(blanket, &mut slots);
        slots
    }

    /// [`Self::blanket_static_at_receiver`] for the blanket a resolution
    /// picked, which names it by its block alone.
    pub(super) fn picked_blanket_static_at(
        &mut self,
        blanket: DefId,
        method: &str,
        receiver_type_id: TypeId,
    ) -> (Vec<TypeId>, TypeId) {
        let BlanketStatic { param, def, .. } = self.picked_blanket_static(blanket);
        self.blanket_static_at_receiver(def, &param, method, receiver_type_id)
    }

    /// Resolve a `static` trait method reached through a value blanket impl
    /// (`impl<T: Bound> Trait for T`). The blanket has no per-type home, so its
    /// statics are indexed under the receiver param name (`T`) and the concrete
    /// receiver's bucket never sees them. Dispatch to the template of
    /// `blanket`, the one the resolution picked, the way an instance method
    /// reached through the same impl already does. `method_id` is the segment
    /// spelling the method, where the source writes one.
    pub(super) fn resolve_blanket_static_method(
        &mut self,
        receiver_type_id: TypeId,
        method: &str,
        blanket: DefId,
        call_id: AstId,
        method_id: Option<AstId>,
        method_type_args: &[TypeId],
        given: Option<GivenSlots<'_>>,
        raw_args: &[Expr],
        args: &[TypeId],
        arg_sites: &[ArgSite],
        expected_type: Option<TypeId>,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> Option<TypeId> {
        let BlanketStatic {
            trait_name,
            param: blanket_param,
            binder,
            module: blanket_module,
            def: blanket_def,
        } = self.picked_blanket_static(blanket);

        let template_name = MethodName::format_local(&binder, Some(&trait_name), method);
        // The blanket's own declaration of the method: its bucket is keyed by
        // the receiver *param*, which no name written at a call site reaches.
        let method_def = self
            .tysys
            .declared_method(blanket_def, method)
            .expect("a blanket is a candidate only where it declares the method");
        // Conformance refuses an unavailable blanket member, so the edge alone.
        if let Some(use_id) = method_id {
            let node = self.tysys.resolutions.defs().ast_id(method_def);
            self.record_reference_to_def(use_id, node);
        }
        let sig = self
            .tysys
            .signatures
            .method_sig(method_def)
            .expect("the decl pass records every impl-declared method's signature");
        // Its defaults come from the template's own signature: no receiver-keyed
        // lookup reaches a blanket, so the call site brings none.
        let static_method_defaults = Param::named_defaults(&sig.params);
        let defaults_module = sig.defaults_module.clone();
        // The method's own slots are solved as an instance call's are: from
        // the turbofish, the arguments, and the type expected of the call.
        let own_params = sig.own_params.clone();
        let slots = sig.own_type_param_ids();
        if sig.decl.return_type == Some(TypeTable::UNKNOWN) {
            return None;
        }
        let blanket_slot = self.tysys.blanket_param_slot(blanket_def, &blanket_param);
        let (param_types, return_type) =
            self.blanket_static_at_receiver(blanket_def, &blanket_param, method, receiver_type_id);

        let mut written = method_type_args.to_vec();
        if self.group_variadic_type_args_of(method, &own_params, &mut written, span) {
            return Some(TypeTable::ERROR);
        }
        let trait_decl = trait_name.canonical();
        // A bound on the method may name the blanket's parameter, which the
        // receiver settles.
        let impl_args = settled_impl_args(&slot_type_bindings(
            &self.tysys.type_table,
            &[blanket_slot],
            &[receiver_type_id],
        ));
        let qualified = format!(
            "{}::{method}",
            self.tysys.type_table.borrow().name(receiver_type_id)
        );
        let (method_type_args, subst) = self.bind_static_type_args(
            written,
            &qualified,
            MethodInferenceInput {
                slots: &slots,
                own_params: &own_params,
                param_types: &param_types,
                args,
                raw_args,
                decl_return_type: return_type,
                expected_return_type: expected_type,
                self_binding: SelfBinding::of_static(receiver_type_id, trait_decl),
                declaring_module: Some(blanket_module.clone()),
                impl_args: &impl_args,
                inst: None,
                span,
            },
        );
        // The variables the call typed its arguments by are answered by this
        // solve: left open, a slot only its default settles is reported.
        if let Some(given) = given {
            let block_slots =
                self.blanket_block_slots(blanket_def, &blanket_param, receiver_type_id);
            let solved: Vec<TypeId> = given
                .slots
                .iter()
                .zip(&given.inst.vars)
                .map(|(&slot, &var)| {
                    if let Some(&answer) = slots
                        .iter()
                        .zip(&method_type_args)
                        .find_map(|(&own, answer)| (own == slot).then_some(answer))
                    {
                        return answer;
                    }
                    let at_receiver = self.substitute_map_in_frame(&block_slots, slot);
                    if at_receiver == slot {
                        var
                    } else {
                        at_receiver
                    }
                })
                .collect();
            self.record_instantiation(given.inst, &solved);
        }
        let param_types: Vec<TypeId> = param_types
            .iter()
            .map(|&t| self.substitute_map_in_frame(&subst, t))
            .collect();
        let return_type = self.substitute_map_in_frame(&subst, return_type);

        // The arguments were resolved before the blanket was found, so a
        // literal among them takes its parameter's type only now.
        let mut args = args.to_vec();
        self.recoerce_literal_args(raw_args, &mut args, &param_types);
        let args = &args[..];
        // Unchecked, a mis-arity or mis-typed call reaches codegen and surfaces
        // as a Wasm validation failure.
        if !self.check_static_call_args(
            &param_types,
            args,
            arg_sites,
            &static_method_defaults,
            span,
        ) {
            return Some(TypeTable::ERROR);
        }

        let receiver_arg_name = self
            .tysys
            .type_table
            .borrow()
            .fq_type_name(receiver_type_id);
        let method_type_arg_names: Vec<FqTypeName> = method_type_args
            .iter()
            .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
            .collect();
        let method_info = LocalMethodName::new(
            self.tysys.fq_receiver_head(receiver_type_id),
            Some(trait_name),
            method.to_string(),
        )
        .with_type_args(&[receiver_arg_name], &method_type_arg_names);

        let func_ref = FunctionRef {
            module_source: blanket_module,
            name: method_info.to_mangled_name(),
            template: self.tysys.trait_env.method_template(blanket_def, method),
            monomorph_info: Some(MonomorphInfo {
                generic_name: template_name,
                impl_type_args: vec![receiver_type_id],
                method_type_args: method_type_args.clone(),
                is_blanket: true,
            }),
            method_info: Some(method_info),
        };
        // The template is written against the blanket parameter, which the
        // monomorphizer binds to the receiver. A default reads the method's own
        // slots as the call solved them.
        let self_binding = SelfBinding::of_static(receiver_type_id, trait_decl);
        let own_bindings = self.own_slot_default_bindings(
            &static_method_defaults,
            &own_params,
            &slots,
            self_binding,
            method_type_args.clone(),
            Some(defaults_module.clone()),
        );
        self.record_default_walk(
            call_id,
            span,
            args,
            &param_types,
            &static_method_defaults,
            defaults_module.clone(),
            Some(self_binding),
            &own_bindings,
            ctx,
        );
        self.sem.types.static_method_dispatch.insert(
            call_id,
            StaticMethodDispatch {
                method_def: Some(method_def),
                defaults_module,
                function_ref: func_ref,
                param_is_mut: Vec::new(),
                type_args: method_type_args,
                param_defaults: static_method_defaults,
                param_types,
                self_in_args: false,
            },
        );

        Some(return_type)
    }

    /// The signature of the static the resolution hands a value blanket, read
    /// at `receiver`: the blanket's own parameter is the receiver, which leaves
    /// the method's slots for the call site to instantiate.
    pub(super) fn blanket_static_signature(
        &mut self,
        receiver: &ReceiverId,
        method_name: &str,
    ) -> Option<CalleeSignature> {
        let Some(receiver_type) = receiver.ty() else {
            return self.unapplied_blanket_static_signature(receiver, method_name);
        };
        let blanket = self
            .resolve_static_callee(StaticQuery::of(receiver, method_name))
            .blanket()?;
        let (param_types, return_type) =
            self.picked_blanket_static_at(blanket, method_name, receiver_type);
        let sig = self
            .tysys
            .declared_method(blanket, method_name)
            .and_then(|method| self.tysys.signatures.method_sig(method))
            .expect("the decl pass records every impl-declared method's signature");
        Some(CalleeSignature {
            param_types,
            slots: sig.own_type_param_ids(),
            declaring_slot_count: 0,
            return_type: Some(return_type),
            slot_decls: (
                slot_names(&sig.own_params, &self.tysys.resolutions),
                sig.own_params.clone(),
            ),
        })
    }

    /// The signature of a blanket's static called on a generic type spelled
    /// without its arguments (`Bx::collect(bx, [1, 2])`): the blanket's
    /// receiver parameter is a slot the arguments settle, so one of them names
    /// the instance while the rest take their parameters' types. Only where
    /// nothing the receiver declares answers the name and a single value
    /// blanket does, since no instance says yet which blankets admit it.
    fn unapplied_blanket_static_signature(
        &mut self,
        receiver: &ReceiverId,
        method_name: &str,
    ) -> Option<CalleeSignature> {
        self.receiver_generic_arity(receiver)?;
        if self.declares_case_named(receiver.key(), method_name)
            || !matches!(
                self.resolve_static_callee(StaticQuery::of(receiver, method_name)),
                StaticLookup::NotStatic
            )
        {
            return None;
        }
        let trait_env = Arc::clone(&self.tysys.trait_env);
        let [(_, sig)] = self.value_blanket_statics(&trait_env, method_name)[..] else {
            return None;
        };
        Some(CalleeSignature::of_method(sig, self.sig_slots(sig)))
    }

    /// Each value blanket declaring a static `method_name`, with the
    /// declaration's signature.
    pub(super) fn value_blanket_statics<'e>(
        &'e self,
        trait_env: &'e TraitEnv,
        method_name: &str,
    ) -> Vec<(&'e BlanketImpl, &'e MethodSig)> {
        trait_env
            .blanket_impls
            .values()
            .flatten()
            .filter(|blanket| blanket.receiver == BlanketReceiver::Value)
            .filter_map(|blanket| {
                let method = self.tysys.declared_method(blanket.def, method_name)?;
                let sig = self.tysys.signatures.method_sig(method)?;
                (sig.self_kind == ast::SelfKind::None).then_some((blanket, sig))
            })
            .collect()
    }

    pub(super) fn resolve_named_type_blanket_static(
        &mut self,
        receiver: &ReceiverId,
        method: &str,
        call_id: AstId,
        method_id: Option<AstId>,
        method_type_args: &[TypeId],
        given: Option<GivenSlots<'_>>,
        args: &[TypeId],
        raw_args: &[Expr],
        expected_type: Option<TypeId>,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> Option<TypeId> {
        // A generic type spelled without its arguments names no instance, so
        // the call's context settles one before the rules are asked.
        let unapplied = self.receiver_generic_arity(receiver);
        let instance = match unapplied {
            Some(_) => self.unapplied_static_instance(receiver, method, args, expected_type),
            None => UnappliedStatic::AsSpelled,
        };
        let settled;
        let receiver = match instance {
            UnappliedStatic::Settled(instance) => {
                settled = receiver.at_type(Some(instance));
                &settled
            }
            UnappliedStatic::AsSpelled | UnappliedStatic::Open => receiver,
        };
        let receiver_ty = receiver
            .ty()
            .expect("a blanket static is asked at the type the call resolved");
        // The rules first, since a blanket is a candidate among the rest: two
        // traits blanketing one receiver supply the name and separate nothing,
        // which is the ambiguity every other spelling of that shape reports.
        let ranked = self.resolve_static_callee(StaticQuery {
            arg_types: args,
            expected_return: expected_type,
            ..StaticQuery::of(receiver, method)
        });
        // Impls of one trait no argument separates may be told apart by the
        // instance the call settles, which the static call's own resolution
        // waits for, and reports where it cannot.
        if matches!(instance, UnappliedStatic::Open)
            && matches!(ranked, StaticLookup::Overloaded { .. })
        {
            return None;
        }
        if self.report_unresolved_static(&ranked, method, args, span) {
            return Some(TypeTable::ERROR);
        }
        let Some(blanket) = ranked.blanket() else {
            if !matches!(instance, UnappliedStatic::Open) {
                return None;
            }
            let prefix = receiver.spelled(self.tysys.resolutions.defs()).to_string();
            let declared = receiver
                .key()
                .decl()
                .and_then(|def| self.declared_self_type(def));
            let params = declared
                .and_then(|ty| self.tysys.type_table.borrow().nominal_type_args(ty))
                .expect("an unapplied generic declares its parameters");
            let names: Vec<String> = params
                .iter()
                .map(|&param| self.tysys.type_id_to_string(param))
                .collect();
            let _ = self.emit(TypeError::CannotInferType {
                message: TypeError::receiver_args_uninferred_message(&names, &prefix, method),
                span,
            });
            return Some(TypeTable::ERROR);
        };
        // The blanket would key on the argument-less head, which carries no
        // layout (a generic variant never becomes its own declaration, WEP
        // 2026-02-09) and reaches WIR build unregistered.
        if let Some(expected) = unapplied
            && !matches!(instance, UnappliedStatic::Settled(_))
        {
            let _ = self.emit(TypeError::MissingTypeArguments {
                name: receiver.spelled(self.tysys.resolutions.defs()).to_string(),
                expected,
                span,
            });
            return Some(TypeTable::ERROR);
        }
        // Built here rather than on the caller's hot path: only a call that
        // actually reaches a blanket static needs the per-argument sites.
        let arg_sites = self.arg_sites_of(call_id, raw_args, args.len(), span);
        self.resolve_blanket_static_method(
            receiver_ty,
            method,
            blanket,
            call_id,
            method_id,
            method_type_args,
            given,
            raw_args,
            args,
            &arg_sites,
            expected_type,
            span,
            ctx,
        )
    }

    /// `receiver::suffix` answered by a value blanket's static, indexed under
    /// the blanket's receiver param, or the diagnostic for a name reaching no
    /// function. `receiver` carries the type the call resolved it to.
    pub(super) fn blanket_static_or_unknown(
        &mut self,
        receiver: &ReceiverId,
        suffix: &str,
        ident: &ast::IdentExpr,
        call: &ast::CallExpr,
        given: Option<GivenSlots<'_>>,
        args: &[TypeId],
        expected_type: Option<TypeId>,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let method_id = ident.segments.last().map(|seg| seg.id);
        let method_type_args = self.resolve_turbofish_args(&call.type_args);
        if let Some(return_type) = self.resolve_named_type_blanket_static(
            receiver,
            suffix,
            call.id,
            method_id,
            &method_type_args,
            given,
            args,
            &call.args,
            expected_type,
            call.span,
            ctx,
        ) {
            return return_type;
        }
        let prefix = receiver.spelled(self.tysys.resolutions.defs());
        let _ = self.emit(TypeError::UnknownFunction {
            name: format!("{prefix}::{suffix}"),
            span: call.span,
        });
        TypeTable::ERROR
    }
}

impl TypeSystem {
    /// The type parameter the blanket impl `blanket` targets, `T` in
    /// `impl<A, T: Holder<Item = A>> Trait for T`: its declared slot, which
    /// need not be the first.
    pub(super) fn blanket_param_slot(&self, blanket: DefId, blanket_param: &str) -> TypeId {
        let table = self.type_table.borrow();
        let slot = table.impl_target_whole(blanket);
        assert!(
            matches!(table.get(slot), ResolvedType::TypeParam { id }
                if id.owner == blanket && table.param_name(*id) == blanket_param),
            "a blanket impl targets its own parameter"
        );
        slot
    }
}
