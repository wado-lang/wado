//! WIR optimization — peephole and structural optimizations on `WirModule`.
//!
//! Runs after `wir_build` and before `codegen::emit`.
//!
//! Current passes:
<<<<<<< HEAD
//! - **Multi-value return SROA**: rewrites functions that return small scalar structs
//!   to use Wasm multi-value returns, eliminating GC struct allocation.
//! - **Multi-value tuple elision**: replaces `MultiValueStructNew` + `StructGet`
//!   sequences with `MultiValueLocalBind` to skip intermediate struct allocation.

use indexmap::IndexSet;

use crate::wir::{
    WirExportDesc, WirFuncType, WirImportDesc, WirInstr, WirModule, WirType, WirTypeDef, WirTypeId,
};

/// Run all WIR-level optimizations on the module (in-place).
pub fn optimize_wir(module: &mut WirModule) {
    // Whole-module pass: rewrite struct-returning functions to multi-value.
    sroa_multi_value_returns(module);

    let types = &module.types;
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            optimize_instrs(body, types);
        }
    }
}

/// Multi-value return SROA (Scalar Replacement of Aggregates).
///
/// Rewrites internal functions that return a small struct of scalar fields into
/// functions that return multiple scalar values directly (Wasm multi-value return).
/// At call sites, the struct allocation + field extraction is replaced with
/// `MultiValueLocalBind`.
///
/// A function is eligible when:
/// - It is not exported, not in an element table, and not referenced by `RefFunc`.
/// - Its single return type is a non-nullable `Ref` to a `WirTypeDef::Struct`
///   with 2–4 fields, all scalar (no `Ref` or `AbstractRef` fields).
/// - Every `Return` in the body wraps a `StructNew` of the matching type.
/// - Every call site stores the result into a temp and reads only via `StructGet`.
fn sroa_multi_value_returns(module: &mut WirModule) {
    let import_func_count = module
        .imports
        .iter()
        .filter(|i| matches!(i.desc, WirImportDesc::Func { .. }))
        .count() as u32;

    // Collect pinned func_ids (exported, in element tables, or RefFunc'd).
    let pinned = collect_pinned_func_ids(module);

    // Phase 1: identify candidate functions.
    let candidates = find_sroa_candidates(module, import_func_count, &pinned);
    if candidates.is_empty() {
        return;
    }

    // Phase 2: validate call sites across all function bodies.
    let confirmed = validate_call_sites(module, &candidates);
    if confirmed.is_empty() {
        return;
    }

    // Phase 3: rewrite confirmed functions and their call sites.
    apply_sroa(module, &confirmed, import_func_count);
}

/// Information about an SROA candidate function.
struct SroaCandidate {
    /// Index into `module.functions`.
    func_array_idx: usize,
    /// The WIR type index of the struct being returned.
    struct_type_idx: u32,
    /// The field types of the returned struct (the new multi-value result types).
    field_types: Vec<WirType>,
    /// Number of fields.
    field_count: usize,
    /// Field names from the struct definition.
    field_names: Vec<String>,
}

/// Returns true if a `WirType` is scalar (representable as a single Wasm value).
fn is_scalar_type(ty: &WirType) -> bool {
    !matches!(
        ty,
        WirType::Ref { .. } | WirType::AbstractRef { .. } | WirType::Unit
    )
}

/// Collect all `func_ids` that must NOT be SROA'd (exports, element tables, `RefFunc`).
fn collect_pinned_func_ids(module: &WirModule) -> IndexSet<u32> {
    let mut pinned = IndexSet::new();

    // Exported functions
    for export in &module.exports {
        if let WirExportDesc::Func { func_id } = &export.desc {
            pinned.insert(func_id.index());
        }
    }

    // Element table functions
    for elem in &module.elements {
        for fid in &elem.func_ids {
            pinned.insert(fid.index());
        }
    }

    // RefFunc references in all function bodies
    for func in &module.functions {
        if let Some(body) = &func.body {
            collect_ref_funcs(body, &mut pinned);
        }
    }

    // Also check global initializers for RefFunc
    for global in &module.globals {
        collect_ref_funcs_instr(&global.init, &mut pinned);
    }

    pinned
}

/// Recursively collect `RefFunc` `func_ids` from instruction lists.
fn collect_ref_funcs(instrs: &[WirInstr], pinned: &mut IndexSet<u32>) {
    for instr in instrs {
        collect_ref_funcs_instr(instr, pinned);
    }
}

/// Recursively collect `RefFunc` `func_ids` from a single instruction.
fn collect_ref_funcs_instr(instr: &WirInstr, pinned: &mut IndexSet<u32>) {
    if let WirInstr::RefFunc { func_id } = instr {
        pinned.insert(func_id.index());
    }
    instr.for_each_child(&mut |child| collect_ref_funcs_instr(child, pinned));
}

/// Phase 1: find functions eligible for SROA.
fn find_sroa_candidates(
    module: &WirModule,
    import_func_count: u32,
    pinned: &IndexSet<u32>,
) -> Vec<(u32, SroaCandidate)> {
    let mut candidates = Vec::new();

    for (i, func) in module.functions.iter().enumerate() {
        let func_id_index = import_func_count + u32::try_from(i).unwrap();

        // Skip pinned functions
        if pinned.contains(&func_id_index) {
            continue;
        }

        // Must have a body
        if func.body.is_none() {
            continue;
        }

        // Look up function type
        let type_idx = func.type_id.index();
        let Some(WirTypeDef::Func(func_type)) = module.types.get(type_idx as usize) else {
            continue;
        };

        // Must return exactly one Ref to a struct
        if func_type.results.len() != 1 {
            continue;
        }
        let WirType::Ref {
            type_id: ref ret_type_id,
            ..
        } = func_type.results[0]
        else {
            continue;
        };

        let struct_type_idx = ret_type_id.index();
        let Some(WirTypeDef::Struct(struct_type)) = module.types.get(struct_type_idx as usize)
        else {
            continue;
        };

        // 2–4 scalar fields
        let field_count = struct_type.fields.len();
        if !(2..=4).contains(&field_count) {
            continue;
        }
        if !struct_type.fields.iter().all(|f| is_scalar_type(&f.ty)) {
            continue;
        }

        // Verify all returns in the body wrap StructNew of the correct type
        let body = func.body.as_ref().unwrap();
        if !all_returns_are_struct_new(body, struct_type_idx) {
            continue;
        }

        let field_types: Vec<WirType> = struct_type.fields.iter().map(|f| f.ty.clone()).collect();
        let field_names: Vec<String> = struct_type.fields.iter().map(|f| f.name.clone()).collect();

        candidates.push((
            func_id_index,
            SroaCandidate {
                func_array_idx: i,
                struct_type_idx,
                field_types,
                field_count,
                field_names,
            },
        ));
    }

    candidates
}

/// Check that every `Return` instruction in the body contains a `StructNew` of the
/// expected struct type.
fn all_returns_are_struct_new(instrs: &[WirInstr], expected_type_idx: u32) -> bool {
    for instr in instrs {
        if !check_return_struct_new(instr, expected_type_idx) {
            return false;
        }
    }
    true
}

fn check_return_struct_new(instr: &WirInstr, expected_type_idx: u32) -> bool {
    match instr {
        WirInstr::Return { value: Some(v) } => match v.as_ref() {
            WirInstr::StructNew { type_id, .. } => type_id.index() == expected_type_idx,
            _ => false,
        },
        WirInstr::Return { value: None } => {
            // Void return is fine for our purposes (won't happen in struct-returning fn)
            true
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            all_returns_are_struct_new(body, expected_type_idx)
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            all_returns_are_struct_new(then_body, expected_type_idx)
                && else_body
                    .as_ref()
                    .is_none_or(|eb| all_returns_are_struct_new(eb, expected_type_idx))
        }
        WirInstr::Seq(body) => all_returns_are_struct_new(body, expected_type_idx),
        _ => true,
    }
}

/// Phase 2: validate that all call sites of candidate functions are SROA-compatible.
///
/// A call site is compatible if:
/// 1. The call result is stored to a temp local via `LocalSet`.
/// 2. Every use of that temp local is `StructGet { expr: LocalGet(temp) }`.
/// 3. The temp local is not used in any other way (plain `LocalGet`, `LocalSet`, etc.).
fn validate_call_sites(
    module: &WirModule,
    candidates: &[(u32, SroaCandidate)],
) -> Vec<(u32, SroaCandidate)> {
    let candidate_ids: IndexSet<u32> = candidates.iter().map(|(id, _)| *id).collect();

    // Scan all function bodies for calls to candidate functions
    let mut invalid: IndexSet<u32> = IndexSet::new();

    for func in &module.functions {
        if let Some(body) = &func.body {
            validate_call_sites_in_body(body, &candidate_ids, &mut invalid);
        }
    }

    candidates
        .iter()
        .filter(|(id, _)| !invalid.contains(id))
        .map(|(id, c)| {
            (
                *id,
                SroaCandidate {
                    func_array_idx: c.func_array_idx,
                    struct_type_idx: c.struct_type_idx,
                    field_types: c.field_types.clone(),
                    field_count: c.field_count,
                    field_names: c.field_names.clone(),
                },
            )
        })
        .collect()
}

/// Validate call sites of candidate functions within a flat instruction list.
fn validate_call_sites_in_body(
    instrs: &[WirInstr],
    candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
) {
    for instr in instrs {
        // Recurse into nested blocks
        match instr {
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                validate_call_sites_in_body(body, candidate_ids, invalid);
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                validate_call_sites_in_body(then_body, candidate_ids, invalid);
                if let Some(eb) = else_body {
                    validate_call_sites_in_body(eb, candidate_ids, invalid);
                }
            }
            WirInstr::Seq(body) => {
                validate_call_sites_in_body(body, candidate_ids, invalid);
            }
            _ => {}
        }

        // Check for calls to candidates in non-LocalSet contexts (invalid)
        check_invalid_call_uses(instr, candidate_ids, invalid);
    }

    // Check that LocalSet(Call(candidate)) temps are only used via StructGet
    for instr in instrs {
        if let WirInstr::LocalSet { name, value } = instr
            && let WirInstr::Call { func_id, .. } = value.as_ref()
            && candidate_ids.contains(&func_id.index())
        {
            // Verify all uses of `name` in this body are StructGet patterns
            if !all_uses_are_struct_get(instrs, name) {
                invalid.insert(func_id.index());
            }
        }
    }
}

/// Check if an instruction uses a candidate call result in an invalid way.
/// Invalid: Call to candidate as a nested expression (not direct child of `LocalSet`).
fn check_invalid_call_uses(
    instr: &WirInstr,
    candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
) {
    match instr {
        // LocalSet { value: Call { func, args } } is the valid pattern for the
        // *outer* call — but we must still check the args for nested candidate calls.
        WirInstr::LocalSet { value, .. } if matches!(value.as_ref(), WirInstr::Call { .. }) => {
            if let WirInstr::Call { args, .. } = value.as_ref() {
                for arg in args {
                    find_nested_candidate_calls(arg, candidate_ids, invalid);
                }
            }
        }
        // Any other instruction that contains a Call to a candidate is invalid
        _ => {
            find_nested_candidate_calls(instr, candidate_ids, invalid);
        }
    }
}

/// Recursively find calls to candidate functions nested in expressions.
fn find_nested_candidate_calls(
    instr: &WirInstr,
    candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
) {
    if let WirInstr::Call { func_id, .. } = instr
        && candidate_ids.contains(&func_id.index())
    {
        invalid.insert(func_id.index());
    }
    instr.for_each_child(&mut |child| find_nested_candidate_calls(child, candidate_ids, invalid));
}

/// Check that every reference to `local_name` in the instruction list is a
/// `StructGet { expr: LocalGet(local_name) }` — i.e., the local is never used
/// directly, only for field extraction.
fn all_uses_are_struct_get(instrs: &[WirInstr], local_name: &str) -> bool {
    for instr in instrs {
        if !check_uses_are_struct_get(instr, local_name, false) {
            return false;
        }
    }
    true
}

/// Recursively verify that `local_name` is only referenced inside `StructGet`.
/// `inside_struct_get` is true when we're already inside a `StructGet { expr }`.
fn check_uses_are_struct_get(instr: &WirInstr, local_name: &str, inside_struct_get: bool) -> bool {
    match instr {
        WirInstr::LocalGet { name } if name == local_name => {
            // Only valid if we're inside a StructGet
            inside_struct_get
        }
        WirInstr::LocalSet { name, value } if name == local_name => {
            // The original assignment — this is fine, but check the value subtree
            check_uses_in_subtree(value, local_name)
        }
        WirInstr::LocalTee { name, .. } if name == local_name => {
            // Tee is not a valid use
            false
        }
        WirInstr::StructGet { expr, .. } => {
            // The expr inside StructGet is checked with inside_struct_get=true
            if !check_uses_are_struct_get(expr, local_name, true) {
                return false;
            }
            // Any other field references (type_id) are fine
            true
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            for child in body {
                if !check_uses_are_struct_get(child, local_name, false) {
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
            if !check_uses_are_struct_get(condition, local_name, false) {
                return false;
            }
            for child in then_body {
                if !check_uses_are_struct_get(child, local_name, false) {
                    return false;
                }
            }
            if let Some(eb) = else_body {
                for child in eb {
                    if !check_uses_are_struct_get(child, local_name, false) {
                        return false;
                    }
                }
            }
            true
        }
        WirInstr::Seq(body) => {
            for child in body {
                if !check_uses_are_struct_get(child, local_name, false) {
                    return false;
                }
            }
            true
        }
        _ => {
            // Check all children with default context
            check_uses_in_subtree(instr, local_name)
        }
    }
}

/// Check that `local_name` is only referenced in `StructGet` patterns within a subtree.
fn check_uses_in_subtree(instr: &WirInstr, local_name: &str) -> bool {
    let mut ok = true;
    instr.for_each_child(&mut |child| {
        if ok && !check_uses_are_struct_get(child, local_name, false) {
            ok = false;
        }
    });
    ok
}

/// Phase 3: apply SROA transformations to confirmed candidates.
fn apply_sroa(module: &mut WirModule, confirmed: &[(u32, SroaCandidate)], _import_func_count: u32) {
    // Build a lookup from func_id_index → candidate info
    let candidate_map: indexmap::IndexMap<u32, &SroaCandidate> =
        confirmed.iter().map(|(id, c)| (*id, c)).collect();

    // Step A: Create new func types and rewrite function signatures + bodies.
    for (_func_id_index, candidate) in confirmed {
        let func = &mut module.functions[candidate.func_array_idx];

        // Create new func type with multi-value results
        let old_type_idx = func.type_id.index() as usize;
        let old_func_type = match &module.types[old_type_idx] {
            WirTypeDef::Func(ft) => ft,
            _ => unreachable!(),
        };
        let new_func_type = WirFuncType {
            name: old_func_type.name.clone(),
            params: old_func_type.params.clone(),
            results: candidate.field_types.clone(),
        };

        // Add the new func type to the module types
        let new_type_idx = u32::try_from(module.types.len()).unwrap();
        module.types.push(WirTypeDef::Func(new_func_type));

        // Update the function's type_id
        let new_type_id = WirTypeId::new(new_type_idx, func.type_id.fq().into());
        func.type_id = new_type_id;

        // Rewrite returns in the body: StructNew → Seq of field values
        if let Some(body) = &mut func.body {
            rewrite_returns_to_multi_value(body);
        }
    }

    // Step B: Rewrite call sites in ALL function bodies.
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            rewrite_call_sites(body, &candidate_map);
        }
    }
}

/// Rewrite `Return { value: StructNew { fields } }` → `Return { value: Seq(fields) }`.
fn rewrite_returns_to_multi_value(instrs: &mut [WirInstr]) {
    for instr in instrs.iter_mut() {
        match instr {
            WirInstr::Return { value: Some(v) } => {
                if let WirInstr::StructNew { fields, .. } =
                    std::mem::replace(v.as_mut(), WirInstr::Nop)
                {
                    **v = WirInstr::Seq(fields);
                }
            }
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                rewrite_returns_to_multi_value(body);
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                rewrite_returns_to_multi_value(then_body);
                if let Some(eb) = else_body {
                    rewrite_returns_to_multi_value(eb);
                }
            }
            WirInstr::Seq(body) => {
                rewrite_returns_to_multi_value(body);
            }
            _ => {}
        }
    }
}

/// Rewrite call sites of SROA'd functions.
///
/// For each `LocalSet { name: T, value: Call { func_id } }` where `func_id` is SROA'd:
/// 1. Replace the `LocalSet` with `MultiValueLocalBind` that binds results to fresh locals.
/// 2. Replace all `StructGet { field, expr: LocalGet(T) }` with `LocalGet(fresh_local)`.
fn rewrite_call_sites(
    instrs: &mut Vec<WirInstr>,
    candidate_map: &indexmap::IndexMap<u32, &SroaCandidate>,
) {
    // Collect replacements: temp_name → (field_name → fresh_local_name)
    let mut replacements: indexmap::IndexMap<String, indexmap::IndexMap<String, String>> =
        indexmap::IndexMap::new();

    // First pass: find call sites and prepare MultiValueLocalBind + replacement map
    let mut result = Vec::with_capacity(instrs.len());
    let mut i = 0;

    while i < instrs.len() {
        // Skip optional DeclareLocal before the LocalSet
        let (skip_declare, set_idx) = match &instrs[i] {
            WirInstr::DeclareLocal { name: dn, .. } if i + 1 < instrs.len() => {
                if is_candidate_call_set(&instrs[i + 1], dn, candidate_map) {
                    (true, i + 1)
                } else {
                    result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
                    i += 1;
                    continue;
                }
            }
            _ => (false, i),
        };

        // Check if this is a LocalSet wrapping a Call to a candidate
        let Some((func_id_idx, temp_name)) =
            extract_candidate_call_info(&instrs[set_idx], candidate_map)
        else {
            result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
            i += 1;
            continue;
        };

        let candidate = candidate_map[&func_id_idx];

        // Generate fresh local names for each field and declare them
        let mut field_map: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        let mut locals: Vec<Option<String>> = Vec::with_capacity(candidate.field_count);
        for (i, field_name) in candidate.field_names.iter().enumerate() {
            let fresh = format!("__sroa_{temp_name}_{field_name}");
            field_map.insert(field_name.clone(), fresh.clone());
            // Emit DeclareLocal for the fresh local with the field's type
            result.push(WirInstr::DeclareLocal {
                name: fresh.clone(),
                ty: candidate.field_types[i].clone(),
            });
            locals.push(Some(fresh));
        }
        replacements.insert(temp_name, field_map);

        // Extract the Call instruction and emit MultiValueLocalBind
        let call_instr = take_call_from_local_set(&mut instrs[set_idx]);
        result.push(WirInstr::MultiValueLocalBind {
            instr: call_instr,
            locals,
        });

        i = if skip_declare {
            set_idx + 1
        } else {
            set_idx + 1
        };
    }

    *instrs = result;

    if replacements.is_empty() {
        // Recurse into nested blocks even if no replacements at this level
        for instr in instrs.iter_mut() {
            recurse_rewrite_call_sites(instr, candidate_map);
        }
        return;
    }

    // Second pass: replace StructGet(LocalGet(temp)) → LocalGet(fresh_local)
    for instr in instrs.iter_mut() {
        replace_struct_gets(instr, &replacements);
    }

    // Recurse into nested blocks
    for instr in instrs.iter_mut() {
        recurse_rewrite_call_sites(instr, candidate_map);
    }
}

/// Recurse into nested instruction bodies for call site rewriting.
fn recurse_rewrite_call_sites(
    instr: &mut WirInstr,
    candidate_map: &indexmap::IndexMap<u32, &SroaCandidate>,
) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            rewrite_call_sites(body, candidate_map);
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            rewrite_call_sites(then_body, candidate_map);
            if let Some(eb) = else_body {
                rewrite_call_sites(eb, candidate_map);
            }
        }
        WirInstr::Seq(body) => {
            rewrite_call_sites(body, candidate_map);
        }
        _ => {}
    }
}

/// Replace `StructGet { field_name, expr: LocalGet(temp) }` with `LocalGet(fresh_local)`
/// for all known replacements. Uses `WirInstr::for_each_boxed_child_mut` for generic traversal.
fn replace_struct_gets(
    instr: &mut WirInstr,
    replacements: &indexmap::IndexMap<String, indexmap::IndexMap<String, String>>,
) {
    // Check if THIS instruction is a StructGet that should be replaced
    if let WirInstr::StructGet {
        field_name, expr, ..
    } = instr
        && let WirInstr::LocalGet { name: temp_name } = expr.as_ref()
        && let Some(field_map) = replacements.get(temp_name.as_str())
        && let Some(fresh_local) = field_map.get(field_name.as_str())
    {
        *instr = WirInstr::LocalGet {
            name: fresh_local.clone(),
        };
        return;
    }

    // Recursively process all children using the generic mutable visitor
    instr.for_each_boxed_child_mut(&mut |child| replace_struct_gets(child, replacements));
}

/// Check if instruction is `LocalSet { name, value: Call { func_id in candidates } }`.
fn is_candidate_call_set(
    instr: &WirInstr,
    expected_name: &str,
    candidate_map: &indexmap::IndexMap<u32, &SroaCandidate>,
) -> bool {
    let WirInstr::LocalSet { name, value } = instr else {
        return false;
    };
    if name != expected_name {
        return false;
    }
    let WirInstr::Call { func_id, .. } = value.as_ref() else {
        return false;
    };
    candidate_map.contains_key(&func_id.index())
}

/// Extract (`func_id_index`, `temp_name`) from a candidate call `LocalSet`.
fn extract_candidate_call_info(
    instr: &WirInstr,
    candidate_map: &indexmap::IndexMap<u32, &SroaCandidate>,
) -> Option<(u32, String)> {
    let WirInstr::LocalSet { name, value } = instr else {
        return None;
    };
    let WirInstr::Call { func_id, .. } = value.as_ref() else {
        return None;
    };
    let idx = func_id.index();
    if candidate_map.contains_key(&idx) {
        Some((idx, name.clone()))
    } else {
        None
    }
}

/// Take the Call instruction out of a `LocalSet`, replacing with Nop.
#[allow(clippy::unnecessary_box_returns)]
fn take_call_from_local_set(instr: &mut WirInstr) -> Box<WirInstr> {
    let WirInstr::LocalSet { value, .. } = std::mem::replace(instr, WirInstr::Nop) else {
        unreachable!()
    };
    value
||||||| empty tree
=======
//! - **Multi-value tuple elision**: replaces `MultiValueStructNew` + `StructGet`
//!   sequences with `MultiValueLocalBind` to skip intermediate struct allocation.

use crate::wir::{WirInstr, WirModule, WirTypeDef};

/// Run all WIR-level optimizations on the module (in-place).
pub fn optimize_wir(module: &mut WirModule) {
    let types = &module.types;
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            optimize_instrs(body, types);
        }
    }
>>>>>>> 124f39a3c54c97b611d93ea0f4b5c5623c0b0a7d
}

/// Recursively optimize a list of instructions.
///
/// First descends into nested instruction bodies (Block, Loop, If, Seq),
/// then applies flat-level optimizations on the current list.
fn optimize_instrs(instrs: &mut Vec<WirInstr>, types: &[WirTypeDef]) {
    for instr in instrs.iter_mut() {
        optimize_nested(instr, types);
    }
    elide_multi_value_structs(instrs, types);
}

/// Recurse into nested instruction bodies.
fn optimize_nested(instr: &mut WirInstr, types: &[WirTypeDef]) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            optimize_instrs(body, types);
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            optimize_instrs(then_body, types);
            if let Some(eb) = else_body {
                optimize_instrs(eb, types);
            }
        }
        WirInstr::Seq(body) => {
            optimize_instrs(body, types);
        }
        _ => {}
    }
}

/// Multi-value tuple elision pass.
///
/// Matches the pattern generated by `translate_let_pattern` for multi-value
/// builtins (`i64.add128`, `i64.sub128`, `i64.mul_wide_u`, `i64.mul_wide_s`):
///
/// ```text
/// DeclareLocal { name: T, .. }                                              // optional
/// LocalSet { name: T, value: MultiValueStructNew { type_id, instr } }
/// LocalSet { name: a, value: StructGet { type_id, field: "0", expr: LocalGet(T) } }
/// LocalSet { name: b, value: StructGet { type_id, field: "1", expr: LocalGet(T) } }
/// ...
/// ```
///
/// Replaces with:
///
/// ```text
/// MultiValueLocalBind { instr, locals: [Some("a"), Some("b"), ...] }
/// ```
///
/// This eliminates a GC struct allocation per multi-value result.
fn elide_multi_value_structs(instrs: &mut Vec<WirInstr>, types: &[WirTypeDef]) {
    let mut result = Vec::with_capacity(instrs.len());
    let mut i = 0;

    while i < instrs.len() {
        // Step 1: Detect optional DeclareLocal + LocalSet(MultiValueStructNew)
        let (has_declare, set_idx) = match &instrs[i] {
            WirInstr::DeclareLocal { name: dn, .. } if i + 1 < instrs.len() => {
                if is_mv_local_set(&instrs[i + 1], dn) {
                    (true, i + 1)
                } else {
                    result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
                    i += 1;
                    continue;
                }
            }
            _ => (false, i),
        };

        // Step 2: Verify LocalSet(MultiValueStructNew) and extract info
        let Some((temp_name, type_id_index)) = extract_mv_info(&instrs[set_idx]) else {
            result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
            i += 1;
            continue;
        };

        // Step 3: Look up struct field count from the type definition
        let Some(WirTypeDef::Struct(s)) = types.get(type_id_index) else {
            result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
            i += 1;
            continue;
        };
        let field_count = s.fields.len();

        // Step 4: Scan consecutive StructGet instructions reading from the temp
        let mut locals: Vec<Option<String>> = vec![None; field_count];
        let mut j = set_idx + 1;

        while j < instrs.len() {
            if let Some((idx, target)) =
                match_field_get(&instrs[j], &temp_name, field_count, &locals)
            {
                locals[idx] = Some(target);
                j += 1;
            } else {
                break;
            }
        }

        // Step 5: Require at least one field consumed
        if locals.iter().all(Option::is_none) {
            if has_declare {
                result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
                result.push(std::mem::replace(&mut instrs[set_idx], WirInstr::Nop));
                i = set_idx + 1;
            } else {
                result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
                i += 1;
            }
            continue;
        }

        // Step 6: Extract the inner instruction and emit MultiValueLocalBind
        let inner = take_mv_inner(&mut instrs[set_idx]);

        result.push(WirInstr::MultiValueLocalBind {
            instr: inner,
            locals,
        });
        i = j;
    }

    *instrs = result;
}

/// Check if an instruction is `LocalSet { name, value: MultiValueStructNew { .. } }`.
fn is_mv_local_set(instr: &WirInstr, expected_name: &str) -> bool {
    matches!(
        instr,
        WirInstr::LocalSet { name, value }
            if name == expected_name
               && matches!(value.as_ref(), WirInstr::MultiValueStructNew { .. })
    )
}

/// Extract `(temp_name, type_id_index)` from a `LocalSet(MultiValueStructNew)`.
fn extract_mv_info(instr: &WirInstr) -> Option<(String, usize)> {
    let WirInstr::LocalSet { name, value } = instr else {
        return None;
    };
    let WirInstr::MultiValueStructNew { type_id, .. } = value.as_ref() else {
        return None;
    };
    Some((name.clone(), type_id.index() as usize))
}

/// Take ownership of the inner instruction from a `LocalSet(MultiValueStructNew)`.
///
/// Replaces the instruction in-place with `Nop`.
/// Returns `Box<WirInstr>` to pass directly into `MultiValueLocalBind::instr`.
#[allow(clippy::unnecessary_box_returns)]
fn take_mv_inner(instr: &mut WirInstr) -> Box<WirInstr> {
    let WirInstr::LocalSet { value, .. } = std::mem::replace(instr, WirInstr::Nop) else {
        unreachable!()
    };
    let WirInstr::MultiValueStructNew { instr: inner, .. } = *value else {
        unreachable!()
    };
    inner
}

/// Try to match a `LocalSet { name, value: StructGet { field: "N", expr: LocalGet(temp) } }`.
///
/// Returns `Some((field_index, target_local_name))` on success.
fn match_field_get(
    instr: &WirInstr,
    temp_name: &str,
    field_count: usize,
    already_bound: &[Option<String>],
) -> Option<(usize, String)> {
    let WirInstr::LocalSet {
        name: target,
        value,
    } = instr
    else {
        return None;
    };
    let WirInstr::StructGet {
        field_name, expr, ..
    } = value.as_ref()
    else {
        return None;
    };
    let WirInstr::LocalGet { name: gn } = expr.as_ref() else {
        return None;
    };
    if gn != temp_name {
        return None;
    }
    let idx: usize = field_name.parse().ok()?;
    if idx >= field_count || already_bound[idx].is_some() {
        return None;
    }
    Some((idx, target.clone()))
}
