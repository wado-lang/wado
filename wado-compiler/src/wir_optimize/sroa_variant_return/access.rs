//! Call-site variant access, shared by both phases.
//!
//! Once a variant arrives as a flat result vector, `ref.test` / `ref.cast` /
//! `struct.get` on the old boxed temp have to become reads of the scalar
//! locals. [`build_variant_replacement`] derives that mapping and
//! [`replace_variant_accesses`] applies it.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirInstr, WirType, WirTypeDef};

use super::layout::VariantSroaInfo;

/// Replacement info for a variant SROA'd temp local at call sites.
pub(super) struct VariantReplacement {
    /// Local name holding the discriminant value.
    disc_local: String,
    /// `case_wir_type_idx` → discriminant value (i32).
    case_disc_values: IndexMap<u32, i32>,
    /// `(case_wir_type_idx, field_name_in_case_struct)` → sroa local name.
    field_to_local: IndexMap<(u32, String), String>,
    /// SROA locals that hold ref types (need `ref.as_non_null` when read).
    ref_locals: IndexSet<String>,
}

/// Per-function local def/use counts, computed in one walk. Replaces the
/// per-site whole-body rescans (`count_local_set_in_body` /
/// `count_local_get` per temp) that made validation O(n²).
#[derive(Default)]
pub(super) struct LocalDefUse {
    /// `LocalSet` + `LocalTee` writes per local name.
    sets: IndexMap<String, usize>,
    /// `LocalGet` reads per local name.
    gets: IndexMap<String, usize>,
}

impl LocalDefUse {
    pub(super) fn of_body(body: &[WirInstr]) -> Self {
        let mut index = Self::default();
        for instr in body {
            index.scan(instr);
        }
        index
    }

    fn scan(&mut self, instr: &WirInstr) {
        match instr {
            WirInstr::LocalGet { name, .. } => {
                *self.gets.entry(name.clone()).or_default() += 1;
            }
            WirInstr::LocalSet { name, .. } | WirInstr::LocalTee { name, .. } => {
                *self.sets.entry(name.clone()).or_default() += 1;
            }
            _ => {}
        }
        instr.for_each_child(&mut |child| self.scan(child));
    }

    pub(super) fn set_count(&self, name: &str) -> usize {
        self.sets.get(name).copied().unwrap_or(0)
    }

    pub(super) fn get_count(&self, name: &str) -> usize {
        self.gets.get(name).copied().unwrap_or(0)
    }
}

/// Build the call-site [`VariantReplacement`] for a variant whose result vector
/// fields are bound to the locals in `field_map` (keyed by the layout's field
/// names — `"discriminant"`, `"payload_N"` or `"caseN_payload_M"`).
///
/// Maps each case struct's `(case_type_idx, field_name)` to the local carrying
/// that field, and records which payload locals hold non-nullable refs (they
/// need `ref.as_non_null` when read). Shared by the function-return rewriter
/// ([`rewrite_call_sites`]) and the nested-slot flattener
/// ([`flatten_variant_slots`]), so both derive the exact same replacement.
pub(super) fn build_variant_replacement(
    field_map: &IndexMap<String, String>,
    vi: &VariantSroaInfo,
    variant_type_idx: u32,
    types: &[WirTypeDef],
) -> VariantReplacement {
    let disc_local = field_map["discriminant"].clone();
    let mut case_disc_values: IndexMap<u32, i32> = IndexMap::default();
    let mut field_to_local: IndexMap<(u32, String), String> = IndexMap::default();

    for (disc_val, case_type_opt) in vi.case_type_indices.iter().enumerate() {
        if let Some(case_type_idx) = case_type_opt {
            case_disc_values.insert(*case_type_idx, i32::try_from(disc_val).unwrap());

            // Look up the case struct type to map field names → sroa locals
            if let Some(WirTypeDef::Struct(st)) = types.get(*case_type_idx as usize) {
                for (field_pos, field) in st.fields.iter().enumerate() {
                    if field_pos == 0 {
                        // Discriminant field
                        field_to_local
                            .insert((*case_type_idx, field.name.clone()), disc_local.clone());
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
                            field_to_local
                                .insert((*case_type_idx, field.name.clone()), sroa_local.clone());
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
    let mut ref_locals = IndexSet::default();
    if let Some(WirTypeDef::Variant(wv)) = types.get(variant_type_idx as usize) {
        for (disc_val_2, case_type_opt_2) in vi.case_type_indices.iter().enumerate() {
            if case_type_opt_2.is_none() {
                continue;
            }
            // Locate the corresponding variant case by discriminant value.
            let Some(wir_case) = wv.cases.iter().find(|c| c.index as usize == disc_val_2) else {
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

    VariantReplacement {
        disc_local,
        case_disc_values,
        field_to_local,
        ref_locals,
    }
}

/// Produce a `LocalGet` for an SROA local, wrapping with `RefAsNonNull` if the local
/// holds a nullable ref type (variant SROA payload locals use nullable types for padding).
fn sroa_local_get(local_name: &str, ref_locals: &IndexSet<String>, result_ty: WirType) -> WirInstr {
    if ref_locals.contains(local_name) {
        // Set the LocalGet's own result type to nullable so downstream
        // cleanup passes don't strip the RefAsNonNull wrapper as
        // redundant. The wrapper is what narrows to the non-null
        // `result_ty` expected by the surrounding consumer (e.g., the
        // callee's non-null `ref T` parameter), after the variant case
        // test has already proved the payload is non-null at runtime.
        let nullable_ty = match &result_ty {
            WirType::Ref { type_id, .. } => WirType::Ref {
                type_id: type_id.clone(),
                nullable: true,
            },
            WirType::AbstractRef { heap_type, .. } => WirType::AbstractRef {
                heap_type: heap_type.clone(),
                nullable: true,
            },
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
pub(super) fn collect_refcast_aliases(
    instrs: &mut [WirInstr],
    variant_replacements: &IndexMap<String, VariantReplacement>,
    aliases: &mut IndexMap<String, (String, u32)>,
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
pub(super) fn replace_variant_accesses(
    instr: &mut WirInstr,
    variant_replacements: &IndexMap<String, VariantReplacement>,
    refcast_aliases: &IndexMap<String, (String, u32)>,
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
                result_ty: WirType::I32,
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

/// Check that every reference to `local_name` is a shape the call-site
/// rewriter (`replace_variant_accesses` / `collect_refcast_aliases`)
/// replaces:
/// - `RefTest { type_id ∈ case_types, expr: LocalGet(name) }` — discriminant test
/// - `StructGet { expr: RefCast { type_id ∈ case_types, expr: LocalGet(name) } }` — payload access
/// - `LocalSet(alias, RefCast { type_id ∈ case_types, expr: LocalGet(name) })` —
///   cast-alias binding, provided the alias is single-def and read only via
///   `StructGet(LocalGet(alias))`.
///
/// The type-id constraint matters: the rewriter's `case_disc_values` /
/// `field_to_local` maps are keyed by the candidate's payload-bearing case
/// types, so an access naming any other type would survive the rewrite and
/// read the deleted temp.
pub(super) fn all_uses_are_variant_access(
    instrs: &[WirInstr],
    local_name: &str,
    case_types: &IndexSet<u32>,
    def_use: &LocalDefUse,
) -> bool {
    instrs
        .iter()
        .all(|instr| check_uses_are_variant_access(instr, local_name, case_types, instrs, def_use))
}

fn check_uses_are_variant_access(
    instr: &WirInstr,
    local_name: &str,
    case_types: &IndexSet<u32>,
    root: &[WirInstr],
    def_use: &LocalDefUse,
) -> bool {
    // Discriminant test: `RefTest { case_type, LocalGet(temp) }`.
    if let WirInstr::RefTest { type_id, expr, .. } = instr
        && let WirInstr::LocalGet { name, .. } = expr.as_ref()
        && name == local_name
    {
        return case_types.contains(&type_id.index());
    }
    // Payload read: `StructGet { RefCast { case_type, LocalGet(temp) } }`.
    if let WirInstr::StructGet { expr, .. } = instr
        && let WirInstr::RefCast {
            type_id,
            expr: rc_expr,
            ..
        } = expr.as_ref()
        && let WirInstr::LocalGet { name, .. } = rc_expr.as_ref()
        && name == local_name
    {
        return case_types.contains(&type_id.index());
    }
    // Cast-alias binding: `LocalSet(alias, RefCast { case_type, LocalGet(temp) })`.
    // `collect_refcast_aliases` Nops the definition and resolves later
    // `StructGet(LocalGet(alias))` reads through the alias map, so the alias
    // must have no other definition and no other kind of use.
    if let WirInstr::LocalSet { name: alias, value } = instr
        && let WirInstr::RefCast {
            type_id,
            expr: rc_expr,
            ..
        } = value.as_ref()
        && let WirInstr::LocalGet { name, .. } = rc_expr.as_ref()
        && name == local_name
        && alias != local_name
    {
        return case_types.contains(&type_id.index())
            && def_use.set_count(alias) == 1
            && instrs_alias_uses_are_struct_get(root, alias);
    }
    // Any other read or re-definition of the temp disqualifies it. The
    // defining `LocalSet(temp, <call>)` reaches the recursion below, which
    // rejects any use of the temp inside its own RHS.
    match instr {
        WirInstr::LocalGet { name, .. } | WirInstr::LocalTee { name, .. } if name == local_name => {
            false
        }
        other => {
            let mut ok = true;
            other.for_each_child(&mut |child| {
                if ok
                    && !check_uses_are_variant_access(child, local_name, case_types, root, def_use)
                {
                    ok = false;
                }
            });
            ok
        }
    }
}

/// Check that every read of `alias` is `StructGet { expr: LocalGet(alias) }`
/// (the shape `replace_variant_accesses` resolves through the alias map).
fn instrs_alias_uses_are_struct_get(instrs: &[WirInstr], alias: &str) -> bool {
    instrs.iter().all(|i| alias_uses_are_struct_get(i, alias))
}

fn alias_uses_are_struct_get(instr: &WirInstr, alias: &str) -> bool {
    if let WirInstr::StructGet { expr, .. } = instr
        && let WirInstr::LocalGet { name, .. } = expr.as_ref()
        && name == alias
    {
        return true;
    }
    match instr {
        WirInstr::LocalGet { name, .. } | WirInstr::LocalTee { name, .. } if name == alias => false,
        other => {
            let mut ok = true;
            other.for_each_child(&mut |child| {
                if ok && !alias_uses_are_struct_get(child, alias) {
                    ok = false;
                }
            });
            ok
        }
    }
}
