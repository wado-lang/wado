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

/// Register all types from the Project into the WirContext.
///
/// This follows the multi-phase registration order from codegen to ensure
/// type dependencies are satisfied.
pub fn register_types(ctx: &mut WirContext<'_>) {
    let entry_tir = ctx.project.entry_module();
    let type_table = &*entry_tir.type_table.borrow();

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

    // Phase 4: Monomorphized entry module structs
    register_mono_entry_types(ctx);

    // Phase 4.5b: Monomorphized variants
    register_mono_variants(ctx);

    // Phase 5: All remaining array types
    register_remaining_arrays(ctx);

    // Register enums from all modules
    register_enums(ctx);

    // Register flags from all modules
    register_flags(ctx);
}

/// Register a single struct type.
fn register_struct(
    ctx: &mut WirContext<'_>,
    tir_struct: &TirStruct,
    type_table: &TypeTable,
    module_source: &ModuleSource,
) {
    let struct_name = StructName::new(module_source.clone(), tir_struct.name.clone());

    // Skip if already registered
    if ctx.struct_type_map.contains_key(&struct_name) {
        return;
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

    let generic_origin = tir_struct.monomorph_info.as_ref().map(|info| WirGenericOrigin {
        base_name: info.generic_name.clone(),
        type_args: info
            .type_args
            .iter()
            .map(|&ta| type_table.mangle_type_name(ta))
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

/// Register a raw GC array type for a given element TypeId.
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
                        mutable: false,
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
    // Generic variants are registered as separate entries with resolved type_params
    // Skip this for now - variants without type_params are already registered
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
