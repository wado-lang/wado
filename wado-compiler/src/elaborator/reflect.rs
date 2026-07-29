//! `ReflectStruct` / `ReflectVariant` / `ReflectEnum` / `ReflectFlags` static-call
//! resolution (WEP 2026-06-13): the `Trait::<T>::method()` form that
//! `resolve_static_method_call` intercepts and routes to the type's synthesized
//! `T^Trait::method`.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::name::{FqTypeName, LocalMethodName, MethodName};
use crate::tir::{FunctionRef, ResolvedType, TypeId, TypeTable};

use super::Elaborator;
use super::trait_query::OnBoundTrait;
use super::types::{FunctionContext, TypeError};

/// The two scalar-kind reflection traits share one static-call resolution shape
/// (WEP 2026-06-13 §3b / §3c): four members — `type_name` / `<value>(&self)` /
/// `from_<value>(raw)` / `members()` — differing only in the compiler items they
/// name, the subject kind, and the scalar value type (`i32` / `u64`).
#[derive(Clone, Copy)]
enum ScalarReflectKind {
    Enum,
    Flags,
}

/// A resolved reflection subject: the declared type name, the instantiation's
/// type args (empty for a plain type), and its members' types with those args
/// substituted.
struct ReflectSubject {
    base_name: String,
    /// The declaring module, carried from the resolved subject. Re-deriving it
    /// from `base_name` picks whichever same-named declaration a name lookup
    /// reaches first, so `P2<i32>` would reflect `p1`'s `Pair`.
    module_source: crate::module_source::ModuleSource,
    type_args: Vec<TypeId>,
    member_types: Vec<TypeId>,
}

#[derive(Clone, Copy)]
pub(super) struct ScalarReflectSpec {
    kind: ScalarReflectKind,
    on_bound: OnBoundTrait,
    trait_item: CompilerItem,
    type_name_item: CompilerItem,
    value_method_item: CompilerItem,
    from_method_item: CompilerItem,
    members_method_item: CompilerItem,
    wire_name_policy_item: CompilerItem,
    member_struct_item: CompilerItem,
    /// The `Members` associated-type name, read off a generic derivation's
    /// bound to source the member pack's arity.
    members_assoc: &'static str,
    /// The scalar bridge type: `i32` (discriminant) or `u64` (bits).
    value_type: TypeId,
}

impl ScalarReflectSpec {
    pub(super) const ENUM: Self = Self {
        kind: ScalarReflectKind::Enum,
        on_bound: OnBoundTrait::ReflectEnum,
        trait_item: CompilerItem::ReflectEnum,
        type_name_item: CompilerItem::ReflectEnumTypeName,
        value_method_item: CompilerItem::ReflectEnumDiscriminant,
        from_method_item: CompilerItem::ReflectEnumFromDiscriminant,
        members_method_item: CompilerItem::ReflectEnumMembers,
        wire_name_policy_item: CompilerItem::ReflectEnumWireNamePolicy,
        member_struct_item: CompilerItem::ReflectEnumCase,
        members_assoc: crate::synthesis::traits::REFLECT_MEMBERS_ASSOC,
        value_type: TypeTable::I32,
    };
    pub(super) const FLAGS: Self = Self {
        kind: ScalarReflectKind::Flags,
        on_bound: OnBoundTrait::ReflectFlags,
        trait_item: CompilerItem::ReflectFlags,
        type_name_item: CompilerItem::ReflectFlagsTypeName,
        value_method_item: CompilerItem::ReflectFlagsBits,
        from_method_item: CompilerItem::ReflectFlagsFromBits,
        members_method_item: CompilerItem::ReflectFlagsMembers,
        wire_name_policy_item: CompilerItem::ReflectFlagsWireNamePolicy,
        member_struct_item: CompilerItem::ReflectFlagsBit,
        members_assoc: crate::synthesis::traits::REFLECT_MEMBERS_ASSOC,
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
    /// Resolve a `ReflectStruct::<T>::method()` trait-qualified static call to the
    /// synthesized `T^ReflectStruct::method` and record the dispatch fact for reify.
    /// Self-contained: it does not go through the bare-`Type::method` static
    /// path, so struct namespaces are never polluted with `T::members()`.
    pub(super) fn resolve_reflect_static_call(
        &mut self,
        self_ty: TypeId,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let method = static_call.method.clone();

        // Generic subject `T: ReflectStruct`: the concrete struct is unknown until
        // monomorphization. Resolve the value-free members here and record a
        // type-param-receiver dispatch that monomorphization redirects to the
        // concrete `Struct^ReflectStruct::method`.
        let subject = self.tysys.type_table.borrow().get(self_ty).clone();
        if let crate::tir::ResolvedType::TypeParam { name, .. } = subject {
            return self.resolve_generic_reflect_static_call(self_ty, &name, static_call, ctx);
        }

        let self_name = self.tysys.type_table.borrow().type_name(self_ty);

        let Some(subject) = self.reflect_struct_subject(self_ty) else {
            let _ = self.emit(TypeError::UnknownFunction {
                name: format!("ReflectStruct::<{self_name}>::{method}"),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        };
        let ReflectSubject {
            base_name: self_name,
            module_source,
            type_args,
            member_types: field_types,
        } = subject;

        let (reflect_trait_name, members_method, from_fields_method, wire_name_policy_method) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            (
                items
                    .trait_name(crate::compiler_item::CompilerItem::ReflectStruct)
                    .to_string(),
                items
                    .method_name(crate::compiler_item::CompilerItem::ReflectStructMembers)
                    .to_string(),
                items
                    .method_name(crate::compiler_item::CompilerItem::ReflectStructFromFields)
                    .to_string(),
                items
                    .method_name(crate::compiler_item::CompilerItem::ReflectStructWireNamePolicy)
                    .to_string(),
            )
        };

        let well_formed = if method == from_fields_method {
            self.check_reflect_from_fields_arg(&field_types, static_call, ctx)
        } else {
            self.reject_reflect_metadata_args(static_call, ctx)
        };
        if !well_formed {
            return TypeTable::ERROR;
        }

        self.tysys
            .type_table
            .borrow_mut()
            .record_bound_driven_synth_request(&self_name, &module_source, &reflect_trait_name);

        let return_type = if method == from_fields_method {
            self_ty
        } else if method == members_method {
            let mut tt = self.tysys.type_table.borrow_mut();
            let (field_module, field_name) = {
                let items = tt.compiler_items();
                let (m, n) =
                    items.require_struct(crate::compiler_item::CompilerItem::ReflectStructField);
                (m.clone(), n.to_string())
            };
            let members: Vec<TypeId> = field_types
                .into_iter()
                .map(|field_ty| {
                    tt.make_generic_instance(
                        field_name.clone(),
                        field_module.clone(),
                        vec![self_ty, field_ty],
                    )
                })
                .collect();
            tt.make_tuple(members)
        } else if method == wire_name_policy_method {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_enum(crate::compiler_item::CompilerItem::CaseStyle)
        } else {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_struct(crate::compiler_item::CompilerItem::String)
        };

        let func_ref = self.reflect_func_ref(
            &self_name,
            &type_args,
            &reflect_trait_name,
            &method,
            module_source,
        );
        self.sem.types.static_method_dispatch.insert(
            static_call.id,
            super::sem::types::StaticMethodDispatch {
                function_ref: func_ref,
                param_is_mut: Vec::new(),
                type_args: Vec::new(),
                param_defaults: Vec::new(),
            },
        );

        return_type
    }

    /// Build the `FunctionRef` targeting a reflect subject's synthesized
    /// `Base^Trait::method`. A generic instance carries the instantiation in
    /// `monomorph_info` and in the mangled name, so monomorphization picks the
    /// instance whose type args match; `type_args` is empty for a plain type.
    fn reflect_func_ref(
        &self,
        base_name: &str,
        type_args: &[TypeId],
        trait_name: &str,
        method: &str,
        module_source: crate::module_source::ModuleSource,
    ) -> FunctionRef {
        let mut method_info = LocalMethodName::new(
            FqTypeName::declared(&module_source, base_name),
            Some(trait_name.to_string()),
            method.to_string(),
        );
        let monomorph_info = if type_args.is_empty() {
            None
        } else {
            let arg_names: Vec<FqTypeName> = type_args
                .iter()
                .map(|t| self.tysys.type_table.borrow().fq_type_name(*t))
                .collect();
            method_info = method_info.with_type_args(&arg_names, &[]);
            Some(crate::tir::MonomorphInfo {
                generic_name: base_name.to_string(),
                impl_type_args: type_args.to_vec(),
                method_type_args: Vec::new(),
                is_blanket: false,
            })
        };
        FunctionRef {
            module_source,
            name: method_info.to_mangled_name(),
            monomorph_info,
            method_info: Some(method_info),
        }
    }

    /// Resolve `ReflectStruct::<T>::method()` where `T` is a generic type parameter
    /// (inside an `impl<T: ReflectStruct<FieldTypes = [..F]>, ..F: …>` derivation). The
    /// value-free members (`type_name` / `wire_name_policy`) resolve to their
    /// fixed return types; `members()` resolves to the constructor-mapped
    /// member pack `[..StructField<T, F>]` read off `T`'s `ReflectStruct<FieldTypes = [..F]>`
    /// bound. Each is recorded as a type-param-receiver dispatch so
    /// monomorphization redirects it to the concrete struct's synthesized
    /// `Struct^ReflectStruct::method`.
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
            type_name_method,
            members_method,
            from_fields_method,
            wire_name_policy_method,
        ) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            (
                items.trait_name(CompilerItem::ReflectStruct).to_string(),
                items
                    .method_name(CompilerItem::ReflectStructTypeName)
                    .to_string(),
                items
                    .method_name(CompilerItem::ReflectStructMembers)
                    .to_string(),
                items
                    .method_name(CompilerItem::ReflectStructFromFields)
                    .to_string(),
                items
                    .method_name(CompilerItem::ReflectStructWireNamePolicy)
                    .to_string(),
            )
        };

        if method == from_fields_method {
            return self.resolve_generic_reflect_from_fields(
                self_ty,
                type_param_name,
                &reflect_trait_name,
                static_call,
                ctx,
            );
        }

        let return_type = if method == type_name_method {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_struct(CompilerItem::String)
        } else if method == wire_name_policy_method {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_enum(CompilerItem::CaseStyle)
        } else if method == members_method {
            let Some(members_ty) =
                self.struct_members_bound_ty(self_ty, type_param_name, &reflect_trait_name)
            else {
                let _ = self.emit(TypeError::UnknownFunction {
                    name: format!(
                        "ReflectStruct::<{type_param_name}>::{method} (no `FieldTypes = [..F]` bound on {type_param_name})"
                    ),
                    span: static_call.span,
                });
                return TypeTable::ERROR;
            };
            members_ty
        } else {
            let _ = self.emit(TypeError::UnknownFunction {
                name: format!("ReflectStruct::<{type_param_name}>::{method}"),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        };

        if !self.reject_reflect_metadata_args(static_call, ctx) {
            return TypeTable::ERROR;
        }

        self.record_type_param_reflect_dispatch(
            type_param_name,
            reflect_trait_name,
            method,
            static_call,
        );

        return_type
    }

    /// Resolve `ReflectStruct::<T>::from_fields(fields)` on a generic subject: the
    /// argument is checked against `T`'s `FieldTypes = [..F]` pack binding and the
    /// call yields `T` itself.
    fn resolve_generic_reflect_from_fields(
        &mut self,
        self_ty: TypeId,
        type_param_name: &str,
        reflect_trait_name: &str,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let method = static_call.method.clone();
        let Some(fields_ty) = self.reflect_pack_bound_ty(
            type_param_name,
            reflect_trait_name,
            crate::synthesis::traits::REFLECT_FIELD_TYPES_ASSOC,
        ) else {
            let _ = self.emit(TypeError::UnknownFunction {
                name: format!(
                    "ReflectStruct::<{type_param_name}>::{method} (no `FieldTypes = [..F]` bound on {type_param_name})"
                ),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        };

        let arg_types: Vec<TypeId> = static_call
            .args
            .iter()
            .map(|arg| self.resolve_expr(arg, ctx, Some(fields_ty)))
            .collect();
        if arg_types.len() != 1 {
            let _ = self.emit(TypeError::ArgumentCountMismatch {
                expected: 1,
                found: arg_types.len(),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        }

        self.record_type_param_reflect_dispatch(
            type_param_name,
            reflect_trait_name.to_string(),
            method,
            static_call,
        );

        self_ty
    }

    /// Record a reflect call on a type-param receiver as a static dispatch that
    /// monomorphization redirects to the concrete subject's synthesized method.
    fn record_type_param_reflect_dispatch(
        &mut self,
        type_param_name: &str,
        reflect_trait_name: String,
        method: String,
        static_call: &ast::StaticMethodCallExpr,
    ) {
        let mut method_info = LocalMethodName::new(
            FqTypeName::binder(type_param_name),
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
                param_is_mut: Vec::new(),
                type_args: Vec::new(),
                param_defaults: Vec::new(),
            },
        );
    }

    /// The `Assoc = [..F]` pack binding on `T`'s bound of the given trait
    /// (`Fields` on `ReflectStruct`, `Cases` on `ReflectVariant`), resolved in the
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

    /// The struct `ReflectStruct::<T>` targets: its declared name, the
    /// instantiation's type args (empty for a plain struct), and its field types
    /// in declaration order with those args substituted. `None` when `T` is not
    /// a struct (the only reflectable kind).
    fn reflect_struct_subject(&self, self_ty: TypeId) -> Option<ReflectSubject> {
        let (base_name, module_source, type_args) =
            match self.tysys.type_table.borrow().get(self_ty).clone() {
                ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args,
                } => (name, module_source, type_args),
                ResolvedType::Struct {
                    name,
                    module_source,
                    ..
                } => (name.to_string(), module_source, Vec::new()),
                _ => {
                    let name = self.tysys.type_table.borrow().type_name(self_ty);
                    let module_source = self.find_struct_module_source(&name);
                    (name, module_source, Vec::new())
                }
            };
        let info = self
            .type_lookup()
            .struct_fields_in(&base_name, &module_source)?;
        let declared: Vec<TypeId> = info.fields.iter().map(|(_, ty, _)| *ty).collect();
        let param_ids = info.type_param_type_ids.clone();
        Some(ReflectSubject {
            member_types: self.substitute_declared_params(&declared, &param_ids, &type_args),
            base_name,
            module_source,
            type_args,
        })
    }

    /// The variant `ReflectVariant::<T>` targets: its declared name, the
    /// instantiation's type args (empty for a plain variant), and its case
    /// payloads in declaration order with those args substituted. `None` when
    /// `T` is not a variant.
    fn reflect_variant_subject(&self, self_ty: TypeId) -> Option<ReflectSubject> {
        let (base_name, module_source, type_args) =
            match self.tysys.type_table.borrow().get(self_ty).clone() {
                ResolvedType::Variant {
                    name,
                    module_source,
                    ..
                } => (name, module_source, Vec::new()),
                ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args,
                } => (name, module_source, type_args),
                _ => return None,
            };
        let info = self
            .type_lookup()
            .variant_case_in(&base_name, &module_source)?;
        let declared: Vec<TypeId> = info.cases.iter().map(|c| c.payload).collect();
        let param_ids = info.type_param_type_ids.clone();
        Some(ReflectSubject {
            member_types: self.substitute_declared_params(&declared, &param_ids, &type_args),
            base_name,
            module_source,
            type_args,
        })
    }

    /// Substitute an instantiation's `type_args` into member types written
    /// against the declaration's own parameters. A no-op for a plain type
    /// (`type_args` empty), which carries no parameters to substitute.
    fn substitute_declared_params(
        &self,
        declared: &[TypeId],
        param_ids: &[TypeId],
        type_args: &[TypeId],
    ) -> Vec<TypeId> {
        if type_args.is_empty() {
            return declared.to_vec();
        }
        let mut tt = self.tysys.type_table.borrow_mut();
        let substitution: crate::hashmap::IndexMap<u32, TypeId> = param_ids
            .iter()
            .zip(type_args)
            .filter_map(|(param, &arg)| match tt.get(*param) {
                ResolvedType::TypeParam { index, .. } => Some((*index, arg)),
                _ => None,
            })
            .collect();
        declared
            .iter()
            .map(|&ty| tt.substitute_type_params(ty, &substitution))
            .collect()
    }

    /// Resolve and type-check the sole receiver of a value-reading reflection
    /// member (`ReflectVariant::discriminant` / `ReflectEnum::discriminant` /
    /// `ReflectFlags::bits`): one argument whose type, refs peeled, is the
    /// subject. Returns whether it is well-formed, emitting the diagnostic
    /// otherwise.
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

    /// Resolve the single field-value tuple argument of `ReflectStruct::from_fields`
    /// against the subject's `FieldTypes`. Returns whether the call is well-formed.
    fn check_reflect_from_fields_arg(
        &mut self,
        field_types: &[TypeId],
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> bool {
        let fields_tuple_ty = self
            .tysys
            .type_table
            .borrow_mut()
            .make_tuple(field_types.to_vec());
        let arg_types: Vec<TypeId> = static_call
            .args
            .iter()
            .map(|arg| self.resolve_expr(arg, ctx, Some(fields_tuple_ty)))
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
        if arg_ty != TypeTable::ERROR && arg_ty != fields_tuple_ty {
            let (expected, found) = {
                let tt = self.tysys.type_table.borrow();
                (tt.type_name(fields_tuple_ty), tt.type_name(arg_ty))
            };
            let _ = self.emit(TypeError::TypeMismatch {
                expected,
                found,
                span: static_call.span,
            });
            return false;
        }
        true
    }

    /// Resolve the args of a no-argument `ReflectStruct` metadata call and reject any
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

    /// Whether `prefix::method` names a `ReflectStruct` trait-qualified static call
    /// (`ReflectStruct::<T>::type_name` / `members`). `prefix` must resolve to the
    /// compiler's `ReflectStruct` trait *in this scope* — `classify_on_bound_trait`
    /// applies the same module check `on_bound` dispatch uses, so a user type or
    /// trait that happens to be named `ReflectStruct` is not hijacked. `method` is
    /// matched through the compiler-item registry so a stdlib rename flows through.
    pub(super) fn is_reflect_trait_call(&self, prefix: &str, method: &str) -> bool {
        if self
            .tysys
            .classify_on_bound_trait(&self.type_lookup(), prefix)
            != Some(super::trait_query::OnBoundTrait::ReflectStruct)
        {
            return false;
        }
        let tt = self.tysys.type_table.borrow();
        let items = tt.compiler_items();
        method == items.method_name(crate::compiler_item::CompilerItem::ReflectStructTypeName)
            || method == items.method_name(crate::compiler_item::CompilerItem::ReflectStructMembers)
            || method
                == items.method_name(crate::compiler_item::CompilerItem::ReflectStructFromFields)
            || method
                == items
                    .method_name(crate::compiler_item::CompilerItem::ReflectStructWireNamePolicy)
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
                == items.method_name(crate::compiler_item::CompilerItem::ReflectVariantDiscriminant)
            || method
                == items.method_name(crate::compiler_item::CompilerItem::ReflectVariantMembers)
            || method
                == items
                    .method_name(crate::compiler_item::CompilerItem::ReflectVariantWireNamePolicy)
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
        let Some(subject) = self.reflect_variant_subject(self_ty) else {
            let _ = self.emit(TypeError::UnknownFunction {
                name: format!("ReflectVariant::<{self_name}>::{method}"),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        };
        let ReflectSubject {
            base_name: self_name,
            module_source,
            type_args,
            member_types: payloads,
        } = subject;

        let (trait_name, discriminant_method, cases_method, wire_name_policy_method) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            (
                items.trait_name(CompilerItem::ReflectVariant).to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantDiscriminant)
                    .to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantMembers)
                    .to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantWireNamePolicy)
                    .to_string(),
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
        } else if method == wire_name_policy_method {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_enum(CompilerItem::CaseStyle)
        } else if method == cases_method {
            let mut tt = self.tysys.type_table.borrow_mut();
            let (case_module, case_name) = {
                let items = tt.compiler_items();
                let (m, n) = items.require_struct(CompilerItem::ReflectVariantCase);
                (m.clone(), n.to_string())
            };
            let members: Vec<TypeId> = payloads
                .into_iter()
                .map(|payload| {
                    tt.make_generic_instance(
                        case_name.clone(),
                        case_module.clone(),
                        vec![self_ty, payload],
                    )
                })
                .collect();
            tt.make_tuple(members)
        } else {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_struct(CompilerItem::String)
        };

        let func_ref =
            self.reflect_func_ref(&self_name, &type_args, &trait_name, &method, module_source);
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
    /// (inside an `impl<T: ReflectVariant<CasePayloads = [..P]>, ..P: …>` derivation).
    /// The value-free members resolve to their fixed return types; `cases()`
    /// resolves to the constructor-mapped member pack `[..Case<T, P>]` read off
    /// `T`'s `CasePayloads = [..P]` bound (WEP 2026-06-13 §3f). Each is recorded as a
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
        let (
            trait_name,
            type_name_method,
            discriminant_method,
            cases_method,
            wire_name_policy_method,
        ) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            (
                items.trait_name(CompilerItem::ReflectVariant).to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantTypeName)
                    .to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantDiscriminant)
                    .to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantMembers)
                    .to_string(),
                items
                    .method_name(CompilerItem::ReflectVariantWireNamePolicy)
                    .to_string(),
            )
        };

        let is_discriminant = method == discriminant_method;
        let return_type = if method == wire_name_policy_method {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_enum(CompilerItem::CaseStyle)
        } else if method == type_name_method {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_struct(CompilerItem::String)
        } else if is_discriminant {
            TypeTable::I32
        } else if method == cases_method {
            let Some(members_ty) =
                self.variant_members_bound_ty(self_ty, type_param_name, &trait_name)
            else {
                let _ = self.emit(TypeError::UnknownFunction {
                    name: format!(
                        "ReflectVariant::<{type_param_name}>::{method} (no `CasePayloads = [..P]` bound on {type_param_name})"
                    ),
                    span: static_call.span,
                });
                return TypeTable::ERROR;
            };
            members_ty
        } else {
            unreachable!("is_reflect_variant_trait_call admits only the trait's methods")
        };

        let args_valid = if is_discriminant {
            self.check_reflect_fields_receiver(self_ty, type_param_name, static_call, ctx)
        } else {
            self.reject_reflect_metadata_args(static_call, ctx)
        };
        if !args_valid {
            return TypeTable::ERROR;
        }

        let mut method_info = LocalMethodName::new(
            FqTypeName::binder(type_param_name),
            Some(trait_name),
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

    /// The constructor-mapped member pack `[..Case<T, P>]` — the type of
    /// `members()` under a `T: ReflectVariant<CasePayloads = [..P]>` bound. The pack
    /// param recurs inside the mapped element as a scalar placeholder, so
    /// per-element substitution binds it to `P_k`. `None` when `T` carries no
    /// `Cases` pack bound.
    fn variant_members_bound_ty(
        &mut self,
        self_ty: TypeId,
        type_param_name: &str,
        trait_name: &str,
    ) -> Option<TypeId> {
        use crate::compiler_item::CompilerItem;
        let cases_ty = self.reflect_pack_bound_ty(
            type_param_name,
            trait_name,
            crate::synthesis::traits::REFLECT_CASE_PAYLOADS_ASSOC,
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
        let member = tt.make_generic_instance(case_name, case_module, vec![self_ty, elem_param]);
        let member_pack = tt.make_mapped_type_pack(pack_name, pack_index, member);
        Some(tt.make_tuple(vec![member_pack]))
    }

    /// The constructor-mapped member pack `[..StructField<T, F>]` — the type of
    /// `members()` under a `T: ReflectStruct<FieldTypes = [..F]>` bound. The variant
    /// analog is [`Self::variant_members_bound_ty`]; both map the projected element
    /// pack through a member constructor. `None` when `T` carries no `FieldTypes`
    /// pack bound.
    fn struct_members_bound_ty(
        &mut self,
        self_ty: TypeId,
        type_param_name: &str,
        reflect_trait_name: &str,
    ) -> Option<TypeId> {
        use crate::compiler_item::CompilerItem;
        let fields_ty = self.reflect_pack_bound_ty(
            type_param_name,
            reflect_trait_name,
            crate::synthesis::traits::REFLECT_FIELD_TYPES_ASSOC,
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
            let (m, n) = items.require_struct(CompilerItem::ReflectStructField);
            (m.clone(), n.to_string())
        };
        let elem_param = tt.make_type_param(pack_name.clone(), pack_index);
        let member = tt.make_generic_instance(field_name, field_module, vec![self_ty, elem_param]);
        let member_pack = tt.make_mapped_type_pack(pack_name, pack_index, member);
        Some(tt.make_tuple(vec![member_pack]))
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
            spec.value_method_item,
            spec.from_method_item,
            spec.members_method_item,
            spec.wire_name_policy_item,
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
            module_source: module_source.clone(),
            name: MethodName::format_local(
                &FqTypeName::declared(&module_source, &self_name),
                Some(&trait_name),
                &method,
            ),
            monomorph_info: None,
            method_info: Some(LocalMethodName::new(
                FqTypeName::declared(&module_source, &self_name),
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
        let mut method_info = LocalMethodName::new(
            FqTypeName::binder(type_param_name),
            Some(trait_name),
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
    /// emitted). `type_name` rejects args; `<value>(&self)` returns the scalar
    /// type; `from_<value>(raw)` takes one scalar arg and returns `Option<Self>`.
    fn check_reflect_scalar_args(
        &mut self,
        spec: ScalarReflectSpec,
        self_ty: TypeId,
        self_name: &str,
        method: &str,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> Option<TypeId> {
        let (type_name_method, value_method, from_method, members_method, wire_name_policy_method) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            (
                items.method_name(spec.type_name_item).to_string(),
                items.method_name(spec.value_method_item).to_string(),
                items.method_name(spec.from_method_item).to_string(),
                items.method_name(spec.members_method_item).to_string(),
                items.method_name(spec.wire_name_policy_item).to_string(),
            )
        };

        if *method == wire_name_policy_method {
            self.reject_reflect_metadata_args(static_call, ctx)
                .then(|| {
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_compiler_enum(CompilerItem::CaseStyle)
                })
        } else if *method == type_name_method {
            self.reject_reflect_metadata_args(static_call, ctx)
                .then(|| {
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_compiler_struct(CompilerItem::String)
                })
        } else if *method == members_method {
            if !self.reject_reflect_metadata_args(static_call, ctx) {
                return None;
            }
            self.scalar_members_return_ty(spec, self_ty, self_name, static_call)
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
            unreachable!("is_reflect_scalar_trait_call admits only the trait's methods")
        }
    }

    /// The return type of `members()` on a payload-free kind: a homogeneous
    /// member tuple. For a concrete subject it is an N-tuple `[M<Self>; N]` (N = the
    /// case / member count); for a generic `T` it is the mapped pack
    /// `[..M<T>]` whose arity is sourced from `T`'s `Members = [..C]` bound.
    /// `None` when a generic subject carries no such bound (diagnostic emitted).
    fn scalar_members_return_ty(
        &mut self,
        spec: ScalarReflectSpec,
        self_ty: TypeId,
        self_name: &str,
        static_call: &ast::StaticMethodCallExpr,
    ) -> Option<TypeId> {
        let subject_is_type_param = matches!(
            self.tysys.type_table.borrow().get(self_ty),
            ResolvedType::TypeParam { .. }
        );
        if !subject_is_type_param {
            return Some(self.scalar_concrete_members_ty(spec, self_ty, self_name));
        }
        let trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_items()
            .trait_name(spec.trait_item)
            .to_string();
        let Some(members_ty) = self.scalar_members_bound_ty(spec, self_ty, self_name, &trait_name)
        else {
            let method = &static_call.method;
            let assoc = spec.members_assoc;
            let _ = self.emit(TypeError::UnknownFunction {
                name: format!(
                    "{trait_name}::<{self_name}>::{method} (no `{assoc} = [..C]` bound on {self_name})"
                ),
                span: static_call.span,
            });
            return None;
        };
        Some(members_ty)
    }

    /// The concrete N-tuple `[M<Self>; N]` for `members()`, N being the
    /// subject's case / bit count.
    fn scalar_concrete_members_ty(
        &mut self,
        spec: ScalarReflectSpec,
        self_ty: TypeId,
        self_name: &str,
    ) -> TypeId {
        let count = self.scalar_member_count(spec, self_name);
        let mut tt = self.tysys.type_table.borrow_mut();
        let (member_module, member_name) = {
            let items = tt.compiler_items();
            let (m, n) = items.require_struct(spec.member_struct_item);
            (m.clone(), n.to_string())
        };
        let member_type = tt.make_generic_instance(member_name, member_module, vec![self_ty]);
        tt.make_tuple(std::iter::repeat_n(member_type, count).collect())
    }

    /// The subject's case (enum) / member (flags) count.
    fn scalar_member_count(&self, spec: ScalarReflectSpec, self_name: &str) -> usize {
        let lookup = self.type_lookup();
        match spec.kind {
            ScalarReflectKind::Enum => lookup
                .enum_case(self_name)
                .map_or(0, |info| info.cases.len()),
            ScalarReflectKind::Flags => lookup
                .flags_case(self_name)
                .map_or(0, |info| info.members.len()),
        }
    }

    /// The mapped member pack `[..M<T>]` — the type of `members()` under a
    /// `T: Trait<Members = [..C]>` bound. Analogous to
    /// [`Self::variant_members_bound_ty`], but the member carries no payload param,
    /// so the mapped element is a constant `M<T>` and the bound pack serves only
    /// to source the arity. `None` when `T` carries no `Members` pack bound.
    fn scalar_members_bound_ty(
        &mut self,
        spec: ScalarReflectSpec,
        self_ty: TypeId,
        type_param_name: &str,
        trait_name: &str,
    ) -> Option<TypeId> {
        let members_ty =
            self.reflect_pack_bound_ty(type_param_name, trait_name, spec.members_assoc)?;
        let mut tt = self.tysys.type_table.borrow_mut();
        let elems = tt.as_tuple(members_ty)?;
        let (pack_name, pack_index) = elems.iter().find_map(|&e| match tt.get(e) {
            crate::tir::ResolvedType::TypePack {
                name,
                index,
                mapped_elem: None,
            } => Some((name.clone(), *index)),
            _ => None,
        })?;
        let (member_module, member_name) = {
            let items = tt.compiler_items();
            let (m, n) = items.require_struct(spec.member_struct_item);
            (m.clone(), n.to_string())
        };
        let member = tt.make_generic_instance(member_name, member_module, vec![self_ty]);
        let member_pack = tt.make_mapped_type_pack(pack_name, pack_index, member);
        Some(tt.make_tuple(vec![member_pack]))
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
