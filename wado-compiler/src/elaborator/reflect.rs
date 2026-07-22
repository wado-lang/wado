//! `Reflect` / `ReflectVariant` / `ReflectEnum` / `ReflectFlags` static-call
//! resolution (WEP 2026-06-13): the `Trait::<T>::method()` form that
//! `resolve_static_method_call` intercepts and routes to the type's synthesized
//! `T^Trait::method`.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::name::{LocalMethodName, MethodName};
use crate::tir::{FunctionRef, ResolvedType, TypeId, TypeTable};

use super::Elaborator;
use super::trait_query::OnBoundTrait;
use super::types::{FunctionContext, TypeError};

/// The two scalar-kind reflection traits share one static-call resolution shape
/// (WEP 2026-06-13 §3b / §3c): four members — `type_name` / `<meta>` /
/// `<value>(&self)` / `from_<value>(raw)` — differing only in the compiler items
/// they name, the subject kind, and the scalar value type (`i32` / `u64`).
#[derive(Clone, Copy)]
enum ScalarReflectKind {
    Enum,
    Flags,
}

#[derive(Clone, Copy)]
pub(super) struct ScalarReflectSpec {
    kind: ScalarReflectKind,
    on_bound: OnBoundTrait,
    trait_item: CompilerItem,
    meta_item: CompilerItem,
    type_name_item: CompilerItem,
    meta_method_item: CompilerItem,
    value_method_item: CompilerItem,
    from_method_item: CompilerItem,
    /// The scalar bridge type: `i32` (discriminant) or `u64` (bits).
    value_type: TypeId,
}

impl ScalarReflectSpec {
    pub(super) const ENUM: Self = Self {
        kind: ScalarReflectKind::Enum,
        on_bound: OnBoundTrait::ReflectEnum,
        trait_item: CompilerItem::ReflectEnum,
        meta_item: CompilerItem::EnumCaseMeta,
        type_name_item: CompilerItem::ReflectEnumTypeName,
        meta_method_item: CompilerItem::ReflectEnumCaseMeta,
        value_method_item: CompilerItem::ReflectEnumDiscriminant,
        from_method_item: CompilerItem::ReflectEnumFromDiscriminant,
        value_type: TypeTable::I32,
    };
    pub(super) const FLAGS: Self = Self {
        kind: ScalarReflectKind::Flags,
        on_bound: OnBoundTrait::ReflectFlags,
        trait_item: CompilerItem::ReflectFlags,
        meta_item: CompilerItem::FlagBitMeta,
        type_name_item: CompilerItem::ReflectFlagsTypeName,
        meta_method_item: CompilerItem::ReflectFlagsBitMeta,
        value_method_item: CompilerItem::ReflectFlagsBits,
        from_method_item: CompilerItem::ReflectFlagsFromBits,
        value_type: TypeTable::U64,
    };

    fn subject_matches(self, subject: &ResolvedType) -> bool {
        match self.kind {
            ScalarReflectKind::Enum => matches!(subject, ResolvedType::Enum { .. }),
            ScalarReflectKind::Flags => matches!(subject, ResolvedType::Flags { .. }),
        }
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Resolve a `Reflect::<T>::method()` trait-qualified static call to the
    /// synthesized `T^Reflect::method` and record the dispatch fact for reify.
    /// Self-contained: it does not go through the bare-`Type::method` static
    /// path, so struct namespaces are never polluted with `T::field_names()`.
    pub(super) fn resolve_reflect_static_call(
        &mut self,
        self_ty: TypeId,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let method = static_call.method.clone();

        // Generic subject `T: Reflect`: the concrete struct is unknown until
        // monomorphization. Resolve the value-free members here and record a
        // type-param-receiver dispatch that monomorphization redirects to the
        // concrete `Struct^Reflect::method`.
        let subject = self.tysys.type_table.borrow().get(self_ty).clone();
        if let crate::tir::ResolvedType::TypeParam { name, .. } = subject {
            return self.resolve_generic_reflect_static_call(self_ty, &name, static_call, ctx);
        }

        let self_name = self.tysys.type_table.borrow().type_name(self_ty);

        let Some(field_types) = self.reflect_subject_field_types(&self_name) else {
            let _ = self.emit(TypeError::UnknownFunction {
                name: format!("Reflect::<{self_name}>::{method}"),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        };

        let (reflect_trait_name, type_name_method, fields_method, field_tokens_method, module_source) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            (
                items
                    .trait_name(crate::compiler_item::CompilerItem::Reflect)
                    .to_string(),
                items
                    .method_name(crate::compiler_item::CompilerItem::ReflectTypeName)
                    .to_string(),
                items
                    .method_name(crate::compiler_item::CompilerItem::ReflectFields)
                    .to_string(),
                items
                    .method_name(crate::compiler_item::CompilerItem::ReflectFieldTokens)
                    .to_string(),
                self.find_struct_module_source(&self_name),
            )
        };

        let is_fields = method == fields_method;
        let is_field_tokens = method == field_tokens_method;
        let args_valid = if is_fields {
            self.check_reflect_fields_receiver(self_ty, &self_name, static_call, ctx)
        } else {
            self.reject_reflect_metadata_args(static_call, ctx)
        };
        if !args_valid {
            return TypeTable::ERROR;
        }

        self.tysys
            .type_table
            .borrow_mut()
            .record_bound_driven_synth_request(&self_name, &module_source, &reflect_trait_name);

        let return_type = if is_fields {
            self.tysys.type_table.borrow_mut().make_tuple(field_types)
        } else if is_field_tokens {
            let mut tt = self.tysys.type_table.borrow_mut();
            let (field_module, field_name) = {
                let items = tt.compiler_items();
                let (m, n) = items.require_struct(crate::compiler_item::CompilerItem::ReflectField);
                (m.clone(), n.to_string())
            };
            let tokens: Vec<TypeId> = field_types
                .into_iter()
                .map(|field_ty| {
                    tt.make_generic_instance(
                        field_name.clone(),
                        field_module.clone(),
                        vec![self_ty, field_ty],
                    )
                })
                .collect();
            tt.make_tuple(tokens)
        } else {
            let mut tt = self.tysys.type_table.borrow_mut();
            let string_type = tt.make_compiler_struct(crate::compiler_item::CompilerItem::String);
            if method == type_name_method {
                string_type
            } else {
                tt.make_list(string_type)
            }
        };

        let func_ref = FunctionRef {
            module_source,
            name: MethodName::format_local(&self_name, Some(&reflect_trait_name), &method),
            monomorph_info: None,
            method_info: Some(LocalMethodName::new(
                self_name.clone(),
                Some(reflect_trait_name.clone()),
                method.clone(),
            )),
        };
        self.sem.types.static_method_dispatch.insert(
            static_call.id,
            super::sem::types::StaticMethodDispatch {
                function_ref: func_ref,
                param_is_mut: if is_fields { vec![false] } else { Vec::new() },
                type_args: Vec::new(),
                param_defaults: Vec::new(),
            },
        );

        return_type
    }

    /// Resolve `Reflect::<T>::method()` where `T` is a generic type parameter
    /// (inside an `impl<T: Reflect<Fields = [..F]>, ..F: …>` derivation). The
    /// value-free members (`field_names` / `type_name`) resolve to their fixed
    /// return types; `fields(self)` resolves to the projected field pack `[..F]`
    /// read off `T`'s `Reflect<Fields = [..F]>` bound. Each is recorded as a
    /// type-param-receiver dispatch so monomorphization redirects it to the
    /// concrete struct's synthesized `Struct^Reflect::method`.
    fn resolve_generic_reflect_static_call(
        &mut self,
        self_ty: TypeId,
        type_param_name: &str,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        use crate::compiler_item::CompilerItem;
        let method = static_call.method.clone();
        let (
            reflect_trait_name,
            field_names_method,
            type_name_method,
            fields_method,
            field_tokens_method,
        ) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            (
                items.trait_name(CompilerItem::Reflect).to_string(),
                items
                    .method_name(CompilerItem::ReflectFieldNames)
                    .to_string(),
                items.method_name(CompilerItem::ReflectTypeName).to_string(),
                items.method_name(CompilerItem::ReflectFields).to_string(),
                items
                    .method_name(CompilerItem::ReflectFieldTokens)
                    .to_string(),
            )
        };

        let is_fields = method == fields_method;
        let return_type = if method == type_name_method {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_struct(CompilerItem::String)
        } else if method == field_names_method {
            let mut tt = self.tysys.type_table.borrow_mut();
            let string_type = tt.make_compiler_struct(CompilerItem::String);
            tt.make_list(string_type)
        } else if is_fields {
            // `fields(self)` returns `Self::Fields = [..F]`. Read the pack off
            // `T`'s `Reflect<Fields = [..F]>` bound and resolve it in the current
            // scope, where `F` is the projected pack registered by `resolve_method`.
            let Some(fields_ty) = self.reflect_pack_bound_ty(
                type_param_name,
                &reflect_trait_name,
                crate::synthesis::traits::REFLECT_FIELDS_ASSOC,
            ) else {
                let _ = self.emit(TypeError::UnknownFunction {
                    name: format!(
                        "Reflect::<{type_param_name}>::{method} (no `Fields = [..F]` bound on {type_param_name})"
                    ),
                    span: static_call.span,
                });
                return TypeTable::ERROR;
            };
            fields_ty
        } else if method == field_tokens_method {
            // `field_tokens()` returns the constructor-mapped token pack
            // `[..Field<T, F>]` read off `T`'s `Fields = [..F]` bound.
            let Some(tokens_ty) = self.field_tokens_bound_ty(
                self_ty,
                type_param_name,
                &reflect_trait_name,
            ) else {
                let _ = self.emit(TypeError::UnknownFunction {
                    name: format!(
                        "Reflect::<{type_param_name}>::{method} (no `Fields = [..F]` bound on {type_param_name})"
                    ),
                    span: static_call.span,
                });
                return TypeTable::ERROR;
            };
            tokens_ty
        } else {
            let _ = self.emit(TypeError::UnknownFunction {
                name: format!("Reflect::<{type_param_name}>::{method}"),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        };

        if is_fields {
            // `fields` takes the subject as its sole receiver argument; resolve
            // it for its fact-recording side effects.
            for arg in &static_call.args {
                self.resolve_expr(arg, ctx, None);
            }
        } else if !self.reject_reflect_metadata_args(static_call, ctx) {
            return TypeTable::ERROR;
        }

        let mut method_info = LocalMethodName::new(
            type_param_name.to_string(),
            Some(reflect_trait_name),
            method,
        );
        method_info.is_type_param_receiver = true;
        let mangled_name = method_info.to_mangled_name();

        let func_ref = FunctionRef {
            module_source: self.current_module_source.clone(),
            name: mangled_name,
            monomorph_info: None,
            method_info: Some(method_info),
        };
        self.sem.types.static_method_dispatch.insert(
            static_call.id,
            super::sem::types::StaticMethodDispatch {
                function_ref: func_ref,
                param_is_mut: if is_fields { vec![false] } else { Vec::new() },
                type_args: Vec::new(),
                param_defaults: Vec::new(),
            },
        );

        return_type
    }

    /// The `Assoc = [..F]` pack binding on `T`'s bound of the given trait
    /// (`Fields` on `Reflect`, `Cases` on `ReflectVariant`), resolved in the
    /// current scope (where `F` is the projected pack). `None` when `T`
    /// carries no such bound.
    fn reflect_pack_bound_ty(
        &mut self,
        type_param_name: &str,
        reflect_trait_name: &str,
        assoc_name: &str,
    ) -> Option<TypeId> {
        let pack_ast = self
            .annotate_ctx
            .trait_ctx
            .type_param_bounds
            .get(type_param_name)?
            .iter()
            .filter(|b| b.name == reflect_trait_name)
            .flat_map(|b| &b.assoc_types)
            .filter(|assoc| assoc.name == assoc_name)
            .find_map(|assoc| match &assoc.ty {
                ast::Type::Tuple(elems)
                    if elems
                        .iter()
                        .any(|e| matches!(e, ast::Type::TypePackSpread(..))) =>
                {
                    Some(assoc.ty.clone())
                }
                _ => None,
            })?;
        Some(self.resolve_type(&pack_ast))
    }

    /// Field types of the struct `Reflect::<T>` targets, in declaration order;
    /// `None` when `T` is not a struct (the only reflectable kind).
    fn reflect_subject_field_types(&self, self_name: &str) -> Option<Vec<TypeId>> {
        self.type_lookup()
            .struct_fields(self_name)
            .map(|info| info.fields.iter().map(|(_, ty, _)| *ty).collect())
    }

    /// Resolve and type-check the sole receiver of `Reflect::<T>::fields`: one
    /// argument whose type, refs peeled, is the subject struct. Returns whether
    /// it is well-formed, emitting the diagnostic otherwise.
    fn check_reflect_fields_receiver(
        &mut self,
        self_ty: TypeId,
        self_name: &str,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> bool {
        let ref_self_ty = self.tysys.type_table.borrow_mut().make_ref(self_ty);
        let arg_types: Vec<TypeId> = static_call
            .args
            .iter()
            .map(|arg| self.resolve_expr(arg, ctx, Some(ref_self_ty)))
            .collect();
        if arg_types.len() != 1 {
            let _ = self.emit(TypeError::ArgumentCountMismatch {
                expected: 1,
                found: arg_types.len(),
                span: static_call.span,
            });
            return false;
        }
        let arg_ty = arg_types[0];
        let peeled = self.tysys.type_table.borrow().peel_refs(arg_ty);
        if arg_ty != TypeTable::ERROR && peeled != self_ty {
            let found = self.tysys.type_table.borrow().type_name(peeled);
            let _ = self.emit(TypeError::TypeMismatch {
                expected: self_name.to_string(),
                found,
                span: static_call.span,
            });
            return false;
        }
        true
    }

    /// Resolve the args of a no-argument `Reflect` metadata call and reject any
    /// that were supplied. Returns whether the call is well-formed.
    fn reject_reflect_metadata_args(
        &mut self,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> bool {
        for arg in &static_call.args {
            self.resolve_expr(arg, ctx, None);
        }
        if static_call.args.is_empty() {
            return true;
        }
        let _ = self.emit(TypeError::ArgumentCountMismatch {
            expected: 0,
            found: static_call.args.len(),
            span: static_call.span,
        });
        false
    }

    /// Whether `prefix::method` names a `Reflect` trait-qualified static call
    /// (`Reflect::<T>::field_names` / `type_name`). `prefix` must resolve to the
    /// compiler's `Reflect` trait *in this scope* — `classify_on_bound_trait`
    /// applies the same module check `on_bound` dispatch uses, so a user type or
    /// trait that happens to be named `Reflect` is not hijacked. `method` is
    /// matched through the compiler-item registry so a stdlib rename flows through.
    pub(super) fn is_reflect_trait_call(&self, prefix: &str, method: &str) -> bool {
        if self
            .tysys
            .classify_on_bound_trait(&self.type_lookup(), prefix)
            != Some(super::trait_query::OnBoundTrait::Reflect)
        {
            return false;
        }
        let tt = self.tysys.type_table.borrow();
        let items = tt.compiler_items();
        method == items.method_name(crate::compiler_item::CompilerItem::ReflectFieldNames)
            || method == items.method_name(crate::compiler_item::CompilerItem::ReflectTypeName)
            || method == items.method_name(crate::compiler_item::CompilerItem::ReflectFields)
            || method == items.method_name(crate::compiler_item::CompilerItem::ReflectFieldTokens)
    }

    /// Whether `prefix::method` names a `ReflectVariant` trait-qualified static
    /// call. Same scope discipline as [`Self::is_reflect_trait_call`].
    pub(super) fn is_reflect_variant_trait_call(&self, prefix: &str, method: &str) -> bool {
        if self
            .tysys
            .classify_on_bound_trait(&self.type_lookup(), prefix)
            != Some(super::trait_query::OnBoundTrait::ReflectVariant)
        {
            return false;
        }
        let tt = self.tysys.type_table.borrow();
        let items = tt.compiler_items();
        method == items.method_name(crate::compiler_item::CompilerItem::ReflectVariantTypeName)
            || method
                == items.method_name(crate::compiler_item::CompilerItem::ReflectVariantCaseMeta)
            || method
                == items.method_name(crate::compiler_item::CompilerItem::ReflectVariantDiscriminant)
            || method == items.method_name(crate::compiler_item::CompilerItem::ReflectVariantCases)
    }

    /// Resolve a `ReflectVariant::<T>::method()` trait-qualified static call to
    /// the synthesized `T^ReflectVariant::method` and record the dispatch fact
    /// for reify. The variant analog of [`Self::resolve_reflect_static_call`].
    pub(super) fn resolve_reflect_variant_static_call(
        &mut self,
        self_ty: TypeId,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        use crate::compiler_item::CompilerItem;
        let method = static_call.method.clone();

        // Generic subject `T: ReflectVariant`: the concrete variant is unknown
        // until monomorphization; mirror `resolve_reflect_static_call`.
        let subject = self.tysys.type_table.borrow().get(self_ty).clone();
        if let crate::tir::ResolvedType::TypeParam { name, .. } = subject {
            return self.resolve_generic_reflect_variant_static_call(
                self_ty,
                &name,
                static_call,
                ctx,
            );
        }

        let self_name = self.tysys.type_table.borrow().type_name(self_ty);
        if !matches!(subject, crate::tir::ResolvedType::Variant { .. }) {
            let _ = self.emit(TypeError::UnknownFunction {
                name: format!("ReflectVariant::<{self_name}>::{method}"),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        }

        let (trait_name, discriminant_method, case_meta_method, cases_method, module_source) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            (
                items.trait_name(CompilerItem::ReflectVariant).to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantDiscriminant)
                    .to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantCaseMeta)
                    .to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantCases)
                    .to_string(),
                self.find_struct_module_source(&self_name),
            )
        };

        let is_discriminant = method == discriminant_method;
        let args_valid = if is_discriminant {
            self.check_reflect_fields_receiver(self_ty, &self_name, static_call, ctx)
        } else {
            self.reject_reflect_metadata_args(static_call, ctx)
        };
        if !args_valid {
            return TypeTable::ERROR;
        }

        self.tysys
            .type_table
            .borrow_mut()
            .record_bound_driven_synth_request(&self_name, &module_source, &trait_name);

        let return_type = if is_discriminant {
            TypeTable::I32
        } else if method == cases_method {
            let payloads: Vec<TypeId> = self
                .lookup_variant_case(&self_name)
                .map(|info| info.cases.iter().map(|c| c.payload).collect())
                .unwrap_or_default();
            let mut tt = self.tysys.type_table.borrow_mut();
            let (case_module, case_name) = {
                let items = tt.compiler_items();
                let (m, n) = items.require_struct(CompilerItem::ReflectVariantCase);
                (m.clone(), n.to_string())
            };
            let tokens: Vec<TypeId> = payloads
                .into_iter()
                .map(|payload| {
                    tt.make_generic_instance(
                        case_name.clone(),
                        case_module.clone(),
                        vec![self_ty, payload],
                    )
                })
                .collect();
            tt.make_tuple(tokens)
        } else {
            let mut tt = self.tysys.type_table.borrow_mut();
            if method == case_meta_method {
                let meta_type = tt.make_compiler_struct(CompilerItem::VariantCaseMeta);
                tt.make_list(meta_type)
            } else {
                tt.make_compiler_struct(CompilerItem::String)
            }
        };

        let func_ref = FunctionRef {
            module_source,
            name: MethodName::format_local(&self_name, Some(&trait_name), &method),
            monomorph_info: None,
            method_info: Some(LocalMethodName::new(
                self_name.clone(),
                Some(trait_name.clone()),
                method.clone(),
            )),
        };
        self.sem.types.static_method_dispatch.insert(
            static_call.id,
            super::sem::types::StaticMethodDispatch {
                function_ref: func_ref,
                param_is_mut: if is_discriminant {
                    vec![false]
                } else {
                    Vec::new()
                },
                type_args: Vec::new(),
                param_defaults: Vec::new(),
            },
        );

        return_type
    }

    /// Resolve `ReflectVariant::<T>::method()` where `T` is a type parameter
    /// (inside an `impl<T: ReflectVariant<Cases = [..P]>, ..P: …>` derivation).
    /// The value-free members resolve to their fixed return types; `cases()`
    /// resolves to the constructor-mapped token pack `[..Case<T, P>]` read off
    /// `T`'s `Cases = [..P]` bound (WEP 2026-06-13 §3f). Each is recorded as a
    /// type-param-receiver dispatch so monomorphization redirects it to the
    /// concrete variant's synthesized `V^ReflectVariant::method`.
    fn resolve_generic_reflect_variant_static_call(
        &mut self,
        self_ty: TypeId,
        type_param_name: &str,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        use crate::compiler_item::CompilerItem;
        let method = static_call.method.clone();
        let (trait_name, type_name_method, case_meta_method, discriminant_method, cases_method) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            (
                items.trait_name(CompilerItem::ReflectVariant).to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantTypeName)
                    .to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantCaseMeta)
                    .to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantDiscriminant)
                    .to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantCases)
                    .to_string(),
            )
        };

        let is_discriminant = method == discriminant_method;
        let return_type = if method == type_name_method {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_struct(CompilerItem::String)
        } else if method == case_meta_method {
            let mut tt = self.tysys.type_table.borrow_mut();
            let meta_type = tt.make_compiler_struct(CompilerItem::VariantCaseMeta);
            tt.make_list(meta_type)
        } else if is_discriminant {
            TypeTable::I32
        } else if method == cases_method {
            let Some(tokens_ty) = self.case_tokens_bound_ty(self_ty, type_param_name, &trait_name)
            else {
                let _ = self.emit(TypeError::UnknownFunction {
                    name: format!(
                        "ReflectVariant::<{type_param_name}>::{method} (no `Cases = [..P]` bound on {type_param_name})"
                    ),
                    span: static_call.span,
                });
                return TypeTable::ERROR;
            };
            tokens_ty
        } else {
            unreachable!("is_reflect_variant_trait_call admits only the four trait methods")
        };

        let args_valid = if is_discriminant {
            self.check_reflect_fields_receiver(self_ty, type_param_name, static_call, ctx)
        } else {
            self.reject_reflect_metadata_args(static_call, ctx)
        };
        if !args_valid {
            return TypeTable::ERROR;
        }

        let mut method_info =
            LocalMethodName::new(type_param_name.to_string(), Some(trait_name), method);
        method_info.is_type_param_receiver = true;
        let mangled_name = method_info.to_mangled_name();

        let func_ref = FunctionRef {
            module_source: self.current_module_source.clone(),
            name: mangled_name,
            monomorph_info: None,
            method_info: Some(method_info),
        };
        self.sem.types.static_method_dispatch.insert(
            static_call.id,
            super::sem::types::StaticMethodDispatch {
                function_ref: func_ref,
                param_is_mut: if is_discriminant {
                    vec![false]
                } else {
                    Vec::new()
                },
                type_args: Vec::new(),
                param_defaults: Vec::new(),
            },
        );

        return_type
    }

    /// The constructor-mapped token pack `[..Case<T, P>]` — the type of
    /// `cases()` under a `T: ReflectVariant<Cases = [..P]>` bound. The pack
    /// param recurs inside the mapped element as a scalar placeholder, so
    /// per-element substitution binds it to `P_k`. `None` when `T` carries no
    /// `Cases` pack bound.
    fn case_tokens_bound_ty(
        &mut self,
        self_ty: TypeId,
        type_param_name: &str,
        trait_name: &str,
    ) -> Option<TypeId> {
        use crate::compiler_item::CompilerItem;
        let cases_ty = self.reflect_pack_bound_ty(
            type_param_name,
            trait_name,
            crate::synthesis::traits::REFLECT_CASES_ASSOC,
        )?;
        let mut tt = self.tysys.type_table.borrow_mut();
        let elems = tt.as_tuple(cases_ty)?;
        let (pack_name, pack_index) = elems.iter().find_map(|&e| match tt.get(e) {
            crate::tir::ResolvedType::TypePack {
                name,
                index,
                mapped_elem: None,
            } => Some((name.clone(), *index)),
            _ => None,
        })?;
        let (case_module, case_name) = {
            let items = tt.compiler_items();
            let (m, n) = items.require_struct(CompilerItem::ReflectVariantCase);
            (m.clone(), n.to_string())
        };
        let elem_param = tt.make_type_param(pack_name.clone(), pack_index);
        let token = tt.make_generic_instance(case_name, case_module, vec![self_ty, elem_param]);
        let token_pack = tt.make_mapped_type_pack(pack_name, pack_index, token);
        Some(tt.make_tuple(vec![token_pack]))
    }

    /// The constructor-mapped token pack `[..Field<T, F>]` — the type of
    /// `field_tokens()` under a `T: Reflect<Fields = [..F]>` bound. The variant
    /// analog is [`Self::case_tokens_bound_ty`]; both map the projected element
    /// pack through a token constructor. `None` when `T` carries no `Fields`
    /// pack bound.
    fn field_tokens_bound_ty(
        &mut self,
        self_ty: TypeId,
        type_param_name: &str,
        reflect_trait_name: &str,
    ) -> Option<TypeId> {
        use crate::compiler_item::CompilerItem;
        let fields_ty = self.reflect_pack_bound_ty(
            type_param_name,
            reflect_trait_name,
            crate::synthesis::traits::REFLECT_FIELDS_ASSOC,
        )?;
        let mut tt = self.tysys.type_table.borrow_mut();
        let elems = tt.as_tuple(fields_ty)?;
        let (pack_name, pack_index) = elems.iter().find_map(|&e| match tt.get(e) {
            crate::tir::ResolvedType::TypePack {
                name,
                index,
                mapped_elem: None,
            } => Some((name.clone(), *index)),
            _ => None,
        })?;
        let (field_module, field_name) = {
            let items = tt.compiler_items();
            let (m, n) = items.require_struct(CompilerItem::ReflectField);
            (m.clone(), n.to_string())
        };
        let elem_param = tt.make_type_param(pack_name.clone(), pack_index);
        let token = tt.make_generic_instance(field_name, field_module, vec![self_ty, elem_param]);
        let token_pack = tt.make_mapped_type_pack(pack_name, pack_index, token);
        Some(tt.make_tuple(vec![token_pack]))
    }

    /// Whether `prefix::method` names a member of the scalar-kind reflection
    /// trait `spec` describes (`ReflectEnum` / `ReflectFlags`). Same scope
    /// discipline as [`Self::is_reflect_trait_call`].
    pub(super) fn is_reflect_scalar_trait_call(
        &self,
        spec: ScalarReflectSpec,
        prefix: &str,
        method: &str,
    ) -> bool {
        if self
            .tysys
            .classify_on_bound_trait(&self.type_lookup(), prefix)
            != Some(spec.on_bound)
        {
            return false;
        }
        let tt = self.tysys.type_table.borrow();
        let items = tt.compiler_items();
        [
            spec.type_name_item,
            spec.meta_method_item,
            spec.value_method_item,
            spec.from_method_item,
        ]
        .into_iter()
        .any(|item| method == items.method_name(item))
    }

    /// Resolve a `ReflectEnum` / `ReflectFlags` `::<T>::method()` static call to
    /// the synthesized `T^Trait::method` and record the dispatch fact for reify
    /// (WEP 2026-06-13 §3b / §3c). Both kinds share the same four-member shape;
    /// `spec` supplies the per-kind items and scalar value type. A type-param
    /// subject routes to [`Self::resolve_generic_reflect_scalar_static_call`].
    pub(super) fn resolve_reflect_scalar_static_call(
        &mut self,
        spec: ScalarReflectSpec,
        self_ty: TypeId,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let method = static_call.method.clone();

        let subject = self.tysys.type_table.borrow().get(self_ty).clone();
        if let ResolvedType::TypeParam { name, .. } = subject {
            return self.resolve_generic_reflect_scalar_static_call(
                spec,
                self_ty,
                &name,
                static_call,
                ctx,
            );
        }

        let (trait_name, self_name) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.compiler_items().trait_name(spec.trait_item).to_string(),
                tt.type_name(self_ty),
            )
        };
        if !spec.subject_matches(&subject) {
            let _ = self.emit(TypeError::UnknownFunction {
                name: format!("{trait_name}::<{self_name}>::{method}"),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        }

        let module_source = self.find_struct_module_source(&self_name);
        let Some(return_type) =
            self.check_reflect_scalar_args(spec, self_ty, &self_name, &method, static_call, ctx)
        else {
            return TypeTable::ERROR;
        };

        self.tysys
            .type_table
            .borrow_mut()
            .record_bound_driven_synth_request(&self_name, &module_source, &trait_name);

        let param_is_mut = self.reflect_scalar_param_is_mut(spec, &method);
        let func_ref = FunctionRef {
            module_source,
            name: MethodName::format_local(&self_name, Some(&trait_name), &method),
            monomorph_info: None,
            method_info: Some(LocalMethodName::new(
                self_name.clone(),
                Some(trait_name.clone()),
                method.clone(),
            )),
        };
        self.sem.types.static_method_dispatch.insert(
            static_call.id,
            super::sem::types::StaticMethodDispatch {
                function_ref: func_ref,
                param_is_mut,
                type_args: Vec::new(),
                param_defaults: Vec::new(),
            },
        );

        return_type
    }

    /// Resolve a scalar-kind `Trait::<T>::method()` where `T` is a type
    /// parameter (inside an `impl<T: Trait> …` derivation). Each member is
    /// recorded as a type-param-receiver dispatch so monomorphization redirects
    /// it to the concrete type's synthesized `S^Trait::method`.
    fn resolve_generic_reflect_scalar_static_call(
        &mut self,
        spec: ScalarReflectSpec,
        self_ty: TypeId,
        type_param_name: &str,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let method = static_call.method.clone();
        let trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_items()
            .trait_name(spec.trait_item)
            .to_string();

        let Some(return_type) = self.check_reflect_scalar_args(
            spec,
            self_ty,
            type_param_name,
            &method,
            static_call,
            ctx,
        ) else {
            return TypeTable::ERROR;
        };

        let param_is_mut = self.reflect_scalar_param_is_mut(spec, &method);
        let mut method_info =
            LocalMethodName::new(type_param_name.to_string(), Some(trait_name), method);
        method_info.is_type_param_receiver = true;
        let mangled_name = method_info.to_mangled_name();

        let func_ref = FunctionRef {
            module_source: self.current_module_source.clone(),
            name: mangled_name,
            monomorph_info: None,
            method_info: Some(method_info),
        };
        self.sem.types.static_method_dispatch.insert(
            static_call.id,
            super::sem::types::StaticMethodDispatch {
                function_ref: func_ref,
                param_is_mut,
                type_args: Vec::new(),
                param_defaults: Vec::new(),
            },
        );

        return_type
    }

    /// Validate a scalar-kind member call's arguments and compute its return
    /// type (shared by the concrete and generic resolvers; `self_ty` is the
    /// subject or the type param). `None` when ill-formed (diagnostic already
    /// emitted). `type_name`/`<meta>` reject args; `<value>(&self)` returns the
    /// scalar type; `from_<value>(raw)` takes one scalar arg and returns
    /// `Option<Self>`.
    fn check_reflect_scalar_args(
        &mut self,
        spec: ScalarReflectSpec,
        self_ty: TypeId,
        self_name: &str,
        method: &str,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> Option<TypeId> {
        let (type_name_method, meta_method, value_method, from_method) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            (
                items.method_name(spec.type_name_item).to_string(),
                items.method_name(spec.meta_method_item).to_string(),
                items.method_name(spec.value_method_item).to_string(),
                items.method_name(spec.from_method_item).to_string(),
            )
        };

        if *method == type_name_method {
            self.reject_reflect_metadata_args(static_call, ctx)
                .then(|| {
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_compiler_struct(CompilerItem::String)
                })
        } else if *method == meta_method {
            self.reject_reflect_metadata_args(static_call, ctx)
                .then(|| {
                    let mut tt = self.tysys.type_table.borrow_mut();
                    let meta_type = tt.make_compiler_struct(spec.meta_item);
                    tt.make_list(meta_type)
                })
        } else if *method == value_method {
            self.check_reflect_fields_receiver(self_ty, self_name, static_call, ctx)
                .then_some(spec.value_type)
        } else if *method == from_method {
            let arg_types: Vec<TypeId> = static_call
                .args
                .iter()
                .map(|arg| self.resolve_expr(arg, ctx, Some(spec.value_type)))
                .collect();
            if arg_types.len() != 1 {
                let _ = self.emit(TypeError::ArgumentCountMismatch {
                    expected: 1,
                    found: arg_types.len(),
                    span: static_call.span,
                });
                return None;
            }
            if arg_types[0] != TypeTable::ERROR && arg_types[0] != spec.value_type {
                let (expected, found) = {
                    let tt = self.tysys.type_table.borrow();
                    (tt.type_name(spec.value_type), tt.type_name(arg_types[0]))
                };
                let _ = self.emit(TypeError::TypeMismatch {
                    expected,
                    found,
                    span: static_call.span,
                });
                return None;
            }
            Some(self.tysys.type_table.borrow_mut().make_option(self_ty))
        } else {
            unreachable!("is_reflect_scalar_trait_call admits only the four trait methods")
        }
    }

    /// Per-parameter mutability of a scalar-kind member's dispatch record:
    /// `<value>(&self)` and `from_<value>(raw)` take one non-mut argument; the
    /// metadata members take none.
    fn reflect_scalar_param_is_mut(&self, spec: ScalarReflectSpec, method: &str) -> Vec<bool> {
        let tt = self.tysys.type_table.borrow();
        let items = tt.compiler_items();
        if method == items.method_name(spec.value_method_item)
            || method == items.method_name(spec.from_method_item)
        {
            vec![false]
        } else {
            Vec::new()
        }
    }
}
