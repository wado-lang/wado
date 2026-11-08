//! Type registration: the WIR type each Wado type lowers to.
//!
//! Every concrete Wado type is given its WIR type before any is defined, so a
//! field may name a type defined after it, its own included: the GC types are
//! emitted as one rec group, where any reference among them is valid.

use crate::defs::DefId;
use crate::module_source::ModuleSource;
use crate::nir::{NirEnum, NirStruct};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId};
use crate::type_table::TypeTable;
use crate::wir::{
    WirArrayType, WirEnumCase, WirEnumType, WirField, WirGenericOrigin, WirMeta, WirName,
    WirStructType, WirType, WirTypeDef, WirTypeId, WirVariantCase, WirVariantRepr, WirVariantType,
};

use crate::hashmap::{IndexMap, IndexSet};

use super::context::WirContext;
use crate::name::{
    CELL_FIELD, RESOURCE_OBJECT_STRUCT, mangle_builtin_array_type, mangle_generic_name,
    wir_enum_type_key, wir_tuple_type_key, wir_type_key, wir_variant_case_key,
};

/// What a concrete Wado type's WIR type is built from.
enum Shape<'a> {
    Struct(&'a NirStruct),
    Tuple(Vec<TypeId>),
    /// Each case's name and payload at the instance's arguments.
    Variant {
        module_source: ModuleSource,
        cases: Vec<(String, TypeId)>,
    },
    Enum(&'a NirEnum),
    Array(TypeId),
}

/// Register the WIR type of every concrete Wado type, then the canonical
/// closure types their function values take.
pub fn register_types(ctx: &mut WirContext<'_>) {
    ctx.handle_box = Some(register_resource_object(ctx));
    let package = ctx.package;
    let shapes: Vec<(WirTypeId, Shape<'_>)> = wado_types(package)
        .into_iter()
        .map(|(type_id, fq, shape)| (ctx.reserve_wado_type(type_id, fq), shape))
        .collect();
    let cells: Vec<(WirTypeId, TypeId)> = cell_payloads(&package.type_table.borrow())
        .into_iter()
        .map(|(name, payload)| {
            let id = ctx.reserve_type(wir_type_key(&ModuleSource::prelude(), &name));
            ctx.cell_type_map.insert(name, id.clone());
            (id, payload)
        })
        .collect();

    let type_table = &*package.type_table.borrow();
    for (id, shape) in shapes {
        define_wado_type(ctx, type_table, &id, shape);
    }
    for (id, payload) in cells {
        let field = WirField {
            name: CELL_FIELD.to_string(),
            ty: cell_field_type(ctx, type_table, payload),
            mutable: true,
        };
        let cell = struct_type(&id, vec![field], Some(ModuleSource::prelude()));
        ctx.define_type(&id, WirTypeDef::Struct(cell));
    }
    register_canonical_closure_types(ctx, type_table);
}

/// Each concrete Wado type a value can have, with the WIR name it lowers to
/// and what its WIR type is built from.
fn wado_types(package: &NirPackage) -> Vec<(TypeId, String, Shape<'_>)> {
    let mut found: IndexMap<TypeId, (String, Shape<'_>)> = IndexMap::default();
    let mut variants: Vec<(TypeId, String, DefId, Vec<TypeId>)> = Vec::new();
    {
        let tt = &*package.type_table.borrow();
        for s in &package.structs {
            let Some((def, args)) = s.instance() else {
                continue;
            };
            let type_id = tt
                .find_struct_instance(def, &args)
                .expect("a concrete struct declaration's type is interned");
            let fq = wir_type_key(&s.module_source, &s.name);
            let previous = found.insert(type_id, (fq, Shape::Struct(s)));
            assert!(previous.is_none(), "one struct type has one declaration");
        }
        for e in &package.enums {
            let type_id = tt
                .find_enum_type(e.def)
                .expect("an enum declaration's type is interned");
            let fq = wir_enum_type_key(&e.module_source, &e.name);
            let previous = found.insert(type_id, (fq, Shape::Enum(e)));
            assert!(previous.is_none(), "one enum type has one declaration");
        }
        for type_id in tt.iter_type_ids().filter(|&t| tt.is_concrete(t)) {
            match tt.get(type_id) {
                ResolvedType::Tuple(elems) => {
                    let names: Vec<String> = elems
                        .iter()
                        .map(|&e| tt.mangle_type_arg_rowless(e))
                        .collect();
                    found.insert(
                        type_id,
                        (wir_tuple_type_key(&names), Shape::Tuple(elems.clone())),
                    );
                }
                ResolvedType::BuiltinArray(elem) => {
                    let fq = mangle_builtin_array_type(&tt.mangle_type_arg_rowless(*elem));
                    found.insert(type_id, (fq, Shape::Array(*elem)));
                }
                ResolvedType::Variant { def, type_args }
                    if is_variant_instance(package, *def, type_args) =>
                {
                    let fq = variant_type_key(tt, *def, type_args);
                    variants.push((type_id, fq, *def, type_args.clone()));
                }
                _ => {}
            }
        }
    }
    for (type_id, fq, def, args) in variants {
        let cases = package
            .variant_cases(def, &args)
            .expect("filtered to declared variants");
        let module_source = package
            .find_variant(def)
            .expect("declared")
            .module_source
            .clone();
        found.insert(
            type_id,
            (
                fq,
                Shape::Variant {
                    module_source,
                    cases,
                },
            ),
        );
    }
    found
        .into_iter()
        .map(|(type_id, (fq, shape))| (type_id, fq, shape))
        .collect()
}

/// Whether `def` applied to `type_args` is an instance of a variant the
/// package declares, rather than the declaration of a generic one.
fn is_variant_instance(package: &NirPackage, def: DefId, type_args: &[TypeId]) -> bool {
    package
        .find_variant(def)
        .is_some_and(|v| v.type_arity() == type_args.len())
}

/// Each concrete cell's WIR struct name, with the payload it holds. Cells of
/// one payload representation share the struct.
fn cell_payloads(type_table: &TypeTable) -> IndexMap<String, TypeId> {
    let mut cells: IndexMap<String, TypeId> = IndexMap::default();
    for cell in type_table.iter_type_ids() {
        if let Some(payload) = type_table.cell_payload(cell)
            && type_table.is_concrete(cell)
        {
            cells
                .entry(type_table.cell_struct_name(cell))
                .or_insert(payload);
        }
    }
    cells
}

fn define_wado_type(
    ctx: &mut WirContext<'_>,
    type_table: &TypeTable,
    id: &WirTypeId,
    shape: Shape<'_>,
) {
    let def = match shape {
        Shape::Struct(s) => {
            let fields = s.fields.iter().map(|f| (f.name.clone(), f.type_id));
            let fields = slot_fields(ctx, type_table, id, fields);
            WirTypeDef::Struct(WirStructType {
                generic_origin: s.monomorph_info.as_ref().map(|info| WirGenericOrigin {
                    base_name: info.generic_name.clone(),
                    type_args: info
                        .impl_type_args
                        .iter()
                        .map(|&ta| type_table.mangle_type_name(ta))
                        .collect(),
                }),
                ..struct_type(id, fields, Some(s.module_source.clone()))
            })
        }
        Shape::Tuple(elems) => {
            let fields = elems.iter().enumerate().map(|(i, &e)| (i.to_string(), e));
            let fields = slot_fields(ctx, type_table, id, fields);
            WirTypeDef::Struct(struct_type(id, fields, None))
        }
        Shape::Variant {
            module_source,
            cases,
        } => {
            let cases: Vec<WirVariantCase> = cases
                .into_iter()
                .enumerate()
                .map(|(i, (name, payload))| WirVariantCase {
                    name,
                    index: u32::try_from(i).expect("too many variant cases"),
                    payload: if matches!(type_table.get(payload), ResolvedType::Unit) {
                        Vec::new()
                    } else {
                        let ty = ctx.type_id_to_wir_type(type_table, payload);
                        vec![ctx.boxed_slot_type(type_table, payload, ty)]
                    },
                })
                .collect();
            register_case_structs(ctx, id, &cases);
            WirTypeDef::Variant(WirVariantType {
                name: WirName {
                    fq: id.fq().to_string(),
                },
                cases,
                repr: WirVariantRepr::default(),
                meta: WirMeta {
                    module_source: Some(module_source),
                    ..WirMeta::default()
                },
                generic_origin: None,
                newtype_origin: None,
            })
        }
        Shape::Enum(e) => WirTypeDef::Enum(WirEnumType {
            name: WirName {
                fq: id.fq().to_string(),
            },
            cases: e
                .cases
                .iter()
                .enumerate()
                .map(|(i, case)| WirEnumCase {
                    name: case.name.clone(),
                    discriminant: i32::try_from(i).expect("too many enum cases"),
                })
                .collect(),
            meta: WirMeta {
                module_source: Some(e.module_source.clone()),
                ..WirMeta::default()
            },
        }),
        Shape::Array(elem) => {
            let ty = ctx.type_id_to_wir_type(type_table, elem);
            WirTypeDef::Array(WirArrayType {
                name: WirName {
                    fq: id.fq().to_string(),
                },
                element_type: ctx.boxed_slot_type(type_table, elem, ty),
                mutable: true,
                meta: WirMeta::default(),
                generic_origin: None,
            })
        }
    };
    ctx.define_type(id, def);
}

/// The WIR fields of struct `id` holding `fields`, less the `()` ones, which
/// have no Wasm representation. Leaving one out records the NIR field names,
/// since a NIR field index then counts it.
fn slot_fields(
    ctx: &mut WirContext<'_>,
    type_table: &TypeTable,
    id: &WirTypeId,
    fields: impl Iterator<Item = (String, TypeId)>,
) -> Vec<WirField> {
    let mut names = Vec::new();
    let mut wir_fields = Vec::new();
    for (name, type_id) in fields {
        let ty = ctx.type_id_to_wir_type(type_table, type_id);
        if !matches!(ty, WirType::Unit) {
            wir_fields.push(WirField {
                name: name.clone(),
                ty: ctx.slot_type(type_table, type_id, ty),
                mutable: true,
            });
        }
        names.push(name);
    }
    if wir_fields.len() != names.len() {
        ctx.nir_field_names.insert(id.index(), names);
    }
    wir_fields
}

fn struct_type(
    id: &WirTypeId,
    fields: Vec<WirField>,
    module: Option<ModuleSource>,
) -> WirStructType {
    WirStructType {
        name: WirName {
            fq: id.fq().to_string(),
        },
        fields,
        meta: WirMeta {
            module_source: module,
            ..WirMeta::default()
        },
        generic_origin: None,
        newtype_origin: None,
        supertype: None,
    }
}

/// The struct each payload case of variant `variant` is, which the emitter
/// places in the variant's subtype hierarchy. A unit case shares the variant's
/// own struct.
fn register_case_structs(ctx: &mut WirContext<'_>, variant: &WirTypeId, cases: &[WirVariantCase]) {
    for case in cases.iter().filter(|c| !c.payload.is_empty()) {
        let mut fields = vec![WirField {
            name: "discriminant".to_string(),
            ty: WirType::I32,
            mutable: false,
        }];
        fields.extend(case.payload.iter().enumerate().map(|(j, ty)| WirField {
            name: format!("payload_{j}"),
            ty: ty.clone(),
            mutable: false,
        }));
        let case_id = ctx.reserve_type(wir_variant_case_key(variant.fq(), &case.name));
        ctx.define_type(
            &case_id,
            WirTypeDef::Struct(struct_type(&case_id, fields, None)),
        );
        ctx.variant_case_info
            .insert(case_id.index(), (variant.index(), case.index));
        ctx.variant_case_types
            .insert((variant.index(), case.index), case_id);
    }
}

/// The WIR type a cell's `value` holds a `payload` as: the payload itself,
/// never in a slot's own cell.
fn cell_field_type(ctx: &WirContext<'_>, type_table: &TypeTable, payload: TypeId) -> WirType {
    let ty = ctx.type_id_to_wir_type(type_table, payload);
    ctx.unboxed_slot_type(type_table, payload, ty)
}

/// The struct an affine resource is (WEP: Affine Types).
fn register_resource_object(ctx: &mut WirContext<'_>) -> WirTypeId {
    let module_source = ModuleSource::prelude();
    let fq = wir_type_key(&module_source, RESOURCE_OBJECT_STRUCT);
    ctx.register_type(
        fq.clone(),
        WirTypeDef::Struct(WirStructType {
            name: WirName { fq },
            fields: vec![WirField {
                name: "value".to_string(),
                ty: WirType::I32,
                mutable: true,
            }],
            meta: WirMeta {
                module_source: Some(module_source),
                ..WirMeta::default()
            },
            generic_origin: None,
            newtype_origin: None,
            supertype: None,
        }),
    )
}

/// Register canonical closure types for all `Function` types found in type tables.
/// Each unique function signature `fn(P1, P2, ...) -> R` gets:
/// - A canonical func type: `(ref struct, P1, P2, ...) -> R`
/// - A canonical closure struct: `{ env: ref struct, func: ref $func_type }`
fn register_canonical_closure_types(ctx: &mut WirContext<'_>, type_table: &TypeTable) {
    // Every unique function signature in the shared type table, each carrying
    // whether its `CanonicalClosure_K` takes the slim `{ env, func }` schema or
    // the inspectable `{ env, inspect, func }` one — decided by
    // whether a `fn(..)^Inspect` for that `(arity, return_type)` survived
    // DCE.
    let mut fn_sigs: Vec<(Vec<WirType>, Vec<WirType>, bool)> = Vec::new();
    let mut seen_keys: IndexSet<String> = IndexSet::default();

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

            // Unit params are erased from the canonical signature, matching
            // the function-signature convention (unit has no Wasm
            // representation); every canonical-key site filters identically.
            let param_wirs: Vec<WirType> = params
                .iter()
                .map(|p| ctx.type_id_to_wir_type(type_table, *p))
                .filter(|t| !matches!(t, WirType::Unit))
                .collect();
            let result_wirs: Vec<WirType> = if type_table.is_stackless(*return_type) {
                vec![]
            } else {
                vec![ctx.type_id_to_wir_type(type_table, *return_type)]
            };
            let key = WirContext::canonical_closure_key(&param_wirs, &result_wirs);
            if seen_keys.insert(key) {
                let is_inspectable = ctx
                    .inspectable_fn_dispatch
                    .contains(&(params.len(), *return_type));
                fn_sigs.push((param_wirs, result_wirs, is_inspectable));
            }
        }
    }

    // Inspectable signatures share the supertype `$canonical_inspectable_base`,
    // which is what a `fn(..)^Inspect` stub `ref.cast`s to, so no per-shape
    // struct map is needed.
    for (param_wirs, result_wirs, is_inspectable) in fn_sigs {
        ctx.get_or_create_canonical_closure_type(param_wirs, result_wirs, is_inspectable);
    }
}

/// The WIR name of variant `def` at `type_args` (none for a plain variant):
/// module-qualified arguments keep two same-named ones apart.
fn variant_type_key(type_table: &TypeTable, def: DefId, type_args: &[TypeId]) -> String {
    let args: Vec<String> = type_args
        .iter()
        .map(|&t| type_table.mangle_type_arg_rowless(t))
        .collect();
    let name = mangle_generic_name(type_table.decl_name(def), &args);
    wir_type_key(type_table.def_module(def), &name)
}
