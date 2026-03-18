//! `NullableRef` optimization for variant types.
//!
//! Rewrites eligible variants (exactly 2 cases: one unit, one with a non-nullable-reference
//! payload) to use a null-niche representation, eliminating the discriminant struct overhead.
//!
//! **Eligibility**: A variant `V` with cases `{Unit, Payload(T)}` qualifies when `T` is a
//! non-nullable reference (concrete struct/array ref, or abstract struct/array ref).
//!
//! **After optimization**:
//! - Unit case → `ref.null none`
//! - Payload case → the payload value itself (as a nullable ref)
//! - No Wasm types are emitted for `V` (base type and case structs become dead)
//!
//! The pass runs **before** SROA and other optimizations so that SROA and the rest of the
//! pipeline see the already-optimized types.

use crate::hashmap::IndexMap;
use crate::wir::{WirAbstractHeapType, WirInstr, WirModule, WirType, WirTypeDef, WirVariantRepr};

/// Run the `NullableRef` optimization on the module.
pub fn nullable_ref_optimize(module: &mut WirModule) {
    // Phase 1: Identify eligible variants.
    // nullable_map: variant_base_type_idx -> (payload_case_idx, nullable_payload_WirType)
    let nullable_map = collect_nullable_variants(&module.types);
    if nullable_map.is_empty() {
        return;
    }

    // Phase 2: Set repr = NullableRef on eligible variant types.
    for (&variant_idx, &(payload_case, _)) in &nullable_map {
        if let WirTypeDef::Variant(vt) = &mut module.types[variant_idx as usize] {
            vt.repr = WirVariantRepr::NullableRef { payload_case };
        }
    }

    // Phase 3: Substitute the variant base type with the nullable payload type everywhere
    // it appears in type definitions (struct fields, func params/results, array elements)
    // and in global variable types.
    update_type_definitions(module, &nullable_map);

    // Phase 4: Transform WIR instruction bodies in all functions.
    // Snapshot variant_case_info so we can identify case structs while mutating functions.
    let vci: IndexMap<u32, (u32, u32)> = module.variant_case_info.clone();
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            transform_body(body, &module.types, &vci, &nullable_map);
        }
    }

    // Phase 5: Mark case struct types as dead so compact_dead_items removes them.
    // The base variant types themselves become unreferenced after Phase 3+4 and will
    // be collected by the normal dce_unreachable_types pass.
    for (&case_struct_idx, &(variant_idx, _)) in &vci {
        if nullable_map.contains_key(&variant_idx) {
            module.dead_type_indices.insert(case_struct_idx);
        }
    }
}

/// Determine whether a variant is eligible for `NullableRef` optimization.
///
/// Returns `(payload_case_idx, nullable_payload_type)` if eligible, `None` otherwise.
fn is_nullable_ref_eligible(cases: &[crate::wir::WirVariantCase]) -> Option<(u32, WirType)> {
    if cases.len() != 2 {
        return None;
    }
    let unit_idx = cases.iter().position(|c| c.payload.is_empty())?;
    let payload_idx = cases.iter().position(|c| c.payload.len() == 1)?;
    if unit_idx == payload_idx {
        return None;
    }
    let payload_ty = &cases[payload_idx].payload[0];
    if !payload_ty.is_nonnull_ref() {
        return None;
    }
    Some((
        u32::try_from(payload_idx).unwrap(),
        payload_ty.clone().as_nullable(),
    ))
}

/// Collect all NullableRef-eligible variants in the module.
///
/// Uses a fixed-point algorithm: a variant is only eligible if its payload type is NOT
/// itself an eligible `NullableRef` variant. When payload type Q is also NullableRef-eligible,
/// Q's `None` case becomes `ref.null`, making `V::Some(Q::None)` indistinguishable from
/// `V::None` — breaking the null-niche representation.
fn collect_nullable_variants(types: &[WirTypeDef]) -> IndexMap<u32, (u32, WirType)> {
    // Collect initial candidates.
    let mut candidates: IndexMap<u32, (u32, WirType)> = IndexMap::default();
    for (idx, typedef) in types.iter().enumerate() {
        if let WirTypeDef::Variant(vt) = typedef
            && let Some((payload_case, nullable_ty)) = is_nullable_ref_eligible(&vt.cases)
        {
            candidates.insert(u32::try_from(idx).unwrap(), (payload_case, nullable_ty));
        }
    }

    // Fixed-point: remove candidates whose payload type is itself a candidate.
    // When payload P is NullableRef-eligible, P's None = ref.null, so V::Some(None) = ref.null
    // = V::None — the representation collapses and loses information.
    loop {
        let to_remove: Vec<u32> = candidates
            .keys()
            .copied()
            .filter(|&variant_idx| {
                if let WirTypeDef::Variant(vt) = &types[variant_idx as usize] {
                    let payload_case_pos =
                        vt.cases.iter().position(|c| c.payload.len() == 1).unwrap();
                    if let WirType::Ref { type_id, .. } = &vt.cases[payload_case_pos].payload[0] {
                        return candidates.contains_key(&type_id.index());
                    }
                }
                false
            })
            .collect();
        if to_remove.is_empty() {
            break;
        }
        for idx in to_remove {
            candidates.shift_remove(&idx);
        }
    }

    candidates
}

/// Replace any `WirType::Ref { type_id }` that refers to a `NullableRef` variant base type
/// with the corresponding nullable payload type.
fn substitute_type(ty: &mut WirType, nullable_map: &IndexMap<u32, (u32, WirType)>) {
    if let WirType::Ref { type_id, .. } = ty
        && let Some((_, nullable_payload)) = nullable_map.get(&type_id.index())
    {
        *ty = nullable_payload.clone();
    }
}

/// Apply type substitution across all type definitions and global variable types.
fn update_type_definitions(module: &mut WirModule, nullable_map: &IndexMap<u32, (u32, WirType)>) {
    for i in 0..module.types.len() {
        match &module.types[i] {
            WirTypeDef::Struct(_) => {
                // Collect field indices whose types need updating
                let field_count = if let WirTypeDef::Struct(s) = &module.types[i] {
                    s.fields.len()
                } else {
                    0
                };
                for field_idx in 0..field_count {
                    let needs_update = if let WirTypeDef::Struct(s) = &module.types[i] {
                        if let WirType::Ref { type_id, .. } = &s.fields[field_idx].ty {
                            nullable_map.contains_key(&type_id.index())
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if needs_update && let WirTypeDef::Struct(s) = &mut module.types[i] {
                        substitute_type(&mut s.fields[field_idx].ty, nullable_map);
                    }
                }
            }
            WirTypeDef::Func(_) => {
                let param_count = if let WirTypeDef::Func(ft) = &module.types[i] {
                    ft.params.len()
                } else {
                    0
                };
                let result_count = if let WirTypeDef::Func(ft) = &module.types[i] {
                    ft.results.len()
                } else {
                    0
                };
                for j in 0..param_count {
                    let needs_update = if let WirTypeDef::Func(ft) = &module.types[i] {
                        if let WirType::Ref { type_id, .. } = &ft.params[j] {
                            nullable_map.contains_key(&type_id.index())
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if needs_update && let WirTypeDef::Func(ft) = &mut module.types[i] {
                        substitute_type(&mut ft.params[j], nullable_map);
                    }
                }
                for j in 0..result_count {
                    let needs_update = if let WirTypeDef::Func(ft) = &module.types[i] {
                        if let WirType::Ref { type_id, .. } = &ft.results[j] {
                            nullable_map.contains_key(&type_id.index())
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if needs_update && let WirTypeDef::Func(ft) = &mut module.types[i] {
                        substitute_type(&mut ft.results[j], nullable_map);
                    }
                }
            }
            WirTypeDef::Array(_) => {
                let needs_update = if let WirTypeDef::Array(a) = &module.types[i] {
                    if let WirType::Ref { type_id, .. } = &a.element_type {
                        nullable_map.contains_key(&type_id.index())
                    } else {
                        false
                    }
                } else {
                    false
                };
                if needs_update && let WirTypeDef::Array(a) = &mut module.types[i] {
                    substitute_type(&mut a.element_type, nullable_map);
                }
            }
            _ => {}
        }
    }

    // Also update case payload types inside variant definitions.
    // This is critical so that build_variant_subtypes in emit.rs emits the correct
    // field types for case structs (e.g., `Option<FieldSizePayload>` → `ref null FieldSizePayload`).
    for typedef in &mut module.types {
        if let WirTypeDef::Variant(v) = typedef {
            for case in &mut v.cases {
                for payload_ty in &mut case.payload {
                    substitute_type(payload_ty, nullable_map);
                }
            }
        }
    }

    for global in &mut module.globals {
        substitute_type(&mut global.ty, nullable_map);
    }
}

/// Recursively transform a list of WIR instructions.
fn transform_body(
    body: &mut Vec<WirInstr>,
    types: &[WirTypeDef],
    vci: &IndexMap<u32, (u32, u32)>,
    nullable_map: &IndexMap<u32, (u32, WirType)>,
) {
    for instr in body.iter_mut() {
        transform_instr(instr, types, vci, nullable_map);
    }
}

/// Recursively transform a single WIR instruction.
///
/// Matches and rewrites variant operations for `NullableRef` types:
/// - `StructNew` on a case struct → `RefNull` (unit) or raw payload (payload case)
/// - `RefTest` on a payload case struct → `I32Eqz(RefIsNull(...))`
/// - `I32Eq(StructGet{variant_base,"discriminant",...}, I32Const(N))` → null check
/// - `StructGet{case_struct,"payload_i",RefCast{case_struct,...}}` → `RefAsNonNull`
/// - `DeclareLocal { ty: variant_base_ref }` → updated to nullable payload type
fn transform_instr(
    instr: &mut WirInstr,
    types: &[WirTypeDef],
    vci: &IndexMap<u32, (u32, u32)>,
    nullable_map: &IndexMap<u32, (u32, WirType)>,
) {
    match instr {
        // Block-structured control flow: update result type and recurse into bodies.
        WirInstr::Block { result, body, .. } => {
            if let Some(ty) = result {
                substitute_type(ty, nullable_map);
            }
            transform_body(body, types, vci, nullable_map);
        }
        WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            transform_body(body, types, vci, nullable_map);
        }
        WirInstr::If {
            result,
            condition,
            then_body,
            else_body,
            ..
        } => {
            if let Some(ty) = result {
                substitute_type(ty, nullable_map);
            }
            transform_instr(condition, types, vci, nullable_map);
            transform_body(then_body, types, vci, nullable_map);
            if let Some(eb) = else_body {
                transform_body(eb, types, vci, nullable_map);
            }
        }

        // DeclareLocal: substitute variant base type or case struct type with nullable payload type.
        WirInstr::DeclareLocal { ty, .. } => {
            // Update if it's the variant BASE type.
            substitute_type(ty, nullable_map);
            // Also update if it's a CASE STRUCT type (pattern match temporaries like __cast_2).
            if let WirType::Ref { type_id, .. } = ty
                && let Some(&(variant_idx, case_idx)) = vci.get(&type_id.index())
                && let Some(&(payload_case, ref nullable_payload)) = nullable_map.get(&variant_idx)
                && case_idx == payload_case
            {
                *ty = nullable_payload.clone();
            }
        }

        // StructNew: variant case construction.
        //
        // Two patterns handled:
        // 1. Case struct (type_id in vci): `StructNew { case_struct, [I32Const(disc), payload?] }`
        // 2. Variant base type (type_id in nullable_map): fallback emitted by translate.rs when
        //    the case struct is not found in type_map (unit cases sometimes use the base type).
        WirInstr::StructNew { type_id, fields } => {
            // Pattern 1: case struct type.
            if let Some(&(variant_idx, case_idx)) = vci.get(&type_id.index())
                && let Some(&(payload_case, _)) = nullable_map.get(&variant_idx)
            {
                if case_idx == payload_case {
                    // Payload case: replace with the payload expression directly.
                    // fields = [I32Const(case_idx), payload_expr]
                    // First recurse into the payload expression, then lift it.
                    if fields.len() >= 2 {
                        transform_instr(&mut fields[1], types, vci, nullable_map);
                        let payload = std::mem::replace(&mut fields[1], WirInstr::Nop);
                        *instr = payload;
                    } else {
                        *instr = WirInstr::Unreachable;
                    }
                    return;
                } else {
                    // Unit case: replace with ref.null none.
                    *instr = WirInstr::RefNull {
                        heap_type: WirAbstractHeapType::None,
                    };
                    return;
                }
            }
            // Pattern 2: variant BASE type (fallback path in translate_variant_construct).
            // The discriminant is the first field (I32Const).
            if let Some(&(payload_case, _)) = nullable_map.get(&type_id.index())
                && let Some(WirInstr::I32Const(disc)) = fields.first()
            {
                let disc = u32::try_from(*disc).unwrap_or(u32::MAX);
                if disc == payload_case {
                    // Payload case: emit payload (second field).
                    if fields.len() >= 2 {
                        transform_instr(&mut fields[1], types, vci, nullable_map);
                        let payload = std::mem::replace(&mut fields[1], WirInstr::Nop);
                        *instr = payload;
                    } else {
                        *instr = WirInstr::Unreachable;
                    }
                } else {
                    // Unit case: replace with ref.null none.
                    *instr = WirInstr::RefNull {
                        heap_type: WirAbstractHeapType::None,
                    };
                }
                return;
            }
            // Not a NullableRef case struct or base: recurse into fields.
            // Also strip any RefAsNonNull wrappers from fields whose type was substituted
            // from non-null to nullable by NullableRef (e.g., Box<T> fields where T = variant).
            // cast_nonnull_fields in translate.rs inserts RefAsNonNull for non-null struct fields,
            // but after NullableRef the field type becomes nullable, so the wrapper is incorrect
            // and would trap when the value is null (= NullableRef None case).
            let nullable_field_indices: Vec<usize> =
                if let Some(WirTypeDef::Struct(st)) = types.get(type_id.index() as usize) {
                    st.fields
                        .iter()
                        .enumerate()
                        .filter_map(|(i, f)| {
                            if matches!(f.ty, WirType::Ref { nullable: true, .. }) {
                                Some(i)
                            } else {
                                None
                            }
                        })
                        .collect()
                } else {
                    Vec::new()
                };
            for (i, f) in fields.iter_mut().enumerate() {
                transform_instr(f, types, vci, nullable_map);
                if nullable_field_indices.contains(&i)
                    && let WirInstr::RefAsNonNull(inner) = f
                {
                    let inner_val = std::mem::replace(inner.as_mut(), WirInstr::Nop);
                    *f = inner_val;
                }
            }
        }

        // RefCast to a payload case struct → RefAsNonNull(expr).
        // This handles the pattern where RefCast is stored in a local (e.g., __cast_2).
        WirInstr::RefCast { type_id, expr, .. } => {
            if let Some(&(variant_idx, case_idx)) = vci.get(&type_id.index())
                && let Some(&(payload_case, _)) = nullable_map.get(&variant_idx)
                && case_idx == payload_case
            {
                transform_instr(expr, types, vci, nullable_map);
                let inner = std::mem::replace(expr, Box::new(WirInstr::Nop));
                *instr = WirInstr::RefAsNonNull(inner);
                return;
            }
            transform_instr(expr, types, vci, nullable_map);
        }

        // RefTest on a payload case struct → I32Eqz(RefIsNull(scrutinee)).
        WirInstr::RefTest { type_id, expr, .. } => {
            if let Some(&(variant_idx, case_idx)) = vci.get(&type_id.index())
                && let Some(&(payload_case, _)) = nullable_map.get(&variant_idx)
                && case_idx == payload_case
            {
                transform_instr(expr, types, vci, nullable_map);
                let scrutinee = std::mem::replace(expr, Box::new(WirInstr::Nop));
                *instr = WirInstr::I32Eqz(Box::new(WirInstr::RefIsNull(scrutinee)));
                return;
            }
            transform_instr(expr, types, vci, nullable_map);
        }

        // I32Eq(StructGet{variant_base, "discriminant", expr}, I32Const(N))
        // → null check for unit case, !null check for payload case.
        WirInstr::I32Eq(lhs, rhs) => {
            if let WirInstr::StructGet {
                type_id,
                field_name,
                expr: inner,
            } = lhs.as_ref()
                && field_name == "discriminant"
                && let Some(&(payload_case, _)) = nullable_map.get(&type_id.index())
                && let WirInstr::I32Const(case_idx) = rhs.as_ref()
            {
                let case_idx = u32::try_from(*case_idx).unwrap_or(u32::MAX);
                // Clone the inner expression, then recurse into it.
                let mut inner_expr = inner.as_ref().clone();
                transform_instr(&mut inner_expr, types, vci, nullable_map);
                let inner_box = Box::new(inner_expr);
                *instr = if case_idx == payload_case {
                    WirInstr::I32Eqz(Box::new(WirInstr::RefIsNull(inner_box)))
                } else {
                    WirInstr::RefIsNull(inner_box)
                };
                return;
            }
            transform_instr(lhs, types, vci, nullable_map);
            transform_instr(rhs, types, vci, nullable_map);
        }

        // StructGet{payload_case_struct, "payload_i", expr}
        // → RefAsNonNull(expr): payload extraction from NullableRef variant.
        // Works regardless of whether expr is RefCast, LocalGet, or another expression,
        // because the NullableRef value IS the payload (accessed as a nullable ref).
        WirInstr::StructGet {
            type_id,
            field_name,
            expr,
        } => {
            if field_name.starts_with("payload_")
                && let Some(&(variant_idx, case_idx)) = vci.get(&type_id.index())
                && let Some(&(payload_case, _)) = nullable_map.get(&variant_idx)
                && case_idx == payload_case
            {
                transform_instr(expr, types, vci, nullable_map);
                let inner = std::mem::replace(expr, Box::new(WirInstr::Nop));
                *instr = WirInstr::RefAsNonNull(inner);
                return;
            }
            transform_instr(expr, types, vci, nullable_map);
        }

        // StructSet: strip RefAsNonNull from value if the target field type became nullable.
        // This handles mutable struct field assignments where the field type was substituted
        // from non-null ref to nullable ref by NullableRef (e.g., TreeMapNode.left/right).
        WirInstr::StructSet {
            type_id,
            field_name,
            expr,
            value,
        } => {
            transform_instr(expr, types, vci, nullable_map);
            transform_instr(value, types, vci, nullable_map);
            // Check if the target field is now nullable after NullableRef substitution.
            let field_is_nullable =
                if let Some(WirTypeDef::Struct(st)) = types.get(type_id.index() as usize) {
                    st.fields
                        .iter()
                        .find(|f| f.name == *field_name)
                        .is_some_and(|f| matches!(f.ty, WirType::Ref { nullable: true, .. }))
                } else {
                    false
                };
            if field_is_nullable && let WirInstr::RefAsNonNull(inner) = value.as_mut() {
                let inner_val = std::mem::replace(inner.as_mut(), WirInstr::Nop);
                **value = inner_val;
            }
        }

        // RefAsNonNull: normalize and simplify after transformation.
        // 1. If inner becomes RefNull (None case), remove the trap-causing wrapper.
        // 2. If inner becomes RefAsNonNull (double-wrap from RefCast+StructGet), collapse.
        WirInstr::RefAsNonNull(inner) => {
            transform_instr(inner, types, vci, nullable_map);
            match inner.as_ref() {
                WirInstr::RefNull { .. } => {
                    // ref.as_non_null(ref.null none) would trap — remove the wrapper.
                    let null_val = std::mem::replace(inner.as_mut(), WirInstr::Nop);
                    *instr = null_val;
                }
                WirInstr::RefAsNonNull(_) => {
                    // Double ref.as_non_null: RefAsNonNull(RefAsNonNull(x)) → RefAsNonNull(x).
                    // This happens when RefCast → RefAsNonNull(expr) and StructGet wraps again.
                    let inner_inner = std::mem::replace(inner.as_mut(), WirInstr::Nop);
                    *instr = inner_inner;
                }
                _ => {}
            }
        }

        // For all other instructions, recurse into Box<WirInstr> children.
        _ => {
            instr.for_each_boxed_child_mut(&mut |child| {
                transform_instr(child, types, vci, nullable_map);
            });
        }
    }
}
