//! The per-type bridge helper a reflection marker (`builtin::struct_field_get`,
//! `builtin::variant_case_extract`, …) calls. `lower` rewrites each marker into
//! a call of it, and `prelower_reach` follows the marker to it, so the two read
//! one spelling and a bridge no marker reaches is never lowered.

use crate::module_source::ModuleSource;
use crate::name::{
    case_construct_helper_name, case_extract_helper_name, case_ref_helper_name,
    field_get_helper_name, field_ref_helper_name, hole_fmt_helper_name, hole_get_helper_name,
    hole_ref_helper_name, template_segment_helper_name,
};
use crate::tir::{FunctionRef, MonomorphInfo, TypeId};
use crate::type_table::TypeTable;

/// A bridge helper: its name, homed where its head type is declared.
pub(crate) struct BridgeCallee {
    pub module_source: ModuleSource,
    pub name: String,
}

impl BridgeCallee {
    /// The helper `name`, homed where `head_ty` is declared.
    pub(crate) fn homed(tt: &TypeTable, head_ty: TypeId, name: String, marker: &str) -> Self {
        let (_, module_source) = tt
            .nominal_head(head_ty)
            .unwrap_or_else(|| panic!("{marker} marker on a type with no declaration"));
        Self {
            module_source,
            name,
        }
    }
}

/// The two-type markers: a field read names the struct and the field type, a
/// hole read the shape and the hole type, a case bridge the variant and the
/// payload type.
const PAIR_MARKERS: &[(&str, fn(&str, &str) -> String)] = &[
    ("struct_field_get", field_get_helper_name),
    ("struct_field_ref", field_ref_helper_name),
    ("hole_get", hole_get_helper_name),
    ("hole_ref", hole_ref_helper_name),
    ("variant_case_extract", case_extract_helper_name),
    ("variant_case_ref", case_ref_helper_name),
    ("variant_case_construct", case_construct_helper_name),
];

/// The markers that name the template shape only.
const SHAPE_MARKERS: &[(&str, fn(&str) -> String)] = &[
    ("hole_fmt", hole_fmt_helper_name),
    ("template_segment", template_segment_helper_name),
];

/// The bridge `func` calls, if it is a marker. `builtin::variant_tag` is not
/// one here: which helper it reads depends on the boxing `lower` plans.
pub(crate) fn bridge_callee(
    tt: &TypeTable,
    func: &FunctionRef,
    type_args: &[TypeId],
) -> Option<BridgeCallee> {
    let mi = func.monomorph_info.as_ref();
    for &(marker, helper_name) in PAIR_MARKERS {
        if func.is_builtin_named(marker) {
            let [head, member] = marker_type_args::<2>(type_args, mi, marker);
            let name = helper_name(
                &tt.mangle_type_arg_unboxed(head),
                &tt.mangle_type_arg_unboxed(member),
            );
            return Some(BridgeCallee::homed(tt, head, name, marker));
        }
    }
    for &(marker, helper_name) in SHAPE_MARKERS {
        if func.is_builtin_named(marker) {
            let [shape] = marker_type_args::<1>(type_args, mi, marker);
            let name = helper_name(&tt.mangle_type_arg_unboxed(shape));
            return Some(BridgeCallee::homed(tt, shape, name, marker));
        }
    }
    None
}

/// The `N` type args a bridge marker names: the call's own, else the method's,
/// else the `impl`'s.
fn marker_type_args<const N: usize>(
    type_args: &[TypeId],
    mi: Option<&MonomorphInfo>,
    marker: &str,
) -> [TypeId; N] {
    let ta = if type_args.is_empty() {
        let mi = mi.unwrap_or_else(|| panic!("{marker} marker without type args or monomorph info"));
        if mi.method_type_args.is_empty() {
            &mi.impl_type_args
        } else {
            &mi.method_type_args
        }
    } else {
        type_args
    };
    *ta.first_chunk::<N>()
        .unwrap_or_else(|| panic!("{marker} marker expects {N} type args, got {ta:?}"))
}
