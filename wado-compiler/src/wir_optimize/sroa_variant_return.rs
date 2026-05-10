//! Variant return SROA (Scalar Replacement of Aggregates) for WIR.
//!
//! Rewrites internal functions that return a variant lowered as
//! `(i32 disc, payload_0, payload_1, ...)` into Wasm multi-value returns,
//! eliminating GC struct allocation at function boundaries for the
//! variant-case ref.
//!
//! Tuple and user-struct return ABIs were lifted to a TIR-level
//! classification (`optimize::multi_value_return`); this pass handles only
//! the variant case, whose layout (shared-vs-per-case payload offsets) is
//! WIR-specific.

use crate::compiler_trace;
use crate::hashmap::IndexSet;
use crate::wir::{
    WirFuncType, WirInstr, WirPackage, WirType, WirTypeDef, WirTypeId, WirVariantType,
};

use super::util::collect_pinned_func_ids;

/// Variant-return SROA (Scalar Replacement of Aggregates).
///
/// Rewrites internal functions that return a variant into functions that
/// return `[i32 disc, payload_0, payload_1, ...]` directly (Wasm
/// multi-value return). At call sites, the struct allocation + field
/// extraction is replaced with `MultiValueLocalBind` of the discriminant
/// + payload fields.
///
/// A function is eligible when:
/// - It is not exported, not in an element table, and not referenced by
///   `RefFunc`.
/// - Its single return type is a non-nullable `Ref` to a
///   `WirTypeDef::Variant` whose total result-vector arity (1 disc + max
///   payload count) is 2-4.
/// - Every `Return` in the body wraps a `StructNew` of one of the
///   variant's case types.
/// - Every call site stores the result into a temp and reads only via
///   `StructGet`.
pub(super) fn sroa_variant_returns(module: &mut WirPackage) {
    // Collect pinned func_ids (exported, in element tables, or RefFunc'd).
    let pinned = collect_pinned_func_ids(module);

    // Phase 1: identify candidate functions.
    let candidates = find_sroa_candidates(module, &pinned);
    compiler_trace!("sroa_variant_return", "candidates = {}", candidates.len());
    if candidates.is_empty() {
        return;
    }

    // Phase 2: validate call sites across all function bodies.
    let confirmed = validate_call_sites(module, &candidates);
    compiler_trace!("sroa_variant_return", "confirmed = {}", confirmed.len());
    if confirmed.is_empty() {
        return;
    }

    // Phase 3: rewrite confirmed functions and their call sites.
    apply_sroa(module, &confirmed);
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
    /// Per-case payload slot offsets in the multi-value result.
    /// `case_slot_offsets[case_discriminant]` is the starting index (0-based)
    /// within the payload portion (after the discriminant) for that case's payloads.
    /// `None` means this case uses shared (homogeneous) layout.
    case_slot_offsets: Option<Vec<usize>>,
}

/// Replacement info for a variant SROA'd temp local at call sites.
struct VariantReplacement {
    /// Local name holding the discriminant value.
    disc_local: String,
    /// `case_wir_type_idx` → discriminant value (i32).
    case_disc_values: crate::hashmap::IndexMap<u32, i32>,
    /// `(case_wir_type_idx, field_name_in_case_struct)` → sroa local name.
    field_to_local: crate::hashmap::IndexMap<(u32, String), String>,
    /// SROA locals that hold ref types (need `ref.as_non_null` when read).
    ref_locals: crate::hashmap::IndexSet<String>,
}

/// Returns true if a `WirType` is a valid Wasm value type for multi-value returns.
///
/// Primitive scalars (i32, i64, f32, f64) are always eligible.
/// Concrete GC refs (`ref $T`, `ref null $T`) are also eligible: Wasm multi-value
/// returns support any value type, including GC refs. This allows SROA of structs
/// with GC ref fields, such as tuples containing String values.
/// Abstract heap refs (`ref null struct`, etc.) are excluded as they lack
/// the precise type information needed for `StructGet` replacement.
pub(super) fn is_eligible_field_type(ty: &WirType) -> bool {
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

/// Phase 1: find functions eligible for SROA.
fn find_sroa_candidates(module: &WirPackage, pinned: &IndexSet<u32>) -> Vec<(u32, SroaCandidate)> {
    let mut candidates = Vec::new();

    for (i, func) in module.functions.iter().enumerate() {
        let func_id_index = crate::wir_build::DEFINED_FUNC_BASE + u32::try_from(i).unwrap();

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

        if let Some(WirTypeDef::Variant(variant_type)) = module.types.get(ret_type_idx as usize)
            && let Some(candidate) =
                try_variant_sroa_candidate(module, i, ret_type_idx, variant_type, body)
        {
            candidates.push((func_id_index, candidate));
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
    module: &WirPackage,
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
    let mut case_idx_to_type_idx: crate::hashmap::IndexMap<u32, u32> =
        crate::hashmap::IndexMap::default();
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

    // Compute the payload types: try shared layout first, fall back to per-case slots.
    let mut homogeneous = true;
    for pos in 0..max_payload_count {
        let mut found: Option<&WirType> = None;
        for case in &variant_type.cases {
            if let Some(ty) = case.payload.get(pos) {
                if let Some(existing) = found {
                    if !wir_types_equal(existing, ty) {
                        homogeneous = false;
                        break;
                    }
                } else {
                    found = Some(ty);
                }
            }
        }
        if !homogeneous {
            break;
        }
    }

    let (field_types, field_names, field_count, case_slot_offsets) = if homogeneous {
        // Shared layout: all cases use the same type at each position
        let mut payload_types: Vec<WirType> = Vec::with_capacity(max_payload_count);
        for pos in 0..max_payload_count {
            let mut found: Option<&WirType> = None;
            for case in &variant_type.cases {
                if let Some(ty) = case.payload.get(pos) {
                    found = Some(ty);
                    break;
                }
            }
            payload_types.push(found?.clone());
        }
        let fc = 1 + max_payload_count;
        let mut ft = Vec::with_capacity(fc);
        ft.push(WirType::I32);
        ft.extend(payload_types.into_iter().map(WirType::as_nullable));
        let mut fn_ = Vec::with_capacity(fc);
        fn_.push("discriminant".to_string());
        for pos in 0..max_payload_count {
            fn_.push(format!("payload_{pos}"));
        }
        (ft, fn_, fc, None)
    } else {
        // Per-case layout: each case gets its own payload slots
        // Layout: [disc, case0_payload_0, ..., case1_payload_0, ...]
        let mut ft = vec![WirType::I32];
        let mut fn_ = vec!["discriminant".to_string()];
        let mut offsets = Vec::with_capacity(variant_type.cases.len());
        for (case_idx, case) in variant_type.cases.iter().enumerate() {
            let offset = ft.len() - 1; // offset within payload portion (after disc)
            offsets.push(offset);
            for (pos, ty) in case.payload.iter().enumerate() {
                ft.push(WirType::as_nullable(ty.clone()));
                fn_.push(format!("case{case_idx}_payload_{pos}"));
            }
        }
        let fc = ft.len();
        if fc > 8 {
            return None; // too many multi-value returns
        }
        (ft, fn_, fc, Some(offsets))
    };

    // Recompute max_payload_count for per-case layout: total payload slots (not per-case max)
    let total_payload_slots = field_count - 1;

    // Collect ALL case type indices (including unit cases) for return validation
    let mut all_case_type_indices: IndexSet<u32> = IndexSet::default();
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
            max_payload_count: total_payload_slots,
            case_slot_offsets,
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
        WirInstr::Block { body, result, .. } => {
            let inner_ok = all_returns_are_variant_struct_new(body, valid_type_indices);
            if result.is_some() {
                // Typed block: the block's exit values are carried via [val, Br(0)] pairs.
                // These Br-exit values must also be StructNew of valid variant case types,
                // otherwise the function cannot be correctly SROA'd.
                inner_ok && all_br_variant_values_are_struct_new(body, valid_type_indices, 0)
            } else {
                inner_ok
            }
        }
        WirInstr::Loop { body, .. } => all_returns_are_variant_struct_new(body, valid_type_indices),
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
        WirInstr::Drop(inner) => check_return_variant_struct_new(inner, valid_type_indices),
        _ => true,
    }
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
                    || matches!(v, WirInstr::StructNew { type_id, .. } if valid_type_indices.contains(&type_id.index()))
            });
            if !is_valid {
                return false;
            }
            i += 1;
        } else {
            // Recurse into nested blocks and ifs (both add a depth level)
            match &instrs[i] {
                WirInstr::Block { body, .. } => {
                    if !all_br_variant_values_are_struct_new(
                        body,
                        valid_type_indices,
                        target_depth + 1,
                    ) {
                        return false;
                    }
                }
                WirInstr::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    if !all_br_variant_values_are_struct_new(
                        then_body,
                        valid_type_indices,
                        target_depth + 1,
                    ) {
                        return false;
                    }
                    if let Some(eb) = else_body
                        && !all_br_variant_values_are_struct_new(
                            eb,
                            valid_type_indices,
                            target_depth + 1,
                        )
                    {
                        return false;
                    }
                }
                _ => {}
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
    module: &WirPackage,
    candidates: &[(u32, SroaCandidate)],
) -> Vec<(u32, SroaCandidate)> {
    let candidate_ids: IndexSet<u32> = candidates.iter().map(|(id, _)| *id).collect();
    let variant_candidate_ids: IndexSet<u32> = candidates
        .iter()
        .filter(|(_, c)| c.variant_info.is_some())
        .map(|(id, _)| *id)
        .collect();

    // Scan all function bodies for calls to candidate functions
    let mut invalid: IndexSet<u32> = IndexSet::default();

    for func in &module.functions {
        if let Some(body) = &func.body {
            validate_call_sites_in_body(
                body,
                body,
                &candidate_ids,
                &variant_candidate_ids,
                &mut invalid,
            );
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
                        case_slot_offsets: vi.case_slot_offsets.clone(),
                    }),
                },
            )
        })
        .collect()
}

/// Validate call sites of candidate functions within a flat instruction list.
///
/// `root_body` is the top-level function body — used when checking that the temp local
/// is only accessed via valid patterns across all scopes, not just the current scope.
/// This prevents SROA when a call site is inside a nested block (If/Block) but the temp
/// local is used in the outer scope in a non-StructGet context (e.g. `return temp`).
fn validate_call_sites_in_body(
    instrs: &[WirInstr],
    root_body: &[WirInstr],
    candidate_ids: &IndexSet<u32>,
    variant_candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
) {
    for instr in instrs {
        // Recurse into nested statement-level blocks
        match instr {
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                validate_call_sites_in_body(
                    body,
                    root_body,
                    candidate_ids,
                    variant_candidate_ids,
                    invalid,
                );
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
                    root_body,
                    candidate_ids,
                    variant_candidate_ids,
                    invalid,
                );
                if let Some(eb) = else_body {
                    validate_call_sites_in_body(
                        eb,
                        root_body,
                        candidate_ids,
                        variant_candidate_ids,
                        invalid,
                    );
                }
            }
            WirInstr::Seq(body) => {
                validate_call_sites_in_body(
                    body,
                    root_body,
                    candidate_ids,
                    variant_candidate_ids,
                    invalid,
                );
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
    // Use root_body (the full function body) to catch uses of the temp local in outer scopes.
    for instr in instrs {
        if let WirInstr::LocalSet { name, value } = instr
            && let Some(func_id_idx) = unwrap_to_candidate_call(value, candidate_ids)
        {
            // Reject when the local has more than one definition: SROA assumes
            // the temp is exclusively defined by this call. With mutable locals
            // (e.g. `let mut s: String;` assigned in multiple branches), the
            // other definitions would be silently dropped, producing wrong code.
            if count_local_set_in_body(root_body, name) > 1 {
                invalid.insert(func_id_idx);
                continue;
            }
            if variant_candidate_ids.contains(&func_id_idx) {
                // Variant candidate: uses must be RefTest or StructGet(RefCast(...))
                if !all_uses_are_variant_access(root_body, name) {
                    invalid.insert(func_id_idx);
                }
            } else {
                // Struct candidate: uses must be StructGet
                if !all_uses_are_struct_get(root_body, name) {
                    invalid.insert(func_id_idx);
                }
            }
        }
    }
}

/// Count `LocalSet { name, .. }` and `LocalTee { name, .. }` for `local_name`
/// across the entire instruction tree.
fn count_local_set_in_body(instrs: &[WirInstr], local_name: &str) -> usize {
    let mut total = 0;
    for instr in instrs {
        total += count_local_set_in_instr(instr, local_name);
    }
    total
}

fn count_local_set_in_instr(instr: &WirInstr, local_name: &str) -> usize {
    let mut count = match instr {
        WirInstr::LocalSet { name, .. } | WirInstr::LocalTee { name, .. } if name == local_name => {
            1
        }
        _ => 0,
    };
    instr.for_each_child(&mut |child| {
        count += count_local_set_in_instr(child, local_name);
    });
    count
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
        WirInstr::LocalGet { name, .. } if name == local_name => {
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
    // Skip trailing Unreachable — translate_stmts_as_value appends Unreachable after
    // break-with-value statements so the Wasm validator sees no fallthrough value.
    // That trailing Unreachable is dead code and must not prevent SROA.
    let effective_body = if matches!(body.last(), Some(WirInstr::Unreachable)) {
        &body[..body.len() - 1]
    } else {
        body
    };
    let body = effective_body;
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

/// Scan prefix instructions in Block wrappers for nested candidate calls.
/// When a `LocalSet { value: Block { body } }` wraps a candidate call
/// as its result, the prefix instructions in the block body may also contain calls
/// to candidates that the rewrite pass cannot reach.
fn find_candidate_calls_in_block_prefix(
    instr: &WirInstr,
    candidate_ids: &IndexSet<u32>,
    invalid: &mut IndexSet<u32>,
) {
    if let WirInstr::Block { body, .. } = instr {
        // All instructions except the last (which is the result value) are prefix.
        // Skip trailing Unreachable — translate_stmts_as_value may append one after
        // a break-with-value; it is dead code and must not be treated as the result.
        let effective_body = if matches!(body.last(), Some(WirInstr::Unreachable)) {
            &body[..body.len() - 1]
        } else {
            body.as_slice()
        };
        if let Some((_, prefix)) = effective_body.split_last() {
            for prefix_instr in prefix {
                find_nested_candidate_calls(prefix_instr, candidate_ids, invalid);
            }
        }
    }
}

/// Unwrap through Block to find the inner Call instruction (for arg checking).
fn unwrap_to_inner_call(instr: &WirInstr) -> Option<&WirInstr> {
    match instr {
        WirInstr::Call { .. } => Some(instr),
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
        WirInstr::LocalGet { name, .. } if name == local_name => {
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
fn apply_sroa(module: &mut WirPackage, confirmed: &[(u32, SroaCandidate)]) {
    // Build a lookup from func_id_index → candidate info
    let candidate_map: crate::hashmap::IndexMap<u32, &SroaCandidate> =
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
            compiler_trace!(
                "sroa_variant_return",
                "applying SROA to function {} (variant = {})",
                func.name,
                candidate.variant_info.is_some()
            );
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
                compiler_trace!(
                    "sroa_variant_return",
                    "rewrite return-with-value (inner = {:?})",
                    std::mem::discriminant(v.as_ref())
                );
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
            WirInstr::Drop(inner) => {
                if inner.always_diverges() {
                    let mut unwrapped = std::mem::replace(inner.as_mut(), WirInstr::Nop);
                    clear_result_types_on_divergent(&mut unwrapped);
                    rewrite_returns_to_multi_value(std::slice::from_mut(&mut unwrapped));
                    *instr = unwrapped;
                } else {
                    rewrite_returns_to_multi_value(std::slice::from_mut(inner.as_mut()));
                }
            }
            // Recurse into boxed children so Returns hidden inside non-tail
            // value positions (LocalSet value, arithmetic operands, …) are
            // rewritten alongside top-level Returns.
            other => {
                other.for_each_boxed_child_mut(&mut |child| {
                    rewrite_returns_to_multi_value(std::slice::from_mut(child));
                });
            }
        }
    }
}

/// Lift `Return` into leaf struct-constructor positions (`StructNew`)
/// within a value expression. Replaces each leaf with
/// `Return { value: <stack-pushing expression> }` and removes block result
/// types (since branches now return directly).
///
/// For typed Blocks (e.g. from `return match { ... }` with `BrTable`), this also
/// rewrites struct-constructor/`Br` pairs inside the block into
/// `Return { ... }`.
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
            // Replace struct constructor with `Return { … }`.
            if matches!(&instrs[i], WirInstr::StructNew { .. }) {
                instrs[i] =
                    struct_constructor_to_return(std::mem::replace(&mut instrs[i], WirInstr::Nop));
                // Remove the Br (now unreachable after Return)
                instrs[i + 1] = WirInstr::Nop;
            }
            // Skip dead code (unreachable) before Br — leave as-is
            i += 2;
        } else {
            // Handle `Seq([..., struct_ctor, Br(target_depth)])` — LabeledBlock
            // exit pattern.
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
                    if let Some(ctor) = seq.pop() {
                        let ret = struct_constructor_to_return(ctor);
                        instrs[i] = if seq.is_empty() {
                            ret
                        } else {
                            seq.push(ret);
                            WirInstr::Seq(seq)
                        };
                    }
                }
            } else {
                // Recurse into nested blocks and ifs (both add 1 to the depth)
                match &mut instrs[i] {
                    WirInstr::Block { body, .. } => {
                        rewrite_struct_new_br_to_return(body, target_depth + 1);
                    }
                    WirInstr::If {
                        then_body,
                        else_body,
                        ..
                    } => {
                        rewrite_struct_new_br_to_return(then_body, target_depth + 1);
                        if let Some(eb) = else_body {
                            rewrite_struct_new_br_to_return(eb, target_depth + 1);
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
    }

    // Handle the fallthrough (last instruction) — if it's a struct constructor
    // without an explicit Br.
    if let Some(last) = instrs.last_mut()
        && matches!(last, WirInstr::StructNew { .. })
    {
        *last = struct_constructor_to_return(std::mem::replace(last, WirInstr::Nop));
    }
}

/// Convert a `StructNew` constructor into a `Return { value }` whose value
/// pushes the struct's fields onto the stack (multi-value-style): the
/// fields are wrapped in a `Seq`.
fn struct_constructor_to_return(ctor: WirInstr) -> WirInstr {
    match ctor {
        WirInstr::StructNew { fields, .. } => WirInstr::Return {
            value: Some(Box::new(WirInstr::Seq(fields))),
        },
        other => other,
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
                    if let WirInstr::StructNew { type_id, fields } =
                        std::mem::replace(v.as_mut(), WirInstr::Nop)
                    {
                        let mut padded =
                            pad_variant_fields(fields, vi, result_types, type_id.index());
                        // Recurse into padded fields: they may contain nested Return { StructNew }
                        // from early returns inside struct field expressions.
                        rewrite_variant_returns_to_multi_value(&mut padded, vi, result_types);
                        **v = WirInstr::Seq(padded);
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
            WirInstr::Drop(inner) => {
                // Drop wraps an expression whose value is discarded.
                // If the inner expression is fully divergent (all paths return),
                // the Drop never executes. We must unwrap it because the inner
                // expression no longer produces a value after return rewriting.
                // If not divergent (e.g., drop(call(...))), keep the Drop.
                if inner.always_diverges() {
                    let mut unwrapped = std::mem::replace(inner.as_mut(), WirInstr::Nop);
                    clear_result_types_on_divergent(&mut unwrapped);
                    rewrite_variant_returns_to_multi_value(
                        std::slice::from_mut(&mut unwrapped),
                        vi,
                        result_types,
                    );
                    *instr = unwrapped;
                } else {
                    rewrite_variant_returns_to_multi_value(
                        std::slice::from_mut(inner.as_mut()),
                        vi,
                        result_types,
                    );
                }
            }
            // For all other instructions (LocalSet, ValueCopy, etc.),
            // recurse into any nested children that might contain Return.
            other => {
                other.for_each_boxed_child_mut(&mut |child| {
                    rewrite_variant_returns_to_multi_value(
                        std::slice::from_mut(child),
                        vi,
                        result_types,
                    );
                });
            }
        }
    }
}

/// Clear result types on If/Block nodes that are fully divergent,
/// so they don't declare values that are never produced.
fn clear_result_types_on_divergent(instr: &mut WirInstr) {
    match instr {
        WirInstr::If {
            result,
            then_body,
            else_body,
            ..
        } => {
            for child in then_body.iter_mut() {
                clear_result_types_on_divergent(child);
            }
            if let Some(eb) = else_body {
                for child in eb.iter_mut() {
                    clear_result_types_on_divergent(child);
                }
            }
            if then_body.iter().any(WirInstr::always_diverges)
                && else_body
                    .as_ref()
                    .is_some_and(|eb| eb.iter().any(WirInstr::always_diverges))
            {
                *result = None;
            }
        }
        WirInstr::Block { result, body, .. } => {
            for child in body.iter_mut() {
                clear_result_types_on_divergent(child);
            }
            if body.iter().any(WirInstr::always_diverges) {
                *result = None;
            }
        }
        WirInstr::Seq(body) => {
            for child in body.iter_mut() {
                clear_result_types_on_divergent(child);
            }
        }
        WirInstr::Drop(inner) => {
            clear_result_types_on_divergent(inner);
        }
        _ => {}
    }
}

/// Pad variant fields with default values for missing payload slots.
/// Also replaces `Nop` fields (unit/void placeholders from `StructNew`) with
/// appropriate default values, since Nop produces no value in flat multi-value returns.
///
/// `case_type_idx`: the WIR type index of the case struct being constructed (needed for
/// per-case slot layout to determine which slots this case's payloads go into).
fn pad_variant_fields(
    fields: Vec<WirInstr>,
    vi: &VariantSroaInfo,
    result_types: &[WirType],
    case_type_idx: u32,
) -> Vec<WirInstr> {
    if let Some(ref offsets) = vi.case_slot_offsets {
        // Per-case slot layout: each case has dedicated payload slots.
        // Find which case this is by matching case_type_idx.
        let disc_expr = fields[0].clone();
        let payload_exprs: Vec<WirInstr> = fields.into_iter().skip(1).collect();

        // Find the case index for this type_id
        let case_idx = vi
            .case_type_indices
            .iter()
            .position(|opt| opt.as_ref() == Some(&case_type_idx));

        // Build the full result: [disc, slot0, slot1, ..., slotN]
        let total_payload_slots = result_types.len() - 1;
        let mut result = Vec::with_capacity(result_types.len());
        result.push(disc_expr);

        // Initialize all payload slots with defaults
        for slot in 0..total_payload_slots {
            result.push(default_value_for_type(&result_types[1 + slot]));
        }

        // Place this case's payloads in their dedicated slots
        if let Some(ci) = case_idx {
            let offset = offsets[ci];
            for (pi, payload) in payload_exprs.into_iter().enumerate() {
                let payload = if matches!(payload, WirInstr::Nop) {
                    default_value_for_type(&result_types[1 + offset + pi])
                } else {
                    payload
                };
                result[1 + offset + pi] = payload;
            }
        }
        // else: unit case (no payloads), all defaults are correct

        result
    } else {
        // Homogeneous layout (original behavior)
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
}

/// Lift `Return` into leaf `StructNew` positions for variant SROA.
fn lift_return_into_variant_leaves(
    expr: &mut WirInstr,
    vi: &VariantSroaInfo,
    result_types: &[WirType],
) {
    match expr {
        WirInstr::StructNew { .. } => {
            if let WirInstr::StructNew { type_id, fields } = std::mem::replace(expr, WirInstr::Nop)
            {
                *expr = WirInstr::Return {
                    value: Some(Box::new(WirInstr::Seq(pad_variant_fields(
                        fields,
                        vi,
                        result_types,
                        type_id.index(),
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
                if let WirInstr::StructNew { type_id, fields } =
                    std::mem::replace(&mut instrs[i], WirInstr::Nop)
                {
                    instrs[i] = WirInstr::Return {
                        value: Some(Box::new(WirInstr::Seq(pad_variant_fields(
                            fields,
                            vi,
                            result_types,
                            type_id.index(),
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
                WirInstr::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    rewrite_variant_struct_new_br_to_return(
                        then_body,
                        target_depth + 1,
                        vi,
                        result_types,
                    );
                    if let Some(eb) = else_body {
                        rewrite_variant_struct_new_br_to_return(
                            eb,
                            target_depth + 1,
                            vi,
                            result_types,
                        );
                    }
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
    if let Some(last) = instrs.last_mut() {
        if matches!(last, WirInstr::StructNew { .. })
            && let WirInstr::StructNew { type_id, fields } = std::mem::replace(last, WirInstr::Nop)
        {
            *last = WirInstr::Return {
                value: Some(Box::new(WirInstr::Seq(pad_variant_fields(
                    fields,
                    vi,
                    result_types,
                    type_id.index(),
                )))),
            };
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
        WirType::Ref { .. } | WirType::AbstractRef { .. } => WirInstr::RefNull {
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
    candidate_map: &crate::hashmap::IndexMap<u32, &SroaCandidate>,
    types: &[WirTypeDef],
) {
    // Collect replacements: temp_name → (field_name → fresh_local_name)
    let mut replacements: crate::hashmap::IndexMap<
        String,
        crate::hashmap::IndexMap<String, String>,
    > = crate::hashmap::IndexMap::default();
    // Variant replacements: temp_name → VariantReplacement
    let mut variant_replacements: crate::hashmap::IndexMap<String, VariantReplacement> =
        crate::hashmap::IndexMap::default();

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
        let mut field_map: crate::hashmap::IndexMap<String, String> =
            crate::hashmap::IndexMap::default();
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
            let mut case_disc_values: crate::hashmap::IndexMap<u32, i32> =
                crate::hashmap::IndexMap::default();
            let mut field_to_local: crate::hashmap::IndexMap<(u32, String), String> =
                crate::hashmap::IndexMap::default();

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
                                // For per-case layout, slot names are
                                // "case{disc_val}_payload_{idx}"; for shared layout,
                                // "payload_{idx}".
                                let payload_name = if vi.case_slot_offsets.is_some() {
                                    format!("case{disc_val}_payload_{payload_idx}")
                                } else {
                                    format!("payload_{payload_idx}")
                                };
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

            // Track which SROA locals hold ref types that need ref.as_non_null when read.
            // The check must be against the ORIGINAL variant-case payload type from
            // `WirVariantCase::payload`, not the case struct's field type: the latter
            // is always declared nullable for the Option<&T> boxing optimisation,
            // which loses the information that a `Some(non_null_ref)` payload is
            // semantically non-null at the Wado source level.
            let mut ref_locals = crate::hashmap::IndexSet::default();
            if let Some(WirTypeDef::Variant(wv)) = types.get(candidate.struct_type_idx as usize) {
                for (disc_val_2, case_type_opt_2) in vi.case_type_indices.iter().enumerate() {
                    if case_type_opt_2.is_none() {
                        continue;
                    }
                    // Locate the corresponding variant case by discriminant value.
                    let Some(wir_case) = wv.cases.iter().find(|c| c.index as usize == disc_val_2)
                    else {
                        continue;
                    };
                    for (payload_idx, payload_ty) in wir_case.payload.iter().enumerate() {
                        let is_non_nullable_ref = matches!(
                            payload_ty,
                            WirType::Ref {
                                nullable: false,
                                ..
                            }
                        );
                        if !is_non_nullable_ref {
                            continue;
                        }
                        let payload_name = if vi.case_slot_offsets.is_some() {
                            format!("case{disc_val_2}_payload_{payload_idx}")
                        } else {
                            format!("payload_{payload_idx}")
                        };
                        if let Some(sroa_local) = field_map.get(&payload_name) {
                            ref_locals.insert(sroa_local.clone());
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
                    ref_locals,
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
        // Collect RefCast aliases: `LocalSet { cast_var, RefCast { type_id, LocalGet(temp) } }`
        // where `temp` is a variant-SROA'd local. After copy propagation, `ref.cast` may
        // reference the SROA temp directly but be stored to an intermediate local, with a
        // separate `StructGet { field, LocalGet(cast_var) }` reading the payload.
        let mut refcast_aliases: crate::hashmap::IndexMap<String, (String, u32)> =
            crate::hashmap::IndexMap::default();
        collect_refcast_aliases(instrs, &variant_replacements, &mut refcast_aliases);

        for instr in instrs.iter_mut() {
            replace_variant_accesses(instr, &variant_replacements, &refcast_aliases);
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
    candidate_map: &crate::hashmap::IndexMap<u32, &SroaCandidate>,
    types: &[WirTypeDef],
) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            rewrite_call_sites(body, candidate_map, types);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            recurse_rewrite_call_sites(condition, candidate_map, types);
            rewrite_call_sites(then_body, candidate_map, types);
            if let Some(eb) = else_body {
                rewrite_call_sites(eb, candidate_map, types);
            }
        }
        WirInstr::Seq(body) => {
            rewrite_call_sites(body, candidate_map, types);
        }
        _ => {
            instr.for_each_boxed_child_mut(&mut |child| {
                recurse_rewrite_call_sites(child, candidate_map, types);
            });
        }
    }
}

/// Replace `StructGet { field_name, expr: LocalGet(temp) }` with `LocalGet(fresh_local)`
/// for all known replacements. Uses `WirInstr::for_each_boxed_child_mut` for generic traversal.
fn replace_struct_gets(
    instr: &mut WirInstr,
    replacements: &crate::hashmap::IndexMap<String, crate::hashmap::IndexMap<String, String>>,
) {
    // Check if THIS instruction is a StructGet that should be replaced
    if let WirInstr::StructGet {
        field_name,
        expr,
        result_ty,
        ..
    } = instr
        && let WirInstr::LocalGet {
            name: temp_name, ..
        } = expr.as_ref()
        && let Some(field_map) = replacements.get(temp_name.as_str())
        && let Some(fresh_local) = field_map.get(field_name.as_str())
    {
        *instr = WirInstr::LocalGet {
            name: fresh_local.clone(),
            result_ty: result_ty.clone(),
        };
        return;
    }

    // Recursively process all children using the generic mutable visitor
    instr.for_each_boxed_child_mut(&mut |child| replace_struct_gets(child, replacements));
}

/// Produce a `LocalGet` for an SROA local, wrapping with `RefAsNonNull` if the local
/// holds a nullable ref type (variant SROA payload locals use nullable types for padding).
fn sroa_local_get(
    local_name: &str,
    ref_locals: &crate::hashmap::IndexSet<String>,
    result_ty: crate::wir::WirType,
) -> WirInstr {
    if ref_locals.contains(local_name) {
        // Set the LocalGet's own result type to nullable so downstream
        // cleanup passes don't strip the RefAsNonNull wrapper as
        // redundant. The wrapper is what narrows to the non-null
        // `result_ty` expected by the surrounding consumer (e.g., the
        // callee's non-null `ref T` parameter), after the variant case
        // test has already proved the payload is non-null at runtime.
        let nullable_ty = match &result_ty {
            crate::wir::WirType::Ref { type_id, .. } => crate::wir::WirType::Ref {
                type_id: type_id.clone(),
                nullable: true,
            },
            crate::wir::WirType::AbstractRef { heap_type, .. } => {
                crate::wir::WirType::AbstractRef {
                    heap_type: heap_type.clone(),
                    nullable: true,
                }
            }
            _ => result_ty.clone(),
        };
        let get = WirInstr::LocalGet {
            name: local_name.to_string(),
            result_ty: nullable_ty,
        };
        WirInstr::RefAsNonNull(Box::new(get))
    } else {
        WirInstr::LocalGet {
            name: local_name.to_string(),
            result_ty,
        }
    }
}

/// Collect `RefCast` aliases: find `LocalSet { cast_var, RefCast { type_id, LocalGet(temp) } }`
/// patterns where `temp` is a variant-SROA'd local, and replace them with Nop.
/// The alias map records `cast_var → (temp, type_id_index)` so that later
/// `StructGet { field, LocalGet(cast_var) }` can be resolved through the alias.
fn collect_refcast_aliases(
    instrs: &mut [WirInstr],
    variant_replacements: &crate::hashmap::IndexMap<String, VariantReplacement>,
    aliases: &mut crate::hashmap::IndexMap<String, (String, u32)>,
) {
    for instr in instrs.iter_mut() {
        if let WirInstr::LocalSet { name, value } = instr
            && let WirInstr::RefCast {
                type_id,
                expr: rc_expr,
                ..
            } = value.as_ref()
            && let WirInstr::LocalGet {
                name: temp_name, ..
            } = rc_expr.as_ref()
            && variant_replacements.contains_key(temp_name.as_str())
        {
            aliases.insert(name.clone(), (temp_name.clone(), type_id.index()));
            *instr = WirInstr::Nop;
            continue;
        }
        match instr {
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                collect_refcast_aliases(body, variant_replacements, aliases);
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                collect_refcast_aliases(then_body, variant_replacements, aliases);
                if let Some(eb) = else_body {
                    collect_refcast_aliases(eb, variant_replacements, aliases);
                }
            }
            WirInstr::Seq(body) => {
                collect_refcast_aliases(body, variant_replacements, aliases);
            }
            _ => {}
        }
    }
}

/// Replace variant access patterns with scalar local accesses for variant SROA'd temps.
///
/// Handles five patterns:
/// 1. `RefTest { type_id, expr: LocalGet(temp) }` → `I32Eq(LocalGet(disc), I32Const(case_disc))`
/// 2. `StructGet { field, expr: RefCast { type_id, expr: LocalGet(temp) } }` → `LocalGet(sroa_local)`
/// 3. `RefAsNonNull(StructGet { field, expr: RefCast { type_id, expr: LocalGet(temp) } })` → same
/// 4. `StructGet { field, expr: LocalGet(cast_alias) }` where `cast_alias` was a `RefCast` alias → same
fn replace_variant_accesses(
    instr: &mut WirInstr,
    variant_replacements: &crate::hashmap::IndexMap<String, VariantReplacement>,
    refcast_aliases: &crate::hashmap::IndexMap<String, (String, u32)>,
) {
    // Pattern 3: `RefAsNonNull(StructGet(RefCast(LocalGet(temp))))` — the
    // variant-payload extraction form emitted by `wir_build::pattern_match`.
    // Replaces with `sroa_local_get`, which applies a non-null narrowing when
    // the original variant payload field was non-nullable.
    if let WirInstr::RefAsNonNull(inner) = instr
        && let WirInstr::StructGet {
            field_name,
            expr: sg_expr,
            result_ty,
            ..
        } = inner.as_ref()
        && let WirInstr::RefCast {
            type_id: cast_type_id,
            expr: rc_expr,
            ..
        } = sg_expr.as_ref()
        && let WirInstr::LocalGet {
            name: temp_name, ..
        } = rc_expr.as_ref()
        && let Some(vr) = variant_replacements.get(temp_name.as_str())
    {
        let key = (cast_type_id.index(), field_name.clone());
        if let Some(local_name) = vr.field_to_local.get(&key) {
            *instr = sroa_local_get(local_name, &vr.ref_locals, result_ty.clone());
            return;
        }
    }

    // Pattern 1: RefTest { type_id, expr: LocalGet(temp) }
    if let WirInstr::RefTest { type_id, expr, .. } = instr
        && let WirInstr::LocalGet {
            name: temp_name, ..
        } = expr.as_ref()
        && let Some(vr) = variant_replacements.get(temp_name.as_str())
        && let Some(&disc_val) = vr.case_disc_values.get(&type_id.index())
    {
        *instr = WirInstr::I32Eq(
            Box::new(WirInstr::LocalGet {
                name: vr.disc_local.clone(),
                result_ty: crate::wir::WirType::I32,
            }),
            Box::new(WirInstr::I32Const(disc_val)),
        );
        return;
    }

    // Pattern 2: StructGet { field, expr: RefCast { type_id, expr: LocalGet(temp) } }
    if let WirInstr::StructGet {
        field_name,
        expr: sg_expr,
        result_ty,
        ..
    } = instr
        && let WirInstr::RefCast {
            type_id: cast_type_id,
            expr: rc_expr,
            ..
        } = sg_expr.as_ref()
        && let WirInstr::LocalGet {
            name: temp_name, ..
        } = rc_expr.as_ref()
        && let Some(vr) = variant_replacements.get(temp_name.as_str())
    {
        let key = (cast_type_id.index(), field_name.clone());
        if let Some(local_name) = vr.field_to_local.get(&key) {
            *instr = sroa_local_get(local_name, &vr.ref_locals, result_ty.clone());
            return;
        }
    }

    // Pattern 4: StructGet { field, LocalGet(cast_alias) } via alias
    if let WirInstr::StructGet {
        field_name,
        expr: sg_expr,
        result_ty,
        ..
    } = instr
        && let WirInstr::LocalGet {
            name: alias_name, ..
        } = sg_expr.as_ref()
        && let Some((temp_name, cast_type_idx)) = refcast_aliases.get(alias_name.as_str())
        && let Some(vr) = variant_replacements.get(temp_name.as_str())
    {
        let key = (*cast_type_idx, field_name.clone());
        if let Some(local_name) = vr.field_to_local.get(&key) {
            *instr = sroa_local_get(local_name, &vr.ref_locals, result_ty.clone());
            return;
        }
    }

    // Recurse into children
    instr.for_each_boxed_child_mut(&mut |child| {
        replace_variant_accesses(child, variant_replacements, refcast_aliases);
    });
}

/// Check if instruction is `LocalSet { name, value: <wrapper>(Call { func_id in candidates }) }`.
fn is_candidate_call_set(
    instr: &WirInstr,
    expected_name: &str,
    candidate_map: &crate::hashmap::IndexMap<u32, &SroaCandidate>,
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
    candidate_map: &crate::hashmap::IndexMap<u32, &SroaCandidate>,
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
    let call = unwrap_and_take_call(*value, &mut prefix);
    (prefix, Box::new(call))
}

/// Recursively unwrap `Block` wrappers to extract the `Call` instruction.
/// Collects any non-result instructions from blocks into `prefix` so they can be
/// emitted before the call.
fn unwrap_and_take_call(instr: WirInstr, prefix: &mut Vec<WirInstr>) -> WirInstr {
    let mut current = instr;
    loop {
        match current {
            WirInstr::Call { .. } => return current,
            WirInstr::Block { ref mut body, .. } => {
                // Extract the call from the block's result position,
                // and collect all preceding statements as prefix.
                if let Some(call) = take_block_result_call(body, prefix) {
                    current = *call;
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

    // Skip trailing Unreachable — translate_stmts_as_value may append one after a
    // break-with-value; it is dead code and must not be treated as the result value.
    let effective_len = if matches!(body.last(), Some(WirInstr::Unreachable)) {
        body.len() - 1
    } else {
        body.len()
    };
    if effective_len == 0 {
        return None;
    }
    let last_idx = effective_len - 1;

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
