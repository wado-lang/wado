//! Single-field parameter SROA for WIR.
//!
//! Rewrites internal functions that take `ref null S` parameters (where S is any
//! single-field struct, including Box<T>) to take the scalar field value directly,
//! eliminating GC struct allocation at every call site.

use crate::hashmap::IndexSet;
use crate::wir::{WirFuncType, WirInstr, WirPackage, WirType, WirTypeDef, WirTypeId};

use super::sroa_variant_return::is_eligible_field_type;
use super::util::collect_pinned_func_ids;

/// Box<T> parameter SROA.
///
/// Rewrites internal functions that take `ref Box<T>` parameters (single-field
/// wrapper structs) to take the inner scalar `T` directly. At call sites, the
/// `StructNew { value: expr }` allocation is replaced with just `expr`.
///
/// A parameter is eligible when:
/// - Its type is `Ref` to a struct with `generic_origin.base_name == "Box"`
/// - The struct has exactly one field named "value"
/// - Within the function body, the parameter is only used via:
///   a. `StructGet { field_name: "value", expr: LocalGet(param) }` — scalar read
///   b. As an argument to another function at a position that is also being SROA'd
/// - The function is not exported, not in an element table, and not `RefFunc`'d
pub(super) fn sroa_single_field_parameters(module: &mut WirPackage) {
    let pinned = collect_pinned_func_ids(module);

    // Phase 1: identify candidate (func_id, param_index) pairs.
    let mut candidates: IndexSet<(u32, usize)> = IndexSet::default();
    // Map from (func_id, param_idx) → (struct_type_id_index, inner WirType, field_name).
    let mut candidate_info: crate::hashmap::IndexMap<(u32, usize), (u32, WirType, String)> =
        crate::hashmap::IndexMap::default();

    for (i, func) in module.functions.iter().enumerate() {
        let func_id_index = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap();
        if pinned.contains(&func_id_index) || func.body.is_none() {
            continue;
        }
        let type_idx = func.type_id.index();
        let Some(WirTypeDef::Func(func_type)) = module.types.get(type_idx as usize) else {
            continue;
        };
        for (pi, param_ty) in func_type.params.iter().enumerate() {
            let WirType::Ref {
                type_id: struct_type_id,
                ..
            } = param_ty
            else {
                continue;
            };
            let struct_type_idx = struct_type_id.index();
            let Some(WirTypeDef::Struct(st)) = module.types.get(struct_type_idx as usize) else {
                continue;
            };
            // Any single-field struct with an eligible (scalar/ref) inner type.
            if st.fields.len() != 1 || !is_eligible_field_type(&st.fields[0].ty) {
                continue;
            }
            candidates.insert((func_id_index, pi));
            candidate_info.insert(
                (func_id_index, pi),
                (
                    struct_type_idx,
                    st.fields[0].ty.clone(),
                    st.fields[0].name.clone(),
                ),
            );
        }
    }

    if candidates.is_empty() {
        return;
    }

    // Phase 2: validate uses — eliminate candidates whose parameter escapes.
    loop {
        let mut invalid: IndexSet<(u32, usize)> = IndexSet::default();

        for &(func_id_index, param_idx) in &candidates {
            let func_array_idx = (func_id_index - crate::wir_build::DEFINED_FUNC_BASE) as usize;
            let func = &module.functions[func_array_idx];
            let param_name = &func.param_names[param_idx];
            let (_, _, ref field_name) = candidate_info[&(func_id_index, param_idx)];
            let field_name = field_name.clone();
            let body = func.body.as_ref().unwrap();
            if !single_field_param_uses_valid(
                body,
                param_name,
                &field_name,
                func_id_index,
                param_idx,
                &candidates,
            ) {
                invalid.insert((func_id_index, param_idx));
            }
        }

        if invalid.is_empty() {
            break;
        }
        for key in &invalid {
            candidates.swap_remove(key);
        }
    }

    if candidates.is_empty() {
        return;
    }

    // Phase 3: apply rewrites.
    // Step A: rewrite function signatures and bodies.
    for &(func_id_index, param_idx) in &candidates {
        let func_array_idx = (func_id_index - crate::wir_build::DEFINED_FUNC_BASE) as usize;
        let param_name = module.functions[func_array_idx].param_names[param_idx].clone();
        let (_, ref inner_ty, ref field_name) = candidate_info[&(func_id_index, param_idx)];
        let inner_ty = inner_ty.clone();
        let field_name = field_name.clone();

        // Create new func type with the scalar param.
        let old_type_idx = module.functions[func_array_idx].type_id.index() as usize;
        let old_func_type = match &module.types[old_type_idx] {
            WirTypeDef::Func(ft) => ft,
            _ => unreachable!(),
        };
        let mut new_params = old_func_type.params.clone();
        new_params[param_idx] = inner_ty;
        let new_func_type = WirFuncType {
            name: old_func_type.name.clone(),
            params: new_params,
            results: old_func_type.results.clone(),
        };
        let new_type_idx = u32::try_from(module.types.len()).unwrap();
        module.types.push(WirTypeDef::Func(new_func_type));

        let fq: std::rc::Rc<str> = module.functions[func_array_idx].type_id.fq().into();
        let new_type_id = WirTypeId::new(new_type_idx, fq);
        module.functions[func_array_idx].type_id = new_type_id;

        // Rewrite body: replace StructGet(LocalGet(param), field) → LocalGet(param).
        if let Some(body) = &mut module.functions[func_array_idx].body {
            for instr in body.iter_mut() {
                rewrite_single_field_param_reads(instr, &param_name, &field_name);
            }
        }
    }

    // Step B: rewrite call sites in ALL function bodies.
    // Build lookup: func_id_index → set of SROA'd param indices.
    let mut sroa_params: crate::hashmap::IndexMap<u32, IndexSet<usize>> =
        crate::hashmap::IndexMap::default();
    for &(func_id_index, param_idx) in &candidates {
        sroa_params
            .entry(func_id_index)
            .or_default()
            .insert(param_idx);
    }

    // Build (func_id, param_idx) → (WirTypeId of the struct type, field name).
    let mut param_struct_info: crate::hashmap::IndexMap<(u32, usize), (WirTypeId, String)> =
        crate::hashmap::IndexMap::default();
    for &(func_id_index, param_idx) in &candidates {
        let (ref struct_type_idx, _, ref field_name) = candidate_info[&(func_id_index, param_idx)];
        if let Some(WirTypeDef::Struct(st)) = module.types.get(*struct_type_idx as usize) {
            let type_id = WirTypeId::new(*struct_type_idx, st.name.fq.as_str().into());
            param_struct_info.insert((func_id_index, param_idx), (type_id, field_name.clone()));
        }
    }

    // Build per-function map of param names → struct type that was unwrapped.
    // This tells us what struct type each scalar param was derived from (so we
    // know whether a bare `LocalGet(param)` at a call site needs a StructGet).
    let mut func_scalar_param_struct: crate::hashmap::IndexMap<
        u32,
        crate::hashmap::IndexMap<String, u32>,
    > = crate::hashmap::IndexMap::default();
    for &(func_id_index, param_idx) in &candidates {
        let func_array_idx = (func_id_index - crate::wir_build::DEFINED_FUNC_BASE) as usize;
        let param_name = module.functions[func_array_idx].param_names[param_idx].clone();
        let (struct_type_idx, _, _) = &candidate_info[&(func_id_index, param_idx)];
        func_scalar_param_struct
            .entry(func_id_index)
            .or_default()
            .insert(param_name, *struct_type_idx);
    }

    let types = std::mem::take(&mut module.types);
    for (i, func) in module.functions.iter_mut().enumerate() {
        let func_id_index = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap();
        let scalar_param_structs = func_scalar_param_struct
            .get(&func_id_index)
            .cloned()
            .unwrap_or_default();
        if let Some(body) = &mut func.body {
            // Rewrite call arguments at SROA'd positions.
            for instr in body.iter_mut() {
                rewrite_single_field_args_at_call_sites(
                    instr,
                    &sroa_params,
                    &param_struct_info,
                    &scalar_param_structs,
                    &types,
                );
            }
        }
    }
    module.types = types;
}

/// Check that every use of `param_name` in the body is a valid single-field SROA use.
fn single_field_param_uses_valid(
    instrs: &[WirInstr],
    param_name: &str,
    field_name: &str,
    _self_func_id: u32,
    _self_param_idx: usize,
    candidates: &IndexSet<(u32, usize)>,
) -> bool {
    for instr in instrs {
        if !single_field_param_use_valid_instr(
            instr,
            param_name,
            field_name,
            candidates,
            SingleFieldCheckCtx::None,
        ) {
            return false;
        }
    }
    true
}

/// Context for tracking where we encounter a `LocalGet(param_name)`.
#[derive(Clone, Copy)]
enum SingleFieldCheckCtx {
    None,
    /// Inside a `StructGet { field_name }` matching the single field — `LocalGet` is valid.
    InsideStructGetField,
}

/// Recursively check that all uses of `param_name` are valid single-field SROA patterns.
fn single_field_param_use_valid_instr(
    instr: &WirInstr,
    param_name: &str,
    expected_field: &str,
    candidates: &IndexSet<(u32, usize)>,
    ctx: SingleFieldCheckCtx,
) -> bool {
    match instr {
        // LocalGet of the param: only valid inside StructGet(field) or as call arg
        WirInstr::LocalGet { name, .. } if name == param_name => {
            matches!(ctx, SingleFieldCheckCtx::InsideStructGetField)
        }
        // StructGet with the expected field name: mark context and check inner
        WirInstr::StructGet {
            field_name, expr, ..
        } if field_name == expected_field => single_field_param_use_valid_instr(
            expr,
            param_name,
            expected_field,
            candidates,
            SingleFieldCheckCtx::InsideStructGetField,
        ),
        // Call: a direct LocalGet(param) as a call arg is valid only at SROA'd positions.
        // Non-LocalGet args are validated recursively.
        WirInstr::Call { func_id, args } => {
            for (ai, arg) in args.iter().enumerate() {
                if let WirInstr::LocalGet { name, .. } = arg
                    && name == param_name
                {
                    // Bare param reference at a call position — only valid if SROA'd.
                    if !candidates.contains(&(func_id.index(), ai)) {
                        return false;
                    }
                } else if !single_field_param_use_valid_instr(
                    arg,
                    param_name,
                    expected_field,
                    candidates,
                    SingleFieldCheckCtx::None,
                ) {
                    return false;
                }
            }
            true
        }
        // Recurse into nested blocks
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            for child in body {
                if !single_field_param_use_valid_instr(
                    child,
                    param_name,
                    expected_field,
                    candidates,
                    SingleFieldCheckCtx::None,
                ) {
                    return false;
                }
            }
            true
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            if !single_field_param_use_valid_instr(
                condition,
                param_name,
                expected_field,
                candidates,
                SingleFieldCheckCtx::None,
            ) {
                return false;
            }
            for child in then_body {
                if !single_field_param_use_valid_instr(
                    child,
                    param_name,
                    expected_field,
                    candidates,
                    SingleFieldCheckCtx::None,
                ) {
                    return false;
                }
            }
            if let Some(eb) = else_body {
                for child in eb {
                    if !single_field_param_use_valid_instr(
                        child,
                        param_name,
                        expected_field,
                        candidates,
                        SingleFieldCheckCtx::None,
                    ) {
                        return false;
                    }
                }
            }
            true
        }
        // Any other instruction: check subtree for escaping uses
        _ => {
            let mut ok = true;
            instr.for_each_child(&mut |child| {
                if ok
                    && !single_field_param_use_valid_instr(
                        child,
                        param_name,
                        expected_field,
                        candidates,
                        SingleFieldCheckCtx::None,
                    )
                {
                    ok = false;
                }
            });
            ok
        }
    }
}

/// Rewrite `StructGet { field, expr: LocalGet(param) }` → `LocalGet(param)`
/// within a function body after SROA (param is now scalar).
fn rewrite_single_field_param_reads(instr: &mut WirInstr, param_name: &str, expected_field: &str) {
    // Check StructGet(LocalGet(param), field) pattern
    if let WirInstr::StructGet {
        field_name,
        expr,
        result_ty,
        ..
    } = instr
        && field_name == expected_field
        && matches!(expr.as_ref(), WirInstr::LocalGet { name, .. } if name == param_name)
    {
        *instr = WirInstr::LocalGet {
            name: param_name.to_string(),
            result_ty: result_ty.clone(),
        };
        return;
    }

    // Recurse
    instr.for_each_boxed_child_mut(&mut |child| {
        rewrite_single_field_param_reads(child, param_name, expected_field);
    });
}

/// Rewrite arguments at SROA'd Call positions to pass the scalar value instead of a struct ref.
///
/// - `StructNew(S, [val])` → `val` (unwrap allocation, eliminate heap alloc)
/// - `LocalGet(x)` where x is already scalar (SROA'd param) and the callee unwraps the
///   *same* struct type → leave as-is (types match)
/// - Other expressions → `StructGet(expr, field_name)` (extract scalar from existing ref)
fn rewrite_single_field_args_at_call_sites(
    instr: &mut WirInstr,
    sroa_params: &crate::hashmap::IndexMap<u32, IndexSet<usize>>,
    param_struct_info: &crate::hashmap::IndexMap<(u32, usize), (WirTypeId, String)>,
    scalar_param_structs: &crate::hashmap::IndexMap<String, u32>,
    types: &[crate::wir::WirTypeDef],
) {
    // Recurse first (bottom-up)
    instr.for_each_boxed_child_mut(&mut |child| {
        rewrite_single_field_args_at_call_sites(
            child,
            sroa_params,
            param_struct_info,
            scalar_param_structs,
            types,
        );
    });

    let WirInstr::Call { func_id, args } = instr else {
        return;
    };
    let Some(param_indices) = sroa_params.get(&func_id.index()) else {
        return;
    };
    let func_id_idx = func_id.index();
    for &pi in param_indices {
        if pi >= args.len() {
            continue;
        }
        let arg = &mut args[pi];
        if let WirInstr::StructNew { fields, .. } = arg
            && fields.len() == 1
        {
            // Unwrap StructNew: skip struct allocation entirely.
            let inner = std::mem::replace(&mut fields[0], WirInstr::Nop);
            *arg = inner;
        } else if let WirInstr::LocalGet { name, .. } = arg
            && let Some(&caller_struct_type) = scalar_param_structs.get(name.as_str())
        {
            // This arg is a scalar param (SROA'd in the caller). Check if the callee
            // unwraps the same struct type. If so, types already match. If the callee
            // unwraps a *different* struct (e.g. the caller's scalar is `ref JsonWriter`
            // but the callee unwraps `JsonWriter` → `ref String`), we need StructGet.
            let Some((callee_struct_type_id, field_name)) =
                param_struct_info.get(&(func_id_idx, pi))
            else {
                continue;
            };
            if caller_struct_type == callee_struct_type_id.index() {
                // Same struct type — types match, no rewrite needed.
            } else {
                // Different struct type — the caller's scalar type IS the callee's
                // wrapper struct. Extract the inner field.
                let result_ty = lookup_struct_field_type(types, callee_struct_type_id, field_name);
                let old_arg = std::mem::replace(arg, WirInstr::Nop);
                *arg = WirInstr::StructGet {
                    type_id: callee_struct_type_id.clone(),
                    field_name: field_name.clone(),
                    expr: Box::new(old_arg),
                    result_ty,
                };
            }
        } else {
            // The argument is an existing struct reference (e.g., from a local variable).
            // Extract the scalar value via StructGet(field_name).
            let Some((struct_type_id, field_name)) = param_struct_info.get(&(func_id_idx, pi))
            else {
                continue;
            };
            let result_ty = lookup_struct_field_type(types, struct_type_id, field_name);
            let old_arg = std::mem::replace(arg, WirInstr::Nop);
            *arg = WirInstr::StructGet {
                type_id: struct_type_id.clone(),
                field_name: field_name.clone(),
                expr: Box::new(old_arg),
                result_ty,
            };
        }
    }
}

fn lookup_struct_field_type(
    types: &[crate::wir::WirTypeDef],
    struct_type_id: &WirTypeId,
    field_name: &str,
) -> crate::wir::WirType {
    if let Some(crate::wir::WirTypeDef::Struct(st)) = types.get(struct_type_id.index() as usize)
        && let Some(f) = st.fields.iter().find(|f| f.name == field_name)
    {
        return f.ty.clone();
    }
    crate::wir::WirType::I32
}
