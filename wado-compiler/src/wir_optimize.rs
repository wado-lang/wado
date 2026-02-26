//! WIR optimization — peephole and structural optimizations on `WirModule`.
//!
//! Runs after `wir_build` and before `codegen::emit`.
//!
//! Current passes:
//! - **Multi-value return SROA**: rewrites functions that return small scalar structs
//!   to use Wasm multi-value returns, eliminating GC struct allocation.
//! - **Multi-value tuple elision**: replaces `MultiValueStructNew` + `StructGet`
//!   sequences with `MultiValueLocalBind` to skip intermediate struct allocation.
//! - **Constant array data promotion**: replaces `ArrayNewFixed` of constant primitive
//!   values with `ArrayNewData` backed by a passive data segment.

use indexmap::IndexSet;

use crate::wir::{
    COMP_FEATURE_ARRAY_APPEND, WirData, WirExportDesc, WirFuncType, WirImportDesc, WirInstr,
    WirModule, WirType, WirTypeDef, WirTypeId,
};

/// Run all WIR-level optimizations on the module (in-place).
pub fn optimize_wir(module: &mut WirModule) {
    // Whole-module pass: rewrite struct-returning functions to multi-value.
    sroa_multi_value_returns(module);

    // Whole-module pass: collapse inlined Array::append sequences back to ArrayNewFixed.
    // Runs before promote/split so that recovered ArrayNewFixed nodes are eligible
    // for data segment promotion and large-literal splitting.
    collapse_array_append_sequences(module);

    // Whole-module pass: promote constant primitive arrays to data segments.
    // Runs before split_large_array_literals so promoted arrays don't get split.
    promote_constant_arrays_to_data(module);

    // Whole-module pass: split large array.new_fixed into array.new_default + array.set.
    split_large_array_literals(module);

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

/// Returns true if a `WirType` is a valid Wasm value type for multi-value returns.
///
/// Primitive scalars (i32, i64, f32, f64) are always eligible.
/// Concrete GC refs (`ref $T`, `ref null $T`) are also eligible: Wasm multi-value
/// returns support any value type, including GC refs. This allows SROA of structs
/// with GC ref fields, such as tuples containing String values.
/// Abstract heap refs (`ref null struct`, etc.) are excluded as they lack
/// the precise type information needed for `StructGet` replacement.
fn is_eligible_field_type(ty: &WirType) -> bool {
    !matches!(ty, WirType::AbstractRef { .. } | WirType::Unit)
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

        // 2–4 fields, all valid Wasm value types (scalars or concrete GC refs)
        let field_count = struct_type.fields.len();
        if !(2..=4).contains(&field_count) {
            continue;
        }
        if !struct_type
            .fields
            .iter()
            .all(|f| is_eligible_field_type(&f.ty))
        {
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
        // Recurse into nested statement-level blocks
        match instr {
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                validate_call_sites_in_body(body, candidate_ids, invalid);
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                // Check condition expression for invalid calls (not in nested block scope)
                find_nested_candidate_calls(condition, candidate_ids, invalid);
                validate_call_sites_in_body(then_body, candidate_ids, invalid);
                if let Some(eb) = else_body {
                    validate_call_sites_in_body(eb, candidate_ids, invalid);
                }
            }
            WirInstr::Seq(body) => {
                validate_call_sites_in_body(body, candidate_ids, invalid);
            }
            // For non-block instructions, check for invalid call uses at this level
            _ => {
                check_invalid_call_uses(instr, candidate_ids, invalid);
            }
        }
    }

    // Check that LocalSet(Call(candidate)) temps are only used via StructGet.
    // Also handles calls wrapped in ValueCopy or trivial inlined blocks.
    for instr in instrs {
        if let WirInstr::LocalSet { name, value } = instr
            && let Some(func_id_idx) = unwrap_to_candidate_call(value, candidate_ids)
        {
            // Verify all uses of `name` in this body are StructGet patterns
            if !all_uses_are_struct_get(instrs, name) {
                invalid.insert(func_id_idx);
            }
        }
    }
}

/// Look through `ValueCopy`, trivial `Block` wrappers, and other transparent
/// expressions to find a `Call` to a candidate function. Returns the `func_id`
/// index if found.
fn unwrap_to_candidate_call(instr: &WirInstr, candidate_ids: &IndexSet<u32>) -> Option<u32> {
    match instr {
        WirInstr::Call { func_id, .. } if candidate_ids.contains(&func_id.index()) => {
            Some(func_id.index())
        }
        // ValueCopy wraps a struct reference — look through it
        WirInstr::ValueCopy { expr, .. } => unwrap_to_candidate_call(expr, candidate_ids),
        // Trivial block from inlining: the block's result value is either:
        // 1. The last instruction in body (implicit value)
        // 2. A Seq([..., value, Br]) pattern (break-with-value)
        WirInstr::Block { body, .. } => extract_block_result_call(body, candidate_ids),
        _ => None,
    }
}

/// Extract a candidate call from the result position of a block body.
/// Handles both implicit block results and explicit `Seq([value, Br])` patterns.
fn extract_block_result_call(body: &[WirInstr], candidate_ids: &IndexSet<u32>) -> Option<u32> {
    let last = body.last()?;
    match last {
        // Block ends with Seq([..., value, Br { depth }]) — break-with-value
        WirInstr::Seq(seq) => {
            if let Some((WirInstr::Br { .. }, rest)) = seq.split_last()
                && let Some((val, _)) = rest.split_last()
            {
                return unwrap_to_candidate_call(val, candidate_ids);
            }
            None
        }
        // Block ends with the value directly (no explicit br)
        other => unwrap_to_candidate_call(other, candidate_ids),
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
        // LocalSet { value: <wrapper>(Call) } is valid — handled separately
        WirInstr::LocalSet { value, .. }
            if unwrap_to_candidate_call(value, candidate_ids).is_some() =>
        {
            // Check args of the underlying call for nested candidate calls
            if let Some(call) = unwrap_to_inner_call(value)
                && let WirInstr::Call { args, .. } = call
            {
                for arg in args {
                    find_nested_candidate_calls(arg, candidate_ids, invalid);
                }
            }
            // Also check prefix instructions in any block wrapper.
            // When the call is wrapped in Block { body: [prefix..., result_call] },
            // the prefix instructions may contain calls to other candidates that
            // would go unrewritten.
            find_candidate_calls_in_block_prefix(value, candidate_ids, invalid);
        }
        // Any other instruction that contains a Call to a candidate is invalid
        _ => {
            find_nested_candidate_calls(instr, candidate_ids, invalid);
        }
    }
}

/// Scan prefix instructions in Block/ValueCopy wrappers for nested candidate calls.
/// When a `LocalSet { value: ValueCopy { Block { body } } }` wraps a candidate call
/// as its result, the prefix instructions in the block body may also contain calls
/// to candidates that the rewrite pass cannot reach.
fn find_candidate_calls_in_block_prefix(
    instr: &WirInstr,
    candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
) {
    match instr {
        WirInstr::ValueCopy { expr, .. } => {
            find_candidate_calls_in_block_prefix(expr, candidate_ids, invalid);
        }
        WirInstr::Block { body, .. } => {
            // All instructions except the last (which is the result value) are prefix
            if let Some((_, prefix)) = body.split_last() {
                for prefix_instr in prefix {
                    find_nested_candidate_calls(prefix_instr, candidate_ids, invalid);
                }
            }
        }
        _ => {}
    }
}

/// Unwrap through ValueCopy/Block to find the inner Call instruction (for arg checking).
fn unwrap_to_inner_call(instr: &WirInstr) -> Option<&WirInstr> {
    match instr {
        WirInstr::Call { .. } => Some(instr),
        WirInstr::ValueCopy { expr, .. } => unwrap_to_inner_call(expr),
        WirInstr::Block { body, .. } => {
            let last = body.last()?;
            match last {
                WirInstr::Seq(seq) => {
                    if let Some((WirInstr::Br { .. }, rest)) = seq.split_last()
                        && let Some((val, _)) = rest.split_last()
                    {
                        return unwrap_to_inner_call(val);
                    }
                    None
                }
                other => unwrap_to_inner_call(other),
            }
        }
        _ => None,
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

        // Extract the Call instruction (and any prefix statements from block wrappers)
        let (prefix_instrs, call_instr) = take_call_from_local_set(&mut instrs[set_idx]);
        // Emit prefix instructions (e.g. local initialization from inlined blocks)
        result.extend(prefix_instrs);
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
///
/// Also handles `ValueCopy { expr: StructGet { field_name, expr: LocalGet(temp) } }` →
/// `LocalGet(fresh_local)`, eliminating the unnecessary copy. After SROA, the fresh local
/// already holds a reference to the value that was in the struct field; the `ValueCopy` was
/// emitted to preserve value semantics when extracting from a shared struct, but since SROA
/// has eliminated the struct, the copy is redundant.
fn replace_struct_gets(
    instr: &mut WirInstr,
    replacements: &indexmap::IndexMap<String, indexmap::IndexMap<String, String>>,
) {
    // Handle ValueCopy(StructGet(LocalGet(temp))) → LocalGet(sroa_fresh).
    // This eliminates the unnecessary shallow copy that was emitted to preserve value
    // semantics when extracting a field from a shared struct. After SROA the struct no
    // longer exists, so the copy serves no purpose.
    if let WirInstr::ValueCopy { expr, .. } = instr
        && let WirInstr::StructGet {
            field_name,
            expr: inner_expr,
            ..
        } = expr.as_ref()
        && let WirInstr::LocalGet { name: temp_name } = inner_expr.as_ref()
        && let Some(field_map) = replacements.get(temp_name.as_str())
        && let Some(fresh_local) = field_map.get(field_name.as_str())
    {
        *instr = WirInstr::LocalGet {
            name: fresh_local.clone(),
        };
        return;
    }

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

/// Check if instruction is `LocalSet { name, value: <wrapper>(Call { func_id in candidates }) }`.
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
    let candidate_ids: IndexSet<u32> = candidate_map.keys().copied().collect();
    unwrap_to_candidate_call(value, &candidate_ids).is_some()
}

/// Extract (`func_id_index`, `temp_name`) from a candidate call `LocalSet`.
/// Handles calls wrapped in `ValueCopy` or trivial inlined `Block`.
fn extract_candidate_call_info(
    instr: &WirInstr,
    candidate_map: &indexmap::IndexMap<u32, &SroaCandidate>,
) -> Option<(u32, String)> {
    let WirInstr::LocalSet { name, value } = instr else {
        return None;
    };
    let candidate_ids: IndexSet<u32> = candidate_map.keys().copied().collect();
    unwrap_to_candidate_call(value, &candidate_ids).map(|idx| (idx, name.clone()))
}

/// Take the Call instruction out of a `LocalSet`, unwrapping through
/// `ValueCopy` and trivial `Block` wrappers. Replaces the instruction with Nop.
/// Returns `(prefix_instrs, call_instr)` where prefix instructions are statements
/// from inside Block wrappers that must be emitted before the call (e.g. initialization
/// of locals used as call arguments).
fn take_call_from_local_set(instr: &mut WirInstr) -> (Vec<WirInstr>, Box<WirInstr>) {
    let WirInstr::LocalSet { value, .. } = std::mem::replace(instr, WirInstr::Nop) else {
        unreachable!()
    };
    let mut prefix = Vec::new();
    let call = unwrap_and_take_call(value, &mut prefix);
    (prefix, call)
}

/// Recursively unwrap `ValueCopy` and `Block` wrappers to extract the `Call` instruction.
/// Collects any non-result instructions from blocks into `prefix` so they can be
/// emitted before the call.
fn unwrap_and_take_call(mut instr: Box<WirInstr>, prefix: &mut Vec<WirInstr>) -> Box<WirInstr> {
    loop {
        match *instr {
            WirInstr::Call { .. } => return instr,
            WirInstr::ValueCopy { expr, .. } => {
                instr = expr;
            }
            WirInstr::Block { ref mut body, .. } => {
                // Extract the call from the block's result position,
                // and collect all preceding statements as prefix.
                if let Some(call) = take_block_result_call(body, prefix) {
                    instr = call;
                } else {
                    unreachable!("expected call in SROA block unwrap");
                }
            }
            _ => unreachable!("unexpected instruction in SROA call unwrap"),
        }
    }
}

/// Take the call instruction from the result position of a block body.
/// Preceding statements in the block are moved into `prefix`.
fn take_block_result_call(
    body: &mut [WirInstr],
    prefix: &mut Vec<WirInstr>,
) -> Option<Box<WirInstr>> {
    if body.is_empty() {
        return None;
    }

    let last_idx = body.len() - 1;

    // Move all statements before the last (result-producing) instruction to prefix
    for item in &mut body[..last_idx] {
        let taken = std::mem::replace(item, WirInstr::Nop);
        if !matches!(taken, WirInstr::Nop) {
            prefix.push(taken);
        }
    }

    let last = &mut body[last_idx];
    match last {
        // Seq([..., value, Br]) — take the value before Br, move others to prefix
        WirInstr::Seq(seq) => {
            if seq.len() >= 2 && matches!(seq.last(), Some(WirInstr::Br { .. })) {
                let val_idx = seq.len() - 2;
                // Move any statements before the value expression to prefix
                for item in &mut seq[..val_idx] {
                    let taken = std::mem::replace(item, WirInstr::Nop);
                    if !matches!(taken, WirInstr::Nop) {
                        prefix.push(taken);
                    }
                }
                let taken = std::mem::replace(&mut seq[val_idx], WirInstr::Nop);
                Some(Box::new(taken))
            } else {
                None
            }
        }
        // Last instruction is the value directly
        other => {
            let taken = std::mem::replace(other, WirInstr::Nop);
            Some(Box::new(taken))
        }
    }
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

/// Minimum element count to trigger `array.new_data` promotion. Arrays with
/// fewer constant elements keep using `array.new_fixed`.
const ARRAY_NEW_DATA_THRESHOLD: usize = 128;

/// Promote constant primitive `ArrayNewFixed` to `ArrayNewData`.
///
/// When all elements of an `ArrayNewFixed` are compile-time constants of a
/// primitive type, packs the values into a passive data segment and replaces
/// the instruction with `ArrayNewData`. This reduces Wasm binary size and
/// initialization overhead compared to pushing N constants + `array.new_fixed`.
fn promote_constant_arrays_to_data(module: &mut WirModule) {
    // Collect element types for array type defs so we can look them up without
    // borrowing `module.types` while mutating other fields.
    let array_elem_types: Vec<Option<WirType>> = module
        .types
        .iter()
        .map(|td| {
            if let WirTypeDef::Array(a) = td {
                Some(a.element_type.clone())
            } else {
                None
            }
        })
        .collect();

    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            for instr in body.iter_mut() {
                promote_arrays_in_instr(instr, &array_elem_types, &mut module.data);
            }
        }
    }

    // Also check global initializers (e.g., `global ITEMS: Array<i32> = [1,2,3]`).
    for global in &mut module.globals {
        promote_arrays_in_instr(&mut global.init, &array_elem_types, &mut module.data);
    }
}

/// Recursively walk an instruction tree and promote eligible `ArrayNewFixed` nodes.
fn promote_arrays_in_instr(
    instr: &mut WirInstr,
    array_elem_types: &[Option<WirType>],
    data: &mut Vec<WirData>,
) {
    // Recurse into children first (bottom-up).
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            for child in body.iter_mut() {
                promote_arrays_in_instr(child, array_elem_types, data);
            }
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            promote_arrays_in_instr(condition, array_elem_types, data);
            for child in then_body.iter_mut() {
                promote_arrays_in_instr(child, array_elem_types, data);
            }
            if let Some(eb) = else_body {
                for child in eb.iter_mut() {
                    promote_arrays_in_instr(child, array_elem_types, data);
                }
            }
        }
        _ => {
            instr.for_each_boxed_child_mut(&mut |child| {
                promote_arrays_in_instr(child, array_elem_types, data);
            });
        }
    }

    // Check if THIS instruction is an eligible ArrayNewFixed.
    if let WirInstr::ArrayNewFixed { type_id, elements } = instr
        && elements.len() >= ARRAY_NEW_DATA_THRESHOLD
    {
        let arr_type_idx = type_id.index() as usize;
        if let Some(Some(elem_type)) = array_elem_types.get(arr_type_idx)
            && let Some(bytes) = try_pack_constant_elements(elem_type, elements)
        {
            let data_index = u32::try_from(data.len()).expect("too many data segments");
            let len = i32::try_from(elements.len()).unwrap_or(0);
            data.push(WirData {
                bytes,
                offset: None, // passive segment
            });
            *instr = WirInstr::ArrayNewData {
                type_id: type_id.clone(),
                data_index,
                offset: Box::new(WirInstr::I32Const(0)),
                len: Box::new(WirInstr::I32Const(len)),
            };
        }
    }
}

/// Try to pack all elements into a byte buffer for `array.new_data`.
///
/// Returns `Some(bytes)` if every element is a compile-time constant matching
/// the expected element type. Returns `None` if any element is non-constant
/// or the element type is not a packable primitive.
fn try_pack_constant_elements(element_type: &WirType, elements: &[WirInstr]) -> Option<Vec<u8>> {
    let byte_width = element_byte_width(element_type)?;
    let mut bytes = Vec::with_capacity(elements.len() * byte_width);

    for elem in elements {
        encode_constant_element(element_type, elem, &mut bytes)?;
    }

    Some(bytes)
}

/// Returns the storage byte width for a primitive element type in a data segment,
/// or `None` for non-primitive types.
fn element_byte_width(ty: &WirType) -> Option<usize> {
    match ty {
        WirType::I8 | WirType::U8 | WirType::Bool => Some(1),
        WirType::I16 | WirType::U16 => Some(2),
        WirType::I32
        | WirType::U32
        | WirType::Char
        | WirType::Enum { .. }
        | WirType::Flags { .. } => Some(4),
        WirType::I64 | WirType::U64 => Some(8),
        WirType::F32 => Some(4),
        WirType::F64 => Some(8),
        _ => None,
    }
}

/// Encode a single constant WIR instruction into little-endian bytes.
/// Returns `None` if the instruction is not a matching constant.
fn encode_constant_element(
    element_type: &WirType,
    instr: &WirInstr,
    bytes: &mut Vec<u8>,
) -> Option<()> {
    match (element_type, instr) {
        // 1-byte types: i8, u8, bool (stored as I32Const in WIR)
        (WirType::I8 | WirType::U8 | WirType::Bool, WirInstr::I32Const(v)) => {
            bytes.push(*v as u8);
        }
        // 2-byte types: i16, u16 (stored as I32Const in WIR)
        (WirType::I16 | WirType::U16, WirInstr::I32Const(v)) => {
            bytes.extend_from_slice(&(*v as u16).to_le_bytes());
        }
        // 4-byte i32 types: i32, u32, char, enum, flags
        (
            WirType::I32
            | WirType::U32
            | WirType::Char
            | WirType::Enum { .. }
            | WirType::Flags { .. },
            WirInstr::I32Const(v),
        ) => {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        // 8-byte i64 types: i64, u64
        (WirType::I64 | WirType::U64, WirInstr::I64Const(v)) => {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        // f32
        (WirType::F32, WirInstr::F32Const(v)) => {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        // f64
        (WirType::F64, WirInstr::F64Const(v)) => {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        _ => return None,
    }
    Some(())
}

/// Maximum element count for `array.new_fixed`. Arrays larger than this are
/// rewritten to `array.new_default` + individual `array.set` instructions.
///
/// `array.new_fixed N` requires all N element values on the Wasm operand stack
/// simultaneously, which causes pathological JIT compilation times in Cranelift's
/// register allocator for large N (e.g. 8 000+ elements → minutes of JIT time).
/// The `array.set` form consumes each value immediately, keeping stack depth low.
const ARRAY_NEW_FIXED_LIMIT: usize = 256;

/// Split large `ArrayNewFixed` instructions into `ArrayNewDefault` + `ArraySet` sequences.
///
/// Walks all function bodies and rewrites any `ArrayNewFixed` with more than
/// [`ARRAY_NEW_FIXED_LIMIT`] elements. Uses a module-level counter for unique local names.
fn split_large_array_literals(module: &mut WirModule) {
    let mut counter: u32 = 0;
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            for instr in body.iter_mut() {
                split_large_arrays_in_instr(instr, &mut counter);
            }
        }
    }
}

/// Recursively walk an instruction tree and replace large `ArrayNewFixed` nodes.
fn split_large_arrays_in_instr(instr: &mut WirInstr, counter: &mut u32) {
    // First, recurse into children so inner ArrayNewFixed nodes are handled first.
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            for child in body.iter_mut() {
                split_large_arrays_in_instr(child, counter);
            }
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            split_large_arrays_in_instr(condition, counter);
            for child in then_body.iter_mut() {
                split_large_arrays_in_instr(child, counter);
            }
            if let Some(eb) = else_body {
                for child in eb.iter_mut() {
                    split_large_arrays_in_instr(child, counter);
                }
            }
        }
        _ => {
            instr.for_each_boxed_child_mut(&mut |child| {
                split_large_arrays_in_instr(child, counter);
            });
        }
    }

    // Now check if THIS instruction is a large ArrayNewFixed that should be split.
    if let WirInstr::ArrayNewFixed { elements, .. } = instr
        && elements.len() > ARRAY_NEW_FIXED_LIMIT
    {
        rewrite_large_array_new_fixed(instr, counter);
    }
}

/// Rewrite a single `ArrayNewFixed` into `Seq([DeclareLocal, LocalSet(ArrayNewDefault), ArraySet*, LocalGet])`.
///
/// The resulting `Seq` is a value-producing sequence: the last instruction (`LocalGet`)
/// leaves the array reference on the stack, making this a drop-in replacement.
fn rewrite_large_array_new_fixed(instr: &mut WirInstr, counter: &mut u32) {
    let WirInstr::ArrayNewFixed { type_id, elements } = std::mem::replace(instr, WirInstr::Nop)
    else {
        return;
    };

    *counter += 1;
    let arr_local = format!("__wir_arr_init_{counter}");
    let len = i32::try_from(elements.len()).unwrap_or(0);
    let raw_ref_type = WirType::Ref {
        type_id: type_id.clone(),
        nullable: true,
    };

    let mut seq = Vec::with_capacity(elements.len() + 3);
    seq.push(WirInstr::DeclareLocal {
        name: arr_local.clone(),
        ty: raw_ref_type,
    });
    seq.push(WirInstr::LocalSet {
        name: arr_local.clone(),
        value: Box::new(WirInstr::ArrayNewDefault {
            type_id: type_id.clone(),
            len: Box::new(WirInstr::I32Const(len)),
        }),
    });
    for (i, elem) in elements.into_iter().enumerate() {
        seq.push(WirInstr::ArraySet {
            type_id: type_id.clone(),
            array: Box::new(WirInstr::LocalGet {
                name: arr_local.clone(),
            }),
            index: Box::new(WirInstr::I32Const(i32::try_from(i).unwrap_or(0))),
            value: Box::new(elem),
        });
    }
    seq.push(WirInstr::LocalGet { name: arr_local });

    *instr = WirInstr::Seq(seq);
}

/// Collapse inlined `Array::append` sequences back to `ArrayNewFixed`.
///
/// After the `SequenceLiteralBuilder` trait path is inlined, array literals like
/// `[10, 20, 30]` become:
///
/// ```text
/// LocalSet { name: X, value: StructNew { ... ArrayNewDefault(N) ... I32Const(0) } }
/// Block { Call { Array::append(receiver, v0) } }
/// Block { Call { Array::append(receiver, v1) } }
/// ...
/// ```
///
/// This pass recognizes that pattern and rewrites it to use `ArrayNewFixed`
/// (replacing `ArrayNewDefault` and removing the append calls), which is then
/// eligible for `promote_constant_arrays_to_data` and `split_large_array_literals`.
fn collapse_array_append_sequences(module: &mut WirModule) {
    let import_func_count = module
        .imports
        .iter()
        .filter(|i| matches!(i.desc, WirImportDesc::Func { .. }))
        .count() as u32;

    // Build set of function indices that have COMP_FEATURE_ARRAY_APPEND.
    let append_func_indices: IndexSet<u32> = module
        .functions
        .iter()
        .enumerate()
        .filter(|(_, f)| f.comp_features & COMP_FEATURE_ARRAY_APPEND != 0)
        .map(|(i, _)| import_func_count + u32::try_from(i).unwrap())
        .collect();

    if append_func_indices.is_empty() {
        return;
    }

    // Build map: type index → is Array<T> struct (has generic_origin.base_name == "Array").
    let array_struct_types: IndexSet<u32> = module
        .types
        .iter()
        .enumerate()
        .filter_map(|(i, td)| {
            if let WirTypeDef::Struct(s) = td
                && s.generic_origin
                    .as_ref()
                    .is_some_and(|g| g.base_name == "Array")
            {
                Some(u32::try_from(i).unwrap())
            } else {
                None
            }
        })
        .collect();

    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            collapse_appends_in_body(body, &append_func_indices, &array_struct_types);
        }
    }
}

/// Describes how the Array<T> is accessed from the local variable.
#[derive(Debug, Clone)]
enum ArrayAccessPath {
    /// The local IS the Array<T> struct directly.
    Direct,
    /// The Array<T> is a field of the local's struct type.
    Field { outer_type_idx: u32 },
}

/// Information about a detected Array<T> initialization via `SequenceLiteralBuilder`.
struct ArrayInitInfo {
    /// Name of the local variable holding the struct.
    local_name: String,
    /// WIR type ID of the raw Wasm array type (e.g., `builtin::array<i32>`).
    raw_array_type_id: WirTypeId,
    /// Expected number of appends (from `ArrayNewDefault` capacity).
    capacity: usize,
    /// How to access the Array<T> from the local.
    access_path: ArrayAccessPath,
    /// Index of the `I32Const(0)` field (the `used` counter) within the Array struct fields.
    /// Needed to rewrite it to `I32Const(N)`.
    used_field_index: usize,
}

/// Scan an instruction tree for init + N×append patterns and collapse them.
/// Recurses into all instruction bodies (blocks, loops, ifs, and also block bodies
/// nested inside tree nodes like `ValueCopy { expr: Block { ... } }`).
fn collapse_appends_in_instr(
    instr: &mut WirInstr,
    append_func_indices: &IndexSet<u32>,
    array_struct_types: &IndexSet<u32>,
) {
    // If this instruction contains a Vec<WirInstr> body, process it.
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            collapse_appends_in_body(body, append_func_indices, array_struct_types);
            return;
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            collapse_appends_in_instr(condition, append_func_indices, array_struct_types);
            collapse_appends_in_body(then_body, append_func_indices, array_struct_types);
            if let Some(eb) = else_body {
                collapse_appends_in_body(eb, append_func_indices, array_struct_types);
            }
            return;
        }
        _ => {}
    }

    // For non-body instructions, recurse into all Box children.
    instr.for_each_boxed_child_mut(&mut |child| {
        collapse_appends_in_instr(child, append_func_indices, array_struct_types);
    });
}

/// Scan a flat instruction list for init + N×append patterns and collapse them.
fn collapse_appends_in_body(
    body: &mut Vec<WirInstr>,
    append_func_indices: &IndexSet<u32>,
    array_struct_types: &IndexSet<u32>,
) {
    // First recurse into all children.
    for instr in body.iter_mut() {
        collapse_appends_in_instr(instr, append_func_indices, array_struct_types);
    }

    // Now scan the flat body for init + append patterns.
    let mut i = 0;
    while i < body.len() {
        if let Some(init_info) = try_match_array_init(&body[i], array_struct_types) {
            let n = init_info.capacity;
            // Check if the next N instructions are matching append calls.
            if n > 0
                && i + n < body.len()
                && let Some(values) = try_match_append_sequence(
                    &body[i + 1..i + 1 + n],
                    &init_info,
                    append_func_indices,
                )
            {
                // Rewrite: replace ArrayNewDefault with ArrayNewFixed in the init.
                rewrite_init_to_fixed(&mut body[i], &init_info, values);
                // Remove the N append instructions.
                body.drain(i + 1..i + 1 + n);
                // Continue from the next instruction after the rewritten init.
                i += 1;
                continue;
            }
        }
        i += 1;
    }
}

/// Try to match a `LocalSet` that initializes an Array<T> via `SequenceLiteralBuilder`.
///
/// Matches patterns like:
/// ```text
/// LocalSet { name: X, value: StructNew { type_id: OUTER,
///   fields: [RefAsNonNull(StructNew { type_id: ARRAY,
///     fields: [RefAsNonNull(ArrayNewDefault { type_id: RAW, len: I32Const(N) }), I32Const(0)]
///   })]
/// }}
/// ```
/// or the direct Array<T> case:
/// ```text
/// LocalSet { name: X, value: StructNew { type_id: ARRAY,
///   fields: [RefAsNonNull(ArrayNewDefault { type_id: RAW, len: I32Const(N) }), I32Const(0)]
/// }}
/// ```
fn try_match_array_init(
    instr: &WirInstr,
    array_struct_types: &IndexSet<u32>,
) -> Option<ArrayInitInfo> {
    let WirInstr::LocalSet { name, value } = instr else {
        return None;
    };

    let WirInstr::StructNew { type_id, fields } = value.as_ref() else {
        return None;
    };

    // Case 1: Direct Array<T> init (LocalSet { name, StructNew { Array<T>, [RefAsNonNull(ArrayNewDefault), I32Const(0)] } })
    if array_struct_types.contains(&type_id.index()) {
        return try_extract_array_new_default(fields, name.clone(), ArrayAccessPath::Direct);
    }

    // Case 2: Wrapper struct with Array<T> field
    // Look through fields for a RefAsNonNull(StructNew { Array<T>, ... })
    for (field_idx, field) in fields.iter().enumerate() {
        let inner_struct_new = match field {
            WirInstr::RefAsNonNull(inner) => inner.as_ref(),
            _ => field,
        };

        if let WirInstr::StructNew {
            type_id: inner_type_id,
            fields: inner_fields,
        } = inner_struct_new
            && array_struct_types.contains(&inner_type_id.index())
        {
            // Find the field name for this index from the outer struct.
            // We need to match against the access path later, so we need
            // the field name. Since WIR doesn't store field names in StructNew,
            // we need to figure it out differently. Actually, looking at the
            // WIR debug output, the `StructGet` uses the field name, and
            // the StructNew field order matches the struct definition order.
            // We'll use the field index to look up the name later in matching.
            // For now, record the outer type and field index.
            let _ = field_idx; // suppress unused warning
            return try_extract_array_new_default(
                inner_fields,
                name.clone(),
                ArrayAccessPath::Field {
                    outer_type_idx: type_id.index(),
                },
            );
        }
    }

    None
}

/// Try to extract `ArrayNewDefault` info from Array<T> struct fields.
/// Expected: `[RefAsNonNull(ArrayNewDefault { type_id, len: I32Const(N) }), I32Const(0)]`
fn try_extract_array_new_default(
    fields: &[WirInstr],
    local_name: String,
    access_path: ArrayAccessPath,
) -> Option<ArrayInitInfo> {
    if fields.len() != 2 {
        return None;
    }

    // First field: RefAsNonNull(ArrayNewDefault { type_id, len: I32Const(N) })
    let WirInstr::RefAsNonNull(inner) = &fields[0] else {
        return None;
    };
    let WirInstr::ArrayNewDefault { type_id, len } = inner.as_ref() else {
        return None;
    };
    let WirInstr::I32Const(capacity) = len.as_ref() else {
        return None;
    };
    let capacity = usize::try_from(*capacity).ok()?;

    // Second field: I32Const(0) (the `used` counter)
    let WirInstr::I32Const(0) = &fields[1] else {
        return None;
    };

    Some(ArrayInitInfo {
        local_name,
        raw_array_type_id: type_id.clone(),
        capacity,
        access_path,
        used_field_index: 1,
    })
}

/// Try to match N consecutive instructions as `Array::append(receiver, value)` calls.
/// Each append may be wrapped in a `Block` (from inlining), possibly with
/// `LocalSet` instructions that copy the receiver into a temporary local.
/// Returns the extracted element values if successful.
fn try_match_append_sequence(
    instrs: &[WirInstr],
    init_info: &ArrayInitInfo,
    append_func_indices: &IndexSet<u32>,
) -> Option<Vec<WirInstr>> {
    let mut values = Vec::with_capacity(instrs.len());

    for instr in instrs {
        // Extract the Call and any local aliases from inside a Block.
        let (call, aliases) = extract_call_from_block(instr);

        let WirInstr::Call { func_id, args } = call else {
            return None;
        };

        // Check if this is a recognized append function.
        if !append_func_indices.contains(&func_id.index()) {
            return None;
        }

        // Verify the receiver matches the access path.
        if args.len() != 2 {
            return None;
        }

        if !receiver_matches_with_aliases(&args[0], init_info, &aliases) {
            return None;
        }

        values.push(args[1].clone());
    }

    Some(values)
}

/// Extract a Call instruction from inside a Block, along with any local
/// aliases created by preceding `LocalSet { name, value: LocalGet }` instructions.
///
/// After inlining, a `push_literal` call often expands to:
/// ```text
/// Block { body: [
///   LocalSet { name: "__local_7", value: LocalGet { name: "__local_0" } },
///   Call { func_id: append, args: [LocalGet { name: "__local_7" }, value] }
/// ] }
/// ```
///
/// Returns the Call instruction and a list of (`alias_name`, `original_name`) pairs.
fn extract_call_from_block(instr: &WirInstr) -> (&WirInstr, Vec<(String, String)>) {
    let WirInstr::Block {
        body, result: None, ..
    } = instr
    else {
        return (instr, Vec::new());
    };

    if body.is_empty() {
        return (instr, Vec::new());
    }

    // The last instruction should be the Call.
    let call = body.last().unwrap();

    // All preceding instructions should be LocalSet aliases (LocalSet copying from LocalGet).
    let mut aliases = Vec::new();
    for preceding in &body[..body.len() - 1] {
        if let WirInstr::LocalSet { name, value } = preceding
            && let WirInstr::LocalGet { name: src_name } = value.as_ref()
        {
            aliases.push((name.clone(), src_name.clone()));
        } else {
            // Non-alias instruction before the call — bail out.
            return (instr, Vec::new());
        }
    }

    (call, aliases)
}

/// Check if a receiver expression matches the expected access path for the Array<T>,
/// resolving local aliases from inline expansion.
fn receiver_matches_with_aliases(
    receiver: &WirInstr,
    init_info: &ArrayInitInfo,
    aliases: &[(String, String)],
) -> bool {
    match &init_info.access_path {
        ArrayAccessPath::Direct => {
            // Receiver should be LocalGet { name } where name resolves to init_info.local_name
            if let WirInstr::LocalGet { name } = receiver {
                resolve_alias(name, aliases) == init_info.local_name
            } else {
                false
            }
        }
        ArrayAccessPath::Field { outer_type_idx } => {
            // Receiver should be StructGet { type_id, expr: LocalGet { name } }
            if let WirInstr::StructGet { type_id, expr, .. } = receiver {
                if type_id.index() != *outer_type_idx {
                    return false;
                }
                if let WirInstr::LocalGet { name } = expr.as_ref() {
                    resolve_alias(name, aliases) == init_info.local_name
                } else {
                    false
                }
            } else {
                false
            }
        }
    }
}

/// Resolve a local name through a chain of aliases.
/// If `name` appears as an alias target, return the source name (recursively).
fn resolve_alias<'a>(name: &'a str, aliases: &'a [(String, String)]) -> &'a str {
    for (alias_name, original_name) in aliases {
        if alias_name == name {
            return resolve_alias(original_name, aliases);
        }
    }
    name
}

/// Rewrite the init instruction to use `ArrayNewFixed` instead of `ArrayNewDefault`,
/// and update the `used` counter from 0 to N.
fn rewrite_init_to_fixed(instr: &mut WirInstr, init_info: &ArrayInitInfo, values: Vec<WirInstr>) {
    let n = i32::try_from(values.len()).unwrap_or(0);

    // Navigate into the instruction tree to find and replace ArrayNewDefault.
    let WirInstr::LocalSet { value, .. } = instr else {
        return;
    };

    let array_fields = match value.as_mut() {
        // Direct Array<T>
        WirInstr::StructNew { fields, .. } if init_info.access_path.is_direct() => fields,
        // Wrapper struct containing Array<T>
        WirInstr::StructNew { fields, .. } => {
            // Find the Array<T> StructNew inside the wrapper fields.
            let Some(inner_fields) = find_inner_array_fields(fields) else {
                return;
            };
            inner_fields
        }
        _ => return,
    };

    // Replace fields[0]: RefAsNonNull(ArrayNewDefault) → RefAsNonNull(ArrayNewFixed)
    if let Some(WirInstr::RefAsNonNull(inner)) = array_fields.first_mut() {
        **inner = WirInstr::ArrayNewFixed {
            type_id: init_info.raw_array_type_id.clone(),
            elements: values,
        };
    }

    // Replace fields[used_field_index]: I32Const(0) → I32Const(N)
    if let Some(used_field) = array_fields.get_mut(init_info.used_field_index) {
        *used_field = WirInstr::I32Const(n);
    }
}

impl ArrayAccessPath {
    fn is_direct(&self) -> bool {
        matches!(self, Self::Direct)
    }
}

/// Find the inner Array<T> fields within a wrapper struct's fields.
/// Looks for RefAsNonNull(StructNew { fields }) pattern.
fn find_inner_array_fields(outer_fields: &mut [WirInstr]) -> Option<&mut Vec<WirInstr>> {
    for field in outer_fields.iter_mut() {
        match field {
            WirInstr::RefAsNonNull(inner) => {
                if let WirInstr::StructNew { fields, .. } = inner.as_mut() {
                    // Check if this has the ArrayNewDefault pattern
                    if fields.len() == 2
                        && matches!(&fields[0], WirInstr::RefAsNonNull(i) if matches!(i.as_ref(), WirInstr::ArrayNewDefault { .. }))
                        && matches!(&fields[1], WirInstr::I32Const(0))
                    {
                        return Some(fields);
                    }
                }
            }
            WirInstr::StructNew { fields, .. } => {
                if fields.len() == 2
                    && matches!(&fields[0], WirInstr::RefAsNonNull(i) if matches!(i.as_ref(), WirInstr::ArrayNewDefault { .. }))
                    && matches!(&fields[1], WirInstr::I32Const(0))
                {
                    return Some(fields);
                }
            }
            _ => {}
        }
    }
    None
}
