//! Struct instantiation: creating concrete structs from generic definitions.

use crate::hashmap::IndexMap;
use crate::tir::{
    InstantiationKey, MonomorphInfo, ResolvedType, TirField, TirStruct, TypeId, TypeTable,
};

use super::state::Monomorphizer;

impl Monomorphizer {
    /// Instantiate a generic struct with concrete type arguments
    pub fn instantiate_struct(
        &mut self,
        generic: &TirStruct,
        key: &InstantiationKey,
        type_table: &mut TypeTable,
    ) -> Option<TirStruct> {
        let mangled_name = self.structs.instantiated.get(key)?.clone();

        // Find the GenericInstance's module_source from the type table.
        // Use the generic's original module (where it was defined) for the struct type,
        // ensuring consistency across modules that share the same type table.
        let struct_module_source = type_table
            .iter_type_ids()
            .find_map(|id| {
                if let ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args,
                } = type_table.get(id)
                    && name == &key.name
                    && type_args == &key.impl_type_args
                {
                    Some(module_source.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| self.current_module_source.clone());

        // Register the concrete struct type in the type table BEFORE substituting field types.
        // This is critical for self-referential structs like:
        //   struct Node<T> { left: Option<&mut Node<T>>, right: Option<&mut Node<T>> }
        // When substituting field types, the inner Node<T> needs to resolve to the
        // monomorphized struct type, not a GenericInstance.
        let concrete_type_id = type_table.make_monomorphized_struct(
            mangled_name.clone(),
            struct_module_source,
            key.name.clone(), // base_name: the original generic struct name
        );

        // Find the GenericInstance TypeId and record the substitution early
        // so that substitute_type can use it for self-references
        for id in type_table.iter_type_ids() {
            if let ResolvedType::GenericInstance {
                name, type_args, ..
            } = type_table.get(id)
                && name == &key.name
                && type_args == &key.impl_type_args
            {
                self.structs.type_substitutions.insert(id, concrete_type_id);
                self.structs.type_to_mangled_name.insert(
                    id,
                    self.structs
                        .instantiated
                        .get(key)
                        .cloned()
                        .unwrap_or_default(),
                );
            }
        }

        // Build substitution map: type param index -> concrete type
        let substitution: IndexMap<u32, TypeId> = generic
            .type_params
            .iter()
            .zip(key.impl_type_args.iter())
            .map(|(param, &arg)| (param.index, arg))
            .collect();

        // Substitute types in fields (now self-references can be resolved)
        let fields: Vec<TirField> = generic
            .fields
            .iter()
            .map(|field| {
                let new_type_id = self.substitute_type(field.type_id, &substitution, type_table);
                TirField {
                    name: field.name.clone(),
                    is_pub: field.is_pub,
                    type_id: new_type_id,
                    index: field.index,
                    span: field.span,
                    is_hidden: field.is_hidden,
                    serde_rename: field.serde_rename.clone(),
                    serde_default: field.serde_default,
                }
            })
            .collect();

        // Create the monomorphized struct
        let concrete = TirStruct {
            name: mangled_name,
            is_pub: generic.is_pub,
            type_params: vec![], // Concrete struct has no type params
            monomorph_info: Some(MonomorphInfo {
                generic_name: generic.name.clone(),
                impl_type_args: key.impl_type_args.clone(),
                method_type_args: key.method_type_args.clone(),
                is_blanket: false,
            }),
            fields,
            span: generic.span,
            serde_rename_all: generic.serde_rename_all.clone(),
        };

        Some(concrete)
    }
}
