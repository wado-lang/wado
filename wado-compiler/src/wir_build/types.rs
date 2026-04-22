//! Type registration — translates TIR type definitions to WIR type definitions.
//!
//! Follows a multi-phase registration order to handle type dependencies
//! correctly.
//!
//! Also contains type-ordering utilities (topological sort) used during registration.

use crate::name::{ModuleSource, StructName};
use crate::tir::{ResolvedType, TirStruct, TirVariantDecl, TypeId, TypeTable};
use crate::wir::{
    WirArrayType, WirEnumCase, WirEnumType, WirField, WirGenericOrigin, WirMeta, WirName,
    WirStructType, WirType, WirTypeDef, WirVariantCase, WirVariantRepr, WirVariantType,
};

use crate::hashmap::{IndexMap, IndexSet};

use super::context::WirContext;

/// A type declaration in topological order (struct or variant).
pub enum TypeDecl<'a> {
    Struct(&'a TirStruct),
    Variant(&'a TirVariantDecl),
}

/// Get type dependencies (FQ `"{module_source}//{name}"` keys) for a given type.
fn get_type_dependencies(type_table: &TypeTable, type_id: TypeId) -> Vec<String> {
    match type_table.get(type_id) {
        ResolvedType::Struct {
            name,
            module_source,
            ..
        } => vec![format!("{module_source}//{name}")],
        ResolvedType::Variant {
            name,
            module_source,
            ..
        } => vec![format!("{module_source}//{name}")],
        ResolvedType::GenericInstance { type_args, .. } => {
            // Skip unresolved generic instances (containing type params or projections)
            if type_args.iter().any(|t| type_table.contains_type_param(*t)) {
                return vec![];
            }
            let mangled_name = type_table.mangle_type_name(type_id);
            let mut deps = vec![mangled_name];
            for arg in type_args {
                deps.extend(get_type_dependencies(type_table, *arg));
            }
            deps
        }
        ResolvedType::BuiltinArray(inner)
        | ResolvedType::Ref(inner)
        | ResolvedType::MutRef(inner)
        | ResolvedType::Reactive(inner) => get_type_dependencies(type_table, *inner),
        ResolvedType::GenericResource { type_args, .. } => type_args
            .iter()
            .flat_map(|a| get_type_dependencies(type_table, *a))
            .collect(),
        _ => vec![],
    }
}

/// Sort structs and variants together topologically so dependencies are registered
/// before dependents. This handles mutual dependencies between structs and variants
/// (e.g., struct with variant field, variant with struct payload).
fn sort_types_topologically<'a>(
    structs: &'a [TirStruct],
    variants: &'a [TirVariantDecl],
    type_table: &TypeTable,
) -> Vec<TypeDecl<'a>> {
    // Use FQ keys ("{module_source}//{name}") to distinguish same-named types
    // from different modules.
    let fq_key = |ms: &ModuleSource, name: &str| format!("{ms}//{name}");

    let struct_keys: IndexSet<String> = structs
        .iter()
        .map(|s| fq_key(&s.module_source, &s.name))
        .collect();
    let variant_keys: IndexSet<String> = variants
        .iter()
        .map(|v| fq_key(&v.module_source, &v.name))
        .collect();
    let all_keys: IndexSet<String> = struct_keys.union(&variant_keys).cloned().collect();

    let mut deps: IndexMap<String, Vec<String>> = IndexMap::default();

    for s in structs {
        let key = fq_key(&s.module_source, &s.name);
        let mut type_deps = Vec::new();
        for field in &s.fields {
            for dep in get_type_dependencies(type_table, field.type_id) {
                if all_keys.contains(&dep) && dep != key {
                    type_deps.push(dep);
                }
            }
        }
        deps.insert(key, type_deps);
    }

    for v in variants {
        let key = fq_key(&v.module_source, &v.name);
        let mut type_deps = Vec::new();
        for case in &v.cases {
            for dep in get_type_dependencies(type_table, case.payload) {
                if all_keys.contains(&dep) && dep != key {
                    type_deps.push(dep);
                }
            }
        }
        deps.insert(key, type_deps);
    }

    let mut in_degree: IndexMap<String, usize> = IndexMap::default();
    for key in &all_keys {
        in_degree.insert(key.clone(), deps.get(key).map(Vec::len).unwrap_or(0));
    }

    let mut dependents: IndexMap<String, Vec<String>> = IndexMap::default();
    for (key, type_deps) in &deps {
        for dep in type_deps {
            dependents.entry(dep.clone()).or_default().push(key.clone());
        }
    }

    let mut queue: Vec<String> = in_degree
        .iter()
        .filter(|&(_, deg)| *deg == 0)
        .map(|(key, _)| key.clone())
        .collect();

    let mut sorted_keys = Vec::new();
    while let Some(key) = queue.pop() {
        sorted_keys.push(key.clone());
        if let Some(deps_on_key) = dependents.get(&key) {
            for dependent in deps_on_key {
                let deg = in_degree.get_mut(dependent).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push(dependent.clone());
                }
            }
        }
    }

    // Append types involved in cycles (in_degree > 0 after Kahn's algorithm).
    // Wasm GC supports mutually recursive types via rec groups, and
    // fixup_abstract_struct_fields resolves forward references afterward.
    for (key, deg) in &in_degree {
        if *deg > 0 {
            sorted_keys.push(key.clone());
        }
    }

    let key_to_struct: IndexMap<String, &TirStruct> = structs
        .iter()
        .map(|s| (fq_key(&s.module_source, &s.name), s))
        .collect();
    let key_to_variant: IndexMap<String, &TirVariantDecl> = variants
        .iter()
        .map(|v| (fq_key(&v.module_source, &v.name), v))
        .collect();

    sorted_keys
        .iter()
        .filter_map(|key| {
            if let Some(s) = key_to_struct.get(key) {
                Some(TypeDecl::Struct(s))
            } else {
                key_to_variant.get(key).map(|v| TypeDecl::Variant(v))
            }
        })
        .collect()
}

/// Register all types from the `FlatPackage` into the `WirContext`.
///
/// This follows a multi-phase registration order to ensure type dependencies
/// are satisfied.
pub fn register_types(ctx: &mut WirContext<'_>) {
    let _type_table = &*ctx.package.type_table.borrow();

    // Phase 0: Internal Box<T> structs
    register_box_structs(ctx);

    // Phase 1: Non-mono library structs & variants (topologically sorted)
    register_library_types(ctx);

    // Phase 1.5: Tuple types
    register_tuple_types(ctx);

    // Phase 2: Non-mono entry module structs & variants
    register_entry_types(ctx);

    // Phase 2.5: Arrays of non-mono structs
    register_nonmono_arrays(ctx);

    // Phase 3: Monomorphized library structs
    register_mono_library_types(ctx);

    // Phase 3.1: Monomorphized variants (first pass — register variants with simple
    // payloads so that array element types can reference them, e.g. Array<Option<i32>>)
    register_mono_variants(ctx);

    // Phase 3.5: Pre-register arrays from mono struct fields
    register_mono_field_arrays(ctx);

    // Phase 3.6: All remaining raw array types (must come before wrapper structs)
    register_remaining_arrays(ctx);

    // Phase 3.7: Array<T> wrapper structs — must come before mono entry structs
    // because entry structs may have Array<T> fields that reference these wrappers
    register_array_wrapper_structs(ctx);

    // Phase 4: Monomorphized entry module structs
    register_mono_entry_types(ctx);

    // Phase 4.5b: Monomorphized variants (second pass — picks up variants whose
    // payloads depend on array or entry struct types registered above)
    register_mono_variants(ctx);

    // Register enums from all modules
    register_enums(ctx);

    // Register flags from all modules
    register_flags(ctx);

    // Phase 5: Canonical closure types for function-typed values
    register_canonical_closure_types(ctx);

    // Final pass: fix up struct fields that resolved to AbstractRef(Struct)
    // because of self-referential or forward-referenced types
    fixup_abstract_struct_fields(ctx);
}

/// Register a single struct type.
fn register_struct(
    ctx: &mut WirContext<'_>,
    tir_struct: &TirStruct,
    type_table: &TypeTable,
    module_source: &ModuleSource,
) {
    // Use the TirStruct's own module_source directly.  The monomorphizer sets
    // this to the module where the generic struct is *defined* (via InstantiationKey),
    // and link.rs dedup ensures only the defining-module copy survives.
    let effective_module = module_source.clone();

    let struct_name = StructName::new(effective_module.clone(), tir_struct.name.clone());

    // Skip if already registered (exact match — works for both mono and non-mono)
    if ctx.struct_type_map.contains_key(&struct_name) {
        return;
    }

    // Also check with newtypes resolved: e.g., Array<Tuple<FieldName,FieldValue>> should
    // reuse the existing Array<Tuple<String,Array<u8>>> when FieldName/FieldValue are newtypes.
    if let Some(ref mono) = tir_struct.monomorph_info {
        let has_newtypes = mono
            .impl_type_args
            .iter()
            .any(|t| type_table.resolve_newtype_base(*t) != *t);
        if has_newtypes {
            let resolved_args: Vec<String> = mono
                .impl_type_args
                .iter()
                .map(|t| type_table.mangle_type_name_resolving_newtypes(*t))
                .collect();
            let resolved_name =
                crate::name::mangle_generic_name(&mono.generic_name, &resolved_args);
            let resolved_sn = StructName::new(effective_module.clone(), resolved_name);
            if let Some(existing) = ctx.struct_type_map.get(&resolved_sn).cloned() {
                ctx.struct_type_map.insert(struct_name, existing);
                return;
            }
        }
    }

    let fq = format!("{effective_module}//{}", tir_struct.name);

    // Pre-register raw array types for BuiltinArray fields before resolving field types,
    // so that concrete array refs are available instead of falling back to abstract arrayref.
    for f in &tir_struct.fields {
        if let ResolvedType::BuiltinArray(elem) = type_table.get(f.type_id) {
            register_raw_array_type(ctx, *elem, type_table);
        }
    }

    let fields: Vec<WirField> = tir_struct
        .fields
        .iter()
        .filter_map(|f| {
            let ty = ctx.type_id_to_wir_type(type_table, f.type_id);
            // Unit-typed fields have no Wasm representation; omit them.
            if matches!(ty, WirType::Unit) {
                return None;
            }
            // Struct fields use non-nullable refs.
            let ty = ty.as_nonnull();
            Some(WirField {
                name: f.name.clone(),
                ty,
                mutable: true, // All fields mutable at Wasm GC level
            })
        })
        .collect();

    let generic_origin = tir_struct
        .monomorph_info
        .as_ref()
        .map(|info| WirGenericOrigin {
            base_name: info.generic_name.clone(),
            type_args: info
                .impl_type_args
                .iter()
                .map(|&ta| type_table.mangle_type_name(ta))
                .collect(),
        });

    let type_id = ctx.register_type(
        fq,
        WirTypeDef::Struct(WirStructType {
            name: WirName {
                fq: format!("{effective_module}//{}", tir_struct.name),
            },
            fields,
            meta: WirMeta {
                module_source: Some(effective_module),
                ..WirMeta::default()
            },
            generic_origin,
            newtype_origin: None,
        }),
    );

    ctx.struct_type_map.insert(struct_name, type_id);
}

/// Register a single variant type.
fn register_variant(
    ctx: &mut WirContext<'_>,
    variant: &TirVariantDecl,
    type_table: &TypeTable,
    module_source: &ModuleSource,
) {
    let fq = format!("{module_source}//{}", variant.name);

    // Skip if already registered
    if ctx.variant_type_map.contains_key(&fq) {
        return;
    }

    let raw_cases: Vec<(String, Vec<WirType>)> = variant
        .cases
        .iter()
        .map(|case| {
            let payload = if type_table.get(case.payload) == &ResolvedType::Unit {
                Vec::new()
            } else {
                vec![ctx.type_id_to_wir_type(type_table, case.payload)]
            };
            (case.name.clone(), payload)
        })
        .collect();

    let cases: Vec<WirVariantCase> = raw_cases
        .iter()
        .enumerate()
        .map(|(i, (name, payload))| WirVariantCase {
            name: name.clone(),
            index: u32::try_from(i).expect("too many variant cases"),
            payload: payload.clone(),
        })
        .collect();

    // Variants don't have monomorph_info directly; generic_origin is None for non-generic
    let generic_origin = None;

    let type_id = ctx.register_type(
        fq.clone(),
        WirTypeDef::Variant(WirVariantType {
            name: WirName { fq: fq.clone() },
            cases: cases.clone(),
            repr: WirVariantRepr::default(),
            meta: WirMeta {
                module_source: Some(module_source.clone()),
                ..WirMeta::default()
            },
            generic_origin,
            newtype_origin: None,
        }),
    );

    ctx.variant_type_map.insert(fq.clone(), type_id.clone());

    // Register case-specific struct types so the translator can reference them.
    // These are "phantom" types — the emitter will skip them and instead map their
    // WIR type indices to the correct Wasm type indices during variant rec group emission.
    for case in &cases {
        if case.payload.is_empty() {
            continue; // Unit cases don't need separate types
        }
        let case_fq = format!("{fq}::{}", case.name);
        let mut fields = vec![WirField {
            name: "discriminant".to_string(),
            ty: crate::wir::WirType::I32,
            mutable: false,
        }];
        for (j, payload_ty) in case.payload.iter().enumerate() {
            fields.push(WirField {
                name: format!("payload_{j}"),
                ty: payload_ty.clone(),
                mutable: false,
            });
        }
        let case_type_id = ctx.register_type(
            case_fq,
            WirTypeDef::Struct(WirStructType {
                name: WirName {
                    fq: format!("{fq}::{}", case.name),
                },
                fields,
                meta: WirMeta::default(),
                generic_origin: None,
                newtype_origin: None,
            }),
        );
        ctx.variant_case_info
            .insert(case_type_id.index(), (type_id.index(), case.index));
    }
}

/// Register a raw GC array type for a given element `TypeId`.
fn register_raw_array_type(
    ctx: &mut WirContext<'_>,
    element_type_id: crate::tir::TypeId,
    type_table: &TypeTable,
) {
    if ctx.array_type_map.contains_key(&element_type_id) {
        return;
    }
    let elem_name = type_table.mangle_type_name(element_type_id);
    if ctx.array_type_by_name.contains_key(&elem_name) {
        // Already registered under a different TypeId
        let existing = ctx.array_type_by_name.get(&elem_name).unwrap().clone();
        ctx.array_type_map.insert(element_type_id, existing);
        return;
    }
    // Fallback: resolve newtypes in element type name for deduplication
    let resolved_name = type_table.mangle_type_name_resolving_newtypes(element_type_id);
    if resolved_name != elem_name && ctx.array_type_by_name.contains_key(&resolved_name) {
        let existing = ctx.array_type_by_name.get(&resolved_name).unwrap().clone();
        ctx.array_type_map.insert(element_type_id, existing);
        return;
    }

    let fq = format!("builtin::array<{elem_name}>");
    let elem_wir_type = ctx.type_id_to_wir_type(type_table, element_type_id);

    let type_id = ctx.register_type(
        fq.clone(),
        WirTypeDef::Array(WirArrayType {
            name: WirName { fq },
            element_type: elem_wir_type,
            mutable: true,
            meta: WirMeta::default(),
            generic_origin: None,
        }),
    );

    ctx.array_type_map.insert(element_type_id, type_id.clone());
    ctx.array_type_by_name
        .insert(elem_name.clone(), type_id.clone());
    // Also register under the newtype-resolved name so that e.g.
    // `Array<[FieldName, FieldValue]>` and `Array<[String, Array<u8>]>` share
    // the same raw array type when FieldName/FieldValue are newtypes.
    if resolved_name != elem_name {
        ctx.array_type_by_name.insert(resolved_name, type_id);
    }
}

fn register_box_structs(ctx: &mut WirContext<'_>) {
    let type_table = &*ctx.package.type_table.borrow();
    for s in &ctx.package.structs {
        if s.monomorph_info
            .as_ref()
            .is_some_and(|info| info.generic_name == "Box")
        {
            register_struct(ctx, s, type_table, &s.module_source);
        }
    }

    // Pre-register Box<i32> if not already present. This is needed for Option<Resource>,
    // Option<Stream>, Option<Future>, etc., where the inner type maps to i32 at the
    // Wasm level but there's no explicit Option<i32> usage in the source code.
    ensure_box_type(ctx, "i32", crate::wir::WirType::I32);
}

/// Ensure a `Box<T>` struct type exists for the given primitive name.
fn ensure_box_type(ctx: &mut WirContext<'_>, prim_name: &str, wir_type: crate::wir::WirType) {
    let box_name =
        crate::name::mangle_generic_name("Box", std::slice::from_ref(&prim_name.to_string()));
    let module_source = ctx
        .package
        .type_table
        .borrow()
        .box_module_source
        .clone()
        .unwrap_or_else(ModuleSource::prelude);
    let struct_name = StructName::new(module_source.clone(), box_name.clone());
    if ctx.struct_type_map.contains_key(&struct_name) {
        return;
    }
    let fq = format!("{module_source}//{box_name}");
    let type_id = ctx.register_type(
        fq.clone(),
        WirTypeDef::Struct(WirStructType {
            name: WirName { fq },
            fields: vec![WirField {
                name: "value".to_string(),
                ty: wir_type,
                mutable: true,
            }],
            meta: WirMeta {
                module_source: Some(module_source),
                ..WirMeta::default()
            },
            generic_origin: Some(WirGenericOrigin {
                base_name: "Box".to_string(),
                type_args: vec![prim_name.to_string()],
            }),
            newtype_origin: None,
        }),
    );
    ctx.struct_type_map.insert(struct_name, type_id);
}

fn register_library_types(ctx: &mut WirContext<'_>) {
    let entry_source = &ctx.package.entry_module_source;
    let type_table = &*ctx.package.type_table.borrow();

    // Collect non-mono library structs and variants for topological sorting
    let structs: Vec<_> = ctx
        .package
        .structs
        .iter()
        .filter(|s| {
            s.module_source != *entry_source
                && s.monomorph_info.is_none()
                && s.type_params.is_empty()
        })
        .cloned()
        .collect();
    let variants: Vec<_> = ctx
        .package
        .variants
        .iter()
        .filter(|v| v.module_source != *entry_source && v.type_params.is_empty())
        .cloned()
        .collect();

    let sorted = sort_types_topologically(&structs, &variants, type_table);
    for decl in sorted {
        match decl {
            TypeDecl::Struct(s) => register_struct(ctx, s, type_table, &s.module_source),
            TypeDecl::Variant(v) => register_variant(ctx, v, type_table, &v.module_source),
        }
    }
}

fn register_tuple_types(ctx: &mut WirContext<'_>) {
    {
        let type_table = &*ctx.package.type_table.borrow();
        for type_id in type_table.iter_type_ids() {
            let resolved = type_table.get(type_id);
            if let ResolvedType::GenericInstance {
                name,
                type_args: elements,
                module_source,
            } = resolved
                && TypeTable::is_tuple_type(name, module_source)
            {
                if ctx.tuple_type_map.contains_key(elements) {
                    continue;
                }
                // Skip unresolved tuples (containing type params or projections)
                if elements.iter().any(|e| type_table.contains_type_param(*e)) {
                    continue;
                }

                // Newtype deduplication: if any element is a newtype, check if the
                // resolved tuple already exists and reuse it.
                let has_newtypes = elements
                    .iter()
                    .any(|e| type_table.resolve_newtype_base(*e) != *e);
                if has_newtypes {
                    let resolved_elements: Vec<TypeId> = elements
                        .iter()
                        .map(|e| type_table.resolve_newtype_base(*e))
                        .collect();
                    if let Some(existing) = ctx.tuple_type_map.get(&resolved_elements).cloned() {
                        ctx.tuple_type_map.insert(elements.clone(), existing);
                        continue;
                    }
                }

                // Use resolved (base) names for tuple display/fq to ensure
                // newtypes map to the same WIR type as their base types.
                let elem_names: Vec<String> = elements
                    .iter()
                    .map(|&e| type_table.mangle_type_name_resolving_newtypes(e))
                    .collect();
                let tuple_display = format!("[{}]", elem_names.join(", "));
                // TODO: should include module_source like other types: "{module_source}//[...]"
                let fq = format!("tuple//{tuple_display}");

                let fields: Vec<WirField> = elements
                    .iter()
                    .enumerate()
                    .filter_map(|(i, &elem_type_id)| {
                        let ty = ctx.type_id_to_wir_type(type_table, elem_type_id);
                        // Unit-typed elements have no Wasm representation; omit them.
                        if matches!(ty, WirType::Unit) {
                            return None;
                        }
                        // Tuple fields use non-nullable refs
                        let ty = ty.as_nonnull();
                        Some(WirField {
                            name: format!("{i}"),
                            ty,
                            mutable: true,
                        })
                    })
                    .collect();

                let wir_type_id = ctx.register_type(
                    fq.clone(),
                    WirTypeDef::Struct(WirStructType {
                        name: WirName { fq },
                        fields,
                        meta: WirMeta::default(),
                        generic_origin: None,
                        newtype_origin: None,
                    }),
                );
                ctx.tuple_type_map
                    .insert(elements.clone(), wir_type_id.clone());
                // Also register under resolved elements key for deduplication
                if has_newtypes {
                    let resolved_elements: Vec<TypeId> = elements
                        .iter()
                        .map(|e| type_table.resolve_newtype_base(*e))
                        .collect();
                    if !ctx.tuple_type_map.contains_key(&resolved_elements) {
                        ctx.tuple_type_map.insert(resolved_elements, wir_type_id);
                    }
                }
            }
        }
    }
}

fn register_entry_types(ctx: &mut WirContext<'_>) {
    let entry_source = &ctx.package.entry_module_source;
    let type_table = &*ctx.package.type_table.borrow();

    let structs: Vec<_> = ctx
        .package
        .structs
        .iter()
        .filter(|s| {
            s.module_source == *entry_source
                && s.monomorph_info.is_none()
                && s.type_params.is_empty()
        })
        .cloned()
        .collect();
    let variants: Vec<_> = ctx
        .package
        .variants
        .iter()
        .filter(|v| v.module_source == *entry_source && v.type_params.is_empty())
        .cloned()
        .collect();

    let sorted = sort_types_topologically(&structs, &variants, type_table);
    for decl in sorted {
        match decl {
            TypeDecl::Struct(s) => register_struct(ctx, s, type_table, &s.module_source),
            TypeDecl::Variant(v) => register_variant(ctx, v, type_table, &v.module_source),
        }
    }
}

fn register_nonmono_arrays(ctx: &mut WirContext<'_>) {
    let type_table = &*ctx.package.type_table.borrow();
    for type_id in type_table.iter_type_ids() {
        if let ResolvedType::BuiltinArray(elem_type_id) = type_table.get(type_id) {
            // Skip unresolved arrays (containing type params or projections)
            if type_table.contains_type_param(*elem_type_id) {
                continue;
            }
            register_raw_array_type(ctx, *elem_type_id, type_table);
        }
    }
}

fn register_mono_library_types(ctx: &mut WirContext<'_>) {
    let entry_source = &ctx.package.entry_module_source;
    let type_table = &*ctx.package.type_table.borrow();

    for s in &ctx.package.structs {
        if s.module_source == *entry_source {
            continue;
        }
        if s.monomorph_info.is_some() {
            // Skip Box<T> (already registered in Phase 0)
            if s.monomorph_info
                .as_ref()
                .is_some_and(|info| info.generic_name == "Box")
            {
                continue;
            }
            register_struct(ctx, s, type_table, &s.module_source);
        }
    }
}

fn register_mono_field_arrays(ctx: &mut WirContext<'_>) {
    // Pre-scan monomorphized struct fields for array types
    let type_table = &*ctx.package.type_table.borrow();
    for s in &ctx.package.structs {
        if s.monomorph_info.is_some() {
            for field in &s.fields {
                if let ResolvedType::BuiltinArray(elem) = type_table.get(field.type_id) {
                    register_raw_array_type(ctx, *elem, type_table);
                }
            }
        }
    }
}

fn register_mono_entry_types(ctx: &mut WirContext<'_>) {
    let entry_source = &ctx.package.entry_module_source;
    let type_table = &*ctx.package.type_table.borrow();

    for s in &ctx.package.structs {
        if s.module_source != *entry_source {
            continue;
        }
        if s.monomorph_info.is_some() {
            if s.monomorph_info
                .as_ref()
                .is_some_and(|info| info.generic_name == "Box")
            {
                continue;
            }
            register_struct(ctx, s, type_table, &s.module_source);
        }
    }
}

fn register_mono_variants(ctx: &mut WirContext<'_>) {
    // Find generic variant instances in all type tables (e.g., Result<i32, String>)
    // and register them as WIR variant types.
    //
    // Multiple passes handle dependencies between variant types. For example,
    // Option<Option<i32>> needs Option<i32> registered first so its payload
    // resolves to a concrete type rather than abstract structref.
    loop {
        let mut to_register: Vec<(
            String,                                  // mangled name
            ModuleSource,                            // module source of the base variant
            Vec<(String, Vec<crate::wir::WirType>)>, // cases: (name, payload types)
        )> = Vec::new();

        {
            let type_table = &*ctx.package.type_table.borrow();
            for type_id in type_table.iter_type_ids() {
                if let ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args,
                } = type_table.get(type_id)
                {
                    // Option is now handled as a regular variant (SubtypeHierarchy).
                    // TODO: NullableRef optimization — when T is non-nullable (ref type,
                    // not another Option), skip variant registration and represent
                    // Option<T> as (ref null T) instead. This avoids the discriminant
                    // struct overhead. See wep-2026-02-09-variant-independent-types.md.

                    // Skip unresolved generic instances (e.g. Option<unknown>
                    // from unresolved null literals, or Result<S::Assoc, E>
                    // from generic trait method signatures)
                    if type_args.iter().any(|t| type_table.contains_type_param(*t)) {
                        continue;
                    }

                    let type_arg_names: Vec<String> = type_args
                        .iter()
                        .map(|t| type_table.mangle_type_name(*t))
                        .collect();
                    let mangled = crate::name::mangle_generic_name(name, &type_arg_names);
                    let fq = format!("{module_source}//{mangled}");
                    if ctx.variant_type_map.contains_key(&fq) {
                        continue;
                    }
                    // Find the base variant declaration using module_source directly.
                    let base = ctx
                        .package
                        .find_variant(module_source, name)
                        .filter(|v| !v.type_params.is_empty());
                    if let Some(base) = base {
                        let variant_tt = type_table;
                        let type_args = type_args.clone();
                        let cases: Vec<(String, Vec<crate::wir::WirType>)> = base
                            .cases
                            .iter()
                            .map(|case| {
                                let payload_resolved = variant_tt.get(case.payload);
                                let payload = match payload_resolved {
                                    ResolvedType::Unit => Vec::new(),
                                    ResolvedType::TypeParam { index, .. } => {
                                        let idx = *index as usize;
                                        if idx < type_args.len() {
                                            let sub_ty = type_table.get(type_args[idx]);
                                            if matches!(sub_ty, ResolvedType::Unit) {
                                                Vec::new()
                                            } else {
                                                vec![ctx.type_id_to_wir_type(
                                                    type_table,
                                                    type_args[idx],
                                                )]
                                            }
                                        } else {
                                            Vec::new()
                                        }
                                    }
                                    ResolvedType::GenericInstance {
                                        name: inner_name,
                                        type_args: inner_type_args,
                                        ..
                                    } if inner_type_args.iter().any(|a| {
                                        matches!(variant_tt.get(*a), ResolvedType::TypeParam { .. })
                                    }) =>
                                    {
                                        // Payload is a GenericInstance containing TypeParams
                                        // (e.g., Payload<T>). Substitute type params with
                                        // concrete type args and resolve in the consumer's
                                        // type table.
                                        let sub_arg_names: Vec<String> = inner_type_args
                                            .iter()
                                            .map(|arg| {
                                                if let ResolvedType::TypeParam { index, .. } =
                                                    variant_tt.get(*arg)
                                                {
                                                    let idx = *index as usize;
                                                    if idx < type_args.len() {
                                                        type_table.mangle_type_name(type_args[idx])
                                                    } else {
                                                        variant_tt.mangle_type_name(*arg)
                                                    }
                                                } else {
                                                    variant_tt.mangle_type_name(*arg)
                                                }
                                            })
                                            .collect();
                                        let mangled = crate::name::mangle_generic_name(
                                            inner_name,
                                            &sub_arg_names,
                                        );
                                        // Find the concrete TypeId in the consumer's type table
                                        let concrete_id = type_table.iter_type_ids().find(|tid| {
                                            type_table.mangle_type_name(*tid) == mangled
                                        });
                                        if let Some(cid) = concrete_id {
                                            vec![ctx.type_id_to_wir_type(type_table, cid)]
                                        } else {
                                            vec![ctx.type_id_to_wir_type(variant_tt, case.payload)]
                                        }
                                    }
                                    _ => {
                                        vec![ctx.type_id_to_wir_type(variant_tt, case.payload)]
                                    }
                                };
                                (case.name.clone(), payload)
                            })
                            .collect();

                        // Skip variants with unresolved payload types (abstract
                        // structref). They will be resolved in a subsequent pass
                        // after their dependencies are registered.
                        let has_unresolved = cases.iter().any(|(_, payloads)| {
                            payloads.iter().any(|ty| {
                                matches!(
                                    ty,
                                    crate::wir::WirType::AbstractRef {
                                        heap_type: crate::wir::WirAbstractHeapType::Struct,
                                        ..
                                    }
                                )
                            })
                        });
                        if !has_unresolved {
                            to_register.push((mangled, module_source.clone(), cases));
                        }
                    }
                }
            }
        }

        if to_register.is_empty() {
            break;
        }

        for (mangled_name, module_source, cases) in to_register {
            let fq = format!("{module_source}//{mangled_name}");
            if ctx.variant_type_map.contains_key(&fq) {
                continue;
            }
            let wir_cases: Vec<WirVariantCase> = cases
                .iter()
                .enumerate()
                .map(|(i, (name, payload))| WirVariantCase {
                    name: name.clone(),
                    index: u32::try_from(i).expect("too many variant cases"),
                    payload: payload.clone(),
                })
                .collect();

            let type_id = ctx.register_type(
                fq.clone(),
                WirTypeDef::Variant(WirVariantType {
                    name: WirName { fq: fq.clone() },
                    cases: wir_cases.clone(),
                    repr: WirVariantRepr::default(),
                    meta: WirMeta {
                        module_source: Some(module_source),
                        ..WirMeta::default()
                    },
                    generic_origin: None,
                    newtype_origin: None,
                }),
            );

            ctx.variant_type_map.insert(fq.clone(), type_id.clone());

            // Register case-specific struct types so the translator can reference them.
            for case in &wir_cases {
                if case.payload.is_empty() {
                    continue; // Unit cases don't need separate types
                }
                let case_fq = format!("{fq}::{}", case.name);
                let mut fields = vec![WirField {
                    name: "discriminant".to_string(),
                    ty: crate::wir::WirType::I32,
                    mutable: false,
                }];
                for (j, payload_ty) in case.payload.iter().enumerate() {
                    fields.push(WirField {
                        name: format!("payload_{j}"),
                        ty: payload_ty.clone(),
                        mutable: false,
                    });
                }
                let case_type_id = ctx.register_type(
                    case_fq,
                    WirTypeDef::Struct(WirStructType {
                        name: WirName {
                            fq: format!("{fq}::{}", case.name),
                        },
                        fields,
                        meta: WirMeta::default(),
                        generic_origin: None,
                        newtype_origin: None,
                    }),
                );
                ctx.variant_case_info
                    .insert(case_type_id.index(), (type_id.index(), case.index));
            }
        }
    } // end loop
}

fn register_remaining_arrays(ctx: &mut WirContext<'_>) {
    let type_table = &*ctx.package.type_table.borrow();
    for type_id in type_table.iter_type_ids() {
        if let ResolvedType::BuiltinArray(elem_type_id) = type_table.get(type_id) {
            if type_table.contains_type_param(*elem_type_id) {
                continue;
            }
            register_raw_array_type(ctx, *elem_type_id, type_table);
        }
    }
}

fn register_enums(ctx: &mut WirContext<'_>) {
    for e in &ctx.package.enums {
        let fq = format!("{}//enum:{}", e.module_source, e.name);
        if ctx.type_map.contains_key(&fq) {
            continue;
        }
        let cases: Vec<WirEnumCase> = e
            .cases
            .iter()
            .enumerate()
            .map(|(i, case)| WirEnumCase {
                name: case.name.clone(),
                discriminant: i32::try_from(i).expect("too many enum cases"),
            })
            .collect();

        ctx.register_type(
            fq,
            WirTypeDef::Enum(WirEnumType {
                name: WirName {
                    fq: format!("{}//enum:{}", e.module_source, e.name),
                },
                cases,
                meta: WirMeta {
                    module_source: Some(e.module_source.clone()),
                    ..WirMeta::default()
                },
            }),
        );
    }
}

fn register_flags(_ctx: &mut WirContext<'_>) {
    // Flags are not yet supported in TIR; this is a placeholder
}

/// Register canonical closure types for all `Function` types found in type tables.
/// Each unique function signature `fn(P1, P2, ...) -> R` gets:
/// - A canonical func type: `(ref struct, P1, P2, ...) -> R`
/// - A canonical closure struct: `{ env: ref struct, func: ref $func_type }`
fn register_canonical_closure_types(ctx: &mut WirContext<'_>) {
    use crate::tir::{PrimitiveType, ResolvedType};
    use crate::wir::WirType;

    // Collect all unique function signatures from the shared type table
    let mut fn_sigs: Vec<(Vec<WirType>, Vec<WirType>)> = Vec::new();
    let mut seen_keys: crate::hashmap::IndexSet<String> = crate::hashmap::IndexSet::default();

    {
        let type_table = &*ctx.package.type_table.borrow();
        for type_id in type_table.iter_type_ids() {
            if let ResolvedType::Function {
                params,
                return_type,
                ..
            } = type_table.get(type_id)
            {
                // Skip if any param/return contains type params (unmonomorphized)
                if params.iter().any(|p| type_table.contains_type_param(*p))
                    || type_table.contains_type_param(*return_type)
                {
                    continue;
                }

                // Skip i128/u128 params/returns
                let has_i128 = params.iter().any(|p| {
                    matches!(
                        type_table.get(*p),
                        ResolvedType::Primitive(PrimitiveType::I128 | PrimitiveType::U128)
                    )
                }) || matches!(
                    type_table.get(*return_type),
                    ResolvedType::Primitive(PrimitiveType::I128 | PrimitiveType::U128)
                );
                if has_i128 {
                    continue;
                }

                let param_wirs: Vec<WirType> = params
                    .iter()
                    .map(|p| ctx.type_id_to_wir_type(type_table, *p))
                    .collect();
                let result_wirs: Vec<WirType> = if *return_type == crate::tir::TypeTable::UNIT
                    || *return_type == crate::tir::TypeTable::NEVER
                {
                    vec![]
                } else {
                    vec![ctx.type_id_to_wir_type(type_table, *return_type)]
                };
                let key = WirContext::canonical_closure_key(&param_wirs, &result_wirs);
                if seen_keys.insert(key) {
                    fn_sigs.push((param_wirs, result_wirs));
                }
            }
        }
    }

    // Register canonical closure types for each signature
    for (param_wirs, result_wirs) in fn_sigs {
        ctx.get_or_create_canonical_closure_type(param_wirs, result_wirs);
    }
}

/// Register Array<T> wrapper structs for all `GenericInstance("Array", [T])` types
/// found in any module's type table.
///
/// In the TIR, `Array<T>` is `GenericInstance { name: "Array", type_args: [T] }`,
/// not a struct definition. We create wrapper structs here to provide the
/// underlying GC array types that the WIR emitter needs.
///
/// Types are processed in dependency order: if `T` is itself `Array<U>`,
/// `Array<U>` is fully registered (raw array + wrapper struct) before
/// `Array<Array<U>>` so that the backing array gets a concrete element type
/// instead of abstract `structref`.
fn register_array_wrapper_structs(ctx: &mut WirContext<'_>) {
    use crate::tir::ResolvedType;

    // Collect unique Array<T> element types from the shared type table.
    let mut array_elem_types: Vec<(crate::tir::TypeId, String)> = Vec::new();
    let tt_rc = ctx.package.type_table.clone();
    {
        let type_table = &*tt_rc.borrow();
        for type_id in type_table.iter_type_ids() {
            if let ResolvedType::GenericInstance {
                name, type_args, ..
            } = type_table.get(type_id)
                && name == "Array"
                && type_args.len() == 1
            {
                if type_table.contains_type_param(type_args[0]) {
                    continue;
                }
                // Use newtype-resolved name for deduplication so that e.g.
                // Array<[FieldName, FieldValue]> and Array<[String, Array<u8>]>
                // are treated as the same type.
                let elem_name = type_table.mangle_type_name_resolving_newtypes(type_args[0]);
                if !array_elem_types.iter().any(|(_, n)| n == &elem_name) {
                    array_elem_types.push((type_args[0], elem_name));
                }
            }
        }
    }

    // Topological sort: process leaf element types (non-Array) before nested ones.
    // Partition into non-array elements (leaf) and array elements (nested).
    let tt = tt_rc.borrow();
    let mut leaf: Vec<(crate::tir::TypeId, String)> = Vec::new();
    let mut nested: Vec<(crate::tir::TypeId, String)> = Vec::new();
    for (elem_tid, elem_name) in &array_elem_types {
        if matches!(tt.get(*elem_tid), ResolvedType::GenericInstance { name, .. } if name == "Array")
        {
            nested.push((*elem_tid, elem_name.clone()));
        } else {
            leaf.push((*elem_tid, elem_name.clone()));
        }
    }
    drop(tt);

    // Process in order: leaf types first, then nested types.
    // Each type gets both its raw array and wrapper struct registered together.
    let ordered = leaf.into_iter().chain(nested);

    for (elem_tid, elem_name) in ordered {
        // Register raw GC array type
        {
            let tt = tt_rc.borrow();
            register_raw_array_type(ctx, elem_tid, &tt);
        }

        // Register wrapper struct
        register_array_wrapper_struct(ctx, &elem_name);
    }
}

/// Register a single `Array<T>` wrapper struct given the element's mangled name.
fn register_array_wrapper_struct(ctx: &mut WirContext<'_>, elem_name: &str) {
    let elem_name_string = elem_name.to_string();
    let mangled =
        crate::name::mangle_generic_name("Array", std::slice::from_ref(&elem_name_string));

    let raw_array_type_id = ctx.array_type_by_name.get(elem_name).cloned();
    let Some(raw_type) = raw_array_type_id else {
        return;
    };

    let module_source = ModuleSource::prelude();
    let struct_name = StructName::new(module_source.clone(), mangled.clone());
    if ctx.struct_type_map.contains_key(&struct_name) {
        return;
    }

    let fq = format!("{module_source}//{mangled}");
    let type_id = ctx.register_type(
        fq.clone(),
        WirTypeDef::Struct(WirStructType {
            name: WirName { fq },
            fields: vec![
                WirField {
                    name: "repr".to_string(),
                    ty: crate::wir::WirType::Ref {
                        type_id: raw_type,
                        nullable: false,
                    },
                    mutable: true,
                },
                WirField {
                    name: "used".to_string(),
                    ty: crate::wir::WirType::I32,
                    mutable: true,
                },
            ],
            meta: WirMeta {
                module_source: Some(module_source),
                ..WirMeta::default()
            },
            generic_origin: Some(WirGenericOrigin {
                base_name: "Array".to_string(),
                type_args: vec![elem_name.to_string()],
            }),
            newtype_origin: None,
        }),
    );
    ctx.struct_type_map.insert(struct_name, type_id);
}

/// Check if a `WirType` is an unresolved abstract struct/array reference.
fn is_abstract_ref(ty: &crate::wir::WirType) -> bool {
    matches!(
        ty,
        crate::wir::WirType::AbstractRef {
            heap_type: crate::wir::WirAbstractHeapType::Struct
                | crate::wir::WirAbstractHeapType::Array,
            ..
        }
    )
}

/// Fix up types that resolved to `AbstractRef(Struct)` or `AbstractRef(Array)`
/// because of forward references or self-referential types.
///
/// After all types are registered, re-scan struct fields, array element types,
/// and function parameters/results. For any abstract refs, try to resolve
/// them to concrete types using the now-complete type registry.
fn fixup_abstract_struct_fields(ctx: &mut WirContext<'_>) {
    use crate::wir::WirType;

    // Phase 1: Fix struct fields
    let mut struct_fixups: Vec<(usize, usize, WirType)> = Vec::new();

    for (wir_idx, typedef) in ctx.types.iter().enumerate() {
        let WirTypeDef::Struct(s) = typedef else {
            continue;
        };

        for (field_idx, field) in s.fields.iter().enumerate() {
            if !is_abstract_ref(&field.ty) {
                continue;
            }

            let module_source = s.meta.module_source.as_ref();
            let struct_name_str = s.name.fq.split("//").nth(1).unwrap_or(&s.name.fq);

            let mut resolved = None;

            // Try resolving from TIR structs
            {
                let type_table = &*ctx.package.type_table.borrow();
                for tir_struct in &ctx.package.structs {
                    // Match by exact name, or by monomorphized name (base name matches TIR name)
                    let name_matches = tir_struct.name == *struct_name_str
                        || struct_name_str.starts_with(&format!("{}<", tir_struct.name));
                    if !name_matches {
                        continue;
                    }
                    if let Some(ms) = module_source
                        && &tir_struct.module_source != ms
                    {
                        continue;
                    }
                    if field_idx < tir_struct.fields.len() {
                        let field_type_id = tir_struct.fields[field_idx].type_id;
                        let wir_type = ctx.type_id_to_wir_type(type_table, field_type_id);
                        if !is_abstract_ref(&wir_type) {
                            // Make struct fields non-nullable (same as register_struct).
                            resolved = Some(wir_type.as_nonnull());
                            break;
                        }
                    }
                }
            }

            // Try resolving from TIR tuple types (tuples have display names like "[T, U]")
            if resolved.is_none() && struct_name_str.starts_with('[') {
                let type_table = &*ctx.package.type_table.borrow();
                for type_id in type_table.iter_type_ids() {
                    if let ResolvedType::GenericInstance {
                        name,
                        type_args: elements,
                        module_source,
                    } = type_table.get(type_id)
                        && TypeTable::is_tuple_type(name, module_source)
                        && field_idx < elements.len()
                    {
                        // Check if this tuple maps to the same WIR type
                        if let Some(wir_tid) = ctx.tuple_type_map.get(elements)
                            && wir_tid.index() == u32::try_from(wir_idx).unwrap_or(u32::MAX)
                        {
                            let elem_type_id = elements[field_idx];
                            let wir_type = ctx.type_id_to_wir_type(type_table, elem_type_id);
                            if !is_abstract_ref(&wir_type) {
                                // Make tuple fields non-nullable.
                                resolved = Some(wir_type.as_nonnull());
                                break;
                            }
                        }
                    }
                }
            }

            if let Some(new_type) = resolved {
                struct_fixups.push((wir_idx, field_idx, new_type));
            }
        }
    }

    for (type_idx, field_idx, new_type) in struct_fixups {
        if let WirTypeDef::Struct(s) = &mut ctx.types[type_idx] {
            s.fields[field_idx].ty = new_type;
        }
    }

    // Phase 1b: Fix variant type case payload types (may be abstract if the payload's
    // generic variant type was not registered when the parent variant was registered).
    // For non-generic variants: look up the TIR variant declaration by module_source + name.
    let mut variant_payload_fixups: Vec<(usize, usize, usize, WirType)> = Vec::new();
    for (wir_idx, typedef) in ctx.types.iter().enumerate() {
        let WirTypeDef::Variant(vt) = typedef else {
            continue;
        };
        let variant_module_source = vt.meta.module_source.clone();
        // Extract the base variant name from the FQ (e.g. "Module//Name" → "Name")
        let variant_display = vt
            .name
            .fq
            .split("//")
            .nth(1)
            .unwrap_or(&vt.name.fq)
            .to_string();
        for (case_idx, case) in vt.cases.iter().enumerate() {
            for (payload_idx, payload_ty) in case.payload.iter().enumerate() {
                if !is_abstract_ref(payload_ty) {
                    continue;
                }
                // Try to find the TIR variant by module_source and display name
                let Some(ms) = &variant_module_source else {
                    continue;
                };
                let type_table = &*ctx.package.type_table.borrow();
                let Some(tir_variant) = ctx.package.find_variant(ms, &variant_display) else {
                    continue;
                };
                let Some(tir_case) = tir_variant.cases.get(case_idx) else {
                    continue;
                };
                let tir_payload_id = tir_case.payload;
                let new_ty = ctx.type_id_to_wir_type(type_table, tir_payload_id);
                if !is_abstract_ref(&new_ty) {
                    variant_payload_fixups.push((
                        wir_idx,
                        case_idx,
                        payload_idx,
                        new_ty.as_nonnull(),
                    ));
                }
            }
        }
    }
    for (wir_idx, case_idx, payload_idx, new_type) in variant_payload_fixups {
        if let WirTypeDef::Variant(vt) = &mut ctx.types[wir_idx]
            && let Some(payload) = vt.cases[case_idx].payload.get_mut(payload_idx)
        {
            *payload = new_type;
        }
    }

    // Phase 1c: Fix variant case struct fields from the (now-resolved) parent variant's
    // case payload types. Variant case structs are generated (not TIR structs), so Phase 1
    // cannot resolve their abstract ref fields.
    let mut case_struct_fixups: Vec<(usize, usize, WirType)> = Vec::new();
    let case_info_snapshot: Vec<(u32, (u32, u32))> = ctx
        .variant_case_info
        .iter()
        .map(|(k, v)| (*k, *v))
        .collect();
    for (case_struct_idx, (variant_wir_idx, case_idx)) in case_info_snapshot {
        let case_struct_idx = case_struct_idx as usize;
        let variant_wir_idx = variant_wir_idx as usize;
        let case_idx = case_idx as usize;
        // Get the case struct's field count
        let field_count = if let WirTypeDef::Struct(s) = &ctx.types[case_struct_idx] {
            s.fields.len()
        } else {
            continue;
        };
        for field_idx in 0..field_count {
            let is_abstract = if let WirTypeDef::Struct(s) = &ctx.types[case_struct_idx] {
                is_abstract_ref(&s.fields[field_idx].ty)
            } else {
                false
            };
            if !is_abstract {
                continue;
            }
            // Payload fields start at field_idx 1 (field_idx 0 = discriminant)
            let payload_idx = if field_idx == 0 {
                continue;
            } else {
                field_idx - 1
            };
            // Get the parent variant's case payload type
            let payload_ty = if let WirTypeDef::Variant(vt) = &ctx.types[variant_wir_idx]
                && let Some(case) = vt.cases.get(case_idx)
                && let Some(ty) = case.payload.get(payload_idx)
            {
                ty.clone()
            } else {
                continue;
            };
            if is_abstract_ref(&payload_ty) {
                continue; // Still abstract; can't resolve
            }
            case_struct_fixups.push((case_struct_idx, field_idx, payload_ty.as_nonnull()));
        }
    }
    for (type_idx, field_idx, new_type) in case_struct_fixups {
        if let WirTypeDef::Struct(s) = &mut ctx.types[type_idx] {
            s.fields[field_idx].ty = new_type;
        }
    }

    // Phase 2: Fix array element types
    let mut array_fixups: Vec<(usize, WirType)> = Vec::new();
    for (wir_idx, typedef) in ctx.types.iter().enumerate() {
        if let WirTypeDef::Array(a) = typedef
            && is_abstract_ref(&a.element_type)
        {
            // Try to resolve via TIR BuiltinArray types
            {
                let type_table = &*ctx.package.type_table.borrow();
                for type_id in type_table.iter_type_ids() {
                    if let crate::tir::ResolvedType::BuiltinArray(elem_tid) =
                        type_table.get(type_id)
                        && let Some(arr_wir_id) = ctx.array_type_map.get(elem_tid)
                        && arr_wir_id.index() == u32::try_from(wir_idx).unwrap()
                    {
                        let wir_type = ctx.type_id_to_wir_type(type_table, *elem_tid);
                        if !is_abstract_ref(&wir_type) {
                            array_fixups.push((wir_idx, wir_type));
                            break;
                        }
                    }
                }
            }
        }
    }
    for (type_idx, new_elem_type) in array_fixups {
        if let WirTypeDef::Array(a) = &mut ctx.types[type_idx] {
            a.element_type = new_elem_type;
        }
    }

    // Phase 3: Fix variant case payload types
    let mut variant_fixups: Vec<(usize, usize, Vec<WirType>)> = Vec::new();
    for (wir_idx, typedef) in ctx.types.iter().enumerate() {
        if let WirTypeDef::Variant(v) = typedef {
            for (case_idx, case) in v.cases.iter().enumerate() {
                if case.payload.iter().any(is_abstract_ref) {
                    // Try to resolve payload types through TIR variant decls
                    let tt = ctx.package.type_table.borrow();
                    let v_base = v.name.fq.split("//").nth(1).unwrap_or(&v.name.fq);
                    for tir_variant in &ctx.package.variants {
                        if tir_variant.name == v_base && case_idx < tir_variant.cases.len() {
                            let payload_type_id = tir_variant.cases[case_idx].payload;
                            if tt.get(payload_type_id) != &crate::tir::ResolvedType::Unit {
                                let wir_type = ctx.type_id_to_wir_type(&tt, payload_type_id);
                                if !is_abstract_ref(&wir_type) {
                                    variant_fixups.push((wir_idx, case_idx, vec![wir_type]));
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    for (type_idx, case_idx, payload) in variant_fixups {
        if let WirTypeDef::Variant(v) = &mut ctx.types[type_idx] {
            v.cases[case_idx].payload = payload;
        }
    }
}
