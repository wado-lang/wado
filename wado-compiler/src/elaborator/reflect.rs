//! `Reflect` and its four kinds: the `Trait::<T>::method()` form
//! `resolve_static_method_call` routes to `T`'s synthesized `T^Trait::method`.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::name::{FqTypeName, LocalMethodName, MethodName};
use crate::tir::{FunctionRef, ResolvedType, TypeId, TypeTable};

use super::Elaborator;
use super::trait_query::OnBoundTrait;
use super::types::{FunctionContext, TypeError};

/// The two payload-free kinds share one static-call resolution shape, differing
/// only in their compiler items, subject kind, and scalar type (`i32` / `u64`).
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

    fn methods(self, tt: &TypeTable) -> ScalarMethods {
        let items = tt.compiler_items();
        ScalarMethods {
            value: items.method_name(self.value_method_item).to_string(),
            from: items.method_name(self.from_method_item).to_string(),
            members: items.method_name(self.members_method_item).to_string(),
            wire_name_policy: items.method_name(self.wire_name_policy_item).to_string(),
        }
    }
}

/// `ReflectStruct`'s member names, resolved once through the compiler-item
/// registry so a stdlib rename flows through both the `is_*_trait_call`
/// predicate and the resolver that dispatches on them.
struct StructMethods {
    members: String,
    from_fields: String,
    defaults: String,
    empty_slots: String,
    wire_name_policy: String,
}

impl StructMethods {
    fn resolve(tt: &TypeTable) -> Self {
        let items = tt.compiler_items();
        Self {
            members: items
                .method_name(CompilerItem::ReflectStructMembers)
                .to_string(),
            from_fields: items
                .method_name(CompilerItem::ReflectStructFromFields)
                .to_string(),
            defaults: items
                .method_name(CompilerItem::ReflectStructDefaults)
                .to_string(),
            empty_slots: items
                .method_name(CompilerItem::ReflectStructEmptySlots)
                .to_string(),
            wire_name_policy: items
                .method_name(CompilerItem::ReflectStructWireNamePolicy)
                .to_string(),
        }
    }

    fn declares(&self, method: &str) -> bool {
        [
            &self.members,
            &self.from_fields,
            &self.defaults,
            &self.empty_slots,
            &self.wire_name_policy,
        ]
        .into_iter()
        .any(|name| name == method)
    }
}

/// `ReflectVariant`'s member names — the variant analog of [`StructMethods`].
struct VariantMethods {
    discriminant: String,
    cases: String,
    wire_name_policy: String,
}

impl VariantMethods {
    fn resolve(tt: &TypeTable) -> Self {
        let items = tt.compiler_items();
        Self {
            discriminant: items
                .method_name(CompilerItem::ReflectVariantDiscriminant)
                .to_string(),
            cases: items
                .method_name(CompilerItem::ReflectVariantMembers)
                .to_string(),
            wire_name_policy: items
                .method_name(CompilerItem::ReflectVariantWireNamePolicy)
                .to_string(),
        }
    }

    fn declares(&self, method: &str) -> bool {
        [&self.discriminant, &self.cases, &self.wire_name_policy]
            .into_iter()
            .any(|name| name == method)
    }
}

/// A scalar kind's member names, sourced from its [`ScalarReflectSpec`].
struct ScalarMethods {
    value: String,
    from: String,
    members: String,
    wire_name_policy: String,
}

impl ScalarMethods {
    fn declares(&self, method: &str) -> bool {
        [
            &self.value,
            &self.from,
            &self.members,
            &self.wire_name_policy,
        ]
        .into_iter()
        .any(|name| name == method)
    }
}

/// Which resolver a `Trait::<T>::method()` call on a reflection trait routes to.
pub(super) enum ReflectDispatch {
    Root,
    Struct,
    Variant,
    Scalar(ScalarReflectSpec),
}

/// How the missing-pack-bound diagnostic spells the binding each payload kind
/// needs on a generic subject.
const STRUCT_PACK_BOUND: &str = "FieldTypes = [..F]";
const VARIANT_PACK_BOUND: &str = "CasePayloads = [..P]";

/// The `[..P]` pack a reflect bound projects: the pack element type itself and
/// the `(name, index)` handle a mapped pack is rebuilt from.
struct PackHead {
    ty: TypeId,
    name: String,
    index: u32,
}

/// The dispatch record's per-parameter mutability. A reflect member takes
/// either one never-`mut` argument — the subject of `discriminant(&self)` /
/// `bits(&self)`, the raw scalar of `from_<value>(raw)` — or none.
fn arg_param_is_mut(takes_argument: bool) -> Vec<bool> {
    if takes_argument {
        vec![false]
    } else {
        Vec::new()
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
        if let ResolvedType::TypeParam { name, .. } = subject {
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

        let (reflect_trait_name, methods) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.compiler_items().trait_fq(CompilerItem::ReflectStruct),
                StructMethods::resolve(&tt),
            )
        };

        let well_formed = if method == methods.from_fields {
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
            .record_bound_driven_synth_request_for(
                self_ty,
                &module_source,
                &reflect_trait_name
                    .canonical()
                    .expect("a compiler trait item names a declaration"),
            );

        let return_type = if method == methods.from_fields {
            self_ty
        } else if method == methods.defaults || method == methods.empty_slots {
            let mut tt = self.tysys.type_table.borrow_mut();
            let slots: Vec<TypeId> = field_types.iter().map(|&f| tt.make_option(f)).collect();
            tt.make_tuple(slots)
        } else if method == methods.members {
            self.payload_members_ty(CompilerItem::ReflectStructField, self_ty, &field_types)
        } else if method == methods.wire_name_policy {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_enum(CompilerItem::CaseStyle)
        } else {
            unreachable!("is_reflect_trait_call admits only the trait's methods")
        };

        let func_ref = self.reflect_func_ref(
            self_ty,
            &self_name,
            &type_args,
            &reflect_trait_name,
            &method,
            module_source,
        );
        self.record_reflect_dispatch(static_call.id, func_ref, Vec::new());

        return_type
    }

    /// Record a reflect call's dispatch fact for reify. A reflection member
    /// carries no method-level type args, no parameter defaults and no
    /// self-in-args, so only the callee and the argument list vary.
    fn record_reflect_dispatch(
        &mut self,
        call_id: ast::AstId,
        function_ref: FunctionRef,
        param_is_mut: Vec<bool>,
    ) {
        self.sem.types.static_method_dispatch.insert(
            call_id,
            super::sem::types::StaticMethodDispatch {
                // Reflection dispatches to an impl the reflect-bridge
                // synthesis mints; there is no declaration to name.
                method_def: None,
                function_ref,
                param_is_mut,
                param_types: Vec::new(),
                type_args: Vec::new(),
                param_defaults: Vec::new(),
                self_in_args: false,
            },
        );
    }

    /// Build the `FunctionRef` targeting a reflect subject's synthesized
    /// `Base^Trait::method`. A generic instance carries the instantiation in
    /// `monomorph_info` and in the mangled name, so monomorphization picks the
    /// instance whose type args match; `type_args` is empty for a plain type.
    fn reflect_func_ref(
        &self,
        self_ty: TypeId,
        base_name: &str,
        type_args: &[TypeId],
        trait_name: &crate::name::FqTraitName,
        method: &str,
        module_source: crate::module_source::ModuleSource,
    ) -> FunctionRef {
        let mut method_info = LocalMethodName::new(
            self.tysys.type_table.borrow().fq_base_type_name(self_ty),
            Some(trait_name.clone()),
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
        let method = static_call.method.clone();
        let (reflect_trait_name, methods) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.compiler_items().trait_fq(CompilerItem::ReflectStruct),
                StructMethods::resolve(&tt),
            )
        };

        if method == methods.from_fields {
            return self.resolve_generic_reflect_from_fields(
                self_ty,
                type_param_name,
                reflect_trait_name.clone(),
                static_call,
                ctx,
            );
        }

        let return_type = if method == methods.wire_name_policy {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_enum(CompilerItem::CaseStyle)
        } else if method == methods.defaults || method == methods.empty_slots {
            let Some(slots_ty) =
                self.struct_defaults_bound_ty(type_param_name, reflect_trait_name.base_name())
            else {
                self.emit_missing_pack_bound(
                    reflect_trait_name.base_name(),
                    type_param_name,
                    &method,
                    STRUCT_PACK_BOUND,
                    static_call,
                );
                return TypeTable::ERROR;
            };
            slots_ty
        } else if method == methods.members {
            let Some(members_ty) = self.payload_member_pack_bound_ty(
                self_ty,
                type_param_name,
                reflect_trait_name.base_name(),
                crate::synthesis::traits::REFLECT_FIELD_TYPES_ASSOC,
                CompilerItem::ReflectStructField,
            ) else {
                self.emit_missing_pack_bound(
                    reflect_trait_name.base_name(),
                    type_param_name,
                    &method,
                    STRUCT_PACK_BOUND,
                    static_call,
                );
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
            Vec::new(),
        );

        return_type
    }

    /// The diagnostic for a reflect member on a generic subject `T` that
    /// carries no pack bound to source the member's shape from. `pack_bound`
    /// spells the missing binding, e.g. `FieldTypes = [..F]`.
    fn emit_missing_pack_bound(
        &mut self,
        trait_name: &str,
        type_param_name: &str,
        method: &str,
        pack_bound: &str,
        static_call: &ast::StaticMethodCallExpr,
    ) {
        let _ = self.emit(TypeError::UnknownFunction {
            name: format!(
                "{trait_name}::<{type_param_name}>::{method} (no `{pack_bound}` bound on {type_param_name})"
            ),
            span: static_call.span,
        });
    }

    /// Resolve `ReflectStruct::<T>::from_fields(fields)` on a generic subject: the
    /// argument is checked against `T`'s `FieldTypes = [..F]` pack binding and the
    /// call yields `T` itself.
    fn resolve_generic_reflect_from_fields(
        &mut self,
        self_ty: TypeId,
        type_param_name: &str,
        reflect_trait_name: crate::name::FqTraitName,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let method = static_call.method.clone();
        let Some(fields_ty) = self.reflect_pack_bound_ty(
            type_param_name,
            reflect_trait_name.base_name(),
            crate::synthesis::traits::REFLECT_FIELD_TYPES_ASSOC,
        ) else {
            self.emit_missing_pack_bound(
                reflect_trait_name.base_name(),
                type_param_name,
                &method,
                STRUCT_PACK_BOUND,
                static_call,
            );
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
            reflect_trait_name,
            method,
            static_call,
            Vec::new(),
        );

        self_ty
    }

    /// Record a reflect call on a type-param receiver as a static dispatch that
    /// monomorphization redirects to the concrete subject's synthesized method.
    fn record_type_param_reflect_dispatch(
        &mut self,
        type_param_name: &str,
        reflect_trait_name: crate::name::FqTraitName,
        method: String,
        static_call: &ast::StaticMethodCallExpr,
        param_is_mut: Vec<bool>,
    ) {
        let mut method_info = LocalMethodName::new(
            FqTypeName::binder(type_param_name),
            Some(reflect_trait_name),
            method,
        );
        method_info.is_type_param_receiver = true;

        let func_ref = FunctionRef {
            module_source: self.current_module_source.clone(),
            name: method_info.to_mangled_name(),
            monomorph_info: None,
            method_info: Some(method_info),
        };
        self.record_reflect_dispatch(static_call.id, func_ref, param_is_mut);
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
                ResolvedType::GenericInstance { type_args, .. } => {
                    let (name, module_source) = self
                        .tysys
                        .type_table
                        .borrow()
                        .nominal_head(self_ty)
                        .expect("a nominal type names a declaration");
                    (name, module_source, type_args)
                }
                ResolvedType::Struct { .. } => {
                    let (name, module_source) = self
                        .tysys
                        .type_table
                        .borrow()
                        .nominal_head(self_ty)
                        .expect("a nominal type names a declaration");
                    (name, module_source, Vec::new())
                }
                _ => {
                    let name = self.tysys.type_table.borrow().type_name(self_ty);
                    let module_source = self.declaring_module_of(&name);
                    (name, module_source, Vec::new())
                }
            };
        let info = self
            .tysys
            .type_def(self_ty)
            .and_then(|def| self.type_lookup().struct_fields_of(def))?;
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
                ResolvedType::Variant { .. } | ResolvedType::GenericInstance { .. } => {
                    let type_args = self
                        .tysys
                        .type_table
                        .borrow()
                        .generic_type_args(self_ty)
                        .unwrap_or_default();
                    let (name, module_source) = self
                        .tysys
                        .type_table
                        .borrow()
                        .nominal_head(self_ty)
                        .expect("a nominal type names a declaration");
                    (name, module_source, type_args)
                }
                _ => return None,
            };
        let info = self
            .tysys
            .type_def(self_ty)
            .and_then(|def| self.type_lookup().variant_cases_of(def))?;
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
                tt.type_names_for_mismatch(fields_tuple_ty, arg_ty)
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
    fn is_reflect_trait_call(&self, prefix: &str, method: &str) -> bool {
        if self
            .tysys
            .classify_on_bound_trait(&self.type_lookup(), prefix)
            != Some(super::trait_query::OnBoundTrait::ReflectStruct)
        {
            return false;
        }
        StructMethods::resolve(&self.tysys.type_table.borrow()).declares(method)
    }

    /// The resolver `prefix::method` routes to, or `None` when the prefix names
    /// no reflection trait in this scope.
    pub(super) fn reflect_dispatch_of(
        &self,
        prefix: &str,
        method: &str,
    ) -> Option<ReflectDispatch> {
        if self.is_reflect_root_trait_call(prefix, method) {
            return Some(ReflectDispatch::Root);
        }
        if self.is_reflect_trait_call(prefix, method) {
            return Some(ReflectDispatch::Struct);
        }
        if self.is_reflect_variant_trait_call(prefix, method) {
            return Some(ReflectDispatch::Variant);
        }
        [ScalarReflectSpec::ENUM, ScalarReflectSpec::FLAGS]
            .into_iter()
            .find(|spec| self.is_reflect_scalar_trait_call(*spec, prefix, method))
            .map(ReflectDispatch::Scalar)
    }

    /// Whether `prefix::method` names `Reflect::<T>::type_name`. Same scope
    /// discipline as [`Self::is_reflect_trait_call`].
    fn is_reflect_root_trait_call(&self, prefix: &str, method: &str) -> bool {
        if self
            .tysys
            .classify_on_bound_trait(&self.type_lookup(), prefix)
            != Some(super::trait_query::OnBoundTrait::Reflect)
        {
            return false;
        }
        let tt = self.tysys.type_table.borrow();
        method
            == tt
                .compiler_items()
                .method_name(CompilerItem::ReflectTypeName)
    }

    /// Resolve `Reflect::<T>::type_name()` to `T^Reflect::type_name`. The root
    /// asks nothing of `T`'s shape, so every kind answers and a generic subject
    /// needs no pack bound.
    pub(super) fn resolve_reflect_root_static_call(
        &mut self,
        self_ty: TypeId,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
        let method = static_call.method.clone();
        if !self.reject_reflect_metadata_args(static_call, ctx) {
            return TypeTable::ERROR;
        }

        let (root_trait_name, string_type) = {
            let mut tt = self.tysys.type_table.borrow_mut();
            let trait_name = tt.compiler_items().trait_fq(CompilerItem::Reflect);
            let string_type = tt.make_compiler_struct(CompilerItem::String);
            (trait_name, string_type)
        };

        let subject = self.tysys.type_table.borrow().get(self_ty).clone();
        if let ResolvedType::TypeParam { name, .. } = subject {
            self.record_type_param_reflect_dispatch(
                &name,
                root_trait_name,
                method,
                static_call,
                Vec::new(),
            );
            return string_type;
        }

        let Some((base_name, module_source, type_args)) = self.reflect_root_subject(self_ty) else {
            let self_name = self.tysys.type_table.borrow().type_name(self_ty);
            let _ = self.emit(TypeError::UnknownFunction {
                name: format!("Reflect::<{self_name}>::{method}"),
                span: static_call.span,
            });
            return TypeTable::ERROR;
        };

        let func_ref = self.reflect_func_ref(
            self_ty,
            &base_name,
            &type_args,
            &root_trait_name,
            &method,
            module_source,
        );
        self.record_reflect_dispatch(static_call.id, func_ref, Vec::new());

        string_type
    }

    /// The declaration `Reflect::<T>` names, and this instantiation's type args.
    /// `None` where no `Reflect` impl is synthesized, which
    /// [`has_reflect_kind`](crate::synthesis::template::has_reflect_kind)
    /// decides — the same answer monomorphization gets for a bounded blanket.
    fn reflect_root_subject(
        &self,
        self_ty: TypeId,
    ) -> Option<(String, crate::module_source::ModuleSource, Vec<TypeId>)> {
        let tt = self.tysys.type_table.borrow();
        if !crate::synthesis::template::has_reflect_kind(self_ty, &tt) {
            return None;
        }
        let (base_name, module_source) = tt.nominal_head(self_ty)?;
        let type_args = tt.generic_type_args(self_ty).unwrap_or_default();
        Some((base_name, module_source, type_args))
    }

    /// Whether `prefix::method` names a `ReflectVariant` trait-qualified static
    /// call. Same scope discipline as [`Self::is_reflect_trait_call`].
    fn is_reflect_variant_trait_call(&self, prefix: &str, method: &str) -> bool {
        if self
            .tysys
            .classify_on_bound_trait(&self.type_lookup(), prefix)
            != Some(super::trait_query::OnBoundTrait::ReflectVariant)
        {
            return false;
        }
        VariantMethods::resolve(&self.tysys.type_table.borrow()).declares(method)
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
        let method = static_call.method.clone();

        // Generic subject `T: ReflectVariant`: the concrete variant is unknown
        // until monomorphization; mirror `resolve_reflect_static_call`.
        let subject = self.tysys.type_table.borrow().get(self_ty).clone();
        if let ResolvedType::TypeParam { name, .. } = subject {
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

        let (trait_name, methods) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.compiler_items().trait_fq(CompilerItem::ReflectVariant),
                VariantMethods::resolve(&tt),
            )
        };

        let is_discriminant = method == methods.discriminant;
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
            .record_bound_driven_synth_request_for(
                self_ty,
                &module_source,
                &trait_name
                    .canonical()
                    .expect("a compiler trait item names a declaration"),
            );

        let return_type = if is_discriminant {
            TypeTable::I32
        } else if method == methods.wire_name_policy {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_enum(CompilerItem::CaseStyle)
        } else if method == methods.cases {
            self.payload_members_ty(CompilerItem::ReflectVariantCase, self_ty, &payloads)
        } else {
            unreachable!("is_reflect_variant_trait_call admits only the trait's methods")
        };

        let func_ref = self.reflect_func_ref(
            self_ty,
            &self_name,
            &type_args,
            &trait_name,
            &method,
            module_source,
        );
        self.record_reflect_dispatch(static_call.id, func_ref, arg_param_is_mut(is_discriminant));

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
        let method = static_call.method.clone();
        let (trait_name, methods) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.compiler_items().trait_fq(CompilerItem::ReflectVariant),
                VariantMethods::resolve(&tt),
            )
        };

        let is_discriminant = method == methods.discriminant;
        let return_type = if method == methods.wire_name_policy {
            self.tysys
                .type_table
                .borrow_mut()
                .make_compiler_enum(CompilerItem::CaseStyle)
        } else if is_discriminant {
            TypeTable::I32
        } else if method == methods.cases {
            let Some(members_ty) = self.payload_member_pack_bound_ty(
                self_ty,
                type_param_name,
                trait_name.base_name(),
                crate::synthesis::traits::REFLECT_CASE_PAYLOADS_ASSOC,
                CompilerItem::ReflectVariantCase,
            ) else {
                self.emit_missing_pack_bound(
                    trait_name.base_name(),
                    type_param_name,
                    &method,
                    VARIANT_PACK_BOUND,
                    static_call,
                );
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

        self.record_type_param_reflect_dispatch(
            type_param_name,
            trait_name,
            method,
            static_call,
            arg_param_is_mut(is_discriminant),
        );

        return_type
    }

    /// Map the `[..P]` pack `T`'s bound projects through `elem`, rewrapped in
    /// the `[..X]` tuple shape a member type carries. `None` when `T` carries
    /// no such pack bound.
    fn map_bound_pack(
        &mut self,
        type_param_name: &str,
        reflect_trait_name: &str,
        assoc_name: &str,
        elem: impl FnOnce(&mut TypeTable, &PackHead) -> TypeId,
    ) -> Option<TypeId> {
        let pack_tuple =
            self.reflect_pack_bound_ty(type_param_name, reflect_trait_name, assoc_name)?;
        let mut tt = self.tysys.type_table.borrow_mut();
        let elems = tt.as_tuple(pack_tuple)?;
        let head = elems.iter().find_map(|&e| match tt.get(e) {
            ResolvedType::TypePack {
                name,
                index,
                mapped_elem: None,
            } => Some(PackHead {
                ty: e,
                name: name.clone(),
                index: *index,
            }),
            _ => None,
        })?;
        let mapped = elem(&mut tt, &head);
        let member_pack = tt.make_mapped_type_pack(head.name, head.index, mapped);
        Some(tt.make_tuple(vec![member_pack]))
    }

    /// The constructor-mapped member pack `[..M<T, P>]` — the type of
    /// `members()` under a `T: Trait<Assoc = [..P]>` bound whose member handle
    /// carries the element as its payload parameter (`StructField<T, F>` /
    /// `VariantCase<T, P>`). The pack param recurs inside the mapped element as
    /// a scalar placeholder, so per-element substitution binds it to `P_k`.
    fn payload_member_pack_bound_ty(
        &mut self,
        self_ty: TypeId,
        type_param_name: &str,
        reflect_trait_name: &str,
        assoc_name: &str,
        member_struct_item: CompilerItem,
    ) -> Option<TypeId> {
        self.map_bound_pack(
            type_param_name,
            reflect_trait_name,
            assoc_name,
            |tt, head| {
                let def = tt.require_compiler_item_def(member_struct_item);
                let elem_param = tt.make_type_param(head.name.clone(), head.index);
                tt.make_generic_instance(def, vec![self_ty, elem_param])
            },
        )
    }

    /// The slot pack `[..Option<F>]` — the type of `defaults()` under a
    /// `T: ReflectStruct<FieldTypes = [..F]>` bound. Maps the field-type pack
    /// through `Option`, as [`Self::payload_member_pack_bound_ty`] maps it
    /// through the member constructor.
    fn struct_defaults_bound_ty(
        &mut self,
        type_param_name: &str,
        reflect_trait_name: &str,
    ) -> Option<TypeId> {
        self.map_bound_pack(
            type_param_name,
            reflect_trait_name,
            crate::synthesis::traits::REFLECT_FIELD_TYPES_ASSOC,
            // The slot names the pack itself, not a `TypeParam` placeholder: a
            // derivation infers `Option<F>`'s payload from it, and inference
            // binds through the pack.
            |tt, head| tt.make_option(head.ty),
        )
    }

    /// Whether `prefix::method` names a member of the scalar-kind reflection
    /// trait `spec` describes (`ReflectEnum` / `ReflectFlags`). Same scope
    /// discipline as [`Self::is_reflect_trait_call`].
    fn is_reflect_scalar_trait_call(
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
        spec.methods(&self.tysys.type_table.borrow())
            .declares(method)
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
                tt.compiler_items().trait_fq(spec.trait_item),
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

        // The subject is an enum or a flags type — `subject_matches` just said
        // so — and its `ResolvedType` carries the declaration, so the module
        // comes off that rather than off the spelling.
        let module_source = self
            .tysys
            .type_table
            .borrow()
            .nominal_def(self_ty)
            .map_or_else(
                || self.declaring_module_of(&self_name),
                |def| self.tysys.resolutions.defs().module(def).clone(),
            );
        let Some(return_type) =
            self.check_reflect_scalar_args(spec, self_ty, &self_name, &method, static_call, ctx)
        else {
            return TypeTable::ERROR;
        };

        self.tysys
            .type_table
            .borrow_mut()
            .record_bound_driven_synth_request_for(
                self_ty,
                &module_source,
                &trait_name
                    .canonical()
                    .expect("a compiler trait item names a declaration"),
            );

        let param_is_mut = self.reflect_scalar_param_is_mut(spec, &method);
        let receiver = self.tysys.type_table.borrow().fq_base_type_name(self_ty);
        let func_ref = FunctionRef {
            module_source,
            name: MethodName::format_local(&receiver, Some(&trait_name), &method),
            monomorph_info: None,
            method_info: Some(LocalMethodName::new(
                receiver,
                Some(trait_name.clone()),
                method.clone(),
            )),
        };
        self.record_reflect_dispatch(static_call.id, func_ref, param_is_mut);

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
            .trait_fq(spec.trait_item);

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
        self.record_type_param_reflect_dispatch(
            type_param_name,
            trait_name,
            method,
            static_call,
            param_is_mut,
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
        let methods = spec.methods(&self.tysys.type_table.borrow());

        if *method == methods.wire_name_policy {
            self.reject_reflect_metadata_args(static_call, ctx)
                .then(|| {
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_compiler_enum(CompilerItem::CaseStyle)
                })
        } else if *method == methods.members {
            if !self.reject_reflect_metadata_args(static_call, ctx) {
                return None;
            }
            self.scalar_members_return_ty(spec, self_ty, self_name, static_call)
        } else if *method == methods.value {
            self.check_reflect_fields_receiver(self_ty, self_name, static_call, ctx)
                .then_some(spec.value_type)
        } else if *method == methods.from {
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
                    tt.type_names_for_mismatch(spec.value_type, arg_types[0])
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
            return Some(self.scalar_concrete_members_ty(spec, self_ty));
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

    /// A reflect trait's associated type for a *concrete* subject, computed
    /// the way the synthesized impl would.
    ///
    /// Synthesis registers these after elaboration, so a call site needing one
    /// — projecting a pack out of `T: ReflectFlags<Members = [..M]>` — finds
    /// nothing in the registry and must compute it.
    pub(super) fn concrete_reflect_assoc_type(
        &mut self,
        subject: TypeId,
        trait_name: &str,
        assoc_name: &str,
    ) -> Option<TypeId> {
        use crate::synthesis::traits::{
            REFLECT_CASE_PAYLOADS_ASSOC, REFLECT_FIELD_SLOTS_ASSOC, REFLECT_FIELD_TYPES_ASSOC,
            REFLECT_MEMBERS_ASSOC,
        };
        if matches!(
            self.tysys.type_table.borrow().get(subject),
            ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. }
        ) {
            return None;
        }
        let (struct_trait, variant_trait, enum_trait, flags_trait) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            (
                items.trait_name(CompilerItem::ReflectStruct).to_string(),
                items.trait_name(CompilerItem::ReflectVariant).to_string(),
                items.trait_name(CompilerItem::ReflectEnum).to_string(),
                items.trait_name(CompilerItem::ReflectFlags).to_string(),
            )
        };
        if trait_name == struct_trait {
            let members = self.reflect_struct_subject(subject)?.member_types;
            return match assoc_name {
                REFLECT_FIELD_TYPES_ASSOC => {
                    Some(self.tysys.type_table.borrow_mut().make_tuple(members))
                }
                REFLECT_FIELD_SLOTS_ASSOC => {
                    let mut tt = self.tysys.type_table.borrow_mut();
                    let slots: Vec<TypeId> = members.iter().map(|&m| tt.make_option(m)).collect();
                    Some(tt.make_tuple(slots))
                }
                REFLECT_MEMBERS_ASSOC => Some(self.payload_members_ty(
                    CompilerItem::ReflectStructField,
                    subject,
                    &members,
                )),
                _ => None,
            };
        }
        if trait_name == variant_trait {
            let members = self.reflect_variant_subject(subject)?.member_types;
            return match assoc_name {
                REFLECT_CASE_PAYLOADS_ASSOC => {
                    Some(self.tysys.type_table.borrow_mut().make_tuple(members))
                }
                REFLECT_MEMBERS_ASSOC => Some(self.payload_members_ty(
                    CompilerItem::ReflectVariantCase,
                    subject,
                    &members,
                )),
                _ => None,
            };
        }
        if assoc_name != REFLECT_MEMBERS_ASSOC {
            return None;
        }
        let spec = if trait_name == enum_trait {
            ScalarReflectSpec::ENUM
        } else if trait_name == flags_trait {
            ScalarReflectSpec::FLAGS
        } else {
            return None;
        };
        let subject_ty = self.tysys.type_table.borrow().get(subject).clone();
        if !spec.subject_matches(&subject_ty) {
            return None;
        }
        Some(self.scalar_concrete_members_ty(spec, subject))
    }

    /// The member tuple for a kind whose handle carries a payload type:
    /// `[StructField<T, F_0>, ...]` / `[VariantCase<T, P_0>, ...]`.
    fn payload_members_ty(
        &mut self,
        member_struct_item: CompilerItem,
        subject: TypeId,
        member_types: &[TypeId],
    ) -> TypeId {
        let mut tt = self.tysys.type_table.borrow_mut();
        let def = tt.require_compiler_item_def(member_struct_item);
        let handles: Vec<TypeId> = member_types
            .iter()
            .map(|&payload| tt.make_generic_instance(def, vec![subject, payload]))
            .collect();
        tt.make_tuple(handles)
    }

    /// The concrete N-tuple `[M<Self>; N]` for `members()`, N being the
    /// subject's case / bit count.
    fn scalar_concrete_members_ty(&mut self, spec: ScalarReflectSpec, self_ty: TypeId) -> TypeId {
        let count = self.scalar_member_count(spec, self_ty);
        let mut tt = self.tysys.type_table.borrow_mut();
        let def = tt.require_compiler_item_def(spec.member_struct_item);
        let member_type = tt.make_generic_instance(def, vec![self_ty]);
        tt.make_tuple(std::iter::repeat_n(member_type, count).collect())
    }

    /// The subject's case (enum) / member (flags) count, off the declaration
    /// its own type names rather than its rendered head.
    fn scalar_member_count(&self, spec: ScalarReflectSpec, self_ty: TypeId) -> usize {
        let Some(def) = self.tysys.type_table.borrow().nominal_def(self_ty) else {
            return 0;
        };
        let lookup = self.type_lookup();
        match spec.kind {
            ScalarReflectKind::Enum => lookup.enum_cases_of(def).map_or(0, |info| info.cases.len()),
            ScalarReflectKind::Flags => lookup
                .flags_members_of(def)
                .map_or(0, |info| info.members.len()),
        }
    }

    /// The mapped member pack `[..M<T>]` — the type of `members()` under a
    /// `T: Trait<Members = [..C]>` bound. Unlike
    /// [`Self::payload_member_pack_bound_ty`] the member carries no payload
    /// param, so the mapped element is a constant `M<T>` and the bound pack
    /// serves only to source the arity.
    fn scalar_members_bound_ty(
        &mut self,
        spec: ScalarReflectSpec,
        self_ty: TypeId,
        type_param_name: &str,
        trait_name: &str,
    ) -> Option<TypeId> {
        self.map_bound_pack(type_param_name, trait_name, spec.members_assoc, |tt, _| {
            let def = tt.require_compiler_item_def(spec.member_struct_item);
            tt.make_generic_instance(def, vec![self_ty])
        })
    }

    /// Per-parameter mutability of a scalar-kind member's dispatch record:
    /// `<value>(&self)` and `from_<value>(raw)` take one argument; the metadata
    /// members take none.
    fn reflect_scalar_param_is_mut(&self, spec: ScalarReflectSpec, method: &str) -> Vec<bool> {
        let tt = self.tysys.type_table.borrow();
        let items = tt.compiler_items();
        arg_param_is_mut(
            method == items.method_name(spec.value_method_item)
                || method == items.method_name(spec.from_method_item),
        )
    }
}
