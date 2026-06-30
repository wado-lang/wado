//! `NullableRef` representation lowering for variant types.
//!
//! Rewrites a 2-case `{Unit, Payload(T)}` variant whose `T` is a non-nullable
//! reference to a null-niche shape: unit case → `ref.null none`, payload case →
//! the payload ref itself, no Wasm types emitted for the variant. Dropping the
//! discriminant struct is a size win, but the pass is a mandatory lowering, not an
//! optimization (see `optimize_wir`): the frontend already emits `None` as
//! `ref.null`, so this makes the WIR type match.
//!
//! Runs before SROA and the optimization passes so they see the lowered types.

use crate::hashmap::IndexMap;
use crate::wir::{WirAbstractHeapType, WirInstr, WirPackage, WirType, WirTypeDef, WirVariantRepr};

/// Lower eligible variants to the `NullableRef` representation. Mandatory at every
/// `-O` — not gated like the optimization passes.
pub(super) fn lower_nullable_refs(module: &mut WirPackage) {
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
        // Result nullability (after Phase 3's func-type substitution) lets us
        // fix multi-value `Return { Seq(fields) }`: its fields still carry the
        // `RefAsNonNull` wrappers `cast_nonnull_fields` minted before
        // NullableRef made the corresponding result slot nullable. The
        // StructNew arm below strips these per field, but a multi-value return
        // was already lifted to a `Seq` in `wir_build`, so it slips past.
        let result_nullable: Vec<bool> =
            if let WirTypeDef::Func(ft) = &module.types[func.type_id.index() as usize] {
                ft.results
                    .iter()
                    .map(|r| matches!(r, WirType::Ref { nullable: true, .. }))
                    .collect()
            } else {
                Vec::new()
            };
        if let Some(body) = &mut func.body {
            transform_body(body, &module.types, &vci, &nullable_map);
            if result_nullable.iter().any(|&n| n) {
                strip_nonnull_in_multivalue_returns(body, &result_nullable);
            }
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
fn update_type_definitions(module: &mut WirPackage, nullable_map: &IndexMap<u32, (u32, WirType)>) {
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
    body: &mut [WirInstr],
    types: &[WirTypeDef],
    vci: &IndexMap<u32, (u32, u32)>,
    nullable_map: &IndexMap<u32, (u32, WirType)>,
) {
    for instr in body.iter_mut() {
        transform_instr(instr, types, vci, nullable_map);
    }
}

/// Strip the bogus `RefAsNonNull` wrapper from a multi-value
/// `Return { Seq(fields) }` slot that NullableRef made nullable.
///
/// `cast_nonnull_fields` (wir_build) wraps every aggregate field whose nominal
/// struct type is a non-null ref. An `Option<Ref>` field lowers to a nullable
/// ref here, so forcing it non-null traps on the `None` (null) value. The
/// `StructNew` arm of [`transform_instr`] already strips this per field, but
/// `optimize::multi_value_return` lifts the aggregate to a `Seq` of result
/// values back in wir_build, so its fields never reach that arm.
fn strip_nonnull_in_multivalue_returns(body: &mut [WirInstr], result_nullable: &[bool]) {
    for instr in body.iter_mut() {
        match instr {
            WirInstr::Return { value: Some(v) } => {
                if let WirInstr::Seq(fields) = v.as_mut()
                    && fields.len() == result_nullable.len()
                {
                    for (i, f) in fields.iter_mut().enumerate() {
                        if result_nullable[i]
                            && let WirInstr::RefAsNonNull(inner) = f
                        {
                            *f = std::mem::replace(inner.as_mut(), WirInstr::Nop);
                        }
                    }
                }
            }
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
                strip_nonnull_in_multivalue_returns(body, result_nullable);
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                strip_nonnull_in_multivalue_returns(then_body, result_nullable);
                if let Some(eb) = else_body {
                    strip_nonnull_in_multivalue_returns(eb, result_nullable);
                }
            }
            _ => {}
        }
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
                ..
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
            result_ty,
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
            // Update result_ty to reflect the field's current type after substitution.
            if let Some(WirTypeDef::Struct(st)) = types.get(type_id.index() as usize)
                && let Some(field) = st.fields.iter().find(|f| f.name == *field_name)
            {
                *result_ty = field.ty.clone();
            }
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

        // Keep result_ty up-to-date for get instructions after NullableRef substitution.
        WirInstr::LocalGet { result_ty, .. } | WirInstr::GlobalGet { result_ty, .. } => {
            substitute_type(result_ty, nullable_map);
            // Also handle case struct types — same substitution as DeclareLocal special case.
            // e.g., `__cast_N: Ref { CaseStruct, non-null }` becomes `nullable_payload` after
            // NullableRef, so result_ty must reflect this to avoid stale is_nonnull_result().
            if let WirType::Ref { type_id, .. } = result_ty
                && let Some(&(variant_idx, case_idx)) = vci.get(&type_id.index())
                && let Some(&(payload_case, ref nullable_payload)) = nullable_map.get(&variant_idx)
                && case_idx == payload_case
            {
                *result_ty = nullable_payload.clone();
            }
        }
        WirInstr::ArrayGet {
            array,
            index,
            result_ty,
            type_id,
        }
        | WirInstr::ArrayGetS {
            array,
            index,
            result_ty,
            type_id,
        }
        | WirInstr::ArrayGetU {
            array,
            index,
            result_ty,
            type_id,
        } => {
            transform_instr(array, types, vci, nullable_map);
            transform_instr(index, types, vci, nullable_map);
            if let Some(WirTypeDef::Array(at)) = types.get(type_id.index() as usize) {
                *result_ty = at.element_type.clone();
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
