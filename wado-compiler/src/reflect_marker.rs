//! The per-type bridge helper a reflection marker (`builtin::struct_field_get`,
//! `builtin::variant_case_extract`, …) calls. `lower` rewrites each marker into
//! a call of it, and `prelower_reach` follows the marker to it, so the two read
//! one spelling and a bridge of a kind [`MARKERS`] lists is lowered only where a
//! marker reaches it.

use crate::module_source::ModuleSource;
use crate::name::MarkerBridge;
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

/// What a marker's type arguments name besides the head type.
#[derive(Clone, Copy)]
enum Shape {
    /// A member type too: a field read names the struct and the field type, a
    /// hole read the shape and the hole type, a case bridge the variant and the
    /// payload type.
    Pair,
    /// The template shape alone.
    Head,
}

/// Every marker `lower` rewrites into a bridge call, and the bridge it calls.
/// `builtin::variant_tag` is not one: which helper it reads depends on the
/// boxing `lower` plans, so its bridge stays a root.
const MARKERS: &[(&str, MarkerBridge, Shape)] = &[
    ("struct_field_get", MarkerBridge::FieldGet, Shape::Pair),
    ("struct_field_ref", MarkerBridge::FieldRef, Shape::Pair),
    ("hole_get", MarkerBridge::HoleGet, Shape::Pair),
    ("hole_ref", MarkerBridge::HoleRef, Shape::Pair),
    (
        "variant_case_extract",
        MarkerBridge::CaseExtract,
        Shape::Pair,
    ),
    ("variant_case_ref", MarkerBridge::CaseRef, Shape::Pair),
    (
        "variant_case_construct",
        MarkerBridge::CaseConstruct,
        Shape::Pair,
    ),
    ("hole_fmt", MarkerBridge::HoleFmt, Shape::Head),
    (
        "template_segment",
        MarkerBridge::TemplateSegment,
        Shape::Head,
    ),
];

/// Whether `name` is a bridge only a [`MARKERS`] marker calls, so a walk that
/// follows the markers reaches it without rooting it.
#[must_use]
pub(crate) fn is_marker_bridge(name: &str) -> bool {
    MARKERS
        .iter()
        .any(|(_, bridge, _)| name.starts_with(bridge.prefix()))
}

/// The bridge `func` calls, if it is one of [`MARKERS`].
pub(crate) fn bridge_callee(
    tt: &TypeTable,
    func: &FunctionRef,
    type_args: &[TypeId],
) -> Option<BridgeCallee> {
    let intrinsic = func.intrinsic()?;
    let &(marker, bridge, shape) = MARKERS.iter().find(|(m, ..)| *m == intrinsic)?;
    let mi = func.monomorph_info.as_ref();
    let (head, name) = match shape {
        Shape::Pair => {
            let [head, member] = marker_type_args::<2>(type_args, mi, marker);
            let name = bridge.pair_name(
                &tt.mangle_type_arg_unboxed(head),
                &tt.mangle_type_arg_unboxed(member),
            );
            (head, name)
        }
        Shape::Head => {
            let [shape] = marker_type_args::<1>(type_args, mi, marker);
            (shape, bridge.shape_name(&tt.mangle_type_arg_unboxed(shape)))
        }
    };
    Some(BridgeCallee::homed(tt, head, name, marker))
}

/// The `N` type args a bridge marker names: the call's own, else the method's,
/// else the `impl`'s.
fn marker_type_args<const N: usize>(
    type_args: &[TypeId],
    mi: Option<&MonomorphInfo>,
    marker: &str,
) -> [TypeId; N] {
    let ta = if type_args.is_empty() {
        let mi =
            mi.unwrap_or_else(|| panic!("{marker} marker without type args or monomorph info"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::name::{
        case_construct_helper_name, case_extract_helper_name, case_ref_helper_name,
        field_get_helper_name, field_ref_helper_name, hole_fmt_helper_name, hole_get_helper_name,
        hole_ref_helper_name, is_type_bridge, template_segment_helper_name,
        variant_tag_helper_name,
    };

    #[test]
    fn every_reflect_bridge_is_reached_through_its_marker() {
        let names = [
            field_get_helper_name("S", "F"),
            field_ref_helper_name("S", "F"),
            hole_get_helper_name("T", "V"),
            hole_ref_helper_name("T", "V"),
            case_extract_helper_name("V", "P"),
            case_ref_helper_name("V", "P"),
            case_construct_helper_name("V", "P"),
            hole_fmt_helper_name("T"),
            template_segment_helper_name("T"),
        ];
        for name in names {
            assert!(is_type_bridge(&name), "{name}");
            assert!(is_marker_bridge(&name), "{name}");
        }
    }

    #[test]
    fn a_variant_tag_bridge_stays_rooted() {
        let name = variant_tag_helper_name("V");
        assert!(is_type_bridge(&name));
        assert!(!is_marker_bridge(&name));
    }
}
