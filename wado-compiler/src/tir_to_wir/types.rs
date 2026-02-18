//! Type registration — translates TIR type definitions to WIR type definitions.
//!
//! Follows the same multi-phase registration order as codegen.rs to handle
//! type dependencies correctly.

use crate::name::{ModuleSource, StructName};
use crate::tir::{ResolvedType, TirStruct, TirVariantDecl, TypeTable};
use crate::wasm_plan::{TypeDecl, sort_types_topologically};
use crate::wir::{
    WirArrayType, WirEnumCase, WirEnumType, WirField, WirGenericOrigin, WirMeta, WirName,
    WirStructType, WirTypeDef, WirVariantCase, WirVariantType,
};

use super::context::WirContext;

/// Register all types from the Project into the `WirContext`.
///
/// This follows the multi-phase registration order from codegen to ensure
/// type dependencies are satisfied.
pub fn register_types(ctx: &mut WirContext<'_>) {
    let entry_tir = ctx.project.entry_module();
    let _type_table = &*entry_tir.type_table.borrow();

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

    // Phase 3.5: Pre-register arrays from mono struct fields
    register_mono_field_arrays(ctx);

    // Phase 3.6: All remaining raw array types (must come before wrapper structs)
    register_remaining_arrays(ctx);

    // Phase 3.7: Array<T> wrapper structs — must come before mono entry structs
    // because entry structs may have Array<T> fields that reference these wrappers
    register_array_wrapper_structs(ctx);

    // Phase 4: Monomorphized entry module structs
    register_mono_entry_types(ctx);

    // Phase 4.5b: Monomorphized variants
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
    let struct_name = StructName::new(module_source.clone(), tir_struct.name.clone());

    // Skip if already registered (exact match)
    if ctx.struct_type_map.contains_key(&struct_name) {
        return;
    }

    // Skip if already registered under a different module_source (name-only match).
    // Monomorphized structs like TreeMap<String,i32> may appear in both the
    // definition module and the entry module. Only register the first occurrence.
    // Only applies to monomorphized structs — non-mono structs with the same name
    // from different modules (e.g., two modules defining `Pair`) are distinct types.
    if tir_struct.monomorph_info.is_some() {
        if let Some(existing) = ctx.lookup_struct_by_name(&tir_struct.name).cloned() {
            ctx.struct_type_map.insert(struct_name, existing);
            return;
        }
    }

    let fq = format!("{module_source}//{}", tir_struct.name);
    let display = tir_struct.name.clone();

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
        .map(|f| WirField {
            name: f.name.clone(),
            ty: ctx.type_id_to_wir_type(type_table, f.type_id),
            mutable: true, // All fields mutable at Wasm GC level
        })
        .collect();

    let generic_origin = tir_struct
        .monomorph_info
        .as_ref()
        .map(|info| WirGenericOrigin {
            base_name: info.generic_name.clone(),
            type_args: info
                .type_args
                .iter()
                .map(|&ta| {
                    // TypeIds may have been removed by DCE; use fallback
                    type_table
                        .try_mangle_type_name(ta)
                        .unwrap_or_else(|| format!("?{}", ta.0))
                })
                .collect(),
        });

    let type_id = ctx.register_type(
        fq,
        WirTypeDef::Struct(WirStructType {
            name: WirName {
                display,
                fq: format!("{module_source}//{}", tir_struct.name),
            },
            fields,
            meta: WirMeta {
                module_source: Some(module_source.clone()),
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

    let display = variant.name.clone();

    let cases: Vec<WirVariantCase> = variant
        .cases
        .iter()
        .enumerate()
        .map(|(i, case)| {
            let payload = if type_table.get(case.payload) == &ResolvedType::Unit {
                Vec::new()
            } else {
                vec![ctx.type_id_to_wir_type(type_table, case.payload)]
            };
            WirVariantCase {
                name: case.name.clone(),
                index: u32::try_from(i).expect("too many variant cases"),
                payload,
            }
        })
        .collect();

    // Variants don't have monomorph_info directly; generic_origin is None for non-generic
    let generic_origin = None;

    let type_id = ctx.register_type(
        fq.clone(),
        WirTypeDef::Variant(WirVariantType {
            name: WirName {
                display,
                fq: fq.clone(),
            },
            cases: cases.clone(),
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
                    display: format!("{}::{}", variant.name, case.name),
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

    let fq = format!("builtin::array<{elem_name}>");
    let elem_wir_type = ctx.type_id_to_wir_type(type_table, element_type_id);

    let type_id = ctx.register_type(
        fq.clone(),
        WirTypeDef::Array(WirArrayType {
            name: WirName {
                display: format!("array<{elem_name}>"),
                fq,
            },
            element_type: elem_wir_type,
            mutable: true,
            meta: WirMeta::default(),
            generic_origin: None,
        }),
    );

    ctx.array_type_map.insert(element_type_id, type_id.clone());
    ctx.array_type_by_name.insert(elem_name, type_id);
}

// === Phase implementations ===

fn register_box_structs(ctx: &mut WirContext<'_>) {
    for (_module_source, tir_mod) in &ctx.project.tir_modules {
        let type_table = &*tir_mod.type_table.borrow();
        for s in &tir_mod.structs {
            if s.monomorph_info
                .as_ref()
                .is_some_and(|info| info.generic_name == "Box")
            {
                register_struct(ctx, s, type_table, &tir_mod.module_source);
            }
        }
    }
}

fn register_library_types(ctx: &mut WirContext<'_>) {
    let entry_source = &ctx.project.entry_module_source;
    for (module_source, tir_mod) in &ctx.project.tir_modules {
        if module_source == entry_source || module_source.is_wasi() {
            continue;
        }
        let type_table = &*tir_mod.type_table.borrow();

        // Collect non-mono structs and variants for topological sorting
        let structs: Vec<_> = tir_mod
            .structs
            .iter()
            .filter(|s| s.monomorph_info.is_none() && s.type_params.is_empty())
            .collect();
        let variants: Vec<_> = tir_mod
            .variants
            .iter()
            .filter(|v| v.type_params.is_empty())
            .collect();

        let structs_slice: Vec<_> = structs.iter().map(|s| (*s).clone()).collect();
        let variants_slice: Vec<_> = variants.iter().map(|v| (*v).clone()).collect();

        let sorted = sort_types_topologically(&structs_slice, &variants_slice, type_table);
        for decl in sorted {
            match decl {
                TypeDecl::Struct(s) => register_struct(ctx, s, type_table, module_source),
                TypeDecl::Variant(v) => register_variant(ctx, v, type_table, module_source),
            }
        }
    }
}

fn register_tuple_types(ctx: &mut WirContext<'_>) {
    for tir_mod in ctx.project.tir_modules.values() {
        let type_table = &*tir_mod.type_table.borrow();
        for type_id in type_table.iter_type_ids() {
            let resolved = type_table.get(type_id);
            if let ResolvedType::Tuple(elements) = resolved {
                if ctx.tuple_type_map.contains_key(elements) {
                    continue;
                }
                let elem_names: Vec<String> = elements
                    .iter()
                    .map(|&e| type_table.mangle_type_name(e))
                    .collect();
                let display = format!("[{}]", elem_names.join(", "));
                let fq = format!("tuple//{display}");

                let fields: Vec<WirField> = elements
                    .iter()
                    .enumerate()
                    .map(|(i, &elem_type_id)| WirField {
                        name: format!("{i}"),
                        ty: ctx.type_id_to_wir_type(type_table, elem_type_id),
                        mutable: true,
                    })
                    .collect();

                let wir_type_id = ctx.register_type(
                    fq.clone(),
                    WirTypeDef::Struct(WirStructType {
                        name: WirName { display, fq },
                        fields,
                        meta: WirMeta::default(),
                        generic_origin: None,
                        newtype_origin: None,
                    }),
                );
                ctx.tuple_type_map.insert(elements.clone(), wir_type_id);
            }
        }
    }
}

fn register_entry_types(ctx: &mut WirContext<'_>) {
    let entry_tir = ctx.project.entry_module();
    let type_table = &*entry_tir.type_table.borrow();
    let module_source = &entry_tir.module_source;

    let structs: Vec<_> = entry_tir
        .structs
        .iter()
        .filter(|s| s.monomorph_info.is_none() && s.type_params.is_empty())
        .cloned()
        .collect();
    let variants: Vec<_> = entry_tir
        .variants
        .iter()
        .filter(|v| v.type_params.is_empty())
        .cloned()
        .collect();

    let sorted = sort_types_topologically(&structs, &variants, type_table);
    for decl in sorted {
        match decl {
            TypeDecl::Struct(s) => register_struct(ctx, s, type_table, module_source),
            TypeDecl::Variant(v) => register_variant(ctx, v, type_table, module_source),
        }
    }
}

fn register_nonmono_arrays(ctx: &mut WirContext<'_>) {
    for tir_mod in ctx.project.tir_modules.values() {
        let type_table = &*tir_mod.type_table.borrow();
        for type_id in type_table.iter_type_ids() {
            if let ResolvedType::BuiltinArray(elem_type_id) = type_table.get(type_id) {
                register_raw_array_type(ctx, *elem_type_id, type_table);
            }
        }
    }
}

fn register_mono_library_types(ctx: &mut WirContext<'_>) {
    let entry_source = &ctx.project.entry_module_source;
    for (module_source, tir_mod) in &ctx.project.tir_modules {
        if module_source == entry_source || module_source.is_wasi() {
            continue;
        }
        let type_table = &*tir_mod.type_table.borrow();

        for s in &tir_mod.structs {
            if s.monomorph_info.is_some() {
                // Skip Box<T> (already registered in Phase 0)
                if s.monomorph_info
                    .as_ref()
                    .is_some_and(|info| info.generic_name == "Box")
                {
                    continue;
                }
                register_struct(ctx, s, type_table, module_source);
            }
        }
    }
}

fn register_mono_field_arrays(ctx: &mut WirContext<'_>) {
    // Pre-scan monomorphized struct fields for array types
    for tir_mod in ctx.project.tir_modules.values() {
        let type_table = &*tir_mod.type_table.borrow();
        for s in &tir_mod.structs {
            if s.monomorph_info.is_some() {
                for field in &s.fields {
                    if let ResolvedType::BuiltinArray(elem) = type_table.get(field.type_id) {
                        register_raw_array_type(ctx, *elem, type_table);
                    }
                }
            }
        }
    }
}

fn register_mono_entry_types(ctx: &mut WirContext<'_>) {
    let entry_tir = ctx.project.entry_module();
    let type_table = &*entry_tir.type_table.borrow();
    let module_source = &entry_tir.module_source;

    for s in &entry_tir.structs {
        if s.monomorph_info.is_some() {
            if s.monomorph_info
                .as_ref()
                .is_some_and(|info| info.generic_name == "Box")
            {
                continue;
            }
            register_struct(ctx, s, type_table, module_source);
        }
    }
}

fn register_mono_variants(ctx: &mut WirContext<'_>) {
    // Find generic variant instances in all type tables (e.g., Result<i32, String>)
    // and register them as WIR variant types.
    let mut to_register: Vec<(
        String,            // mangled name (e.g., "Result<i32, String>")
        ModuleSource,      // module source of the base variant
        Vec<(String, Vec<crate::wir::WirType>)>, // cases: (name, payload types)
    )> = Vec::new();

    for tir_mod in ctx.project.tir_modules.values() {
        let type_table = &*tir_mod.type_table.borrow();
        for type_id in type_table.iter_type_ids() {
            if let ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } = type_table.get(type_id)
            {
                // Skip Option (handled specially)
                if name == "Option" {
                    continue;
                }
                // Check if this generic instance is a variant by looking for the
                // base variant declaration in the module
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| type_table.mangle_type_name(*t))
                    .collect();
                let mangled = crate::name::mangle_generic_name(name, &type_arg_names);
                let fq = format!("{module_source}//{mangled}");
                if ctx.variant_type_map.contains_key(&fq) {
                    continue;
                }
                // Find the base variant declaration in the source module
                if let Some(source_mod) = ctx.project.tir_modules.get(module_source) {
                    let source_type_table = &*source_mod.type_table.borrow();
                    let base_variant = source_mod
                        .variants
                        .iter()
                        .find(|v| v.name == *name && !v.type_params.is_empty());
                    if let Some(base) = base_variant {
                        // Build a type param substitution map:
                        // base.type_params[i].name -> type_args[i]
                        let type_args = type_args.clone();
                        let cases: Vec<(String, Vec<crate::wir::WirType>)> = base
                            .cases
                            .iter()
                            .map(|case| {
                                // Resolve the payload through type param substitution
                                let payload_resolved =
                                    source_type_table.get(case.payload);
                                let payload = match payload_resolved {
                                    ResolvedType::Unit => Vec::new(),
                                    ResolvedType::TypeParam { index, .. } => {
                                        // Substitute type param with concrete type arg
                                        let idx = *index as usize;
                                        if idx < type_args.len() {
                                            vec![ctx.type_id_to_wir_type(
                                                type_table,
                                                type_args[idx],
                                            )]
                                        } else {
                                            Vec::new()
                                        }
                                    }
                                    _ => {
                                        // Non-param payload (e.g., concrete type)
                                        vec![ctx.type_id_to_wir_type(
                                            source_type_table,
                                            case.payload,
                                        )]
                                    }
                                };
                                (case.name.clone(), payload)
                            })
                            .collect();
                        to_register.push((mangled, module_source.clone(), cases));
                    }
                }
            }
        }
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
                name: WirName {
                    display: mangled_name.clone(),
                    fq: fq.clone(),
                },
                cases: wir_cases.clone(),
                meta: WirMeta {
                    module_source: Some(module_source),
                    ..WirMeta::default()
                },
                generic_origin: None,
                newtype_origin: None,
            }),
        );

        ctx.variant_type_map.insert(fq.clone(), type_id.clone());

        // Register case-specific struct types
        for case in &wir_cases {
            if case.payload.is_empty() {
                continue;
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
                        display: format!("{}::{}", mangled_name, case.name),
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
}

fn register_remaining_arrays(ctx: &mut WirContext<'_>) {
    for tir_mod in ctx.project.tir_modules.values() {
        let type_table = &*tir_mod.type_table.borrow();
        for type_id in type_table.iter_type_ids() {
            if let ResolvedType::BuiltinArray(elem_type_id) = type_table.get(type_id) {
                register_raw_array_type(ctx, *elem_type_id, type_table);
            }
        }
    }
}

fn register_enums(ctx: &mut WirContext<'_>) {
    for (module_source, tir_mod) in &ctx.project.tir_modules {
        for e in &tir_mod.enums {
            let fq = format!("{module_source}//enum:{}", e.name);
            if ctx.type_map.contains_key(&fq) {
                continue;
            }
            let display = e.name.clone();
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
                        display,
                        fq: format!("{module_source}//enum:{}", e.name),
                    },
                    cases,
                    meta: WirMeta {
                        module_source: Some(module_source.clone()),
                        ..WirMeta::default()
                    },
                }),
            );
        }
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

    // Collect all unique function signatures from all modules
    let mut fn_sigs: Vec<(Vec<WirType>, Vec<WirType>)> = Vec::new();
    let mut seen_keys: indexmap::IndexSet<String> = indexmap::IndexSet::new();

    for tir_mod in ctx.project.tir_modules.values() {
        let type_table = &*tir_mod.type_table.borrow();
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
                let result_wirs: Vec<WirType> =
                    if *return_type == crate::tir::TypeTable::UNIT
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
/// not a struct definition. The old codegen creates wrapper structs on-the-fly
/// via `get_or_create_array_struct_type`. We replicate that here.
fn register_array_wrapper_structs(ctx: &mut WirContext<'_>) {
    use crate::tir::ResolvedType;

    // Collect unique Array<T> element types across all modules
    let mut array_elem_types: Vec<(crate::tir::TypeId, String, ModuleSource)> = Vec::new();
    for tir_mod in ctx.project.tir_modules.values() {
        let type_table = &*tir_mod.type_table.borrow();
        for type_id in type_table.iter_type_ids() {
            if let ResolvedType::GenericInstance { name, type_args, .. } = type_table.get(type_id) {
                if name == "Array" && type_args.len() == 1 {
                    let elem_name = type_table.mangle_type_name(type_args[0]);
                    if !array_elem_types.iter().any(|(_, n, _)| n == &elem_name) {
                        // Ensure raw array type is registered
                        register_raw_array_type(ctx, type_args[0], type_table);
                        array_elem_types.push((
                            type_args[0],
                            elem_name,
                            tir_mod.module_source.clone(),
                        ));
                    }
                }
            }
        }
    }

    for (_, elem_name, _module_source) in &array_elem_types {
        let mangled =
            crate::name::mangle_generic_name("Array", std::slice::from_ref(elem_name));

        // Skip if already registered (by name)
        if ctx.lookup_struct_by_name(&mangled).is_some() {
            continue;
        }

        let raw_array_type_id = ctx.array_type_by_name.get(elem_name).cloned();
        let Some(raw_type) = raw_array_type_id else {
            continue;
        };

        let module_source = ModuleSource::core("prelude");
        let struct_name = StructName::new(module_source.clone(), mangled.clone());
        if ctx.struct_type_map.contains_key(&struct_name) {
            continue;
        }

        let fq = format!("{module_source}//{mangled}");
        let type_id = ctx.register_type(
            fq.clone(),
            WirTypeDef::Struct(WirStructType {
                name: WirName {
                    display: mangled.clone(),
                    fq,
                },
                fields: vec![
                    WirField {
                        name: "repr".to_string(),
                        ty: crate::wir::WirType::Ref {
                            type_id: raw_type,
                            nullable: true,
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
                    module_source: Some(module_source.clone()),
                    ..WirMeta::default()
                },
                generic_origin: Some(WirGenericOrigin {
                    base_name: "Array".to_string(),
                    type_args: vec![elem_name.clone()],
                }),
                newtype_origin: None,
            }),
        );
        ctx.struct_type_map.insert(struct_name, type_id);
    }
}

/// Check if a WirType is an unresolved abstract struct/array reference.
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
            let struct_name_str = &s.name.display;

            let mut resolved = None;

            // Try resolving from TIR structs
            for tir_mod in ctx.project.tir_modules.values() {
                let type_table = &*tir_mod.type_table.borrow();
                for tir_struct in &tir_mod.structs {
                    if &tir_struct.name != struct_name_str {
                        continue;
                    }
                    if let Some(ms) = module_source {
                        if &tir_mod.module_source != ms {
                            continue;
                        }
                    }
                    if field_idx < tir_struct.fields.len() {
                        let wir_type =
                            ctx.type_id_to_wir_type(type_table, tir_struct.fields[field_idx].type_id);
                        if !is_abstract_ref(&wir_type) {
                            resolved = Some(wir_type);
                            break;
                        }
                    }
                }
                if resolved.is_some() {
                    break;
                }
            }

            // Try resolving from TIR tuple types (tuples have display names like "[T, U]")
            if resolved.is_none() && struct_name_str.starts_with('[') {
                for tir_mod in ctx.project.tir_modules.values() {
                    let type_table = &*tir_mod.type_table.borrow();
                    for type_id in type_table.iter_type_ids() {
                        if let ResolvedType::Tuple(elements) = type_table.get(type_id) {
                            if field_idx < elements.len() {
                                // Check if this tuple maps to the same WIR type
                                if let Some(wir_tid) = ctx.tuple_type_map.get(elements) {
                                    if wir_tid.index() == u32::try_from(wir_idx).unwrap_or(u32::MAX) {
                                        let wir_type =
                                            ctx.type_id_to_wir_type(type_table, elements[field_idx]);
                                        if !is_abstract_ref(&wir_type) {
                                            resolved = Some(wir_type);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if resolved.is_some() {
                        break;
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

    // Phase 2: Fix array element types
    let mut array_fixups: Vec<(usize, WirType)> = Vec::new();
    for (wir_idx, typedef) in ctx.types.iter().enumerate() {
        if let WirTypeDef::Array(a) = typedef {
            if is_abstract_ref(&a.element_type) {
                // Try to resolve via TIR BuiltinArray types
                for tir_mod in ctx.project.tir_modules.values() {
                    let type_table = &*tir_mod.type_table.borrow();
                    for type_id in type_table.iter_type_ids() {
                        if let crate::tir::ResolvedType::BuiltinArray(elem_tid) =
                            type_table.get(type_id)
                        {
                            if let Some(arr_wir_id) = ctx.array_type_map.get(elem_tid) {
                                if arr_wir_id.index() == u32::try_from(wir_idx).unwrap() {
                                    let wir_type =
                                        ctx.type_id_to_wir_type(&type_table, *elem_tid);
                                    if !is_abstract_ref(&wir_type) {
                                        array_fixups.push((wir_idx, wir_type));
                                        break;
                                    }
                                }
                            }
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

    // Phase 3: Fix function parameter/result types
    // Function types are registered during translation, after type registration.
    // Some param/result types may reference types that weren't available yet.
    // Re-resolve by scanning TIR functions for matching signatures.
    let mut func_fixups: Vec<(usize, Vec<WirType>, Vec<WirType>)> = Vec::new();
    for (wir_idx, typedef) in ctx.types.iter().enumerate() {
        if let WirTypeDef::Func(f) = typedef {
            let has_abstract =
                f.params.iter().any(is_abstract_ref) || f.results.iter().any(is_abstract_ref);
            if !has_abstract {
                continue;
            }

            // Find the TIR function that matches this func type
            let fq = &f.name.display;
            let mut resolved_params = None;
            let mut resolved_results = None;
            for body in &ctx.pending_bodies {
                let tir_func = body.tir_func.borrow();
                let tt = body.type_table.borrow();
                if fq.contains(&tir_func.name.to_string()) {
                    let params: Vec<WirType> = tir_func
                        .params
                        .iter()
                        .map(|p| ctx.type_id_to_wir_type(&tt, p.type_id))
                        .collect();
                    let results: Vec<WirType> = if tt.get(tir_func.return_type)
                        != &crate::tir::ResolvedType::Unit
                    {
                        vec![ctx.type_id_to_wir_type(&tt, tir_func.return_type)]
                    } else {
                        Vec::new()
                    };
                    // Only accept if it actually resolves more types
                    let new_abstract_count =
                        params.iter().filter(|t| is_abstract_ref(t)).count()
                            + results.iter().filter(|t| is_abstract_ref(t)).count();
                    let old_abstract_count =
                        f.params.iter().filter(|t| is_abstract_ref(t)).count()
                            + f.results.iter().filter(|t| is_abstract_ref(t)).count();
                    if new_abstract_count < old_abstract_count {
                        resolved_params = Some(params);
                        resolved_results = Some(results);
                        break;
                    }
                }
            }

            if resolved_params.is_some() || resolved_results.is_some() {
                let params = resolved_params.unwrap_or_else(|| f.params.clone());
                let results = resolved_results.unwrap_or_else(|| f.results.clone());
                func_fixups.push((wir_idx, params, results));
            }
        }
    }
    for (type_idx, params, results) in func_fixups {
        if let WirTypeDef::Func(f) = &mut ctx.types[type_idx] {
            f.params = params;
            f.results = results;
        }
    }

    // Phase 4: Fix variant case payload types
    let mut variant_fixups: Vec<(usize, usize, Vec<WirType>)> = Vec::new();
    for (wir_idx, typedef) in ctx.types.iter().enumerate() {
        if let WirTypeDef::Variant(v) = typedef {
            for (case_idx, case) in v.cases.iter().enumerate() {
                if case.payload.iter().any(is_abstract_ref) {
                    // Try to resolve payload types through TIR variant decls
                    for tir_mod in ctx.project.tir_modules.values() {
                        let tt = tir_mod.type_table.borrow();
                        for tir_variant in &tir_mod.variants {
                            if tir_variant.name == v.name.display
                                && case_idx < tir_variant.cases.len()
                            {
                                let payload_type_id = tir_variant.cases[case_idx].payload;
                                if tt.get(payload_type_id) != &crate::tir::ResolvedType::Unit {
                                    let wir_type = ctx.type_id_to_wir_type(&tt, payload_type_id);
                                    if !is_abstract_ref(&wir_type) {
                                        variant_fixups
                                            .push((wir_idx, case_idx, vec![wir_type]));
                                        break;
                                    }
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
