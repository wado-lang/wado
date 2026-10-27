//! Canonical ABI layout computation for the WebAssembly Component Model.
//!
//! This module provides pure functions for computing sizes, alignments, and
//! field offsets according to the Canonical ABI specification. It operates on
//! AST `Type`s (which is what `CmInterfaceRegistry` stores for WASI function signatures).
//!
//! Reference: <https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md>

use crate::ast::Type;
use crate::component_model::{CmInterfaceRegistry, cm_layout_with_registry};
use crate::name::UNIT_TYPE_NAME;

/// Round `offset` up to the next multiple of `align`.
pub fn align_to(offset: u32, align: u32) -> u32 {
    debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
    (offset + align - 1) & !(align - 1)
}

/// Canonical ABI size and alignment of the primitive `name`, `()` included.
pub fn primitive_size_align(name: &str) -> Option<(u32, u32)> {
    Some(match name {
        "bool" | "u8" | "i8" => (1, 1),
        "u16" | "i16" => (2, 2),
        "i32" | "u32" | "f32" | "char" => (4, 4),
        "i64" | "u64" | "f64" => (8, 8),
        UNIT_TYPE_NAME => (0, 1),
        _ => return None,
    })
}

/// Layout information for a composite type (record, tuple).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmLayout {
    /// Total size in bytes (including trailing padding for alignment).
    pub size: u32,
    /// Alignment in bytes.
    pub align: u32,
    /// Byte offset of each field/element.
    pub offsets: Vec<u32>,
}

impl CmLayout {
    pub fn size_align(&self) -> (u32, u32) {
        (self.size, self.align)
    }

    /// The offset every payload-bearing case shares, on a variant layout. A
    /// field layout's `offsets` are per field and have no such entry.
    pub fn payload_offset(&self) -> u32 {
        self.offsets[1]
    }
}

/// The case count `option` and `result` lay out with, so their discriminant
/// takes its width where every other variant's does.
pub const OPTION_OR_RESULT_CASES: usize = 2;

/// Lay fields out in order, each at its own alignment, the whole padded to the
/// widest. `size_align` is what reads a field's own size and alignment.
fn layout_fields_by<'a>(
    fields: impl Iterator<Item = &'a Type>,
    mut size_align: impl FnMut(&Type) -> (u32, u32),
) -> CmLayout {
    let mut offset: u32 = 0;
    let mut max_align: u32 = 1;
    let mut offsets = Vec::new();
    for ty in fields {
        let (field_size, field_align) = size_align(ty);
        offset = align_to(offset, field_align);
        offsets.push(offset);
        offset += field_size;
        max_align = max_align.max(field_align);
    }
    CmLayout {
        size: align_to(offset, max_align),
        align: max_align,
        offsets,
    }
}

/// Lay a variant out: the discriminant at offset 0, then every payload at one
/// shared offset after it. `size_align` is what reads a payload's own pair.
fn layout_variant_by<'a>(
    case_count: usize,
    payloads: impl Iterator<Item = &'a Type>,
    mut size_align: impl FnMut(&Type) -> (u32, u32),
) -> CmLayout {
    let mut payload_size = 0u32;
    let mut payload_align = 1u32;
    for ty in payloads {
        let (size, align) = size_align(ty);
        payload_size = payload_size.max(size);
        payload_align = payload_align.max(align);
    }
    let disc = cm_discriminant_byte_size(case_count);
    let align = disc.max(payload_align);
    let payload_offset = align_to(disc, payload_align);
    CmLayout {
        size: align_to(payload_offset + payload_size, align),
        align,
        offsets: vec![0, payload_offset],
    }
}

/// Layout for a variant: the discriminant `case_count` calls for, then the
/// payload-bearing cases at one shared offset. `offsets` is `[disc, payload]`.
/// A variant whose cases carry no payload lays out as its bare discriminant.
pub fn layout_variant_with_registry<'a>(
    case_count: usize,
    payloads: impl Iterator<Item = &'a Type>,
    registry: &CmInterfaceRegistry,
) -> CmLayout {
    layout_variant_by(case_count, payloads, |ty| {
        cm_layout_with_registry(ty, registry)
    })
}

/// Registry-aware layout for `option<T>`.
pub fn layout_option_with_registry(inner: &Type, registry: &CmInterfaceRegistry) -> CmLayout {
    layout_variant_with_registry(OPTION_OR_RESULT_CASES, std::iter::once(inner), registry)
}

/// Registry-aware layout for `result<T, E>`.
pub fn layout_result_with_registry(
    ok: &Type,
    err: &Type,
    registry: &CmInterfaceRegistry,
) -> CmLayout {
    layout_variant_with_registry(OPTION_OR_RESULT_CASES, [ok, err].into_iter(), registry)
}

/// The offset a variant's case payloads start at, for a caller needing no size.
pub fn variant_payload_offset_with_registry<'a>(
    case_count: usize,
    payloads: impl Iterator<Item = &'a Type>,
    registry: &CmInterfaceRegistry,
) -> u32 {
    layout_variant_with_registry(case_count, payloads, registry).payload_offset()
}

/// Layout for a tuple, each element resolved through the registry so a named
/// record, variant or newtype lands at its true offset. A record lays out the
/// same way, as a tuple of its field types.
pub fn layout_tuple_with_registry(elements: &[Type], registry: &CmInterfaceRegistry) -> CmLayout {
    layout_fields_with_registry(elements.iter(), registry)
}

/// Registry-aware layout over field types the caller already holds.
pub fn layout_fields_with_registry<'a>(
    fields: impl Iterator<Item = &'a Type>,
    registry: &CmInterfaceRegistry,
) -> CmLayout {
    layout_fields_by(fields, |ty| cm_layout_with_registry(ty, registry))
}

/// CM Canonical ABI byte size for a flags type given its label count.
/// Per the CM spec: ≤8 labels → 1 byte, ≤16 → 2 bytes, >16 → ceil(n/32)*4 bytes.
pub fn cm_flags_byte_size(count: usize) -> u32 {
    if count == 0 {
        0
    } else if count <= 8 {
        1
    } else if count <= 16 {
        2
    } else {
        4 * (count as u32).div_ceil(32)
    }
}

/// CM Canonical ABI alignment for a flags type given its label count.
pub fn cm_flags_byte_align(count: usize) -> u32 {
    if count <= 8 {
        1
    } else if count <= 16 {
        2
    } else {
        4
    }
}

/// CM Canonical ABI byte size of the discriminant an enum or variant with
/// `count` cases carries. Per the CM spec `discriminant_type`.
pub fn cm_discriminant_byte_size(count: usize) -> u32 {
    if count <= 256 {
        1
    } else if count <= 65536 {
        2
    } else {
        4
    }
}

/// Compute the flat (core Wasm) parameter types for a Canonical ABI type.
///
/// This is used for lowered function signatures: compound types are flattened
/// into sequences of core value types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmValType {
    I32,
    I64,
    F32,
    F64,
}

impl CmValType {
    /// Join two value types for variant union flattening, per the Canonical ABI
    /// `join` operation: equal types stay; `{i32, f32}` (either order) widens to
    /// `i32`; any other mismatch widens to `i64`. This is order-independent —
    /// picking the "larger" type is wrong, e.g. `join(f32, i32)` must be `i32`,
    /// not `f32`.
    pub fn join(a: Option<Self>, b: Option<Self>) -> Self {
        match (a, b) {
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => Self::I32,
            (Some(a), Some(b)) if a == b => a,
            (Some(a), Some(b)) if a.size() == 4 && b.size() == 4 => Self::I32,
            (Some(_), Some(_)) => Self::I64,
        }
    }

    const fn size(self) -> u32 {
        match self {
            Self::I32 | Self::F32 => 4,
            Self::I64 | Self::F64 => 8,
        }
    }
}

/// Join two flat-type unions slot by slot, per the Canonical ABI variant
/// flattening: the union has `max(len)` slots, each the [`CmValType::join`]
/// of the two sides' slots at that position.
pub fn join_flat_unions(a: &[CmValType], b: &[CmValType]) -> Vec<CmValType> {
    (0..a.len().max(b.len()))
        .map(|i| CmValType::join(a.get(i).copied(), b.get(i).copied()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AstId, NamedType};
    use crate::compiler_item::CompilerItem;
    use crate::token::Span;

    fn span() -> Span {
        Span::new(0, 0, 1, 1)
    }

    fn named_type(name: &str) -> Type {
        Type::Named(NamedType::new(AstId::fresh(), name.to_string(), span()))
    }

    fn unit_type() -> Type {
        Type::unit(AstId::fresh(), span())
    }

    /// The bundled bindings' registry, which builds a `String`, `List`,
    /// `Option` or `Result` as a site naming its declaration.
    struct Abi(CmInterfaceRegistry);

    impl Abi {
        fn new() -> Self {
            Self((*CmInterfaceRegistry::build_from_stdlib().0).clone())
        }

        fn item(&self, item: CompilerItem, args: Vec<Type>) -> Type {
            self.0.type_decls().item_type(item, args, span())
        }

        fn string(&self) -> Type {
            self.item(CompilerItem::String, vec![])
        }

        fn list(&self, elem: Type) -> Type {
            self.item(CompilerItem::List, vec![elem])
        }

        fn option(&self, inner: Type) -> Type {
            self.item(CompilerItem::Option, vec![inner])
        }

        fn result(&self, ok: Type, err: Type) -> Type {
            self.item(CompilerItem::Result, vec![ok, err])
        }

        fn size_align(&self, ty: &Type) -> (u32, u32) {
            cm_layout_with_registry(ty, &self.0)
        }
    }

    #[test]
    fn test_canonical_join() {
        use CmValType::*;
        assert_eq!(CmValType::join(Some(I32), Some(I32)), I32);
        assert_eq!(CmValType::join(Some(F64), Some(F64)), F64);
        // {i32, f32} widens to i32 — order-independent (the bug was f32-first).
        assert_eq!(CmValType::join(Some(I32), Some(F32)), I32);
        assert_eq!(CmValType::join(Some(F32), Some(I32)), I32);
        assert_eq!(CmValType::join(Some(I64), Some(F64)), I64);
        assert_eq!(CmValType::join(Some(F64), Some(I64)), I64);
        assert_eq!(CmValType::join(Some(I32), Some(I64)), I64);
        assert_eq!(CmValType::join(Some(F64), Some(I32)), I64);
        assert_eq!(CmValType::join(Some(F32), None), F32);
        assert_eq!(CmValType::join(None, Some(I64)), I64);
        assert_eq!(CmValType::join(None, None), I32);
    }

    #[test]
    fn test_primitive_layouts() {
        let abi = Abi::new();
        for (name, expected) in [
            ("bool", (1, 1)),
            ("u8", (1, 1)),
            ("i8", (1, 1)),
            ("u16", (2, 2)),
            ("i16", (2, 2)),
            ("i32", (4, 4)),
            ("u32", (4, 4)),
            ("f32", (4, 4)),
            ("char", (4, 4)),
            ("i64", (8, 8)),
            ("u64", (8, 8)),
            ("f64", (8, 8)),
        ] {
            assert_eq!(abi.size_align(&named_type(name)), expected, "{name}");
        }
    }

    #[test]
    fn test_string_and_list_are_ptr_len() {
        // string = list<u8> = (ptr: i32, len: i32), whatever the element
        let abi = Abi::new();
        let string = abi.string();
        assert_eq!(abi.size_align(&string), (8, 4));
        let list_i32 = abi.list(named_type("i32"));
        assert_eq!(abi.size_align(&list_i32), (8, 4));
        let option_string = abi.option(string);
        let list_option_string = abi.list(option_string);
        assert_eq!(abi.size_align(&list_option_string), (8, 4));
    }

    #[test]
    fn test_unit_size() {
        let abi = Abi::new();
        assert_eq!(abi.size_align(&unit_type()), (0, 1));
        assert_eq!(abi.size_align(&Type::Tuple(vec![])), (0, 1));
    }

    #[test]
    fn test_align_to() {
        assert_eq!(align_to(0, 4), 0);
        assert_eq!(align_to(1, 4), 4);
        assert_eq!(align_to(3, 4), 4);
        assert_eq!(align_to(4, 4), 4);
        assert_eq!(align_to(5, 4), 8);
        assert_eq!(align_to(0, 1), 0);
        assert_eq!(align_to(7, 1), 7);
        assert_eq!(align_to(1, 8), 8);
        assert_eq!(align_to(9, 8), 16);
    }

    /// Every discriminant the compiler loads, stores or lays out takes its
    /// width from here, so a boundary that slips moves all four at once.
    #[test]
    fn a_discriminant_widens_at_the_spec_boundaries() {
        assert_eq!(cm_discriminant_byte_size(0), 1);
        assert_eq!(cm_discriminant_byte_size(256), 1);
        assert_eq!(cm_discriminant_byte_size(257), 2);
        assert_eq!(cm_discriminant_byte_size(65536), 2);
        assert_eq!(cm_discriminant_byte_size(65537), 4);
    }

    /// A variant past 256 cases carries a two-byte discriminant, which also
    /// pushes its payload to the offset that width calls for.
    #[test]
    fn a_wide_discriminant_moves_the_payload() {
        let abi = Abi::new();
        let payload = named_type("i32");
        let layout = layout_variant_with_registry(257, std::iter::once(&payload), &abi.0);
        assert_eq!(layout.align, 4);
        assert_eq!(layout.offsets, vec![0, 4]);
        assert_eq!(layout.size, 8);
    }

    #[test]
    fn test_option_layouts() {
        let abi = Abi::new();
        let string = abi.string();
        let list_u8 = abi.list(named_type("u8"));
        // The payload sits at its own alignment past the one-byte discriminant.
        for (inner, expected, payload_offset) in [
            (named_type("i32"), (8, 4), 4),
            (named_type("bool"), (2, 1), 1),
            (named_type("i64"), (16, 8), 8),
            (string, (12, 4), 4),
            (list_u8, (12, 4), 4),
        ] {
            let layout = layout_option_with_registry(&inner, &abi.0);
            assert_eq!(layout.size_align(), expected, "{inner:?}");
            assert_eq!(layout.offsets, vec![0, payload_offset], "{inner:?}");
            let option = abi.option(inner);
            assert_eq!(abi.size_align(&option), expected);
        }
    }

    #[test]
    fn test_result_layouts() {
        let abi = Abi::new();
        let string = abi.string();
        let option_string = abi.option(string.clone());
        for (ok, err, expected) in [
            (unit_type(), unit_type(), (1, 1)),
            (named_type("i32"), named_type("i32"), (8, 4)),
            (string, named_type("i32"), (12, 4)),
            (option_string, named_type("i32"), (16, 4)),
        ] {
            let layout = layout_result_with_registry(&ok, &err, &abi.0);
            assert_eq!(layout.size_align(), expected, "{ok:?}, {err:?}");
            let result = abi.result(ok, err);
            assert_eq!(abi.size_align(&result), expected);
        }
        let unit = layout_result_with_registry(&unit_type(), &unit_type(), &abi.0);
        assert_eq!(unit.offsets, vec![0, 1]);
    }

    #[test]
    fn test_tuple_layouts() {
        let abi = Abi::new();
        let string = abi.string();
        // A record lays out as the tuple of its field types.
        for (elems, expected, offsets) in [
            (
                vec![named_type("i32"), named_type("i32")],
                (8, 4),
                vec![0, 4],
            ),
            (vec![string.clone(), string.clone()], (16, 4), vec![0, 8]),
            (
                vec![named_type("bool"), named_type("i32")],
                (8, 4),
                vec![0, 4],
            ),
            (
                vec![named_type("i32"), named_type("bool")],
                (8, 4),
                vec![0, 4],
            ),
            (
                vec![named_type("u8"), named_type("u16"), named_type("u32")],
                (8, 4),
                vec![0, 2, 4],
            ),
            (
                vec![named_type("bool"), named_type("i64")],
                (16, 8),
                vec![0, 8],
            ),
            (vec![string, named_type("i32")], (12, 4), vec![0, 8]),
            (
                vec![named_type("i32"), named_type("u64")],
                (16, 8),
                vec![0, 8],
            ),
        ] {
            let layout = layout_tuple_with_registry(&elems, &abi.0);
            assert_eq!(layout.size_align(), expected, "{elems:?}");
            assert_eq!(layout.offsets, offsets, "{elems:?}");
            assert_eq!(abi.size_align(&Type::Tuple(elems)), expected);
        }
    }
}
