//! Single module type/signature collection and name resolution helpers.

use crate::hashmap::IndexMap;

use crate::ast::{self, Item, Module, Type};
use crate::compiler_host::CompilerHost;
use crate::name::ModuleSource;
use crate::tir::{TypeId, TypeTable};

use super::Resolver;
use super::types::{
    BlanketTraitImplIndex, EnumCaseData, EnumInfo, FlagsInfo, FlagsMemberData, StructFieldInfo,
    TraitDeclIndex, TraitImplIndex, VariantCaseData, VariantInfo,
};
use crate::name::MethodName;
use std::sync::Arc;

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn collect_types(&mut self, module: &Module) {
        // First, collect types from loaded modules (so aliases like Instant = u64 are available)
        for (module_source, loaded_module) in self.loaded_modules {
            for item in &loaded_module.items {
                if let Item::Type(newtype_decl) = item {
                    // Only add if not already present (main module takes priority)
                    if !self.newtypes.contains_key(&newtype_decl.name) {
                        // Resolve the base type
                        let base_type_id = self.resolve_type(&newtype_decl.ty);
                        // Create a newtype wrapping the base type
                        let newtype_id = self.type_table.borrow_mut().make_newtype(
                            newtype_decl.name.clone(),
                            module_source.clone(),
                            base_type_id,
                        );
                        self.newtypes.insert(newtype_decl.name.clone(), newtype_id);
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
                    // Set up type parameters in scope for resolving field types
                    let old_type_params = std::mem::take(&mut self.current_type_params);
                    for (index, param) in struct_decl.type_params.iter().enumerate() {
                        let type_id = self
                            .type_table
                            .borrow_mut()
                            .make_type_param(param.name.clone(), index as u32);
                        self.current_type_params
                            .insert(param.name.clone(), (index as u32, type_id));
                    }

                    let mut fields = Vec::new();
                    for field in &struct_decl.fields {
                        let type_id = self.resolve_type(&field.ty);
                        fields.push((field.name.clone(), type_id, field.is_pub));
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
                            self.type_table
                                .borrow_mut()
                                .make_type_param(param.name.clone(), i as u32)
                        })
                        .collect();

                    self.struct_fields.insert(
                        struct_decl.name.clone(),
                        StructFieldInfo {
                            module_source: self.current_module_source.clone(),
                            fields,
                            type_param_bounds,
                            type_param_type_ids,
                        },
                    );

                    // Restore type params scope
                    self.current_type_params = old_type_params;
                }
                Item::Type(newtype_decl) => {
                    // Resolve the base type
                    let base_type_id = self.resolve_type(&newtype_decl.ty);
                    // Create a newtype wrapping the base type
                    let newtype_id = self.type_table.borrow_mut().make_newtype(
                        newtype_decl.name.clone(),
                        self.current_module_source.clone(),
                        base_type_id,
                    );
                    self.newtypes.insert(newtype_decl.name.clone(), newtype_id);
                }
                Item::Variant(variant_decl) => {
                    // Set up type parameters in scope for resolving field types
                    let old_type_params = std::mem::take(&mut self.current_type_params);
                    let mut type_param_type_ids = Vec::new();
                    for (index, param) in variant_decl.type_params.iter().enumerate() {
                        let type_id = self
                            .type_table
                            .borrow_mut()
                            .make_type_param(param.name.clone(), index as u32);
                        self.current_type_params
                            .insert(param.name.clone(), (index as u32, type_id));
                        type_param_type_ids.push(type_id);
                    }

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
                            self.resolve_type(payload_ty)
                        } else {
                            TypeTable::UNIT
                        };
                        cases.push(VariantCaseData {
                            name: case.name.clone(),
                            payload,
                        });
                    }

                    self.variant_cases.insert(
                        variant_decl.name.clone(),
                        VariantInfo {
                            module_source: self.current_module_source.clone(),
                            type_params,
                            cases,
                            type_param_type_ids,
                        },
                    );

                    let comp_features = super::item::extract_comp_features(&variant_decl.attrs);
                    if comp_features != 0 {
                        self.type_table.borrow_mut().register_comp_feature_variant(
                            comp_features,
                            self.current_module_source.clone(),
                        );
                    }

                    // Restore type params scope
                    self.current_type_params = old_type_params;
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
                        })
                        .collect();
                    self.enum_cases.insert(
                        enum_decl.name.clone(),
                        EnumInfo {
                            module_source: self.current_module_source.clone(),
                            cases,
                        },
                    );
                }
                Item::Flags(flags_decl) => {
                    // Create a newtype over u32 for the flags type
                    let flags_type = self.type_table.borrow_mut().make_newtype(
                        flags_decl.name.clone(),
                        self.current_module_source.clone(),
                        TypeTable::U32,
                    );
                    // Add to newtypes so it can be used as a type name
                    self.newtypes.insert(flags_decl.name.clone(), flags_type);
                    // Store member info with bitmask values (1 << index)
                    let members: Vec<FlagsMemberData> = flags_decl
                        .flags
                        .iter()
                        .enumerate()
                        .map(|(i, m)| FlagsMemberData {
                            name: m.name.clone(),
                            bitmask: 1u32 << i,
                        })
                        .collect();
                    self.flags_cases.insert(
                        flags_decl.name.clone(),
                        FlagsInfo {
                            type_id: flags_type,
                            members,
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
                    let return_type = func
                        .return_type
                        .as_ref()
                        .map(|t| self.resolve_type(t))
                        .unwrap_or(TypeTable::UNIT);
                    self.function_return_types
                        .insert(func.name.clone(), return_type);
                }
                Item::Impl(impl_block) => {
                    // Set up type parameters from impl block before resolving method signatures
                    let old_type_params = std::mem::take(&mut self.current_type_params);
                    let old_type_param_bounds = std::mem::take(&mut self.current_type_param_bounds);

                    // First, collect explicit type params from impl<T>, skipping concrete types
                    // (e.g., `impl<i32, T> IndexValue<i32> for Triple<T>` — skip "i32").
                    let mut actual_idx = 0u32;
                    for param in &impl_block.type_params {
                        if self.is_known_type_name(&param.name) {
                            continue;
                        }
                        let type_id = self
                            .type_table
                            .borrow_mut()
                            .make_type_param(param.name.clone(), actual_idx);
                        self.current_type_params
                            .insert(param.name.clone(), (actual_idx, type_id));
                        if !param.bounds.is_empty() {
                            self.current_type_param_bounds.insert(
                                param.name.clone(),
                                param.bounds.iter().map(|b| b.name.clone()).collect(),
                            );
                        }
                        actual_idx += 1;
                    }

                    // Also collect type params from generic type: impl Array<T> {...}
                    // The type args in Array<T> are type parameters
                    if let ast::Type::Generic(generic) = &impl_block.ty {
                        let offset = actual_idx as usize;
                        for (i, arg) in generic.args.iter().enumerate() {
                            if let ast::Type::Named(named) = arg {
                                let name = &named.name;
                                if !self.current_type_params.contains_key(name)
                                    && !self.is_known_type_name(name)
                                {
                                    let index = (offset + i) as u32;
                                    let type_id = self
                                        .type_table
                                        .borrow_mut()
                                        .make_type_param(name.clone(), index);
                                    self.current_type_params
                                        .insert(name.clone(), (index, type_id));
                                }
                            }
                        }
                    }

                    // Set up associated type bindings for trait implementations
                    let old_associated_type_bindings =
                        std::mem::take(&mut self.current_associated_type_bindings);
                    if impl_block.trait_type.is_some() {
                        for binding in &impl_block.associated_types {
                            let type_id = self.resolve_type(&binding.ty);
                            self.current_associated_type_bindings
                                .insert(binding.name.clone(), type_id);
                        }
                    }

                    // Collect method signatures with mangled names
                    let struct_name = self.get_type_name(&impl_block.ty);
                    let trait_name = impl_block
                        .trait_type
                        .as_ref()
                        .map(|t| self.get_type_name(t));

                    for method in &impl_block.methods {
                        let return_type = method
                            .return_type
                            .as_ref()
                            .map(|t| self.resolve_type(t))
                            .unwrap_or(TypeTable::UNIT);

                        let mangled_name = MethodName::format_local(
                            &struct_name,
                            trait_name.as_deref(),
                            &method.name,
                        );
                        self.function_return_types.insert(mangled_name, return_type);
                    }

                    // Restore type parameters, bounds, and associated type bindings
                    self.current_type_params = old_type_params;
                    self.current_type_param_bounds = old_type_param_bounds;
                    self.current_associated_type_bindings = old_associated_type_bindings;
                }
                _ => {}
            }
        }
    }

    pub(super) fn get_type_name_static(ty: &Type) -> String {
        match ty {
            Type::Named(named) => named.name.clone(),
            Type::Generic(generic) => generic.name.clone(),
            Type::Reference(inner) | Type::MutReference(inner) => Self::get_type_name_static(inner),
            _ => "Unknown".to_string(),
        }
    }

    /// Build trait impl and trait declaration indices from all loaded modules.
    /// Called once in `resolve_all_modules` before per-module resolution begins.
    /// The indices enable O(1) trait lookup by type/trait name instead of scanning all modules.
    pub(super) fn build_trait_indices(
        modules: &IndexMap<ModuleSource, Module>,
    ) -> (
        Arc<TraitImplIndex>,
        Arc<TraitDeclIndex>,
        Arc<BlanketTraitImplIndex>,
    ) {
        let mut impl_index: TraitImplIndex = IndexMap::default();
        let mut decl_index: TraitDeclIndex = IndexMap::default();
        let mut blanket_index: BlanketTraitImplIndex = Vec::new();
        for (module_source, module) in modules {
            for (item_idx, item) in module.items.iter().enumerate() {
                match item {
                    Item::Impl(impl_block) if impl_block.trait_type.is_some() => {
                        let type_name = Self::get_type_name_static(&impl_block.ty);
                        // Detect blanket impls: impl_ty is a type parameter from type_params
                        let is_blanket = impl_block
                            .type_params
                            .iter()
                            .any(|tp| tp.name == type_name && !tp.bounds.is_empty());
                        if is_blanket {
                            blanket_index.push((module_source.clone(), item_idx));
                        }
                        impl_index
                            .entry(type_name)
                            .or_default()
                            .push((module_source.clone(), item_idx));
                    }
                    Item::Trait(trait_decl) => {
                        decl_index
                            .entry(trait_decl.name.clone())
                            .or_insert((module_source.clone(), item_idx));
                    }
                    _ => {}
                }
            }
        }
        (
            Arc::new(impl_index),
            Arc::new(decl_index),
            Arc::new(blanket_index),
        )
    }

    pub(super) fn get_type_name(&self, ty: &Type) -> String {
        match ty {
            Type::Named(named) => named.name.clone(),
            Type::Generic(generic) => generic.name.clone(),
            Type::Reference(inner) => self.get_type_name(inner),
            Type::MutReference(inner) => self.get_type_name(inner),
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
}
