//! WIR optimization — peephole and structural optimizations on `WirModule`.
//!
//! Runs after `wir_build` and before `codegen::emit`.
//!
//! Current passes:
//! - **Multi-value return SROA**: rewrites functions that return small scalar structs
//!   to use Wasm multi-value returns, eliminating GC struct allocation.
//! - **Box parameter SROA**: rewrites functions that take `ref null Box<T>` parameters
//!   (where Box<T> is a single-field wrapper struct) to take the scalar `T` directly,
//!   eliminating GC struct allocation at every call site.
//! - **Multi-value tuple elision**: replaces `MultiValueStructNew` + `StructGet`
//!   sequences with `MultiValueLocalBind` to skip intermediate struct allocation.
//! - **Constant array data promotion**: replaces `ArrayNewFixed` of constant primitive
//!   values with `ArrayNewData` backed by a passive data segment.

use indexmap::IndexSet;

use crate::optimize::OptLevel;
use crate::wir::{
    COMP_FEATURE_ARRAY_APPEND, COMP_FEATURE_STRING_APPEND, COMP_FEATURE_STRING_APPEND_CHAR,
    WirData, WirExportDesc, WirFuncId, WirFuncType, WirImportDesc, WirInstr, WirModule, WirType,
    WirTypeDef, WirTypeId, WirVariantType,
};

/// Run all WIR-level optimizations on the module (in-place).
///
/// Skipped entirely at `-O0`.
pub fn optimize_wir(module: &mut WirModule, opt_level: OptLevel) {
    if opt_level == OptLevel::O0 {
        return;
    }
    // Whole-module pass: rewrite struct-returning functions to multi-value.
    sroa_multi_value_returns(module);

    // Whole-module pass: rewrite Box<T> parameters from `ref null Box<T>` to scalar `T`.
    sroa_box_parameters(module);

    // Whole-module pass: collapse inlined Array::append sequences back to ArrayNewFixed.
    // Runs before promote/split so that recovered ArrayNewFixed nodes are eligible
    // for data segment promotion and large-literal splitting.
    collapse_array_append_sequences(module);

    // Whole-module pass: rewrite String::append of short constant strings to
    // sequences of String::append_char calls, eliminating GC allocations.
    simplify_short_string_appends(module);

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
    /// The WIR type index of the struct/variant being returned.
    struct_type_idx: u32,
    /// The field types of the new multi-value result types.
    /// For structs: the struct field types directly.
    /// For variants: [i32 (discriminant), `payload_type_0`, `payload_type_1`, ...].
    field_types: Vec<WirType>,
    /// Number of multi-value result fields.
    field_count: usize,
    /// Field names for the multi-value results.
    /// For structs: struct field names.
    /// For variants: ["discriminant", "`payload_0`", "`payload_1`", ...].
    field_names: Vec<String>,
    /// Variant-specific info (None for struct candidates).
    variant_info: Option<VariantSroaInfo>,
}

/// Additional info needed for variant SROA.
struct VariantSroaInfo {
    /// WIR type indices of the case struct types (index = case discriminant).
    case_type_indices: Vec<Option<u32>>,
    /// Number of payload fields per case.
    case_payload_counts: Vec<usize>,
    /// Maximum payload count across all cases.
    max_payload_count: usize,
}

/// Replacement info for a variant SROA'd temp local at call sites.
struct VariantReplacement {
    /// Local name holding the discriminant value.
    disc_local: String,
    /// `case_wir_type_idx` → discriminant value (i32).
    case_disc_values: indexmap::IndexMap<u32, i32>,
    /// `(case_wir_type_idx, field_name_in_case_struct)` → sroa local name.
    field_to_local: indexmap::IndexMap<(u32, String), String>,
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

/// Structural equality for `WirType` (not derived because `WirTypeId` has no `PartialEq`).
fn wir_types_equal(a: &WirType, b: &WirType) -> bool {
    match (a, b) {
        (WirType::I8, WirType::I8)
        | (WirType::I16, WirType::I16)
        | (WirType::I32, WirType::I32)
        | (WirType::I64, WirType::I64)
        | (WirType::U8, WirType::U8)
        | (WirType::U16, WirType::U16)
        | (WirType::U32, WirType::U32)
        | (WirType::U64, WirType::U64)
        | (WirType::F32, WirType::F32)
        | (WirType::F64, WirType::F64)
        | (WirType::Bool, WirType::Bool)
        | (WirType::Char, WirType::Char)
        | (WirType::Unit, WirType::Unit) => true,
        (
            WirType::Ref {
                type_id: a_id,
                nullable: a_null,
            },
            WirType::Ref {
                type_id: b_id,
                nullable: b_null,
            },
        ) => a_id.index() == b_id.index() && a_null == b_null,
        (WirType::Enum { type_id: a_id }, WirType::Enum { type_id: b_id })
        | (WirType::Flags { type_id: a_id }, WirType::Flags { type_id: b_id }) => {
            a_id.index() == b_id.index()
        }
        _ => false,
    }
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

        let ret_type_idx = ret_type_id.index();
        let body = func.body.as_ref().unwrap();

        match module.types.get(ret_type_idx as usize) {
            // --- Struct SROA (existing) ---
            Some(WirTypeDef::Struct(struct_type)) => {
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
                if !all_returns_are_struct_new(body, ret_type_idx) {
                    continue;
                }

                let field_types: Vec<WirType> =
                    struct_type.fields.iter().map(|f| f.ty.clone()).collect();
                let field_names: Vec<String> =
                    struct_type.fields.iter().map(|f| f.name.clone()).collect();

                candidates.push((
                    func_id_index,
                    SroaCandidate {
                        func_array_idx: i,
                        struct_type_idx: ret_type_idx,
                        field_types,
                        field_count,
                        field_names,
                        variant_info: None,
                    },
                ));
            }
            // --- Variant SROA (new) ---
            Some(WirTypeDef::Variant(variant_type)) => {
                if let Some(candidate) =
                    try_variant_sroa_candidate(module, i, ret_type_idx, variant_type, body)
                {
                    candidates.push((func_id_index, candidate));
                }
            }
            _ => {}
        }
    }

    candidates
}

/// Try to create a variant SROA candidate. Returns None if the variant is ineligible.
///
/// A variant is eligible if:
/// - All payload types across all cases are eligible scalar types
/// - Max payload count across all cases is ≤ 3 (so total fields ≤ 4: disc + 3 payloads)
/// - All returns are `StructNew` of the variant's case types
/// - Case type indices can be resolved via `variant_case_info`
fn try_variant_sroa_candidate(
    module: &WirModule,
    func_array_idx: usize,
    variant_type_idx: u32,
    variant_type: &WirVariantType,
    body: &[WirInstr],
) -> Option<SroaCandidate> {
    // Collect per-case info: case type index and payload count
    let mut case_type_indices: Vec<Option<u32>> = Vec::with_capacity(variant_type.cases.len());
    let mut case_payload_counts: Vec<usize> = Vec::with_capacity(variant_type.cases.len());
    let mut max_payload_count: usize = 0;

    // Build a mapping of case_wir_type_idx for this variant from variant_case_info
    let mut case_idx_to_type_idx: indexmap::IndexMap<u32, u32> = indexmap::IndexMap::new();
    for (&case_wir_idx, &(parent_variant_idx, case_index)) in &module.variant_case_info {
        if parent_variant_idx == variant_type_idx {
            case_idx_to_type_idx.insert(case_index, case_wir_idx);
        }
    }

    for case in &variant_type.cases {
        let payload_count = case.payload.len();
        case_payload_counts.push(payload_count);
        if payload_count > max_payload_count {
            max_payload_count = payload_count;
        }

        // Check payload types are eligible
        for ty in &case.payload {
            if !is_eligible_field_type(ty) {
                return None;
            }
        }

        if payload_count > 0 {
            // Must have a case type registered
            let &case_type_idx = case_idx_to_type_idx.get(&case.index)?;
            case_type_indices.push(Some(case_type_idx));
        } else {
            case_type_indices.push(None);
        }
    }

    // Total multi-value fields: discriminant + max_payload_count
    let field_count = 1 + max_payload_count;
    if field_count > 4 {
        return None;
    }

    // Compute the payload types: use the type from whichever case provides it.
    // All cases that have a payload at a given position must use the same type.
    let mut payload_types: Vec<WirType> = Vec::with_capacity(max_payload_count);
    for pos in 0..max_payload_count {
        let mut found: Option<&WirType> = None;
        for case in &variant_type.cases {
            if let Some(ty) = case.payload.get(pos) {
                if let Some(existing) = found {
                    if !wir_types_equal(existing, ty) {
                        // Different cases have different types at the same position
                        return None;
                    }
                } else {
                    found = Some(ty);
                }
            }
        }
        payload_types.push(found?.clone());
    }

    // Build multi-value field types: [i32 (discriminant), payload_0, payload_1, ...]
    let mut field_types = Vec::with_capacity(field_count);
    field_types.push(WirType::I32);
    field_types.extend(payload_types);

    // Build field names
    let mut field_names = Vec::with_capacity(field_count);
    field_names.push("discriminant".to_string());
    for pos in 0..max_payload_count {
        field_names.push(format!("payload_{pos}"));
    }

    // Collect ALL case type indices (including unit cases) for return validation
    let mut all_case_type_indices: IndexSet<u32> = IndexSet::new();
    for &opt in &case_type_indices {
        if let Some(idx) = opt {
            all_case_type_indices.insert(idx);
        }
    }
    // Also include StructNew of the base variant type (for unit cases like None)
    all_case_type_indices.insert(variant_type_idx);

    // Verify all returns are StructNew of one of the variant's case types
    if !all_returns_are_variant_struct_new(body, &all_case_type_indices) {
        return None;
    }

    Some(SroaCandidate {
        func_array_idx,
        struct_type_idx: variant_type_idx,
        field_types,
        field_count,
        field_names,
        variant_info: Some(VariantSroaInfo {
            case_type_indices,
            case_payload_counts,
            max_payload_count,
        }),
    })
}

/// Check that every `Return` in the body is a `StructNew` of one of the variant's case types.
fn all_returns_are_variant_struct_new(
    instrs: &[WirInstr],
    valid_type_indices: &IndexSet<u32>,
) -> bool {
    for instr in instrs {
        if !check_return_variant_struct_new(instr, valid_type_indices) {
            return false;
        }
    }
    true
}

fn check_return_variant_struct_new(instr: &WirInstr, valid_type_indices: &IndexSet<u32>) -> bool {
    match instr {
        WirInstr::Return { value: Some(v) } => {
            value_expr_is_variant_struct_new(v, valid_type_indices)
        }
        WirInstr::Return { value: None } => true,
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            all_returns_are_variant_struct_new(body, valid_type_indices)
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            all_returns_are_variant_struct_new(then_body, valid_type_indices)
                && else_body
                    .as_ref()
                    .is_none_or(|eb| all_returns_are_variant_struct_new(eb, valid_type_indices))
        }
        WirInstr::Seq(body) => all_returns_are_variant_struct_new(body, valid_type_indices),
        _ => true,
    }
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
        WirInstr::Return { value: Some(v) } => value_expr_is_struct_new(v, expected_type_idx),
        WirInstr::Return { value: None } => {
            // Void return is fine for our purposes (won't happen in struct-returning fn)
            true
        }
        WirInstr::Block { body, result, .. } => {
            let inner_ok = all_returns_are_struct_new(body, expected_type_idx);
            if result.is_some() {
                // Typed block: the block's exit values are carried via [val, Br(0)] pairs.
                // These Br-exit values must also be StructNew, otherwise the function
                // cannot be correctly SROA'd (rewrite_returns_to_multi_value only handles
                // explicit Return instructions, not Block/Br exits).
                inner_ok && all_br_values_are_struct_new(body, expected_type_idx, 0)
            } else {
                inner_ok
            }
        }
        WirInstr::Loop { body, .. } => all_returns_are_struct_new(body, expected_type_idx),
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

/// Check if a value-position expression always produces a `StructNew` of the expected type.
/// This handles `return match { ... }` where the return value is a `Seq` or `If` expression
/// that ultimately produces `StructNew` in all branches.
fn value_expr_is_struct_new(expr: &WirInstr, expected_type_idx: u32) -> bool {
    match expr {
        WirInstr::StructNew { type_id, .. } => type_id.index() == expected_type_idx,
        WirInstr::Seq(items) => {
            // In a Seq used as a value expression, the last element produces the value
            items
                .last()
                .is_some_and(|last| value_expr_is_struct_new(last, expected_type_idx))
        }
        WirInstr::If {
            then_body,
            else_body,
            result: Some(_),
            ..
        } => {
            // Typed If: the last instruction in each branch produces the value
            let then_ok = then_body
                .last()
                .is_some_and(|last| value_expr_is_struct_new(last, expected_type_idx));
            let else_ok = else_body.as_ref().is_some_and(|eb| {
                eb.last()
                    .is_some_and(|last| value_expr_is_struct_new(last, expected_type_idx))
            });
            then_ok && else_ok
        }
        WirInstr::Block {
            body,
            result: Some(_),
            ..
        } => {
            // Typed Block (e.g. from BrTable match): check that StructNew/Br pairs
            // and the fallthrough all produce the expected type.
            all_br_values_are_struct_new(body, expected_type_idx, 0)
        }
        _ => false,
    }
}

/// Check that all `StructNew`; `Br` pairs targeting `target_depth` and the fallthrough
/// `StructNew` in a typed block are of the expected type.
///
/// Also handles `Seq([..., val, Br(depth)])` patterns where the exit value and branch
/// are wrapped in a `Seq` (e.g. the `LabeledBlock` exit pattern).
fn all_br_values_are_struct_new(
    instrs: &[WirInstr],
    expected_type_idx: u32,
    target_depth: u32,
) -> bool {
    let mut i = 0;
    while i < instrs.len() {
        if i + 1 < instrs.len()
            && matches!(&instrs[i + 1], WirInstr::Br { depth } if *depth == target_depth)
        {
            // [val, Br(depth)] pair: the instruction before Br is the exit value.
            // OR dead code (unreachable path — skip it).
            let is_valid = contains_unreachable(&instrs[i])
                || matches!(&instrs[i], WirInstr::StructNew { type_id, .. } if type_id.index() == expected_type_idx);
            if !is_valid {
                return false;
            }
            i += 2;
        } else if let WirInstr::Seq(seq) = &instrs[i]
            && seq.last().is_some_and(
                |last| matches!(last, WirInstr::Br { depth } if *depth == target_depth),
            )
        {
            // Seq([..., val, Br(depth)]): the Br is wrapped in a Seq (LabeledBlock exit pattern).
            // The instruction before the Br within the Seq is the exit value.
            let exit_val = seq.len().checked_sub(2).and_then(|j| seq.get(j));
            let is_valid = exit_val.is_some_and(|v| {
                contains_unreachable(v)
                    || matches!(v, WirInstr::StructNew { type_id, .. } if type_id.index() == expected_type_idx)
            });
            if !is_valid {
                return false;
            }
            i += 1;
        } else {
            // Recurse into nested blocks
            if let WirInstr::Block { body, .. } = &instrs[i]
                && !all_br_values_are_struct_new(body, expected_type_idx, target_depth + 1)
            {
                return false;
            }
            i += 1;
        }
    }
    true
}

/// Check if an instruction contains `Unreachable` (indicating dead code).
fn contains_unreachable(instr: &WirInstr) -> bool {
    match instr {
        WirInstr::Unreachable => true,
        WirInstr::Seq(items) => items.iter().any(contains_unreachable),
        _ => false,
    }
}

/// Check if a value-position expression always produces a `StructNew` of one of the valid
/// variant case types. Handles `return match { ... }` for variant SROA.
fn value_expr_is_variant_struct_new(expr: &WirInstr, valid_type_indices: &IndexSet<u32>) -> bool {
    match expr {
        WirInstr::StructNew { type_id, .. } => valid_type_indices.contains(&type_id.index()),
        WirInstr::Seq(items) => items
            .last()
            .is_some_and(|last| value_expr_is_variant_struct_new(last, valid_type_indices)),
        WirInstr::If {
            then_body,
            else_body,
            result: Some(_),
            ..
        } => {
            let then_ok = then_body
                .last()
                .is_some_and(|last| value_expr_is_variant_struct_new(last, valid_type_indices));
            let else_ok = else_body.as_ref().is_some_and(|eb| {
                eb.last()
                    .is_some_and(|last| value_expr_is_variant_struct_new(last, valid_type_indices))
            });
            then_ok && else_ok
        }
        WirInstr::Block {
            body,
            result: Some(_),
            ..
        } => all_br_variant_values_are_struct_new(body, valid_type_indices, 0),
        _ => false,
    }
}

/// Variant version of `all_br_values_are_struct_new`.
fn all_br_variant_values_are_struct_new(
    instrs: &[WirInstr],
    valid_type_indices: &IndexSet<u32>,
    target_depth: u32,
) -> bool {
    let mut i = 0;
    while i < instrs.len() {
        if i + 1 < instrs.len()
            && matches!(&instrs[i + 1], WirInstr::Br { depth } if *depth == target_depth)
        {
            let is_valid = contains_unreachable(&instrs[i])
                || matches!(&instrs[i], WirInstr::StructNew { type_id, .. } if valid_type_indices.contains(&type_id.index()));
            if !is_valid {
                return false;
            }
            i += 2;
        } else {
            if let WirInstr::Block { body, .. } = &instrs[i]
                && !all_br_variant_values_are_struct_new(body, valid_type_indices, target_depth + 1)
            {
                return false;
            }
            i += 1;
        }
    }
    true
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
    let variant_candidate_ids: IndexSet<u32> = candidates
        .iter()
        .filter(|(_, c)| c.variant_info.is_some())
        .map(|(id, _)| *id)
        .collect();

    // Scan all function bodies for calls to candidate functions
    let mut invalid: IndexSet<u32> = IndexSet::new();

    for func in &module.functions {
        if let Some(body) = &func.body {
            validate_call_sites_in_body(body, &candidate_ids, &variant_candidate_ids, &mut invalid);
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
                    variant_info: c.variant_info.as_ref().map(|vi| VariantSroaInfo {
                        case_type_indices: vi.case_type_indices.clone(),
                        case_payload_counts: vi.case_payload_counts.clone(),
                        max_payload_count: vi.max_payload_count,
                    }),
                },
            )
        })
        .collect()
}

/// Validate call sites of candidate functions within a flat instruction list.
fn validate_call_sites_in_body(
    instrs: &[WirInstr],
    candidate_ids: &IndexSet<u32>,
    variant_candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
) {
    for instr in instrs {
        // Recurse into nested statement-level blocks
        match instr {
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                validate_call_sites_in_body(body, candidate_ids, variant_candidate_ids, invalid);
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                // Check condition expression for invalid calls (not in nested block scope)
                find_nested_candidate_calls(condition, candidate_ids, invalid);
                validate_call_sites_in_body(
                    then_body,
                    candidate_ids,
                    variant_candidate_ids,
                    invalid,
                );
                if let Some(eb) = else_body {
                    validate_call_sites_in_body(eb, candidate_ids, variant_candidate_ids, invalid);
                }
            }
            WirInstr::Seq(body) => {
                validate_call_sites_in_body(body, candidate_ids, variant_candidate_ids, invalid);
            }
            // For non-block instructions, check for invalid call uses at this level
            _ => {
                check_invalid_call_uses(instr, candidate_ids, invalid);
            }
        }
    }

    // Check that LocalSet(Call(candidate)) temps are only used via valid patterns.
    // For struct candidates: StructGet(LocalGet(temp))
    // For variant candidates: RefTest(LocalGet(temp)) or StructGet(RefCast(LocalGet(temp)))
    for instr in instrs {
        if let WirInstr::LocalSet { name, value } = instr
            && let Some(func_id_idx) = unwrap_to_candidate_call(value, candidate_ids)
        {
            if variant_candidate_ids.contains(&func_id_idx) {
                // Variant candidate: uses must be RefTest or StructGet(RefCast(...))
                if !all_uses_are_variant_access(instrs, name) {
                    invalid.insert(func_id_idx);
                }
            } else {
                // Struct candidate: uses must be StructGet
                if !all_uses_are_struct_get(instrs, name) {
                    invalid.insert(func_id_idx);
                }
            }
        }
    }
}

/// Check that every reference to `local_name` is a valid variant access pattern:
/// - `RefTest { expr: LocalGet(name) }` — discriminant test
/// - `StructGet { expr: RefCast { expr: LocalGet(name) } }` — payload access
fn all_uses_are_variant_access(instrs: &[WirInstr], local_name: &str) -> bool {
    for instr in instrs {
        if !check_uses_are_variant_access(instr, local_name, VariantAccessCtx::None) {
            return false;
        }
    }
    true
}

/// Context for checking variant access patterns.
#[derive(Clone, Copy)]
enum VariantAccessCtx {
    /// Not inside any variant access pattern.
    None,
    /// Inside `RefTest` or `RefCast` — `LocalGet` is valid here.
    InsideRefTestOrCast,
}

fn check_uses_are_variant_access(
    instr: &WirInstr,
    local_name: &str,
    ctx: VariantAccessCtx,
) -> bool {
    match instr {
        WirInstr::LocalGet { name } if name == local_name => {
            matches!(ctx, VariantAccessCtx::InsideRefTestOrCast)
        }
        WirInstr::LocalSet { name, value } if name == local_name => {
            // The original assignment — check value subtree
            check_variant_uses_in_subtree(value, local_name)
        }
        WirInstr::LocalTee { name, .. } if name == local_name => false,
        WirInstr::RefTest { expr, .. } | WirInstr::RefCast { expr, .. } => {
            check_uses_are_variant_access(expr, local_name, VariantAccessCtx::InsideRefTestOrCast)
        }
        WirInstr::StructGet { expr, .. } => {
            // StructGet can wrap RefCast which wraps LocalGet — check the chain
            check_uses_are_variant_access(expr, local_name, ctx)
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            for child in body {
                if !check_uses_are_variant_access(child, local_name, VariantAccessCtx::None) {
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
            if !check_uses_are_variant_access(condition, local_name, VariantAccessCtx::None) {
                return false;
            }
            for child in then_body {
                if !check_uses_are_variant_access(child, local_name, VariantAccessCtx::None) {
                    return false;
                }
            }
            if let Some(eb) = else_body {
                for child in eb {
                    if !check_uses_are_variant_access(child, local_name, VariantAccessCtx::None) {
                        return false;
                    }
                }
            }
            true
        }
        WirInstr::Seq(body) => {
            for child in body {
                if !check_uses_are_variant_access(child, local_name, VariantAccessCtx::None) {
                    return false;
                }
            }
            true
        }
        _ => check_variant_uses_in_subtree(instr, local_name),
    }
}

fn check_variant_uses_in_subtree(instr: &WirInstr, local_name: &str) -> bool {
    let mut ok = true;
    instr.for_each_child(&mut |child| {
        if ok && !check_uses_are_variant_access(child, local_name, VariantAccessCtx::None) {
            ok = false;
        }
    });
    ok
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
///
/// Returns `None` if the prefix instructions (everything before the result)
/// contain `Br` instructions that target the block itself.  Removing the block
/// wrapper in that case would corrupt those branch depths.
fn extract_block_result_call(body: &[WirInstr], candidate_ids: &IndexSet<u32>) -> Option<u32> {
    let last = body.last()?;

    // Check prefix instructions for branches targeting this block.
    // Any `Br` in the prefix that targets this block (at relative depth 0
    // from the block scope, accounting for nested if/block/loop) would become
    // invalid once the block wrapper is removed.
    let prefix = &body[..body.len() - 1];
    if instrs_have_br_at_depth(prefix, 0) {
        return None;
    }

    match last {
        // Block ends with Seq([..., value, Br { depth }]) — break-with-value
        WirInstr::Seq(seq) => {
            if let Some((WirInstr::Br { .. }, rest)) = seq.split_last()
                && let Some((val, _)) = rest.split_last()
            {
                // Also check Seq items before the value for branches targeting the block.
                let seq_prefix = &rest[..rest.len() - 1];
                if instrs_have_br_at_depth(seq_prefix, 0) {
                    return None;
                }
                return unwrap_to_candidate_call(val, candidate_ids);
            }
            None
        }
        // Block ends with the value directly (no explicit br)
        other => unwrap_to_candidate_call(other, candidate_ids),
    }
}

/// Check if any instruction in the slice contains a `Br` that targets the block
/// at `target_depth` levels above the current nesting position.
///
/// `target_depth` is 0 when checking from directly inside the block.
/// Nested `if`/`block`/`loop` increase the depth by 1 for their bodies.
fn instrs_have_br_at_depth(instrs: &[WirInstr], target_depth: u32) -> bool {
    instrs
        .iter()
        .any(|instr| instr_has_br_at_depth(instr, target_depth))
}

fn instr_has_br_at_depth(instr: &WirInstr, target_depth: u32) -> bool {
    match instr {
        WirInstr::Br { depth } => *depth == target_depth,
        WirInstr::BrIf { depth, condition } => {
            *depth == target_depth || instr_has_br_at_depth(condition, target_depth)
        }
        WirInstr::BrTable {
            index,
            targets,
            default,
        } => {
            targets.contains(&target_depth)
                || *default == target_depth
                || instr_has_br_at_depth(index, target_depth)
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            instr_has_br_at_depth(condition, target_depth)
                || instrs_have_br_at_depth(then_body, target_depth + 1)
                || else_body
                    .as_ref()
                    .is_some_and(|eb| instrs_have_br_at_depth(eb, target_depth + 1))
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            instrs_have_br_at_depth(body, target_depth + 1)
        }
        WirInstr::Seq(items) => instrs_have_br_at_depth(items, target_depth),
        // All other instructions (arithmetic, struct ops, etc.) cannot contain `Br`.
        _ => false,
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
            if let Some(vi) = &candidate.variant_info {
                rewrite_variant_returns_to_multi_value(body, vi, &candidate.field_types);
            } else {
                rewrite_returns_to_multi_value(body);
            }
        }
    }

    // Step B: Rewrite call sites in ALL function bodies.
    // Use indexed access to split borrows between module.types and module.functions.
    for i in 0..module.functions.len() {
        if module.functions[i].body.is_some() {
            let body = module.functions[i].body.as_mut().unwrap();
            rewrite_call_sites(body, &candidate_map, &module.types);
        }
    }
}

/// Rewrite `Return { value: StructNew { fields } }` → `Return { value: Seq(fields) }`.
/// Also handles `return match { ... }` where the return value is a complex expression
/// (`Seq`, `If`, `Block`) that ultimately produces `StructNew` in all branches. In that case,
/// the Return is lifted into each leaf branch to avoid block result type issues.
fn rewrite_returns_to_multi_value(instrs: &mut [WirInstr]) {
    for instr in instrs.iter_mut() {
        match instr {
            WirInstr::Return { value: Some(v) } => {
                match v.as_ref() {
                    WirInstr::StructNew { .. } => {
                        // Direct StructNew → Seq of fields
                        if let WirInstr::StructNew { fields, .. } =
                            std::mem::replace(v.as_mut(), WirInstr::Nop)
                        {
                            **v = WirInstr::Seq(fields);
                        }
                    }
                    WirInstr::Seq(_) | WirInstr::If { .. } | WirInstr::Block { .. } => {
                        // Complex value expr (e.g. return match { ... }):
                        // Lift the Return into each StructNew leaf, then replace
                        // the outer Return with the unwrapped expression.
                        let mut value_expr = std::mem::replace(v.as_mut(), WirInstr::Nop);
                        lift_return_into_struct_new_leaves(&mut value_expr);
                        // Replace the entire Return instruction with the rewritten expression
                        *instr = value_expr;
                    }
                    _ => {}
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

/// Lift `Return` into leaf `StructNew` positions within a value expression.
/// Replaces each `StructNew { fields }` with `Return { value: Seq(fields) }`
/// and removes block result types (since branches now return directly).
///
/// For typed Blocks (e.g. from `return match { ... }` with `BrTable`), this also
/// rewrites `StructNew; Br` pairs inside the block into `Return { Seq(fields) }`.
fn lift_return_into_struct_new_leaves(expr: &mut WirInstr) {
    match expr {
        WirInstr::StructNew { .. } => {
            if let WirInstr::StructNew { fields, .. } = std::mem::replace(expr, WirInstr::Nop) {
                *expr = WirInstr::Return {
                    value: Some(Box::new(WirInstr::Seq(fields))),
                };
            }
        }
        WirInstr::Seq(items) => {
            if let Some(last) = items.last_mut() {
                lift_return_into_struct_new_leaves(last);
            }
        }
        WirInstr::If {
            then_body,
            else_body,
            result,
            ..
        } => {
            // Clear the block result type since branches now return directly
            *result = None;
            if let Some(last) = then_body.last_mut() {
                lift_return_into_struct_new_leaves(last);
            }
            if let Some(eb) = else_body
                && let Some(last) = eb.last_mut()
            {
                lift_return_into_struct_new_leaves(last);
            }
        }
        WirInstr::Block { body, result, .. } => {
            if result.is_some() {
                // Typed block: rewrite StructNew/Br pairs at all depths, then clear result
                rewrite_struct_new_br_to_return(body, 0);
                *result = None;
            }
        }
        _ => {}
    }
}

/// Rewrite `StructNew; Br { depth }` pairs that target the outer block (at `target_depth`)
/// into `Return { Seq(fields) }; Nop` (Nop replaces the Br). Also rewrites the fallthrough
/// `StructNew` at the end of the block.
///
/// Also handles `Seq([..., StructNew, Br(depth)])` patterns where the exit value and
/// branch are wrapped in a `Seq` (e.g. the `LabeledBlock` exit pattern).
fn rewrite_struct_new_br_to_return(instrs: &mut [WirInstr], target_depth: u32) {
    let mut i = 0;
    while i + 1 < instrs.len() {
        if matches!(&instrs[i + 1], WirInstr::Br { depth } if *depth == target_depth) {
            if matches!(&instrs[i], WirInstr::StructNew { .. }) {
                // Replace StructNew with Return { Seq(fields) }
                if let WirInstr::StructNew { fields, .. } =
                    std::mem::replace(&mut instrs[i], WirInstr::Nop)
                {
                    instrs[i] = WirInstr::Return {
                        value: Some(Box::new(WirInstr::Seq(fields))),
                    };
                }
                // Remove the Br (now unreachable after Return)
                instrs[i + 1] = WirInstr::Nop;
            }
            // Skip dead code (unreachable) before Br — leave as-is
            i += 2;
        } else {
            // Handle Seq([..., StructNew, Br(target_depth)]) — LabeledBlock exit pattern
            let is_seq_exit = if let WirInstr::Seq(seq) = &instrs[i] {
                seq.last().is_some_and(
                    |last| matches!(last, WirInstr::Br { depth } if *depth == target_depth),
                ) && seq.len() >= 2
                    && matches!(seq.get(seq.len() - 2), Some(WirInstr::StructNew { .. }))
            } else {
                false
            };
            if is_seq_exit {
                if let WirInstr::Seq(mut seq) = std::mem::replace(&mut instrs[i], WirInstr::Nop) {
                    seq.pop(); // remove Br
                    if let Some(WirInstr::StructNew { fields, .. }) = seq.pop() {
                        let ret = WirInstr::Return {
                            value: Some(Box::new(WirInstr::Seq(fields))),
                        };
                        instrs[i] = if seq.is_empty() {
                            ret
                        } else {
                            seq.push(ret);
                            WirInstr::Seq(seq)
                        };
                    }
                }
            } else {
                // Recurse into nested blocks (which add 1 to the depth)
                if let WirInstr::Block { body, .. } = &mut instrs[i] {
                    rewrite_struct_new_br_to_return(body, target_depth + 1);
                }
            }
            i += 1;
        }
    }

    // Handle the fallthrough (last instruction) — if it's a StructNew without Br.
    // The `matches!` guard before `mem::replace` is intentional to avoid replacing
    // non-StructNew instructions with Nop.
    #[allow(clippy::collapsible_if)]
    if let Some(last) = instrs.last_mut()
        && matches!(last, WirInstr::StructNew { .. })
    {
        if let WirInstr::StructNew { fields, .. } = std::mem::replace(last, WirInstr::Nop) {
            *last = WirInstr::Return {
                value: Some(Box::new(WirInstr::Seq(fields))),
            };
        }
    }
}

/// Rewrite variant returns to multi-value.
///
/// Transforms `Return { StructNew { type_id: case_type, fields } }` into
/// `Return { Seq([discriminant, payload_0, ..., default_padding...]) }`.
/// Unit cases (no payload) get default values (0/0.0/ref.null) for payload slots.
/// Also handles `return match { ... }` where the return value is a complex expression.
fn rewrite_variant_returns_to_multi_value(
    instrs: &mut [WirInstr],
    vi: &VariantSroaInfo,
    result_types: &[WirType],
) {
    for instr in instrs.iter_mut() {
        match instr {
            WirInstr::Return { value: Some(v) } => match v.as_ref() {
                WirInstr::StructNew { .. } => {
                    if let WirInstr::StructNew { fields, .. } =
                        std::mem::replace(v.as_mut(), WirInstr::Nop)
                    {
                        **v = WirInstr::Seq(pad_variant_fields(fields, vi, result_types));
                    }
                }
                WirInstr::Seq(_) | WirInstr::If { .. } | WirInstr::Block { .. } => {
                    let mut value_expr = std::mem::replace(v.as_mut(), WirInstr::Nop);
                    lift_return_into_variant_leaves(&mut value_expr, vi, result_types);
                    *instr = value_expr;
                }
                _ => {}
            },
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                rewrite_variant_returns_to_multi_value(body, vi, result_types);
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                rewrite_variant_returns_to_multi_value(then_body, vi, result_types);
                if let Some(eb) = else_body {
                    rewrite_variant_returns_to_multi_value(eb, vi, result_types);
                }
            }
            WirInstr::Seq(body) => {
                rewrite_variant_returns_to_multi_value(body, vi, result_types);
            }
            _ => {}
        }
    }
}

/// Pad variant fields with default values for missing payload slots.
/// Also replaces `Nop` fields (unit/void placeholders from `StructNew`) with
/// appropriate default values, since Nop produces no value in flat multi-value returns.
fn pad_variant_fields(
    fields: Vec<WirInstr>,
    vi: &VariantSroaInfo,
    result_types: &[WirType],
) -> Vec<WirInstr> {
    let payload_count = fields.len() - 1; // subtract discriminant
    let mut new_fields = fields;
    // Replace any Nop payload fields with default values for their type position
    for (i, field) in new_fields.iter_mut().enumerate().skip(1) {
        if matches!(field, WirInstr::Nop) {
            let pos = i - 1; // payload position (skip discriminant)
            if pos < result_types.len() - 1 {
                *field = default_value_for_type(&result_types[1 + pos]);
            }
        }
    }
    for pos in payload_count..vi.max_payload_count {
        let ty = &result_types[1 + pos]; // +1 to skip discriminant
        new_fields.push(default_value_for_type(ty));
    }
    new_fields
}

/// Lift `Return` into leaf `StructNew` positions for variant SROA.
fn lift_return_into_variant_leaves(
    expr: &mut WirInstr,
    vi: &VariantSroaInfo,
    result_types: &[WirType],
) {
    match expr {
        WirInstr::StructNew { .. } => {
            if let WirInstr::StructNew { fields, .. } = std::mem::replace(expr, WirInstr::Nop) {
                *expr = WirInstr::Return {
                    value: Some(Box::new(WirInstr::Seq(pad_variant_fields(
                        fields,
                        vi,
                        result_types,
                    )))),
                };
            }
        }
        WirInstr::Seq(items) => {
            if let Some(last) = items.last_mut() {
                lift_return_into_variant_leaves(last, vi, result_types);
            }
        }
        WirInstr::If {
            then_body,
            else_body,
            result,
            ..
        } => {
            *result = None;
            if let Some(last) = then_body.last_mut() {
                lift_return_into_variant_leaves(last, vi, result_types);
            }
            if let Some(eb) = else_body
                && let Some(last) = eb.last_mut()
            {
                lift_return_into_variant_leaves(last, vi, result_types);
            }
        }
        WirInstr::Block { body, result, .. } => {
            if result.is_some() {
                rewrite_variant_struct_new_br_to_return(body, 0, vi, result_types);
                *result = None;
            }
        }
        _ => {}
    }
}

/// Variant version of `rewrite_struct_new_br_to_return`.
fn rewrite_variant_struct_new_br_to_return(
    instrs: &mut [WirInstr],
    target_depth: u32,
    vi: &VariantSroaInfo,
    result_types: &[WirType],
) {
    let mut i = 0;
    while i + 1 < instrs.len() {
        if matches!(&instrs[i + 1], WirInstr::Br { depth } if *depth == target_depth) {
            if matches!(&instrs[i], WirInstr::StructNew { .. }) {
                if let WirInstr::StructNew { fields, .. } =
                    std::mem::replace(&mut instrs[i], WirInstr::Nop)
                {
                    instrs[i] = WirInstr::Return {
                        value: Some(Box::new(WirInstr::Seq(pad_variant_fields(
                            fields,
                            vi,
                            result_types,
                        )))),
                    };
                }
                instrs[i + 1] = WirInstr::Nop;
            }
            i += 2;
        } else {
            match &mut instrs[i] {
                WirInstr::Block { body, .. } => {
                    rewrite_variant_struct_new_br_to_return(
                        body,
                        target_depth + 1,
                        vi,
                        result_types,
                    );
                }
                WirInstr::Seq(items) => {
                    rewrite_variant_struct_new_br_to_return(items, target_depth, vi, result_types);
                }
                _ => {}
            }
            i += 1;
        }
    }

    // Handle fallthrough.
    // The `matches!` guard before `mem::replace` is intentional to avoid replacing
    // non-StructNew instructions with Nop.
    #[allow(clippy::collapsible_if)]
    if let Some(last) = instrs.last_mut() {
        if matches!(last, WirInstr::StructNew { .. }) {
            if let WirInstr::StructNew { fields, .. } = std::mem::replace(last, WirInstr::Nop) {
                *last = WirInstr::Return {
                    value: Some(Box::new(WirInstr::Seq(pad_variant_fields(
                        fields,
                        vi,
                        result_types,
                    )))),
                };
            }
        } else if let WirInstr::Seq(items) = last {
            rewrite_variant_struct_new_br_to_return(items, target_depth, vi, result_types);
        }
    }
}

/// Produce a default (zero) value for a given WIR type.
fn default_value_for_type(ty: &WirType) -> WirInstr {
    match ty {
        WirType::I32
        | WirType::I8
        | WirType::I16
        | WirType::U8
        | WirType::U16
        | WirType::U32
        | WirType::Bool
        | WirType::Char
        | WirType::Enum { .. }
        | WirType::Flags { .. } => WirInstr::I32Const(0),
        WirType::I64 | WirType::U64 => WirInstr::I64Const(0),
        WirType::F32 => WirInstr::F32Const(0.0),
        WirType::F64 => WirInstr::F64Const(0.0),
        WirType::Ref { nullable: true, .. } => WirInstr::RefNull {
            heap_type: crate::wir::WirAbstractHeapType::None,
        },
        _ => WirInstr::I32Const(0), // fallback
    }
}

/// Rewrite call sites of SROA'd functions.
///
/// For each `LocalSet { name: T, value: Call { func_id } }` where `func_id` is SROA'd:
/// 1. Replace the `LocalSet` with `MultiValueLocalBind` that binds results to fresh locals.
/// 2. For struct candidates: replace `StructGet { field, expr: LocalGet(T) }` with `LocalGet`.
/// 3. For variant candidates: replace `RefTest` with `I32Eq` and
///    `StructGet { RefCast { LocalGet(T) } }` with `LocalGet`.
fn rewrite_call_sites(
    instrs: &mut Vec<WirInstr>,
    candidate_map: &indexmap::IndexMap<u32, &SroaCandidate>,
    types: &[WirTypeDef],
) {
    // Collect replacements: temp_name → (field_name → fresh_local_name)
    let mut replacements: indexmap::IndexMap<String, indexmap::IndexMap<String, String>> =
        indexmap::IndexMap::new();
    // Variant replacements: temp_name → VariantReplacement
    let mut variant_replacements: indexmap::IndexMap<String, VariantReplacement> =
        indexmap::IndexMap::new();

    // First pass: find call sites and prepare MultiValueLocalBind + replacement map
    let mut result = Vec::with_capacity(instrs.len());
    let mut i = 0;

    while i < instrs.len() {
        // Skip optional DeclareLocal before the LocalSet
        let set_idx = match &instrs[i] {
            WirInstr::DeclareLocal { name: dn, .. } if i + 1 < instrs.len() => {
                if is_candidate_call_set(&instrs[i + 1], dn, candidate_map) {
                    i + 1
                } else {
                    result.push(std::mem::replace(&mut instrs[i], WirInstr::Nop));
                    i += 1;
                    continue;
                }
            }
            _ => i,
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
        for (fi, field_name) in candidate.field_names.iter().enumerate() {
            let fresh = format!("__sroa_{temp_name}_{field_name}");
            field_map.insert(field_name.clone(), fresh.clone());
            // Emit DeclareLocal for the fresh local with the field's type
            result.push(WirInstr::DeclareLocal {
                name: fresh.clone(),
                ty: candidate.field_types[fi].clone(),
            });
            locals.push(Some(fresh));
        }

        if let Some(vi) = &candidate.variant_info {
            // Variant candidate: build VariantReplacement
            let disc_local = field_map["discriminant"].clone();
            let mut case_disc_values: indexmap::IndexMap<u32, i32> = indexmap::IndexMap::new();
            let mut field_to_local: indexmap::IndexMap<(u32, String), String> =
                indexmap::IndexMap::new();

            for (disc_val, case_type_opt) in vi.case_type_indices.iter().enumerate() {
                if let Some(case_type_idx) = case_type_opt {
                    case_disc_values.insert(*case_type_idx, i32::try_from(disc_val).unwrap());

                    // Look up the case struct type to map field names → sroa locals
                    if let Some(WirTypeDef::Struct(st)) = types.get(*case_type_idx as usize) {
                        for (field_pos, field) in st.fields.iter().enumerate() {
                            if field_pos == 0 {
                                // Discriminant field
                                field_to_local.insert(
                                    (*case_type_idx, field.name.clone()),
                                    disc_local.clone(),
                                );
                            } else {
                                let payload_idx = field_pos - 1;
                                let payload_name = format!("payload_{payload_idx}");
                                if let Some(sroa_local) = field_map.get(&payload_name) {
                                    field_to_local.insert(
                                        (*case_type_idx, field.name.clone()),
                                        sroa_local.clone(),
                                    );
                                }
                            }
                        }
                    }
                }
            }

            variant_replacements.insert(
                temp_name,
                VariantReplacement {
                    disc_local,
                    case_disc_values,
                    field_to_local,
                },
            );
        } else {
            // Struct candidate: use existing field_map
            replacements.insert(temp_name, field_map);
        }

        // Extract the Call instruction (and any prefix statements from block wrappers)
        let (prefix_instrs, call_instr) = take_call_from_local_set(&mut instrs[set_idx]);
        // Emit prefix instructions (e.g. local initialization from inlined blocks)
        result.extend(prefix_instrs);
        result.push(WirInstr::MultiValueLocalBind {
            instr: call_instr,
            locals,
        });

        i = set_idx + 1;
    }

    *instrs = result;

    if replacements.is_empty() && variant_replacements.is_empty() {
        // Recurse into nested blocks even if no replacements at this level
        for instr in instrs.iter_mut() {
            recurse_rewrite_call_sites(instr, candidate_map, types);
        }
        return;
    }

    // Second pass: replace struct and variant access patterns
    if !replacements.is_empty() {
        for instr in instrs.iter_mut() {
            replace_struct_gets(instr, &replacements);
        }
    }
    if !variant_replacements.is_empty() {
        for instr in instrs.iter_mut() {
            replace_variant_accesses(instr, &variant_replacements);
        }
    }

    // Recurse into nested blocks
    for instr in instrs.iter_mut() {
        recurse_rewrite_call_sites(instr, candidate_map, types);
    }
}

/// Recurse into nested instruction bodies for call site rewriting.
fn recurse_rewrite_call_sites(
    instr: &mut WirInstr,
    candidate_map: &indexmap::IndexMap<u32, &SroaCandidate>,
    types: &[WirTypeDef],
) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            rewrite_call_sites(body, candidate_map, types);
        }
        WirInstr::If {
            then_body,
            else_body,
            ..
        } => {
            rewrite_call_sites(then_body, candidate_map, types);
            if let Some(eb) = else_body {
                rewrite_call_sites(eb, candidate_map, types);
            }
        }
        WirInstr::Seq(body) => {
            rewrite_call_sites(body, candidate_map, types);
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

/// Replace variant access patterns with scalar local accesses for variant SROA'd temps.
///
/// Handles three patterns:
/// 1. `RefTest { type_id, expr: LocalGet(temp) }` → `I32Eq(LocalGet(disc), I32Const(case_disc))`
/// 2. `StructGet { field, expr: RefCast { type_id, expr: LocalGet(temp) } }` → `LocalGet(sroa_local)`
/// 3. `ValueCopy { StructGet { field, expr: RefCast { type_id, expr: LocalGet(temp) } } }` → same
fn replace_variant_accesses(
    instr: &mut WirInstr,
    variant_replacements: &indexmap::IndexMap<String, VariantReplacement>,
) {
    // Pattern 3: ValueCopy wrapping StructGet(RefCast(LocalGet(temp)))
    if let WirInstr::ValueCopy { expr, .. } = instr
        && let WirInstr::StructGet {
            field_name,
            expr: sg_expr,
            ..
        } = expr.as_ref()
        && let WirInstr::RefCast {
            type_id: cast_type_id,
            expr: rc_expr,
            ..
        } = sg_expr.as_ref()
        && let WirInstr::LocalGet { name: temp_name } = rc_expr.as_ref()
        && let Some(vr) = variant_replacements.get(temp_name.as_str())
    {
        let key = (cast_type_id.index(), field_name.clone());
        if let Some(local_name) = vr.field_to_local.get(&key) {
            *instr = WirInstr::LocalGet {
                name: local_name.clone(),
            };
            return;
        }
    }

    // Pattern 1: RefTest { type_id, expr: LocalGet(temp) }
    if let WirInstr::RefTest { type_id, expr, .. } = instr
        && let WirInstr::LocalGet { name: temp_name } = expr.as_ref()
        && let Some(vr) = variant_replacements.get(temp_name.as_str())
        && let Some(&disc_val) = vr.case_disc_values.get(&type_id.index())
    {
        *instr = WirInstr::I32Eq(
            Box::new(WirInstr::LocalGet {
                name: vr.disc_local.clone(),
            }),
            Box::new(WirInstr::I32Const(disc_val)),
        );
        return;
    }

    // Pattern 2: StructGet { field, expr: RefCast { type_id, expr: LocalGet(temp) } }
    if let WirInstr::StructGet {
        field_name,
        expr: sg_expr,
        ..
    } = instr
        && let WirInstr::RefCast {
            type_id: cast_type_id,
            expr: rc_expr,
            ..
        } = sg_expr.as_ref()
        && let WirInstr::LocalGet { name: temp_name } = rc_expr.as_ref()
        && let Some(vr) = variant_replacements.get(temp_name.as_str())
    {
        let key = (cast_type_id.index(), field_name.clone());
        if let Some(local_name) = vr.field_to_local.get(&key) {
            *instr = WirInstr::LocalGet {
                name: local_name.clone(),
            };
            return;
        }
    }

    // Recurse into children
    instr.for_each_boxed_child_mut(&mut |child| {
        replace_variant_accesses(child, variant_replacements);
    });
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
    (prefix, Box::new(call))
}

/// Recursively unwrap `ValueCopy` and `Block` wrappers to extract the `Call` instruction.
/// Collects any non-result instructions from blocks into `prefix` so they can be
/// emitted before the call.
#[allow(clippy::boxed_local)]
fn unwrap_and_take_call(mut instr: Box<WirInstr>, prefix: &mut Vec<WirInstr>) -> WirInstr {
    loop {
        match *instr {
            WirInstr::Call { .. } => return *instr,
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

/// Box<T> parameter SROA.
///
/// Rewrites internal functions that take `ref null Box<T>` parameters (single-field
/// wrapper structs) to take the inner scalar `T` directly. At call sites, the
/// `StructNew { value: expr }` allocation is replaced with just `expr`.
///
/// A parameter is eligible when:
/// - Its type is `Ref { nullable: true }` to a struct with `generic_origin.base_name == "Box"`
/// - The struct has exactly one field named "value"
/// - Within the function body, the parameter is only used via:
///   a. `StructGet { field_name: "value", expr: LocalGet(param) }` — scalar read
///   b. As an argument to another function at a position that is also being SROA'd
/// - The function is not exported, not in an element table, and not `RefFunc`'d
fn sroa_box_parameters(module: &mut WirModule) {
    let import_func_count = module
        .imports
        .iter()
        .filter(|i| matches!(i.desc, WirImportDesc::Func { .. }))
        .count() as u32;

    let pinned = collect_pinned_func_ids(module);

    // Phase 1: identify candidate (func_id, param_index) pairs.
    let mut candidates: IndexSet<(u32, usize)> = IndexSet::new();
    // Map from (func_id, param_idx) → (box_type_id_index, inner WirType).
    let mut candidate_info: indexmap::IndexMap<(u32, usize), (u32, WirType)> =
        indexmap::IndexMap::new();

    for (i, func) in module.functions.iter().enumerate() {
        let func_id_index = import_func_count + u32::try_from(i).unwrap();
        if pinned.contains(&func_id_index) || func.body.is_none() {
            continue;
        }
        let type_idx = func.type_id.index();
        let Some(WirTypeDef::Func(func_type)) = module.types.get(type_idx as usize) else {
            continue;
        };
        for (pi, param_ty) in func_type.params.iter().enumerate() {
            let WirType::Ref {
                type_id: box_type_id,
                nullable: true,
            } = param_ty
            else {
                continue;
            };
            let box_type_idx = box_type_id.index();
            let Some(WirTypeDef::Struct(st)) = module.types.get(box_type_idx as usize) else {
                continue;
            };
            let Some(ref origin) = st.generic_origin else {
                continue;
            };
            if origin.base_name != "Box" || st.fields.len() != 1 || st.fields[0].name != "value" {
                continue;
            }
            candidates.insert((func_id_index, pi));
            candidate_info.insert(
                (func_id_index, pi),
                (box_type_idx, st.fields[0].ty.clone()),
            );
        }
    }

    if candidates.is_empty() {
        return;
    }

    // Phase 2: validate uses — eliminate candidates whose parameter escapes.
    loop {
        let mut invalid: IndexSet<(u32, usize)> = IndexSet::new();

        for &(func_id_index, param_idx) in &candidates {
            let func_array_idx = (func_id_index - import_func_count) as usize;
            let func = &module.functions[func_array_idx];
            let param_name = &func.param_names[param_idx];
            let body = func.body.as_ref().unwrap();
            if !box_param_uses_valid(body, param_name, func_id_index, param_idx, &candidates) {
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
        let func_array_idx = (func_id_index - import_func_count) as usize;
        let param_name = module.functions[func_array_idx].param_names[param_idx].clone();
        let (_, ref inner_ty) = candidate_info[&(func_id_index, param_idx)];
        let inner_ty = inner_ty.clone();

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

        // Rewrite body: replace StructGet(LocalGet(param), "value") → LocalGet(param).
        if let Some(body) = &mut module.functions[func_array_idx].body {
            for instr in body.iter_mut() {
                rewrite_box_param_reads(instr, &param_name);
            }
        }
    }

    // Step B: rewrite call sites in ALL function bodies.
    // Build lookup: func_id_index → set of SROA'd param indices.
    let mut sroa_params: indexmap::IndexMap<u32, IndexSet<usize>> = indexmap::IndexMap::new();
    for &(func_id_index, param_idx) in &candidates {
        sroa_params
            .entry(func_id_index)
            .or_default()
            .insert(param_idx);
    }

    // Build (func_id, param_idx) → WirTypeId of the Box struct type.
    let mut param_box_type_id: indexmap::IndexMap<(u32, usize), WirTypeId> =
        indexmap::IndexMap::new();
    for &(func_id_index, param_idx) in &candidates {
        let (box_type_idx, _) = &candidate_info[&(func_id_index, param_idx)];
        if let Some(WirTypeDef::Struct(st)) = module.types.get(*box_type_idx as usize) {
            let type_id = WirTypeId::new(*box_type_idx, st.name.fq.as_str().into());
            param_box_type_id.insert((func_id_index, param_idx), type_id);
        }
    }

    // Build per-function set of param names that are already scalar (SROA'd).
    let mut func_scalar_params: indexmap::IndexMap<u32, IndexSet<String>> =
        indexmap::IndexMap::new();
    for &(func_id_index, param_idx) in &candidates {
        let func_array_idx = (func_id_index - import_func_count) as usize;
        let param_name = module.functions[func_array_idx].param_names[param_idx].clone();
        func_scalar_params
            .entry(func_id_index)
            .or_default()
            .insert(param_name);
    }

    for (i, func) in module.functions.iter_mut().enumerate() {
        let func_id_index = import_func_count + u32::try_from(i).unwrap();
        let scalar_params = func_scalar_params
            .get(&func_id_index)
            .cloned()
            .unwrap_or_default();
        if let Some(body) = &mut func.body {
            // Rewrite call arguments at SROA'd positions.
            for instr in body.iter_mut() {
                rewrite_box_args_at_call_sites(
                    instr,
                    &sroa_params,
                    &param_box_type_id,
                    &scalar_params,
                );
            }
        }
    }
}

/// Check that every use of `param_name` in the body is a valid Box SROA use.
fn box_param_uses_valid(
    instrs: &[WirInstr],
    param_name: &str,
    _self_func_id: u32,
    _self_param_idx: usize,
    candidates: &IndexSet<(u32, usize)>,
) -> bool {
    for instr in instrs {
        if !box_param_use_valid_instr(instr, param_name, candidates, BoxCheckCtx::None) {
            return false;
        }
    }
    true
}

/// Context for tracking where we encounter a `LocalGet(param_name)`.
#[derive(Clone, Copy)]
enum BoxCheckCtx {
    None,
    /// Inside a `StructGet { field_name: "value" }` — `LocalGet` is valid.
    InsideStructGetValue,
}

/// Recursively check that all uses of `param_name` are valid Box SROA patterns.
fn box_param_use_valid_instr(
    instr: &WirInstr,
    param_name: &str,
    candidates: &IndexSet<(u32, usize)>,
    ctx: BoxCheckCtx,
) -> bool {
    match instr {
        // LocalGet of the param: only valid inside StructGet("value") or as call arg
        WirInstr::LocalGet { name } if name == param_name => {
            matches!(ctx, BoxCheckCtx::InsideStructGetValue)
        }
        // StructGet with field "value": mark context and check inner
        WirInstr::StructGet {
            field_name, expr, ..
        } if field_name == "value" => box_param_use_valid_instr(
            expr,
            param_name,
            candidates,
            BoxCheckCtx::InsideStructGetValue,
        ),
        // Call: a direct LocalGet(param) as a call arg is valid only at SROA'd positions.
        // Non-LocalGet args (e.g., StructGet("value", LocalGet(param))) are validated recursively.
        WirInstr::Call { func_id, args } => {
            for (ai, arg) in args.iter().enumerate() {
                if let WirInstr::LocalGet { name } = arg
                    && name == param_name
                {
                    // Bare param reference at a call position — only valid if SROA'd.
                    if !candidates.contains(&(func_id.index(), ai)) {
                        return false;
                    }
                } else if !box_param_use_valid_instr(
                    arg,
                    param_name,
                    candidates,
                    BoxCheckCtx::None,
                ) {
                    return false;
                }
            }
            true
        }
        // Recurse into nested blocks
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            for child in body {
                if !box_param_use_valid_instr(child, param_name, candidates, BoxCheckCtx::None) {
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
            if !box_param_use_valid_instr(condition, param_name, candidates, BoxCheckCtx::None) {
                return false;
            }
            for child in then_body {
                if !box_param_use_valid_instr(child, param_name, candidates, BoxCheckCtx::None) {
                    return false;
                }
            }
            if let Some(eb) = else_body {
                for child in eb {
                    if !box_param_use_valid_instr(
                        child,
                        param_name,
                        candidates,
                        BoxCheckCtx::None,
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
                    && !box_param_use_valid_instr(child, param_name, candidates, BoxCheckCtx::None)
                {
                    ok = false;
                }
            });
            ok
        }
    }
}

/// Rewrite `StructGet { field: "value", expr: LocalGet(param) }` → `LocalGet(param)`
/// within a function body after SROA (param is now scalar).
fn rewrite_box_param_reads(instr: &mut WirInstr, param_name: &str) {
    // Check StructGet(LocalGet(param), "value") pattern
    if let WirInstr::StructGet {
        field_name,
        expr,
        ..
    } = instr
        && field_name == "value"
        && matches!(expr.as_ref(), WirInstr::LocalGet { name } if name == param_name)
    {
        *instr = WirInstr::LocalGet {
            name: param_name.to_string(),
        };
        return;
    }

    // Recurse
    instr.for_each_boxed_child_mut(&mut |child| rewrite_box_param_reads(child, param_name));
}


/// Rewrite arguments at SROA'd Call positions to pass the scalar value instead of a Box ref.
///
/// - `StructNew(Box<T>, [val])` → `val` (unwrap allocation, eliminate heap alloc)
/// - `LocalGet(x)` where x is already scalar (SROA'd param) → leave as-is
/// - Other expressions → `StructGet(expr, "value")` (extract scalar from existing Box ref)
fn rewrite_box_args_at_call_sites(
    instr: &mut WirInstr,
    sroa_params: &indexmap::IndexMap<u32, IndexSet<usize>>,
    param_box_type_id: &indexmap::IndexMap<(u32, usize), WirTypeId>,
    scalar_params: &IndexSet<String>,
) {
    // Recurse first (bottom-up)
    instr.for_each_boxed_child_mut(&mut |child| {
        rewrite_box_args_at_call_sites(child, sroa_params, param_box_type_id, scalar_params);
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
            // Unwrap StructNew: skip Box allocation entirely.
            let inner = std::mem::replace(&mut fields[0], WirInstr::Nop);
            *arg = inner;
        } else if let WirInstr::LocalGet { name } = arg
            && scalar_params.contains(name.as_str())
        {
            // Already scalar (this function's own SROA'd param) — no change needed.
        } else {
            // The argument is an existing Box reference (e.g., from a local variable).
            // Extract the scalar value via StructGet("value").
            let Some(box_type_id) = param_box_type_id.get(&(func_id_idx, pi)) else {
                continue;
            };
            let old_arg = std::mem::replace(arg, WirInstr::Nop);
            *arg = WirInstr::StructGet {
                type_id: box_type_id.clone(),
                field_name: "value".to_string(),
                expr: Box::new(old_arg),
            };
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
    elide_redundant_value_copies(instrs);
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

/// Remove unnecessary `ValueCopy` wrappers from `LocalSet` instructions.
///
/// A `ValueCopy` deep-copies a GC value to maintain Wado's value semantics.
/// This copy is unnecessary when the source expression provably produces a
/// **fresh** value — one with no pre-existing aliases. Fresh values include
/// GC constructors (`StructNew`, `ArrayNew*`), function call return values,
/// and block expressions whose result is itself fresh.
fn elide_redundant_value_copies(instrs: &mut [WirInstr]) {
    for instr in instrs.iter_mut() {
        let WirInstr::LocalSet { value, .. } = instr else {
            continue;
        };
        let is_fresh = if let WirInstr::ValueCopy { expr, .. } = value.as_ref() {
            is_fresh_wir_value(expr)
        } else {
            false
        };
        if is_fresh {
            let old = std::mem::replace(value.as_mut(), WirInstr::Nop);
            if let WirInstr::ValueCopy { expr, .. } = old {
                *value = expr;
            }
        }
    }
}

/// Check if a WIR instruction provably produces a fresh value (no aliases).
fn is_fresh_wir_value(instr: &WirInstr) -> bool {
    match instr {
        // GC constructors create fresh values.
        WirInstr::StructNew { .. }
        | WirInstr::ArrayNew { .. }
        | WirInstr::ArrayNewDefault { .. }
        | WirInstr::ArrayNewData { .. }
        | WirInstr::ArrayNewFixed { .. } => true,

        // Function calls return fresh values (callee constructs them).
        WirInstr::Call { .. } | WirInstr::CallRef { .. } => true,

        // RefAsNonNull wrapping a fresh value is still fresh.
        WirInstr::RefAsNonNull(inner) => is_fresh_wir_value(inner),

        // Block with a result: trace through to find the result value.
        WirInstr::Block {
            body,
            result: Some(_),
            ..
        } => block_result_is_fresh(body),

        // Seq: check the effective result instruction.
        WirInstr::Seq(body) => find_seq_result(body).is_some_and(is_fresh_wir_value),

        _ => false,
    }
}

/// Determine if a block's result value is fresh.
///
/// The block's result is produced by either:
/// - A trailing `Seq([..., result_expr, Br(0)])` — the instruction before `Br`
/// - A trailing `Seq([..., LocalGet(x), Br(0)])` — trace back to find `x`'s value
/// - The last instruction in the body (fall-through)
fn block_result_is_fresh(body: &[WirInstr]) -> bool {
    let result_instr = match body.last() {
        Some(WirInstr::Seq(seq)) => find_seq_result(seq),
        Some(instr) => Some(instr),
        None => return false,
    };
    match result_instr {
        Some(WirInstr::LocalGet { name }) => {
            // Trace back: find what this local was set to within the block.
            find_local_set_value(body, name).is_some_and(is_fresh_wir_value)
        }
        Some(instr) => is_fresh_wir_value(instr),
        None => false,
    }
}

/// Find the effective result instruction of a `Seq`.
///
/// In a `Seq` ending with `Br`, the result is the instruction just before `Br`
/// (it leaves a value on the stack that `Br` carries to the enclosing block).
/// Otherwise, the last instruction is the result.
fn find_seq_result(body: &[WirInstr]) -> Option<&WirInstr> {
    if body.len() >= 2 && matches!(body.last(), Some(WirInstr::Br { .. })) {
        Some(&body[body.len() - 2])
    } else {
        body.last()
    }
}

/// Scan backward through a block body to find the value assigned to a local.
fn find_local_set_value<'a>(body: &'a [WirInstr], target: &str) -> Option<&'a WirInstr> {
    for instr in body.iter().rev() {
        if let WirInstr::LocalSet { name, value } = instr
            && name == target
        {
            return Some(value.as_ref());
        }
    }
    None
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
            bytes.push(v.cast_unsigned() as u8);
        }
        // 2-byte types: i16, u16 (stored as I32Const in WIR)
        (WirType::I16 | WirType::U16, WirInstr::I32Const(v)) => {
            bytes.extend_from_slice(&(v.cast_unsigned() as u16).to_le_bytes());
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
            // Check if the next instructions are matching append calls.
            // Each append may be a single instruction (Block wrapping LocalSet+Call)
            // or multiple flat instructions (LocalSet* + Call) from block flattening.
            if n > 0
                && let Some((values, consumed)) =
                    try_match_append_sequence(&body[i + 1..], n, &init_info, append_func_indices)
            {
                // Rewrite: replace ArrayNewDefault with ArrayNewFixed in the init.
                rewrite_init_to_fixed(&mut body[i], &init_info, values);
                // Remove the consumed append instructions.
                body.drain(i + 1..i + 1 + consumed);
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

/// Try to match N append operations starting from the given instruction slice.
/// Each append may be either:
/// - A single `Block` instruction wrapping `LocalSet* + Call` (from inlined labeled blocks)
/// - A sequence of flat `LocalSet* + Call` instructions (from flattened blocks)
///
/// Returns the extracted element values and the total number of instructions consumed,
/// or `None` if the pattern doesn't match.
fn try_match_append_sequence(
    instrs: &[WirInstr],
    expected_count: usize,
    init_info: &ArrayInitInfo,
    append_func_indices: &IndexSet<u32>,
) -> Option<(Vec<WirInstr>, usize)> {
    let mut values = Vec::with_capacity(expected_count);
    let mut consumed = 0;

    while values.len() < expected_count && consumed < instrs.len() {
        let instr = &instrs[consumed];

        // Try pattern 1: Block wrapping LocalSet* + Call (from inlined labeled blocks)
        let (call, aliases, value_bindings) = extract_call_from_block(instr);
        if let WirInstr::Call { func_id, args } = call
            && append_func_indices.contains(&func_id.index())
            && args.len() == 2
            && receiver_matches_with_aliases(&args[0], init_info, &aliases)
        {
            let element = resolve_value_binding(&args[1], &value_bindings, &aliases);
            values.push(element);
            consumed += 1;
            continue;
        }

        // Try pattern 2: Flat LocalSet* + Call sequence (from flattened blocks)
        // Collect leading LocalSet instructions, then expect a matching Call.
        let mut flat_aliases = Vec::new();
        let mut flat_value_bindings = Vec::new();
        let mut j = consumed;
        while j < instrs.len() {
            if let WirInstr::LocalSet { name, value } = &instrs[j] {
                if let WirInstr::LocalGet { name: src_name } = value.as_ref() {
                    flat_aliases.push((name.clone(), src_name.clone()));
                } else {
                    flat_value_bindings.push((name.clone(), *value.clone()));
                }
                j += 1;
            } else {
                break;
            }
        }
        // We must have consumed at least one LocalSet (otherwise pattern 1 would match)
        // and the next instruction must be a matching Call.
        if j > consumed
            && j < instrs.len()
            && let WirInstr::Call { func_id, args } = &instrs[j]
            && append_func_indices.contains(&func_id.index())
            && args.len() == 2
            && receiver_matches_with_aliases(&args[0], init_info, &flat_aliases)
        {
            let element = resolve_value_binding(&args[1], &flat_value_bindings, &flat_aliases);
            values.push(element);
            consumed = j + 1;
            continue;
        }

        // Neither pattern matched
        return None;
    }

    if values.len() == expected_count {
        Some((values, consumed))
    } else {
        None
    }
}

/// Resolve a value through value bindings and aliases from the enclosing block.
/// If the instruction is a `LocalGet` that refers to a value binding, return the
/// bound value. If it refers to an alias, resolve through the alias chain.
/// Otherwise return a clone of the instruction as-is.
fn resolve_value_binding(
    instr: &WirInstr,
    value_bindings: &[(String, WirInstr)],
    aliases: &[(String, String)],
) -> WirInstr {
    if let WirInstr::LocalGet { name } = instr {
        for (binding_name, binding_value) in value_bindings {
            if binding_name == name {
                return binding_value.clone();
            }
        }
        // Also resolve through aliases (e.g., inlined parameter that aliases a caller local)
        let resolved = resolve_alias(name, aliases);
        if resolved != name {
            return WirInstr::LocalGet {
                name: resolved.to_string(),
            };
        }
    }
    instr.clone()
}

/// Extract a Call instruction from inside a Block, along with local aliases
/// and value bindings from preceding `LocalSet` instructions.
///
/// After inlining, a `push_literal` call often expands to:
/// ```text
/// Block { body: [
///   LocalSet { name: "__local_7", value: LocalGet { name: "__local_0" } },
///   Call { func_id: append, args: [LocalGet { name: "__local_7" }, value] }
/// ] }
/// ```
///
/// For non-scalar elements (e.g., `String`), the element value is materialized
/// in a separate `LocalSet`:
/// ```text
/// Block { body: [
///   LocalSet { name: "__local_4", value: LocalGet { name: "__local_0" } },
///   LocalSet { name: "__local_5", value: StructNew { String, ... } },
///   Call { func_id: append, args: [LocalGet("__local_4"), LocalGet("__local_5")] }
/// ] }
/// ```
///
/// Returns the Call instruction, a list of (`alias_name`, `original_name`) pairs,
/// and a list of (`binding_name`, `value_expr`) pairs for non-alias bindings.
fn extract_call_from_block(
    instr: &WirInstr,
) -> (&WirInstr, Vec<(String, String)>, Vec<(String, WirInstr)>) {
    // Accept both Block (from inlined labeled blocks) and Seq (from flattened blocks).
    let body = match instr {
        WirInstr::Block {
            body, result: None, ..
        }
        | WirInstr::Seq(body) => body,
        _ => return (instr, Vec::new(), Vec::new()),
    };

    if body.is_empty() {
        return (instr, Vec::new(), Vec::new());
    }

    // The last instruction should be the Call.
    let call = body.last().unwrap();

    // Preceding instructions should be LocalSet: either aliases (LocalGet) or value bindings.
    let mut aliases = Vec::new();
    let mut value_bindings = Vec::new();
    for preceding in &body[..body.len() - 1] {
        if let WirInstr::LocalSet { name, value } = preceding {
            if let WirInstr::LocalGet { name: src_name } = value.as_ref() {
                aliases.push((name.clone(), src_name.clone()));
            } else {
                value_bindings.push((name.clone(), *value.clone()));
            }
        } else {
            // Non-LocalSet instruction before the call — bail out.
            return (instr, Vec::new(), Vec::new());
        }
    }

    (call, aliases, value_bindings)
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

/// Rewrite `String::append(buf, "short_constant")` calls into sequences of
/// `String::append_char(buf, ch)` calls when the constant string is ≤8 bytes.
///
/// This eliminates GC allocations for the temporary `String` struct and its
/// backing `array<u8>` that are created for each short constant string argument.
///
/// Pattern matched (WIR):
/// ```text
/// Call { func_id: <string_append>,
///   args: [receiver, StructNew { String,
///     fields: [RefAsNonNull(ArrayNewData { data_index, offset: 0, len: L }), I32Const(L)]
///   }]
/// }
/// ```
/// Rewritten to:
/// ```text
/// Call { func_id: <string_append_char>, args: [receiver, I32Const(byte0)] }
/// Call { func_id: <string_append_char>, args: [receiver, I32Const(byte1)] }
/// ...
/// ```
fn simplify_short_string_appends(module: &mut WirModule) {
    let import_func_count = module
        .imports
        .iter()
        .filter(|i| matches!(i.desc, WirImportDesc::Func { .. }))
        .count() as u32;

    // Find string_append and string_append_char function indices.
    let mut append_func_id: Option<WirFuncId> = None;
    let mut append_char_func_id: Option<WirFuncId> = None;

    for (i, f) in module.functions.iter().enumerate() {
        let idx = import_func_count + u32::try_from(i).unwrap();
        if f.comp_features & COMP_FEATURE_STRING_APPEND != 0 {
            append_func_id = Some(WirFuncId::new(idx, f.name.fq.as_str().into()));
        }
        if f.comp_features & COMP_FEATURE_STRING_APPEND_CHAR != 0 {
            append_char_func_id = Some(WirFuncId::new(idx, f.name.fq.as_str().into()));
        }
    }

    let (Some(append_id), Some(append_char_id)) = (append_func_id, append_char_func_id) else {
        return;
    };

    let data = &module.data;
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            simplify_short_appends_in_body(body, &append_id, &append_char_id, data);
        }
    }
}

/// Recursively process instructions, looking for Call patterns to rewrite.
fn simplify_short_appends_in_instr(
    instr: &mut WirInstr,
    append_id: &WirFuncId,
    append_char_id: &WirFuncId,
    data: &[WirData],
) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } | WirInstr::Seq(body) => {
            simplify_short_appends_in_body(body, append_id, append_char_id, data);
            return;
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            simplify_short_appends_in_instr(condition, append_id, append_char_id, data);
            simplify_short_appends_in_body(then_body, append_id, append_char_id, data);
            if let Some(eb) = else_body {
                simplify_short_appends_in_body(eb, append_id, append_char_id, data);
            }
            return;
        }
        _ => {}
    }

    instr.for_each_boxed_child_mut(&mut |child| {
        simplify_short_appends_in_instr(child, append_id, append_char_id, data);
    });
}

/// Process a flat instruction body, replacing short-constant `String::append` calls.
fn simplify_short_appends_in_body(
    body: &mut Vec<WirInstr>,
    append_id: &WirFuncId,
    append_char_id: &WirFuncId,
    data: &[WirData],
) {
    // First recurse into children.
    for instr in body.iter_mut() {
        simplify_short_appends_in_instr(instr, append_id, append_char_id, data);
    }

    // Scan for String::append calls with short constant string args.
    let mut i = 0;
    while i < body.len() {
        if let Some(replacements) =
            try_rewrite_short_string_append(&body[i], append_id, append_char_id, data)
        {
            let n = replacements.len();
            body.splice(i..=i, replacements);
            i += n;
        } else {
            i += 1;
        }
    }
}

/// Maximum byte length for short-constant string append optimization.
const MAX_SHORT_STRING_APPEND_LEN: usize = 8;

/// Try to match and rewrite a single `String::append(buf, "short")` call.
/// Returns `None` if the instruction doesn't match the pattern.
/// Returns `Some(vec![append_char calls])` on success.
fn try_rewrite_short_string_append(
    instr: &WirInstr,
    append_id: &WirFuncId,
    append_char_id: &WirFuncId,
    data: &[WirData],
) -> Option<Vec<WirInstr>> {
    // Match: Call { func_id: string_append, args: [receiver, string_arg] }
    // Also match: Block { body: [Call { ... }] } (optimizer sometimes wraps in blocks)
    let call = match instr {
        WirInstr::Call { .. } => instr,
        WirInstr::Block { body, .. } | WirInstr::Seq(body) if body.len() == 1 => &body[0],
        _ => return None,
    };

    let WirInstr::Call { func_id, args } = call else {
        return None;
    };

    if func_id != append_id || args.len() != 2 {
        return None;
    }

    // Match the second arg: StructNew { fields: [RefAsNonNull(ArrayNewData { ... }), I32Const(len)] }
    let WirInstr::StructNew { fields, .. } = &args[1] else {
        return None;
    };

    if fields.len() != 2 {
        return None;
    }

    // Extract ArrayNewData from RefAsNonNull wrapper.
    let array_new_data = match &fields[0] {
        WirInstr::RefAsNonNull(inner) => inner.as_ref(),
        other => other,
    };

    let WirInstr::ArrayNewData {
        data_index,
        offset,
        len,
        ..
    } = array_new_data
    else {
        return None;
    };

    // Verify offset is 0 and len is a small constant.
    let WirInstr::I32Const(0) = offset.as_ref() else {
        return None;
    };
    let WirInstr::I32Const(str_len_i32) = len.as_ref() else {
        return None;
    };
    let str_len = usize::try_from(*str_len_i32).ok()?;

    if str_len == 0 || str_len > MAX_SHORT_STRING_APPEND_LEN {
        return None;
    }

    // Verify the used field matches.
    let WirInstr::I32Const(used) = &fields[1] else {
        return None;
    };
    if *used != *str_len_i32 {
        return None;
    }

    // Get the actual bytes from the data segment.
    let seg = data.get(*data_index as usize)?;
    if seg.bytes.len() < str_len {
        return None;
    }
    let bytes = &seg.bytes[..str_len];

    // Clone the receiver expression for each append_char call.
    let receiver = &args[0];

    let mut replacements = Vec::with_capacity(str_len);
    for &byte in bytes {
        replacements.push(WirInstr::Call {
            func_id: append_char_id.clone(),
            args: vec![receiver.clone(), WirInstr::I32Const(i32::from(byte))],
        });
    }

    Some(replacements)
}
