//! Struct instantiation: creating concrete structs from generic definitions.

use crate::tir::{InstantiationKey, MonomorphInfo, TirField, TirStruct, TypeArgMap};
use crate::type_table::TypeTable;

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
        let concrete_type_id =
            type_table.make_monomorphized_struct_from_args(generic.def, key.impl_type_args.clone());
        let head_def = key
            .def
            .expect("a struct instantiation names its declaration");

        let substitution: TypeArgMap = generic
            .type_params
            .iter()
            .map(|param| param.id)
            .zip(key.impl_type_args.iter().copied())
            .collect();

        // Record this monomorphized struct's associated-type resolutions so
        // that projections like `I::Item` (where `I` is later substituted with
        // this monomorphized struct, which carries no type args) can still be
        // resolved by `resolve_assoc_type`.
        // Same identity, same reason: the base declaration is `head_def`'s
        // own node, kept conditional on the declaration having been interned
        // (what the `(name, module)` type lookup this replaced tested for).
        let head_ast_id = type_table.defs().ast_id(head_def);
        let base_decl = type_table.type_of_symbol(&head_ast_id).map(|_| head_ast_id);
        if let Some(base_decl) = base_decl {
            type_table.register_mono_type(base_decl, concrete_type_id);
            type_table.register_monomorphized_assoc_types(
                concrete_type_id,
                base_decl,
                &key.impl_type_args,
            );
        }

        // Substitute types in fields
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
            def: generic.def,
            type_args: key.impl_type_args.clone(),
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
            wire_name_policy: generic.wire_name_policy,
        };

        Some(concrete)
    }
}
