//! How a variant packs into a Wasm result vector.
//!
//! [`compute_variant_layout`] is the single source of truth both phases build
//! on: the widening of a whole function's return and the flattening of one
//! nested result slot.

use crate::hashmap::IndexMap;
use crate::wir::{WirAbstractHeapType, WirInstr, WirPackage, WirType, WirVariantType};

/// Result-vector cap for the shared (homogeneous) layout:
/// 1 discriminant + 3 shared payload slots.
const MAX_SHARED_RESULT_FIELDS: usize = 4;

/// Result-vector cap for the per-case layout: 1 discriminant + up to 7 payload
/// slots *summed over the cases*. Any one case is still held to
/// [`MAX_SHARED_RESULT_FIELDS`].
pub(super) const MAX_PER_CASE_RESULT_FIELDS: usize = 8;

/// The multi-value layout of a variant: `[i32 disc, payload_0, ...]`.
///
/// The single source of truth for how a variant's cases pack into a Wasm
/// result vector, shared by function-return widening
/// ([`analyze_variant_layout`]) and nested result-slot flattening
/// ([`flatten_variant_slots`]).
pub(super) struct VariantLayout {
    /// Field types of the result vector (`[i32, payload_types...]`).
    pub(super) field_types: Vec<WirType>,
    /// Field names (`["discriminant", "payload_0" | "caseN_payload_M", ...]`).
    pub(super) field_names: Vec<String>,
    /// Per-case slot layout.
    pub(super) variant_info: VariantSroaInfo,
}

/// Additional info needed for variant SROA.
#[derive(Clone)]
pub(super) struct VariantSroaInfo {
    /// WIR type indices of the case struct types (index = case discriminant).
    pub(super) case_type_indices: Vec<Option<u32>>,
    /// The result vector minus the discriminant. Under the per-case layout that
    /// is the sum over cases, not any one case's payload count.
    pub(super) payload_slot_count: usize,
    /// Per-case payload slot offsets in the multi-value result.
    /// `case_slot_offsets[case_discriminant]` is the starting index (0-based)
    /// within the payload portion (after the discriminant) for that case's payloads.
    /// `None` means this case uses shared (homogeneous) layout.
    pub(super) case_slot_offsets: Option<Vec<usize>>,
}

/// Returns true if a `WirType` is a valid Wasm value type for multi-value returns.
///
/// Whitelist: every type here has a Wasm value-type representation and a
/// default padding value in [`default_value_for_type`]. Concrete GC refs are
/// eligible (Wasm multi-value returns support any value type). Abstract heap
/// refs are excluded as they lack the precise type information needed for
/// `StructGet` replacement; `Unit` has no Wasm representation.
fn is_eligible_field_type(ty: &WirType) -> bool {
    match ty {
        WirType::I8
        | WirType::I16
        | WirType::I32
        | WirType::I64
        | WirType::U8
        | WirType::U16
        | WirType::U32
        | WirType::U64
        | WirType::F32
        | WirType::F64
        | WirType::V128
        | WirType::Bool
        | WirType::Char
        | WirType::Enum { .. }
        | WirType::Flags { .. }
        | WirType::Ref { .. } => true,
        WirType::AbstractRef { .. } | WirType::Unit => false,
    }
}

/// Compute the multi-value layout of a variant, or `None` when it does not fit
/// the small-tuple model.
///
/// A variant is eligible (layout-wise) if:
/// - All payload types across all cases are eligible value types
/// - The result vector fits [`MAX_SHARED_RESULT_FIELDS`] (shared layout) or
///   [`MAX_PER_CASE_RESULT_FIELDS`] (per-case layout)
/// - Case type indices can be resolved via `variant_case_info`
pub(super) fn compute_variant_layout(
    module: &WirPackage,
    variant_type_idx: u32,
    variant_type: &WirVariantType,
) -> Option<VariantLayout> {
    // Collect per-case info: case type index and payload count
    let mut case_type_indices: Vec<Option<u32>> = Vec::with_capacity(variant_type.cases.len());
    let mut max_payload_count: usize = 0;

    // Build a mapping of case_wir_type_idx for this variant from variant_case_info
    let mut case_idx_to_type_idx: IndexMap<u32, u32> = IndexMap::default();
    for (&case_wir_idx, &(parent_variant_idx, case_index)) in &module.variant_case_info {
        if parent_variant_idx == variant_type_idx {
            case_idx_to_type_idx.insert(case_index, case_wir_idx);
        }
    }

    for case in &variant_type.cases {
        let payload_count = case.payload.len();
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

    // A case the shared vector cannot hold is out of scope under the per-case
    // layout too, which only grows from here.
    if 1 + max_payload_count > MAX_SHARED_RESULT_FIELDS {
        return None;
    }

    // Compute the payload types: try shared layout first, fall back to per-case slots.
    let mut homogeneous = true;
    for pos in 0..max_payload_count {
        let mut found: Option<&WirType> = None;
        for case in &variant_type.cases {
            if let Some(ty) = case.payload.get(pos) {
                if let Some(existing) = found {
                    if existing != ty {
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

    let (field_types, field_names, case_slot_offsets) = if homogeneous {
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
        let mut ft = Vec::with_capacity(1 + max_payload_count);
        ft.push(WirType::I32);
        ft.extend(payload_types.into_iter().map(WirType::as_nullable));
        let mut fn_ = Vec::with_capacity(1 + max_payload_count);
        fn_.push("discriminant".to_string());
        for pos in 0..max_payload_count {
            fn_.push(format!("payload_{pos}"));
        }
        (ft, fn_, None)
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
        if ft.len() > MAX_PER_CASE_RESULT_FIELDS {
            return None;
        }
        (ft, fn_, Some(offsets))
    };

    let payload_slot_count = field_types.len() - 1;

    Some(VariantLayout {
        field_types,
        field_names,
        variant_info: VariantSroaInfo {
            case_type_indices,
            payload_slot_count,
            case_slot_offsets,
        },
    })
}

/// Pad variant fields with default values for missing payload slots.
/// Also replaces `Nop` fields (unit/void placeholders from `StructNew`) with
/// appropriate default values, since Nop produces no value in flat multi-value returns.
///
/// `case_type_idx`: the WIR type index of the case struct being constructed (needed for
/// per-case slot layout to determine which slots this case's payloads go into).
pub(super) fn pad_variant_fields(
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
        for pos in payload_count..vi.payload_slot_count {
            let ty = &result_types[1 + pos]; // +1 to skip discriminant
            new_fields.push(default_value_for_type(ty));
        }
        new_fields
    }
}

/// Produce a default (zero) value for a given WIR type, used to pad the
/// result-vector slots a variant case leaves unused. Exhaustive over every
/// type [`is_eligible_field_type`] admits — a wrong-typed pad is invalid
/// Wasm, so an unpaddable type is a bug in the eligibility whitelist.
pub(super) fn default_value_for_type(ty: &WirType) -> WirInstr {
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
        WirType::V128 => WirInstr::V128Const(0),
        WirType::Ref { .. } | WirType::AbstractRef { .. } => WirInstr::RefNull {
            heap_type: WirAbstractHeapType::None,
        },
        WirType::Unit => panic!("variant SROA cannot pad a unit-typed result slot"),
    }
}
