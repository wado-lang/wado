//! Single module type/signature collection and name resolution helpers.

use crate::ast::{self, Item, Module, Type};
use crate::compiler_host::CompilerHost;
use crate::tir::{TypeId, TypeTable};

use super::Resolver;
use super::types::{
    EnumCaseData, EnumInfo, FlagsInfo, FlagsMemberData, GenericNewtypeInfo, StructFieldInfo,
    VariantCaseData, VariantInfo,
};
use crate::name::MethodName;

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn collect_types(&mut self, module: &Module) {
        // First, collect types from loaded modules (so aliases like Instant = u64 are available)
        for (module_source, loaded_module) in self.loaded_modules {
            for item in &loaded_module.items {
                if let Item::Newtype(newtype_decl) = item {
                    if !newtype_decl.type_params.is_empty() {
                        // Generic newtype: store definition only if not already present
                        if self.lookup_generic_newtype(&newtype_decl.name).is_none() {
                            let type_params = newtype_decl
                                .type_params
                                .iter()
                                .map(|p| p.name.clone())
                                .collect();
                            self.local_generic_newtypes.insert(
                                newtype_decl.name.clone(),
                                GenericNewtypeInfo {
                                    module_source: module_source.clone(),
                                    type_params,
                                    base_type_ast: newtype_decl.ty.clone(),
                                },
                            );
                        }
                    } else if self.lookup_newtype(&newtype_decl.name).is_none() {
                        // Concrete newtype: resolve immediately
                        let base_type_id = self.resolve_type(&newtype_decl.ty);
                        let newtype_id = self.type_table.borrow_mut().make_newtype(
                            newtype_decl.name.clone(),
                            module_source.clone(),
                            base_type_id,
                        );
                        self.local_newtypes
                            .insert(newtype_decl.name.clone(), newtype_id);
                    }
                }
            }
        }

        // First pass: collect generic struct names from loaded modules (needed for resolve_generic_type)
        for loaded_module in self.loaded_modules.values() {
            for item in &loaded_module.items {
                if let Item::Struct(struct_decl) = item
                    && !struct_decl.type_params.is_empty()
                {
                    self.generic_struct_names.insert(struct_decl.name.clone());
                }
            }
        }

        // Also collect generic struct names from the current module
        for item in &module.items {
            if let Item::Struct(struct_decl) = item
                && !struct_decl.type_params.is_empty()
            {
                self.generic_struct_names.insert(struct_decl.name.clone());
            }
        }

        // Then collect struct fields from the main module
        for item in &module.items {
            match item {
                Item::Struct(struct_decl) => {
                    // Set up type parameters in scope for resolving field types.
                    // Use an inherited scope so caller-provided context stays
                    // visible — only `type_params` and `type_param_bounds` are
                    // replaced, matching the original `mem::take` semantics.
                    let mut scope = self.enter_inherited_type_param_scope();
                    scope.trait_ctx.type_params.clear();
                    scope.trait_ctx.type_param_bounds.clear();
                    scope.register_generic_params(&struct_decl.type_params, 0);

                    let mut fields = Vec::new();
                    let mut field_ast_ids = Vec::new();
                    let mut field_defaults: Vec<Option<ast::Expr>> = Vec::new();
                    for field in &struct_decl.fields {
                        let type_id = scope.resolve_type(&field.ty);
                        fields.push((field.name.clone(), type_id, field.is_pub));
                        field_ast_ids.push(field.id);
                        field_defaults.push(field.default.clone());
                    }
                    // Extract type parameter bounds
                    let type_param_bounds: Vec<(String, Vec<String>)> = struct_decl
                        .type_params
                        .iter()
                        .map(|p| {
                            (
                                p.name.clone(),
                                p.bounds.iter().map(|b| b.name.clone()).collect(),
                            )
                        })
                        .collect();
                    // Collect TypeIds for struct's own type params in declaration order.
                    let type_param_type_ids: Vec<TypeId> = struct_decl
                        .type_params
                        .iter()
                        .enumerate()
                        .map(|(i, param)| {
                            scope
                                .type_table
                                .borrow_mut()
                                .make_type_param(param.name.clone(), i as u32)
                        })
                        .collect();

                    let module_source = scope.current_module_source.clone();
                    scope.local_struct_fields.insert(
                        struct_decl.name.clone(),
                        StructFieldInfo {
                            name: struct_decl.name.clone(),
                            module_source,
                            fields,
                            field_ast_ids,
                            field_defaults,
                            type_param_bounds,
                            type_param_type_ids,
                        },
                    );

                    drop(scope);
                }
                Item::Newtype(newtype_decl) => {
                    if newtype_decl.type_params.is_empty() {
                        // Concrete newtype: resolve immediately
                        let base_type_id = self.resolve_type(&newtype_decl.ty);
                        let newtype_id = self.type_table.borrow_mut().make_newtype(
                            newtype_decl.name.clone(),
                            self.current_module_source.clone(),
                            base_type_id,
                        );
                        self.local_newtypes
                            .insert(newtype_decl.name.clone(), newtype_id);
                    } else {
                        // Generic newtype: store definition for lazy instantiation
                        let type_params = newtype_decl
                            .type_params
                            .iter()
                            .map(|p| p.name.clone())
                            .collect();
                        self.local_generic_newtypes.insert(
                            newtype_decl.name.clone(),
                            GenericNewtypeInfo {
                                module_source: self.current_module_source.clone(),
                                type_params,
                                base_type_ast: newtype_decl.ty.clone(),
                            },
                        );
                    }
                }
                Item::Variant(variant_decl) => {
                    // Set up type parameters in scope for resolving field types.
                    // Use an inherited scope so caller-provided context stays
                    // visible — only `type_params` is replaced, matching the
                    // original `mem::take(&mut self.trait_ctx.type_params)`.
                    let mut scope = self.enter_inherited_type_param_scope();
                    scope.trait_ctx.type_params.clear();
                    scope.register_generic_params(&variant_decl.type_params, 0);
                    let type_param_type_ids: Vec<TypeId> = variant_decl
                        .type_params
                        .iter()
                        .filter_map(|p| scope.trait_ctx.type_params.get(&p.name).map(|&(_, id)| id))
                        .collect();

                    // Collect type parameters
                    let type_params: Vec<String> = variant_decl
                        .type_params
                        .iter()
                        .map(|p| p.name.clone())
                        .collect();

                    // Collect variant cases with resolved payload types
                    let mut cases = Vec::new();
                    for case in &variant_decl.cases {
                        // Each variant case has exactly one payload type.
                        // Unit variants have `()` (unit type) payload.
                        let payload = if let Some(payload_ty) = &case.payload {
                            scope.resolve_type(payload_ty)
                        } else {
                            TypeTable::UNIT
                        };
                        cases.push(VariantCaseData {
                            name: case.name.clone(),
                            payload,
                            ast_id: case.id,
                        });
                    }

                    let module_source = scope.current_module_source.clone();
                    scope.local_variant_cases.insert(
                        variant_decl.name.clone(),
                        VariantInfo {
                            name: variant_decl.name.clone(),
                            module_source: module_source.clone(),
                            type_params,
                            cases,
                            type_param_type_ids,
                        },
                    );

                    let comp_features = super::item::extract_comp_features(&variant_decl.attrs);
                    if comp_features != 0 {
                        scope
                            .type_table
                            .borrow_mut()
                            .register_comp_feature_variant(comp_features, module_source);
                    }

                    drop(scope);
                }
                Item::Enum(enum_decl) => {
                    // Collect enum cases (no field types, just names and indices)
                    let cases: Vec<EnumCaseData> = enum_decl
                        .cases
                        .iter()
                        .enumerate()
                        .map(|(index, case)| EnumCaseData {
                            name: case.name.clone(),
                            index: index as u32,
                            ast_id: case.id,
                        })
                        .collect();
                    self.local_enum_cases.insert(
                        enum_decl.name.clone(),
                        EnumInfo::new(
                            enum_decl.name.clone(),
                            self.current_module_source.clone(),
                            cases,
                        ),
                    );
                }
                Item::Flags(flags_decl) => {
                    // Create a distinct Flags type (not a newtype over u32)
                    let flags_type = self
                        .type_table
                        .borrow_mut()
                        .make_flags(flags_decl.name.clone(), self.current_module_source.clone());
                    // Add to newtypes so it can be used as a type name
                    self.local_newtypes
                        .insert(flags_decl.name.clone(), flags_type);
                    // Store member info with bitmask values (1 << index)
                    let members: Vec<FlagsMemberData> = flags_decl
                        .flags
                        .iter()
                        .enumerate()
                        .map(|(i, m)| FlagsMemberData {
                            name: m.name.clone(),
                            bitmask: 1u32 << i,
                            ast_id: m.id,
                        })
                        .collect();
                    self.local_flags_cases.insert(
                        flags_decl.name.clone(),
                        FlagsInfo {
                            type_id: flags_type,
                            members,
                            module_source: self.current_module_source.clone(),
                        },
                    );
                }
                Item::Trait(trait_decl) => {
                    let comp_features = super::item::extract_comp_features(&trait_decl.attrs);
                    if comp_features != 0 {
                        self.type_table.borrow_mut().register_comp_feature_trait(
                            comp_features,
                            self.current_module_source.clone(),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Collect function signatures for call resolution
    pub(super) fn collect_function_signatures(&mut self, module: &Module) {
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    // Set up the function's own type parameters before resolving the return type,
                    // so that associated type projections like `V::Output` can be resolved.
                    // Use inherited scope to preserve caller-provided context;
                    // only `type_params` and `type_param_bounds` are replaced
                    // (matching the original `mem::take` semantics).
                    let mut scope = self.enter_inherited_type_param_scope();
                    scope.trait_ctx.type_params.clear();
                    scope.trait_ctx.type_param_bounds.clear();
                    scope.register_generic_params(&func.type_params, 0);
                    let return_type = func
                        .return_type
                        .as_ref()
                        .map(|t| scope.resolve_type(t))
                        .unwrap_or(TypeTable::UNIT);
                    drop(scope);
                    self.function_return_types
                        .insert(func.name.clone(), return_type);
                }
                Item::Impl(impl_block) => {
                    // Set up type parameters from impl block before resolving method signatures.
                    // Use inherited scope to preserve caller-provided context;
                    // only `type_params`, `type_param_bounds`, and
                    // `assoc_type_bindings` are replaced (matching the
                    // original `mem::take` semantics for those fields).
                    let mut scope = self.enter_inherited_type_param_scope();
                    scope.trait_ctx.type_params.clear();
                    scope.trait_ctx.type_param_bounds.clear();
                    scope.trait_ctx.assoc_type_bindings.clear();

                    // First, collect explicit type params from impl<T>, skipping concrete types
                    // (e.g., `impl<i32, T> IndexValue<i32> for Triple<T>` — skip "i32").
                    let mut actual_idx = 0u32;
                    for param in &impl_block.type_params {
                        if scope.is_known_type_name(&param.name) {
                            continue;
                        }
                        let type_id = if param.is_pack {
                            scope
                                .type_table
                                .borrow_mut()
                                .make_type_pack(param.name.clone(), actual_idx)
                        } else {
                            scope
                                .type_table
                                .borrow_mut()
                                .make_type_param(param.name.clone(), actual_idx)
                        };
                        scope
                            .trait_ctx
                            .type_params
                            .insert(param.name.clone(), (actual_idx, type_id));
                        scope
                            .trait_ctx
                            .type_param_decls
                            .insert(param.name.clone(), param.id);
                        if !param.bounds.is_empty() {
                            scope
                                .trait_ctx
                                .type_param_bounds
                                .insert(param.name.clone(), param.bounds.clone());
                        }
                        actual_idx += 1;
                    }

                    // Also collect type params from generic type: impl Array<T> {...}
                    // The type args in Array<T> are type parameters.
                    // For ref types (impl Trait for &Container<T>), unwrap the reference first.
                    let impl_inner_ty = match &impl_block.ty {
                        ast::Type::Reference(inner) | ast::Type::MutReference(inner) => {
                            inner.as_ref()
                        }
                        other => other,
                    };
                    if let ast::Type::Generic(generic) = impl_inner_ty {
                        let offset = actual_idx as usize;
                        for (i, arg) in generic.args.iter().enumerate() {
                            if let ast::Type::Named(named) = arg {
                                let name = &named.name;
                                if !scope.trait_ctx.type_params.contains_key(name)
                                    && !scope.is_known_type_name(name)
                                {
                                    let index = (offset + i) as u32;
                                    let type_id = scope
                                        .type_table
                                        .borrow_mut()
                                        .make_type_param(name.clone(), index);
                                    scope
                                        .trait_ctx
                                        .type_params
                                        .insert(name.clone(), (index, type_id));
                                }
                            }
                        }
                    }

                    // Set up associated type bindings for trait implementations
                    if impl_block.trait_type.is_some() {
                        for binding in &impl_block.associated_types {
                            let type_id = scope.resolve_type(&binding.ty);
                            scope
                                .trait_ctx
                                .assoc_type_bindings
                                .insert(binding.name.clone(), type_id);
                        }
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

                    // Collect method signatures with mangled names
                    let struct_name = scope.get_type_name(&impl_block.ty);
                    let trait_name = impl_block
                        .trait_type
                        .as_ref()
                        .map(|t| scope.get_type_name(t));

                    for method in &impl_block.methods {
                        // Set up method-level type parameters so that `V::Output`-style
                        // associated type projections can be resolved in the return type.
                        let mut method_type_param_names: Vec<String> = Vec::new();
                        let offset = scope.trait_ctx.type_params.len();
                        for (i, param) in method.type_params.iter().enumerate() {
                            let idx = (offset + i) as u32;
                            let type_id = scope
                                .type_table
                                .borrow_mut()
                                .make_type_param(param.name.clone(), idx);
                            scope
                                .trait_ctx
                                .type_params
                                .insert(param.name.clone(), (idx, type_id));
                            scope
                                .trait_ctx
                                .type_param_decls
                                .insert(param.name.clone(), param.id);
                            if !param.bounds.is_empty() {
                                scope
                                    .trait_ctx
                                    .type_param_bounds
                                    .insert(param.name.clone(), param.bounds.clone());
                            }
                            method_type_param_names.push(param.name.clone());
                        }

                        let return_type = method
                            .return_type
                            .as_ref()
                            .map(|t| scope.resolve_type(t))
                            .unwrap_or(TypeTable::UNIT);

                        // Remove method-level type params from scope
                        for name in &method_type_param_names {
                            scope.trait_ctx.type_params.shift_remove(name);
                            scope.trait_ctx.type_param_bounds.shift_remove(name);
                        }

                        let mangled_name = MethodName::format_local(
                            &struct_name,
                            trait_name.as_deref(),
                            &method.name,
                        );
                        scope
                            .function_return_types
                            .insert(mangled_name, return_type);
                    }

                    // Type parameters, bounds, and associated type bindings
                    // are auto-restored on `drop(scope)`.
                    drop(scope);
                }
                _ => {}
            }
        }
    }

    pub(super) fn get_type_name_static(ty: &Type) -> String {
        match ty {
            Type::Named(named) if named.name == "()" => TypeTable::UNIT_TYPE_NAME.to_string(),
            Type::Named(named) => named.name.clone(),
            Type::Generic(generic) => generic.name.clone(),
            Type::Reference(_) => "&".to_string(),
            Type::MutReference(_) => "&mut".to_string(),
            Type::Tuple(elems) => {
                if elems.is_empty() {
                    TypeTable::UNIT_TYPE_NAME.to_string()
                } else {
                    TypeTable::TUPLE_TYPE_NAME.to_string()
                }
            }
            _ => "Unknown".to_string(),
        }
    }

    pub(super) fn get_type_name(&self, ty: &Type) -> String {
        match ty {
            Type::Named(named) if named.name == "()" => TypeTable::UNIT_TYPE_NAME.to_string(),
            Type::Named(named) => named.name.clone(),
            Type::Generic(generic) => generic.name.clone(),
            Type::Reference(_) => "&".to_string(),
            Type::MutReference(_) => "&mut".to_string(),
            Type::Tuple(elems) => {
                if elems.is_empty() {
                    TypeTable::UNIT_TYPE_NAME.to_string()
                } else {
                    TypeTable::TUPLE_TYPE_NAME.to_string()
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
            Type::Reference(inner) => format!("&{}", self.get_type_name_full(inner)),
            Type::MutReference(inner) => format!("&mut {}", self.get_type_name_full(inner)),
            _ => self.get_type_name(ty),
        }
    }
}
