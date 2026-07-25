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

        // Use module_source from the InstantiationKey, which carries
        // the module where the generic struct was defined.
        let struct_module_source = key.module_source.clone();

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
        let mut instance_ids = Vec::new();
        for id in type_table.iter_type_ids() {
            if let ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } = type_table.get(id)
                && name == &key.name
                && module_source == &key.module_source
                && type_args == &key.impl_type_args
            {
                self.structs.type_substitutions.insert(id, concrete_type_id);
                self.structs
                    .type_to_mangled_name
                    .insert(id, mangled_name.clone());
                instance_ids.push(id);
            }
        }

        // Build substitution map: type param index -> concrete type
        let substitution: IndexMap<u32, TypeId> = generic
            .type_params
            .iter()
            .zip(key.impl_type_args.iter())
            .map(|(param, &arg)| (param.index, arg))
            .collect();

        // Record this monomorphized struct's associated-type resolutions so
        // that projections like `I::Item` (where `I` is later substituted with
        // this monomorphized struct, which carries no type args) can still be
        // resolved by `resolve_assoc_type`.
        type_table.register_monomorphized_assoc_types(concrete_type_id, &key.name, &substitution);

        // Register them on the `GenericInstance` spelling too: a reflect
        // projection (`FieldTypes` / `Members`) is read off receivers that are
        // still spelled as instances at the call sites this pass has not yet
        // substituted, and those readers hold the table immutably.
        for id in instance_ids {
            type_table.register_monomorphized_assoc_types(id, &key.name, &substitution);
        }

        // Substitute types in fields (now self-references can be resolved)
        let fields: Vec<TirField> = generic
            .fields
            .iter()
            .map(|field| {
                let new_type_id = self.substitute_type(field.type_id, &substitution, type_table);
                TirField {
                    name: field.name.clone(),
                    visibility: field.visibility,
                    type_id: new_type_id,
                    index: field.index,
                    span: field.span,
                    is_secret: field.is_secret,
                    wire_name_override: field.wire_name_override.clone(),
                    serde_default: field.serde_default,
                    serde_positional: field.serde_positional,
                    default_expr: None,
                }
            })
            .collect();

        let concrete = TirStruct {
            name: mangled_name,
            module_source: key.module_source.clone(),
            visibility: generic.visibility,
            type_params: vec![], // Concrete struct has no type params
            monomorph_info: Some(MonomorphInfo {
                generic_name: generic.name.clone(),
                impl_type_args: key.impl_type_args.clone(),
                method_type_args: key.method_type_args.clone(),
                is_blanket: false,
            }),
            fields,
            span: generic.span,
            wire_name_policy: generic.wire_name_policy.clone(),
        };

        Some(concrete)
    }
}
