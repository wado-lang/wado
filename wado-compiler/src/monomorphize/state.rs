//! Monomorphizer state: instantiation tracking and name generation.

use crate::hashmap::IndexMap;
use crate::name::{MethodName, ModuleSource, mangle_generic_name};
use crate::tir::{InstantiationKey, ResolvedType, TypeId, TypeTable};

/// Tracks struct monomorphization state
pub(super) struct StructInstState {
    /// Map from (`generic_name`, `type_args`) to mangled name
    pub instantiated: IndexMap<InstantiationKey, String>,
    /// Work queue of pending struct instantiations
    pub pending: Vec<InstantiationKey>,
    /// Map from `GenericInstance` `TypeId` to monomorphized Struct `TypeId`
    pub type_substitutions: IndexMap<TypeId, TypeId>,
    /// Map from `GenericInstance` `TypeId` to mangled struct name
    pub type_to_mangled_name: IndexMap<TypeId, String>,
    /// Reverse lookup: mangled struct name -> `InstantiationKey`
    pub mangled_to_key: IndexMap<String, InstantiationKey>,
}

/// Tracks function monomorphization state
pub(super) struct FuncInstState {
    /// Map from (`generic_func_name`, `type_args`) to mangled function name
    pub instantiated: IndexMap<InstantiationKey, String>,
    /// Work queue of pending function instantiations
    pub pending: Vec<InstantiationKey>,
    /// Reverse lookup: mangled function name -> `InstantiationKey`
    pub mangled_to_key: IndexMap<String, InstantiationKey>,
    /// Map from concrete trait method function name → module where it's defined.
    /// Used to resolve the correct module when substituting type param receivers.
    pub trait_method_locations: IndexMap<String, ModuleSource>,
}

/// Monomorphizer collects generic instantiations and generates concrete types
pub(super) struct Monomorphizer {
    /// The module source where monomorphized entities are being generated
    pub current_module_source: ModuleSource,
    pub structs: StructInstState,
    pub functions: FuncInstState,
    /// Number of impl-level type params in the function currently being instantiated.
    /// Set by `instantiate_function` before calling `substitute_types_in_block`.
    /// Used to distinguish impl-level (struct) type params from method-level type params
    /// in the substitution map when rewriting static method calls.
    pub current_impl_type_param_count: usize,
    /// Base struct name of the impl block being instantiated (e.g., `TreeMap` for
    /// `impl<K,V> TreeMap<K,V>`). Used to restrict impl type arg propagation to
    /// calls on the same struct — calls to other structs within the same impl block
    /// must not receive these type args.
    pub current_impl_struct_name: String,
}

impl Monomorphizer {
    pub fn new(current_module_source: ModuleSource) -> Self {
        Self {
            current_module_source,
            structs: StructInstState {
                instantiated: IndexMap::default(),
                pending: Vec::new(),
                type_substitutions: IndexMap::default(),
                type_to_mangled_name: IndexMap::default(),
                mangled_to_key: IndexMap::default(),
            },
            functions: FuncInstState {
                instantiated: IndexMap::default(),
                pending: Vec::new(),
                mangled_to_key: IndexMap::default(),
                trait_method_locations: IndexMap::default(),
            },
            current_impl_type_param_count: 0,
            current_impl_struct_name: String::new(),
        }
    }

    /// Queue a struct instantiation if not already queued. Returns true if newly queued.
    pub fn try_queue_struct(&mut self, key: InstantiationKey, mangled_name: String) -> bool {
        if self.structs.instantiated.contains_key(&key) {
            return false;
        }
        self.structs
            .instantiated
            .insert(key.clone(), mangled_name.clone());
        self.structs
            .mangled_to_key
            .insert(mangled_name, key.clone());
        self.structs.pending.push(key);
        true
    }

    /// Queue a function instantiation if not already queued. Returns true if newly queued.
    pub fn try_queue_function(&mut self, key: InstantiationKey, mangled_name: String) -> bool {
        if self.functions.instantiated.contains_key(&key) {
            return false;
        }
        self.functions
            .instantiated
            .insert(key.clone(), mangled_name.clone());
        self.functions
            .mangled_to_key
            .insert(mangled_name, key.clone());
        self.functions.pending.push(key);
        true
    }

    /// Generate monomorphized struct name: `Box` + `[i32]` -> `"Box<i32>"`
    pub fn instantiation_name(&self, key: &InstantiationKey, type_table: &TypeTable) -> String {
        let args: Vec<String> = key
            .impl_type_args
            .iter()
            .map(|&t| type_table.mangle_type_name(t))
            .collect();
        mangle_generic_name(&key.name, &args)
    }

    /// Generate instantiated function name: `identity` + `[i32]` -> `"identity<i32>"`
    pub fn function_instantiation_name(
        &self,
        key: &InstantiationKey,
        type_table: &TypeTable,
    ) -> String {
        // For free functions, all type args are method-level.
        // For fallback from method_instantiation_name_inner (no method_info),
        // combine both for backwards-compatible naming.
        let mut args: Vec<String> = key
            .impl_type_args
            .iter()
            .map(|t| type_table.mangle_type_name(*t))
            .collect();
        args.extend(
            key.method_type_args
                .iter()
                .map(|t| type_table.mangle_type_name(*t)),
        );
        mangle_generic_name(&key.name, &args)
    }

    /// Generate instantiated method name
    /// Format: `StructWithImplArgs::methodWithMethodArgs`
    /// e.g., `Container::transform` with `[i32, i64]` and `impl_type_params_count=1` -> `"Container<i32>::transform<i64>"`
    pub fn method_instantiation_name(
        &self,
        key: &InstantiationKey,
        type_table: &TypeTable,
        impl_type_params_count: usize,
    ) -> String {
        self.method_instantiation_name_inner(key, type_table, impl_type_params_count, &[])
    }

    pub fn method_instantiation_name_inner(
        &self,
        key: &InstantiationKey,
        type_table: &TypeTable,
        impl_type_params_count: usize,
        impl_type_params: &[crate::tir::TirTypeParam],
    ) -> String {
        // Use method_info metadata instead of parsing key.name
        let Some(ref method_info) = key.method_info else {
            // Fallback to regular function naming if no method_info
            return self.function_instantiation_name(key, type_table);
        };

        // impl_type_args and method_type_args are now separate in InstantiationKey
        let _ = impl_type_params_count; // no longer needed for split

        let impl_arg_names: Vec<String> = key
            .impl_type_args
            .iter()
            .map(|t| type_table.mangle_type_name(*t))
            .collect();

        // Blanket impl: struct name IS the type param (e.g., "I").
        // Detected by checking if base_struct_name matches an impl type param name.
        let is_blanket = impl_type_params
            .iter()
            .any(|p| p.name == method_info.base_struct_name);

        let mangled_struct = if is_blanket && !impl_arg_names.is_empty() {
            // Replace struct name entirely: "I" → "StrCharIter"
            MethodName::format_struct_with_args(
                &impl_arg_names[0],
                &[],
                method_info.trait_name.as_deref(),
            )
        } else {
            // Normal: append type args: "Array" → "Array<i32>"
            MethodName::format_struct_with_args(
                &method_info.struct_name,
                &impl_arg_names,
                method_info.trait_name.as_deref(),
            )
        };

        // Build method name: transform<i64> (using method type args)
        let method_arg_names: Vec<String> = key
            .method_type_args
            .iter()
            .map(|t| type_table.mangle_type_name(*t))
            .collect();
        let mangled_method =
            MethodName::format_method_with_args(&method_info.method_name, &method_arg_names);

        MethodName::join_struct_method(&mangled_struct, &mangled_method)
    }

    /// Get the struct name from a `type_id`, unwrapping references if needed
    /// For generic instances, returns the mangled name with type args (e.g., "Array<i32>")
    pub fn get_struct_name_from_type(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
    ) -> Option<String> {
        match type_table.get(type_id) {
            ResolvedType::Struct { name, .. }
            | ResolvedType::Enum { name, .. }
            | ResolvedType::Variant { name, .. } => Some(name.clone()),
            ResolvedType::Primitive(prim) => Some(prim.as_str().to_string()),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                // Return the mangled name with type args (e.g., "Array<i32>", "Box<String>")
                let args: Vec<String> = type_args
                    .iter()
                    .map(|arg| type_table.mangle_type_name(*arg))
                    .collect();
                Some(mangle_generic_name(name, &args))
            }
            ResolvedType::BuiltinArray(elem) => {
                let arg = type_table.mangle_type_name(*elem);
                Some(mangle_generic_name("Array", &[arg]))
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.get_struct_name_from_type(*inner, type_table)
            }
            _ => None,
        }
    }

    /// Get the base struct name and type args from a `type_id`, unwrapping references if needed
    /// Returns (`base_name`, `type_args`) for `GenericInstance`, (name, []) for Struct
    pub fn get_struct_info_from_type(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
    ) -> Option<(String, Vec<TypeId>)> {
        match type_table.get(type_id) {
            ResolvedType::Struct { name, .. } => {
                // For monomorphized structs with names like "Array<i32>", look up the
                // original InstantiationKey to get the base name and type_args
                if let Some(key) = self.structs.mangled_to_key.get(name) {
                    Some((key.name.clone(), key.impl_type_args.clone()))
                } else {
                    Some((name.clone(), vec![]))
                }
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => Some((name.clone(), type_args.clone())),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.get_struct_info_from_type(*inner, type_table)
            }
            _ => None,
        }
    }
}
