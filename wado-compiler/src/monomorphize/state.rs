//! Monomorphizer state: instantiation tracking and name generation.

use std::sync::Arc;

use crate::elaborator::trait_env::TraitEnv;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::name::{LocalMethodName, MethodName, mangle_generic_name};
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
    /// Map from canonicalised `InstantiationKey` to the mangled function
    /// name. Keys are canonicalised by [`Monomorphizer::canonicalize_key`]
    /// before insert/lookup so that pre-/post-substitution `TypeId`
    /// variants of the same logical type collapse onto a single entry.
    pub instantiated: IndexMap<InstantiationKey, String>,
    /// Work queue of pending function instantiations. Holds canonicalised
    /// keys.
    pub pending: Vec<InstantiationKey>,
    /// Project-wide trait knowledge inherited from the package. Used by
    /// receiver-substitution and comparison-lowering paths to find the
    /// module that owns `impl <trait> for <type>` without rebuilding a
    /// parallel "mangled name → module" index.
    pub trait_env: Arc<TraitEnv>,
}

impl FuncInstState {
    /// Look up the module owning the impl that backs `info`, restricted
    /// to fully concrete impls (no impl-level type parameters). Mono uses
    /// this — rather than the broader [`TraitEnv::impl_module_for`] —
    /// because a generic impl's post-substitution function is materialised
    /// in the receiver type's module, not the impl block's module: that
    /// invariant is what `call_rewrite`'s "ref-blanket" path relies on to
    /// route `&List<i32>^Inspect::inspect` through the `&T`-blanket
    /// instantiation rather than collapsing it to `List<i32>::inspect`.
    ///
    /// The lookup uses `info.struct_name` (the post-substitution type
    /// name) rather than `info.base_struct_name`, which mirrors the
    /// legacy `trait_method_locations.contains_key(<full mangled name>)`
    /// semantics: a concrete impl's key is the full type name (e.g. the
    /// `impl Ord for i32` block keys ("i32", "Ord")), and post-mono
    /// substitution emits trait-method calls whose `struct_name` is the
    /// resolved concrete type ("i32", not "Score"). Querying by
    /// `base_struct_name` would miss this case for newtypes whose
    /// `struct_name` has been resolved through the newtype.
    pub fn impl_module(
        &self,
        info: &LocalMethodName,
        type_module: Option<&ModuleSource>,
    ) -> Option<ModuleSource> {
        let trait_name = info
            .base_trait_name
            .as_deref()
            .or(info.trait_name.as_deref())?;
        self.trait_env
            .concrete_impl_module_for(&info.struct_name, trait_name, type_module)
            .cloned()
    }

    /// `true` when `info` denotes a trait method whose impl is already
    /// known to the project as a concrete (non-generic) impl block. The
    /// existence check used by mono's blanket-vs-concrete branch needs to
    /// match the legacy `trait_method_locations.contains_key` semantics,
    /// which only catalogued non-generic impl methods.
    pub fn has_impl(&self, info: &LocalMethodName) -> bool {
        // Existence-only check; any candidate module suffices so no hint is
        // needed.
        self.impl_module(info, None).is_some()
    }

    /// Module of any non-blanket `impl <trait> for <Type>` block — broader
    /// than [`Self::impl_module`] in that it also returns generic impls
    /// (`impl<T> IntoIterator for List<T>`) which live in the receiver
    /// type's own module by convention. Used by the type-param dispatch
    /// path to distinguish "the receiver type has a non-blanket impl,
    /// fall through to the receiver's module" from "no impl at all, use
    /// the blanket's module".
    pub fn generic_or_concrete_impl_module(
        &self,
        info: &LocalMethodName,
        type_module: Option<&ModuleSource>,
    ) -> Option<ModuleSource> {
        let trait_name = info
            .base_trait_name
            .as_deref()
            .or(info.trait_name.as_deref())?;
        if let Some(m) = self
            .trait_env
            .impl_module_for(&info.struct_name, trait_name, type_module)
        {
            return Some(m.clone());
        }
        if info.base_struct_name != info.struct_name
            && let Some(m) =
                self.trait_env
                    .impl_module_for(&info.base_struct_name, trait_name, type_module)
        {
            return Some(m.clone());
        }
        None
    }
}

/// Monomorphizer collects generic instantiations and generates concrete types.
///
/// Per issue #1110 (4): there is no "current module" notion at this layer.
/// Every `FunctionRef::module_source` is set by its producer to the body's
/// home module, and the monomorphizer reads that field directly — never the
/// monomorphizer's own location — to key into `generic_functions` /
/// `instantiated`. Keeping a `current_module_source` here would invite the
/// "fall back to the current module" pattern the issue forbids.
pub(super) struct Monomorphizer {
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
    pub fn new(trait_env: Arc<TraitEnv>) -> Self {
        Self {
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
                trait_env,
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

    /// Look up the mangled name for a function instantiation by key.
    pub fn lookup_function_instantiation(&self, key: &InstantiationKey) -> Option<&String> {
        self.functions.instantiated.get(key)
    }

    /// Queue a function instantiation if not already queued. Returns true
    /// if newly queued.
    ///
    /// Dedupe is keyed solely on the full `InstantiationKey`. Together
    /// with faithful Ref/MutRef mangling (so `[&T]` and `[T]` mangle to
    /// distinct names) and `TypeRewriter`'s post-monomorphisation
    /// type-id rewrite over **every** `TypeId` field reachable from a
    /// `TirFunction` body — including `Call`/`MethodCall::type_args` —
    /// every concrete instantiation site reaches `try_queue_function`
    /// with `GenericInstance`/`Struct`-canonicalised type args. That
    /// makes `function_id_for(func)` injective over `project.functions`
    /// by construction — the load-bearing invariant for DCE's
    /// position-based retain (asserted at `optimize/dce.rs:675`).
    pub fn try_queue_function(&mut self, key: InstantiationKey, mangled_name: String) -> bool {
        if self.functions.instantiated.contains_key(&key) {
            return false;
        }
        self.functions
            .instantiated
            .insert(key.clone(), mangled_name);
        self.functions.pending.push(key);
        true
    }

    /// Generate monomorphized struct name: `Box` + `[i32]` -> `"Box<i32>"`.
    ///
    /// Uses `mangle_type_arg_for_generic` so that `Variant`/`Enum`/
    /// `Resource`/`Newtype`/`Flags` type arguments are qualified by their
    /// declaring `ModuleSource`. This matches what the type-table-side
    /// `get_type_name_info(GenericInstance)` produces for the same type
    /// arg, so the struct registered here and the `GenericInstance`
    /// looked up by mangled name in `substitute_type` agree on a single
    /// identity. Without this, a generic instantiated over a variant
    /// (e.g. `IterMap<ListIter<Color>, String>`) was registered with an
    /// unqualified name from one side and looked up with a qualified
    /// name from the other, producing two distinct wasm-GC types for the
    /// same struct and an "expected (ref $type), found (ref $type)" ICE
    /// at codegen-time validation.
    pub fn instantiation_name(&self, key: &InstantiationKey, type_table: &TypeTable) -> String {
        let args: Vec<String> = key
            .impl_type_args
            .iter()
            .map(|&t| type_table.mangle_type_arg_for_generic(t))
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
            .map(|t| type_table.mangle_type_arg_for_generic(*t))
            .collect();
        args.extend(
            key.method_type_args
                .iter()
                .map(|t| type_table.mangle_type_arg_for_generic(*t)),
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
            .map(|t| type_table.mangle_type_arg_for_generic(*t))
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
            // Normal: append type args: "List" → "List<i32>"
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
            .map(|t| type_table.mangle_type_arg_for_generic(*t))
            .collect();
        let mangled_method =
            MethodName::format_method_with_args(&method_info.method_name, &method_arg_names);

        MethodName::join_struct_method(&mangled_struct, &mangled_method)
    }

    /// Get the struct name from a `type_id`, unwrapping references if needed
    /// For generic instances, returns the mangled name with type args (e.g., "List<i32>")
    pub fn get_struct_name_from_type(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
    ) -> Option<String> {
        match type_table.get(type_id) {
            ResolvedType::Struct { name, .. }
            | ResolvedType::Enum { name, .. }
            | ResolvedType::Variant { name, .. }
            | ResolvedType::Flags { name, .. } => Some(name.clone()),
            ResolvedType::Primitive(prim) => Some(prim.as_str().to_string()),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                // Return the mangled name with type args (e.g., "List<i32>", "Box<String>")
                let args: Vec<String> = type_args
                    .iter()
                    .map(|arg| type_table.mangle_type_arg_for_generic(*arg))
                    .collect();
                Some(mangle_generic_name(name, &args))
            }
            ResolvedType::BuiltinArray(elem) => {
                let arg = type_table.mangle_type_name(*elem);
                Some(mangle_generic_name(TypeTable::ARRAY_TYPE_NAME, &[arg]))
            }
            // Newtypes are transparent for method lookup — unwrap to base type,
            // same as Ref/MutRef. The elaborator already resolves methods through
            // newtypes, so the monomorphizer needs to see the base type to find
            // the correct generic function template.
            ResolvedType::Newtype { base_type, .. } => {
                self.get_struct_name_from_type(*base_type, type_table)
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.get_struct_name_from_type(*inner, type_table)
            }
            _ => None,
        }
    }

    /// If `type_id` (peeling references) is a newtype that has its OWN
    /// non-blanket impl of `trait_name`, return the newtype's own mangled name
    /// (e.g. `"ByteList"`); otherwise `None`.
    ///
    /// Unlike [`Self::get_struct_name_from_type`], which makes newtypes
    /// transparent by peeling to the base, this preserves the newtype's identity
    /// — but only when the newtype actually overrides the trait. The collect path
    /// tries this name first so the queued instantiation (`ByteList^Trait::method`)
    /// matches the call the rewrite emits, not the inherited base
    /// (`List^Trait::method`). Newtypes without their own impl (e.g. `Meters`,
    /// simd `v128` lanes) return `None` and keep peeling to the base, so their
    /// dispatch is unchanged.
    pub fn newtype_own_struct_name_with_impl(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
        trait_name: Option<&str>,
    ) -> Option<String> {
        let trait_name = trait_name?;
        let mut tid = type_id;
        loop {
            match type_table.get(tid) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => tid = *inner,
                ResolvedType::Newtype { base_type, .. } => {
                    let base = *base_type;
                    let own = type_table.mangle_type_name(tid);
                    if self
                        .functions
                        .trait_env
                        .impl_module_for(&own, trait_name, None)
                        .is_some()
                    {
                        return Some(own);
                    }
                    // This newtype has no own impl, but an inner newtype in a
                    // `type B = A` chain still might, so keep peeling.
                    tid = base;
                }
                _ => return None,
            }
        }
    }

    /// Build the ordered list of `(mangled_method_name, trait_name)` formats to
    /// probe when resolving a generic method call on `receiver_type_id`.
    ///
    /// A newtype's OWN impl is tried before its base so the resolution lands on
    /// the newtype's own instantiation (`ByteList^serialize`) rather than the
    /// inherited base (`List^serialize`). For each candidate struct name both
    /// the inherent (`None`) and trait-qualified formats are emitted. Returns
    /// the newtype's own name alongside the list so callers can also seed their
    /// candidate set with it. Shared by the collect (`func_inst.rs`) and rewrite
    /// (`call_rewrite.rs`) paths so the two stay in lockstep.
    pub fn newtype_aware_method_names(
        &self,
        receiver_type_id: TypeId,
        type_table: &TypeTable,
        method_name: &str,
        struct_name: &str,
        trait_name: Option<&str>,
    ) -> (Option<String>, Vec<(String, Option<String>)>) {
        let own_name = self.newtype_own_struct_name_with_impl(
            receiver_type_id,
            type_table,
            trait_name,
        );
        let mut names: Vec<(String, Option<String>)> = Vec::new();
        let mut push_for = |s: &str| {
            names.push((MethodName::format_local(s, None, method_name), None));
            if let Some(tn) = trait_name {
                names.push((
                    MethodName::format_local(s, Some(tn), method_name),
                    Some(tn.to_string()),
                ));
            }
        };
        if let Some(ref own) = own_name {
            push_for(own);
        }
        push_for(struct_name);
        (own_name, names)
    }

    /// Build the candidate struct-name set for trait-fallback template lookup,
    /// ordered newtype-own first, then the method's base/impl struct names, then
    /// the receiver's struct name. Mirrors the ordering of the name list from
    /// [`Self::newtype_aware_method_names`].
    pub fn newtype_aware_candidates<'a>(
        &self,
        own_name: Option<&'a str>,
        info: Option<&'a LocalMethodName>,
        struct_name: &'a str,
    ) -> Vec<&'a str> {
        let mut c: Vec<&'a str> = Vec::new();
        if let Some(own) = own_name {
            c.push(own);
        }
        if let Some(info) = info {
            c.push(info.base_struct_name.as_str());
            c.push(info.struct_name.as_str());
        }
        c.push(struct_name);
        c
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
                // For monomorphized structs with names like "List<i32>", look up the
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
            // The raw GC array's methods live in `impl Array<T>`; its base
            // method-owner name is "Array" with the element as the type arg.
            ResolvedType::BuiltinArray(elem) => {
                Some((TypeTable::ARRAY_TYPE_NAME.to_string(), vec![*elem]))
            }
            // Newtypes are transparent — unwrap to base type for struct info lookup
            ResolvedType::Newtype { base_type, .. } => {
                self.get_struct_info_from_type(*base_type, type_table)
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.get_struct_info_from_type(*inner, type_table)
            }
            _ => None,
        }
    }
}
