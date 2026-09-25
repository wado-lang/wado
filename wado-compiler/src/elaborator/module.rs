//! Single module type/signature collection and name resolution helpers.

use crate::ast::{Item, Module, Type};
use crate::compiler_host::CompilerHost;
use crate::tir::TypeTable;

use super::Elaborator;
use super::scope::{BinderInScope, ScopedBound};
use super::types::{
    EnumInfo, FlagsInfo, GenericNewtypeInfo, ParamSlot, StructFieldInfo, VariantCaseData,
    VariantInfo,
};
use crate::elaborator::item::{
    register_enum_compiler_items, register_function_compiler_item, register_method_compiler_item,
    register_trait_compiler_item, register_variant_compiler_items,
};
use crate::name::{FqTypeName, MethodName, RefKind, TUPLE_TYPE_NAME, UNIT_TYPE_NAME};

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn collect_types(&mut self, module: &Module) {
        for item in &module.items {
            match item {
                Item::Struct(struct_decl) => {
                    let mut scope = self.enter_inherited_type_param_scope();
                    scope.annotate_ctx.trait_ctx.type_params.clear();
                    scope.annotate_ctx.trait_ctx.type_param_bounds.clear();
                    scope.register_generic_params(&struct_decl.type_params, 0);

                    let mut fields = Vec::new();
                    for field in &struct_decl.fields {
                        let type_id = scope.resolve_type(&field.ty);
                        scope.reject_written_annotation(&field.ty);
                        fields.push((field.name.clone(), type_id, field.visibility));
                    }
                    let type_param_type_ids = Elaborator::<H>::slot_type_ids(
                        &ParamSlot::list(&struct_decl.type_params),
                        &scope.tysys.type_table,
                    );

                    let info = StructFieldInfo::of_decl(
                        scope.current_module_source.clone(),
                        struct_decl,
                        fields,
                        type_param_type_ids,
                    );
                    let def = scope.tysys.def_at(struct_decl.id);
                    scope.sem.decls.local.struct_fields.insert(def, info);

                    drop(scope);
                }
                Item::Newtype(newtype_decl) => {
                    let def = self.tysys.def_at(newtype_decl.id);
                    if newtype_decl.type_params.is_empty() {
                        let base_type_id = self.resolve_type(&newtype_decl.ty);
                        self.sem.decls.local.declare_newtype(
                            &self.tysys.type_table,
                            def,
                            newtype_decl.id,
                            base_type_id,
                        );
                    } else {
                        self.sem
                            .decls
                            .local
                            .generic_newtypes
                            .insert(def, GenericNewtypeInfo::of_decl(newtype_decl));
                    }
                }
                Item::Variant(variant_decl) => {
                    let mut scope = self.enter_inherited_type_param_scope();
                    scope.annotate_ctx.trait_ctx.type_params.clear();
                    scope.register_generic_params(&variant_decl.type_params, 0);
                    let type_param_type_ids = Elaborator::<H>::slot_type_ids(
                        &ParamSlot::list(&variant_decl.type_params),
                        &scope.tysys.type_table,
                    );

                    let cases = VariantCaseData::collect(variant_decl, |payload_ty| {
                        scope.reject_written_annotation(payload_ty);
                        scope.resolve_type(payload_ty)
                    });

                    let module_source = scope.current_module_source.clone();
                    let def = scope.tysys.def_at(variant_decl.id);
                    scope.sem.decls.local.variant_cases.insert(
                        def,
                        VariantInfo::of_decl(
                            module_source.clone(),
                            variant_decl,
                            cases,
                            type_param_type_ids,
                        ),
                    );
                    register_variant_compiler_items(
                        &scope.tysys.type_table,
                        variant_decl,
                        &module_source,
                        scope.logger,
                    );
                    drop(scope);
                }
                Item::Enum(enum_decl) => {
                    self.sem.decls.local.enum_cases.insert(
                        self.tysys.def_at(enum_decl.id),
                        EnumInfo::of_decl(self.current_module_source.clone(), enum_decl),
                    );
                    register_enum_compiler_items(
                        &self.tysys.type_table,
                        enum_decl,
                        &self.current_module_source,
                        self.logger,
                    );
                }
                Item::Flags(flags_decl) => {
                    // The batch pass (`annotate_modules`) reports a flags wider than a word.
                    if flags_decl.flags.len() > FlagsInfo::MAX_MEMBERS {
                        continue;
                    }
                    let def = self.tysys.def_at(flags_decl.id);
                    self.sem.decls.local.declare_flags(
                        &self.tysys.type_table,
                        def,
                        self.current_module_source.clone(),
                        flags_decl,
                    );
                }
                Item::Function(func) => {
                    register_function_compiler_item(
                        &self.tysys.type_table,
                        &func.attrs,
                        &func.name,
                        self.tysys.resolutions.defs().def_at(func.id),
                        &self.current_module_source,
                        func.span,
                        self.logger,
                    );
                }
                Item::Trait(trait_decl) => {
                    register_trait_compiler_item(
                        &self.tysys.type_table,
                        &trait_decl.attrs,
                        trait_decl.id,
                        &trait_decl.name,
                        &trait_decl.methods,
                        &trait_decl.associated_types,
                        &self.current_module_source,
                        trait_decl.span,
                        self.logger,
                    );
                    // Register `#[compiler_item("...")]` on individual trait
                    // method declarations against the trait as their owner type
                    // — the trait body is the only place a serde protocol
                    // method and its owning trait are both in scope.
                    let owner_head = FqTypeName::declared(
                        self.tysys.resolutions.defs(),
                        self.tysys.def_at(trait_decl.id),
                    );
                    for method in &trait_decl.methods {
                        register_method_compiler_item(
                            &self.tysys.type_table,
                            &method.attrs,
                            &method.name,
                            (self.tysys.def_at(method.id), None),
                            &trait_decl.name,
                            &owner_head,
                            &self.current_module_source,
                            method.span,
                            self.logger,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Collect function signatures for call resolution
    pub(super) fn collect_function_signatures(&mut self, module: &Module) {
        self.register_module_assoc_types(module);
        for item in &module.items {
            if let Item::Impl(impl_block) = item {
                self.record_impl_block_sig(impl_block);
            }
        }
        for item in &module.items {
            if let Item::Impl(impl_block) = item {
                let mut scope = self.enter_impl_scope(impl_block);

                if impl_block.trait_type.is_some() {
                    scope.enforce_impl_assoc_type_bounds(impl_block);
                    scope.enforce_impl_supertraits(impl_block);
                }

                // Resolve the trait type for its side effect of recording a
                // use->def reference from the trait-name identifier in the
                // impl header (`impl Greet for Bot`) to the trait decl. The
                // resulting TypeId is unused because trait bounds are
                // checked via trait_query, not via type substitution.
                if let Some(trait_type) = &impl_block.trait_type {
                    let _ = scope.resolve_type(trait_type);
                }
                // Resolve the implementing type for its reference-recording
                // side effect (already performed when method signatures are
                // later resolved, but doing it here ensures the ref is
                // recorded even for impls with no methods referencing it).
                let _ = scope.resolve_type(&impl_block.ty);

                // The receiver is named by the module that declares it — the
                // written name alone is not an identity.
                let struct_name = scope.impl_receiver_name(impl_block);
                let trait_name = impl_block
                    .trait_type
                    .as_ref()
                    .map(|t| scope.fq_trait_name(t).head_only());

                // Register methods that carry `#[compiler_item("...")]`
                // against the impl's owning type. This is the only place
                // where the method declaration AND its owner type are
                // simultaneously in scope.
                for method in &impl_block.methods {
                    let defs = scope.tysys.resolutions.defs();
                    let decl = (defs.def_at(method.id), Some(defs.def_at(impl_block.id)));
                    register_method_compiler_item(
                        &scope.tysys.type_table,
                        &method.attrs,
                        &method.name,
                        decl,
                        &scope.get_type_name(&impl_block.ty),
                        &struct_name,
                        &scope.current_module_source,
                        method.span,
                        scope.logger,
                    );
                }

                for method in &impl_block.methods {
                    // Set up method-level type parameters so that `V::Output`-style
                    // associated type projections can be resolved in the return type.
                    let mut method_type_param_names: Vec<String> = Vec::new();
                    let offset = scope.annotate_ctx.trait_ctx.type_params.len();
                    let self_binding = scope.self_binding();
                    for (i, param) in method.type_params.iter().enumerate() {
                        let idx = (offset + i) as u32;
                        let type_id = scope.tysys.type_table.borrow_mut().make_declared_param(
                            param.name.clone(),
                            idx,
                            param.is_pack,
                        );
                        scope.bind_param(
                            &param.name,
                            BinderInScope::declared(idx, type_id, param.id),
                            ScopedBound::pin_declared(param, self_binding),
                        );
                        method_type_param_names.push(param.name.clone());
                    }

                    let return_type = method
                        .return_type
                        .as_ref()
                        .map(|t| scope.resolve_type(t))
                        .unwrap_or(TypeTable::UNIT);

                    // Remove method-level type params from scope
                    for name in &method_type_param_names {
                        scope.annotate_ctx.trait_ctx.type_params.shift_remove(name);
                        scope
                            .annotate_ctx
                            .trait_ctx
                            .type_param_bounds
                            .shift_remove(name);
                    }

                    let mangled_name =
                        MethodName::format_local(&struct_name, trait_name.as_ref(), &method.name);
                    scope
                        .sem
                        .decls
                        .function_return_types
                        .insert(mangled_name, return_type);
                }

                // Type parameters, bounds, and associated type bindings
                // are auto-restored on `drop(scope)`.
                drop(scope);
            }
        }
    }

    pub(super) fn get_type_name(&self, ty: &Type) -> String {
        match ty {
            Type::Named(named) => named.name.clone(),
            Type::Generic(generic) => generic.name.clone(),
            // The `ns$Name` alias a namespace import registers, which is the
            // form the receiver registries are keyed by.
            Type::NamespacedGeneric(g) => self
                .sem
                .imports
                .canonical_ns_ref(&format!("{}::{}", g.namespace, g.name))
                .unwrap_or_else(|| g.name.clone()),
            Type::Reference(_) | Type::MutReference(_) => RefKind::from_ast(ty)
                .expect("ref classify")
                .prefix()
                .to_string(),
            Type::Tuple(elems) => {
                if elems.is_empty() {
                    UNIT_TYPE_NAME.to_string()
                } else {
                    TUPLE_TYPE_NAME.to_string()
                }
            }
            Type::Function(func_type) => {
                // Build function type string: "fn(T1, T2) -> R"
                let param_strs: Vec<String> = func_type
                    .params
                    .iter()
                    .map(|p| self.get_type_name(p))
                    .collect();
                let return_str = self.get_type_name(&func_type.return_type);
                format!("fn({}) -> {}", param_strs.join(", "), return_str)
            }
            _ => "Unknown".to_string(),
        }
    }

    pub(super) fn get_type_name_full(&self, ty: &Type) -> String {
        match ty {
            Type::Named(named) => named.name.clone(),
            Type::Generic(generic) => {
                let args: Vec<String> = generic
                    .args
                    .iter()
                    .map(|a| self.get_type_name_full(a))
                    .collect();
                format!("{}<{}>", generic.name, args.join(", "))
            }
            // What the programmer wrote: this renders for diagnostics, where
            // `get_type_name`'s `ns$Name` registry key is not a spelling.
            Type::NamespacedGeneric(g) => {
                let args: Vec<String> = g.args.iter().map(|a| self.get_type_name_full(a)).collect();
                format!("{}::{}<{}>", g.namespace, g.name, args.join(", "))
            }
            Type::Reference(inner) => format!("&{}", self.get_type_name_full(inner)),
            Type::MutReference(inner) => format!("&mut {}", self.get_type_name_full(inner)),
            _ => self.get_type_name(ty),
        }
    }
}
