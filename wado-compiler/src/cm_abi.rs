//! Canonical ABI layout computation for the WebAssembly Component Model.
//!
//! This module provides pure functions for computing sizes, alignments, and
//! field offsets according to the Canonical ABI specification. It operates on
//! AST `Type`s (which is what `WasiRegistry` stores for WASI function signatures).
//!
//! Reference: <https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md>

#[cfg(test)]
use crate::ast::{AstId, NamedType};
use crate::ast::{GenericType, Type};
#[cfg(test)]
use crate::token::Span;

/// Round `offset` up to the next multiple of `align`.
pub fn align_to(offset: u32, align: u32) -> u32 {
    debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
    (offset + align - 1) & !(align - 1)
}

/// Canonical ABI size in bytes for a given type.
pub fn cm_size(ty: &Type) -> u32 {
    match ty {
        Type::Named(named) => cm_size_named(&named.name),
        Type::Generic(generic) => cm_size_generic(generic),
        Type::Tuple(elems) => {
            if elems.is_empty() {
                return 0; // unit type
            }
            let layout = layout_tuple(elems);
            layout.size
        }
        Type::Reference(_) | Type::MutReference(_) => 4, // resource handle
        _ => 0,
    }
}

/// Canonical ABI alignment in bytes for a given type.
pub fn cm_align(ty: &Type) -> u32 {
    match ty {
        Type::Named(named) => cm_align_named(&named.name),
        Type::Generic(generic) => cm_align_generic(generic),
        Type::Tuple(elems) => {
            if elems.is_empty() {
                return 1; // unit type
            }
            elems.iter().map(cm_align).max().unwrap_or(1)
        }
        Type::Reference(_) | Type::MutReference(_) => 4, // resource handle
        _ => 1,
    }
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

/// Compute the layout for a record (struct with named fields).
pub fn layout_record(fields: &[(&str, &Type)]) -> CmLayout {
    let types: Vec<&Type> = fields.iter().map(|(_, ty)| *ty).collect();
    compute_layout(&types)
}

/// Compute the layout for a tuple (positional elements).
pub fn layout_tuple(elements: &[Type]) -> CmLayout {
    let types: Vec<&Type> = elements.iter().collect();
    compute_layout(&types)
}

/// Core layout computation: given a list of field types, compute offsets.
fn compute_layout(types: &[&Type]) -> CmLayout {
    let mut offset: u32 = 0;
    let mut max_align: u32 = 1;
    let mut offsets = Vec::with_capacity(types.len());

    for ty in types {
        let field_align = cm_align(ty);
        let field_size = cm_size(ty);

        // Align offset for this field
        offset = align_to(offset, field_align);
        offsets.push(offset);

        offset += field_size;
        max_align = max_align.max(field_align);
    }

    // Trailing padding to satisfy overall alignment
    let size = align_to(offset, max_align);

    CmLayout {
        size,
        align: max_align,
        offsets,
    }
}

/// Canonical ABI size for named (primitive) types.
fn cm_size_named(name: &str) -> u32 {
    match name {
        "bool" | "u8" | "i8" => 1,
        "u16" | "i16" => 2,
        "i32" | "u32" | "f32" | "char" => 4,
        "i64" | "u64" | "f64" => 8,
        "String" => 8, // (ptr: i32, len: i32)
        "()" => 0,     // unit
        // Unknown named types (enums, resources) are i32 handles
        _ => 4,
    }
}

/// Canonical ABI alignment for named (primitive) types.
fn cm_align_named(name: &str) -> u32 {
    match name {
        "bool" | "u8" | "i8" => 1,
        "u16" | "i16" => 2,
        "i32" | "u32" | "f32" | "char" => 4,
        "i64" | "u64" | "f64" => 8,
        "String" => 4, // (ptr: i32, len: i32) — aligned to i32
        "()" => 1,     // unit
        // Unknown named types (enums, resources) are i32 handles
        _ => 4,
    }
}

/// Canonical ABI size for generic types.
fn cm_size_generic(generic: &GenericType) -> u32 {
    match generic.name.as_str() {
        // list<T> is (ptr: i32, len: i32)
        "Array" => 8,
        // option<T>: discriminant byte + padding + payload
        "Option" if generic.args.len() == 1 => {
            let inner = &generic.args[0];
            let payload_align = cm_align(inner);
            let payload_size = cm_size(inner);
            // discriminant (1 byte) + padding to payload alignment + payload
            let payload_offset = align_to(1, payload_align);
            align_to(payload_offset + payload_size, cm_align_option(inner))
        }
        // result<T, E>: discriminant (u8, 1 byte) + max(ok_size, err_size)
        // Result is a variant with 2 cases; CM spec discriminant is u8.
        "Result" if generic.args.len() == 2 => {
            let ok_size = cm_size(&generic.args[0]);
            let err_size = cm_size(&generic.args[1]);
            let payload_size = ok_size.max(err_size);
            let payload_align = cm_align(&generic.args[0]).max(cm_align(&generic.args[1]));
            let payload_offset = align_to(1, payload_align); // after u8 discriminant
            align_to(
                payload_offset + payload_size,
                cm_align_result(&generic.args[0], &generic.args[1]),
            )
        }
        // Tuple<T, U, ...>
        "Tuple" => {
            let layout = layout_tuple(&generic.args);
            layout.size
        }
        // Stream<T>, Future<T> are i32 handles
        "Stream" | "Future" => 4,
        // Own<T>, Borrow<T> are i32 handles
        "Own" | "Borrow" => 4,
        _ => 4,
    }
}

/// Canonical ABI alignment for generic types.
fn cm_align_generic(generic: &GenericType) -> u32 {
    match generic.name.as_str() {
        "Array" => 4, // (ptr: i32, len: i32) — aligned to i32
        "Option" if generic.args.len() == 1 => cm_align_option(&generic.args[0]),
        "Result" if generic.args.len() == 2 => cm_align_result(&generic.args[0], &generic.args[1]),
        "Tuple" => generic.args.iter().map(cm_align).max().unwrap_or(1),
        "Stream" | "Future" | "Own" | "Borrow" => 4,
        _ => 4,
    }
}

/// Alignment for option<T>: max(1, align(T))
fn cm_align_option(inner: &Type) -> u32 {
    1_u32.max(cm_align(inner))
}

/// Alignment for result<T, E>: max(1, align(T), align(E))
/// Result is a variant with 2 cases; discriminant is u8 (1 byte).
fn cm_align_result(ok: &Type, err: &Type) -> u32 {
    1_u32.max(cm_align(ok)).max(cm_align(err))
}

/// Layout for option<T>: discriminant offset + payload offset.
pub fn layout_option(inner: &Type) -> CmLayout {
    let payload_align = cm_align(inner);
    let payload_size = cm_size(inner);
    let overall_align = cm_align_option(inner);

    // discriminant at offset 0 (1 byte)
    let payload_offset = align_to(1, payload_align);
    let size = align_to(payload_offset + payload_size, overall_align);

    CmLayout {
        size,
        align: overall_align,
        offsets: vec![0, payload_offset], // [discriminant, payload]
    }
}

/// Layout for result<T, E>: discriminant (i32) + payload.
pub fn layout_result(ok: &Type, err: &Type) -> CmLayout {
    let payload_align = cm_align(ok).max(cm_align(err));
    let payload_size = cm_size(ok).max(cm_size(err));
    let overall_align = cm_align_result(ok, err);

    // Result is a variant with 2 cases; CM spec discriminant is u8 (1 byte).
    let disc_size = 1u32;
    let payload_offset = align_to(disc_size, payload_align);
    let size = align_to(payload_offset + payload_size, overall_align);

    CmLayout {
        size,
        align: overall_align,
        offsets: vec![0, payload_offset], // [discriminant, payload]
    }
}

/// Registry-aware layout for option<T>.
pub fn layout_option_with_registry(
    inner: &Type,
    registry: &crate::component_model::WasiRegistry,
) -> CmLayout {
    layout_option_with_registry_scoped(inner, registry, None)
}

/// Package-scoped registry-aware layout for option<T>.
pub fn layout_option_with_registry_scoped(
    inner: &Type,
    registry: &crate::component_model::WasiRegistry,
    wasi_package: Option<&str>,
) -> CmLayout {
    let payload_align =
        crate::component_model::cm_align_with_registry_scoped(inner, registry, wasi_package);
    let payload_size =
        crate::component_model::cm_size_with_registry_scoped(inner, registry, wasi_package);
    let overall_align = 1u32.max(payload_align);
    let payload_offset = align_to(1, payload_align);
    let size = align_to(payload_offset + payload_size, overall_align);
    CmLayout {
        size,
        align: overall_align,
        offsets: vec![0, payload_offset],
    }
}

/// Registry-aware layout for result<T, E>.
pub fn layout_result_with_registry(
    ok: &Type,
    err: &Type,
    registry: &crate::component_model::WasiRegistry,
) -> CmLayout {
    layout_result_with_registry_scoped(ok, err, registry, None)
}

/// Package-scoped registry-aware layout for result<T, E>.
pub fn layout_result_with_registry_scoped(
    ok: &Type,
    err: &Type,
    registry: &crate::component_model::WasiRegistry,
    wasi_package: Option<&str>,
) -> CmLayout {
    let payload_align =
        crate::component_model::cm_align_with_registry_scoped(ok, registry, wasi_package).max(
            crate::component_model::cm_align_with_registry_scoped(err, registry, wasi_package),
        );
    let payload_size =
        crate::component_model::cm_size_with_registry_scoped(ok, registry, wasi_package).max(
            crate::component_model::cm_size_with_registry_scoped(err, registry, wasi_package),
        );
    let overall_align = 1u32.max(payload_align);
    let disc_size = 1u32;
    let payload_offset = align_to(disc_size, payload_align);
    let size = align_to(payload_offset + payload_size, overall_align);
    CmLayout {
        size,
        align: overall_align,
        offsets: vec![0, payload_offset],
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
    /// Join two value types for union flattening (Canonical ABI `join` operation).
    /// When one side is absent, uses the other. When both present, picks the larger type.
    pub fn join(a: Option<Self>, b: Option<Self>) -> Self {
        match (a, b) {
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => Self::I32,
            (Some(a), Some(b)) => {
                if a.size() >= b.size() {
                    a
                } else {
                    b
                }
            }
        }
    }

    const fn size(self) -> u32 {
        match self {
            Self::I32 | Self::F32 => 4,
            Self::I64 | Self::F64 => 8,
        }
    }
}

/// Flatten a type into its core Wasm value types for the flat ABI.
///
/// Compound types like String and list<T> are lowered to (i32, i32) pairs.
/// Simple types map to their corresponding core value type.
pub fn cm_flat_types(ty: &Type) -> Vec<CmValType> {
    let mut out = Vec::new();
    flatten_type(ty, &mut out);
    out
}

fn flatten_type(ty: &Type, out: &mut Vec<CmValType>) {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "bool" | "u8" | "i8" | "u16" | "i16" | "i32" | "u32" | "char" => {
                out.push(CmValType::I32);
            }
            "i64" | "u64" => out.push(CmValType::I64),
            "f32" => out.push(CmValType::F32),
            "f64" => out.push(CmValType::F64),
            "String" => {
                out.push(CmValType::I32); // ptr
                out.push(CmValType::I32); // len
            }
            "()" => {} // unit — no values
            // Resource handles, enums — i32
            _ => out.push(CmValType::I32),
        },
        Type::Generic(generic) => match generic.name.as_str() {
            "Array" => {
                out.push(CmValType::I32); // ptr
                out.push(CmValType::I32); // len
            }
            "Stream" | "Future" | "Own" | "Borrow" => out.push(CmValType::I32),
            "Option" if generic.args.len() == 1 => {
                // option<T> flattens to: discriminant i32 + flatten(T)
                out.push(CmValType::I32);
                flatten_type(&generic.args[0], out);
            }
            "Result" if generic.args.len() == 2 => {
                // result<T, E> flattens to: discriminant i32 + union(flatten(T), flatten(E))
                out.push(CmValType::I32);
                let mut ok_flat = Vec::new();
                let mut err_flat = Vec::new();
                flatten_type(&generic.args[0], &mut ok_flat);
                flatten_type(&generic.args[1], &mut err_flat);
                // Union: take the longer list, using join on overlapping positions
                let max_len = ok_flat.len().max(err_flat.len());
                for i in 0..max_len {
                    let ok_val = ok_flat.get(i).copied();
                    let err_val = err_flat.get(i).copied();
                    out.push(CmValType::join(ok_val, err_val));
                }
            }
            _ => out.push(CmValType::I32),
        },
        Type::Reference(_) | Type::MutReference(_) => out.push(CmValType::I32),
        Type::Tuple(elems) => {
            if elems.is_empty() {
                return; // unit
            }
            for elem in elems {
                flatten_type(elem, out);
            }
        }
        _ => {}
    }
}

/// For tuple return types, return the list of `CmPrimitiveType`s.
/// Returns `None` if the type is not a tuple or not all elements are primitives.
pub fn cm_tuple_primitive_types(ty: &Type) -> Option<Vec<crate::component_model::CmPrimitiveType>> {
    use crate::component_model::CmPrimitiveType;
    let elements = match ty {
        Type::Tuple(elems) if !elems.is_empty() => elems,
        Type::Generic(g) if g.name == "Tuple" && !g.args.is_empty() => &g.args,
        _ => return None,
    };
    let mut prims = Vec::new();
    for elem in elements {
        match elem {
            Type::Named(n) => match n.name.as_str() {
                "i32" => prims.push(CmPrimitiveType::I32),
                "i64" => prims.push(CmPrimitiveType::I64),
                "u32" => prims.push(CmPrimitiveType::U32),
                "u64" => prims.push(CmPrimitiveType::U64),
                "f32" => prims.push(CmPrimitiveType::F32),
                "f64" => prims.push(CmPrimitiveType::F64),
                // Resource types, enums → i32 handle in CM ABI
                _ => prims.push(CmPrimitiveType::I32),
            },
            // Future, Stream, Own, Borrow → i32 handle in CM ABI
            Type::Generic(g)
                if matches!(g.name.as_str(), "Future" | "Stream" | "Own" | "Borrow") =>
            {
                prims.push(CmPrimitiveType::I32);
            }
            _ => return None,
        }
    }
    Some(prims)
}

/// For result<T, E> return types, return `(ok_is_resource, err_is_enum)`.
/// Returns `None` if the type is not a Result.
pub fn cm_result_return_info(ty: &Type) -> Option<(bool, bool)> {
    let Type::Generic(g) = ty else { return None };
    if g.name != "Result" || g.args.len() != 2 {
        return None;
    }
    let ok_type = &g.args[0];
    let err_type = &g.args[1];

    // Both unit → still a result but with no payload
    let ok_is_resource = matches!(ok_type, Type::Named(n) if !matches!(
        n.name.as_str(),
        "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "f32" | "f64" | "bool" | "char" | "String" | "()"
    ));
    let err_is_enum = matches!(err_type, Type::Named(n) if !matches!(
        n.name.as_str(),
        "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "f32" | "f64" | "bool" | "char" | "String" | "()"
    ));

    Some((ok_is_resource, err_is_enum))
}

/// Helper: create a `Type::Named` with a dummy span. Useful for tests.
#[cfg(test)]
pub(crate) fn named_type(name: &str) -> Type {
    Type::Named(NamedType {
        id: AstId::fresh(),
        name: name.to_string(),
        span: Span::new(0, 0, 1, 1),
        source_interface: None,
    })
}

/// Helper: create a `Type::Generic` with a dummy span. Useful for tests.
#[cfg(test)]
pub(crate) fn generic_type(name: &str, args: Vec<Type>) -> Type {
    Type::Generic(GenericType {
        id: AstId::fresh(),
        name: name.to_string(),
        args,
        span: Span::new(0, 0, 1, 1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_primitive_sizes() {
        assert_eq!(cm_size(&named_type("bool")), 1);
        assert_eq!(cm_size(&named_type("u8")), 1);
        assert_eq!(cm_size(&named_type("i8")), 1);
        assert_eq!(cm_size(&named_type("u16")), 2);
        assert_eq!(cm_size(&named_type("i16")), 2);
        assert_eq!(cm_size(&named_type("i32")), 4);
        assert_eq!(cm_size(&named_type("u32")), 4);
        assert_eq!(cm_size(&named_type("f32")), 4);
        assert_eq!(cm_size(&named_type("char")), 4);
        assert_eq!(cm_size(&named_type("i64")), 8);
        assert_eq!(cm_size(&named_type("u64")), 8);
        assert_eq!(cm_size(&named_type("f64")), 8);
    }

    #[test]
    fn test_string_size() {
        // String = list<u8> = (ptr: i32, len: i32)
        assert_eq!(cm_size(&named_type("String")), 8);
        assert_eq!(cm_align(&named_type("String")), 4);
    }

    #[test]
    fn test_list_size() {
        // list<T> = (ptr: i32, len: i32) regardless of T
        let list_i32 = generic_type("Array", vec![named_type("i32")]);
        assert_eq!(cm_size(&list_i32), 8);
        assert_eq!(cm_align(&list_i32), 4);

        let list_string = generic_type("Array", vec![named_type("String")]);
        assert_eq!(cm_size(&list_string), 8);
        assert_eq!(cm_align(&list_string), 4);
    }

    #[test]
    fn test_unit_size() {
        assert_eq!(cm_size(&named_type("()")), 0);
        assert_eq!(cm_size(&Type::Tuple(vec![])), 0);
    }

    #[test]
    fn test_primitive_alignments() {
        assert_eq!(cm_align(&named_type("bool")), 1);
        assert_eq!(cm_align(&named_type("u8")), 1);
        assert_eq!(cm_align(&named_type("i16")), 2);
        assert_eq!(cm_align(&named_type("i32")), 4);
        assert_eq!(cm_align(&named_type("f32")), 4);
        assert_eq!(cm_align(&named_type("i64")), 8);
        assert_eq!(cm_align(&named_type("f64")), 8);
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

    #[test]
    fn test_option_i32() {
        // option<i32>: disc(1 byte) + 3 padding + i32(4 bytes) = 8 bytes, align 4
        let opt = generic_type("Option", vec![named_type("i32")]);
        assert_eq!(cm_size(&opt), 8);
        assert_eq!(cm_align(&opt), 4);

        let layout = layout_option(&named_type("i32"));
        assert_eq!(layout.size, 8);
        assert_eq!(layout.align, 4);
        assert_eq!(layout.offsets, vec![0, 4]); // disc at 0, payload at 4
    }

    #[test]
    fn test_option_bool() {
        // option<bool>: disc(1 byte) + bool(1 byte) = 2 bytes, align 1
        let opt = generic_type("Option", vec![named_type("bool")]);
        assert_eq!(cm_size(&opt), 2);
        assert_eq!(cm_align(&opt), 1);

        let layout = layout_option(&named_type("bool"));
        assert_eq!(layout.size, 2);
        assert_eq!(layout.align, 1);
        assert_eq!(layout.offsets, vec![0, 1]);
    }

    #[test]
    fn test_option_string() {
        // option<string>: disc(1 byte) + 3 padding + string(8 bytes) = 12 bytes, align 4
        let opt = generic_type("Option", vec![named_type("String")]);
        assert_eq!(cm_size(&opt), 12);
        assert_eq!(cm_align(&opt), 4);

        let layout = layout_option(&named_type("String"));
        assert_eq!(layout.size, 12);
        assert_eq!(layout.align, 4);
        assert_eq!(layout.offsets, vec![0, 4]); // disc at 0, payload at 4
    }

    #[test]
    fn test_option_i64() {
        // option<i64>: disc(1 byte) + 7 padding + i64(8 bytes) = 16 bytes, align 8
        let opt = generic_type("Option", vec![named_type("i64")]);
        assert_eq!(cm_size(&opt), 16);
        assert_eq!(cm_align(&opt), 8);

        let layout = layout_option(&named_type("i64"));
        assert_eq!(layout.size, 16);
        assert_eq!(layout.align, 8);
        assert_eq!(layout.offsets, vec![0, 8]);
    }

    #[test]
    fn test_option_resource() {
        // option<own<resource>>: disc(1 byte) + 3 padding + handle(4 bytes) = 8 bytes, align 4
        let own_resource = generic_type("Own", vec![named_type("Fields")]);
        let opt = generic_type("Option", vec![own_resource]);
        assert_eq!(cm_size(&opt), 8);
        assert_eq!(cm_align(&opt), 4);
    }

    #[test]
    fn test_result_unit_unit() {
        // result<(), ()>: disc(u8, 1 byte) + max(0, 0) = 1 byte, align 1
        let result = generic_type("Result", vec![named_type("()"), named_type("()")]);
        assert_eq!(cm_size(&result), 1);
        assert_eq!(cm_align(&result), 1);

        let layout = layout_result(&named_type("()"), &named_type("()"));
        assert_eq!(layout.size, 1);
        assert_eq!(layout.align, 1);
        assert_eq!(layout.offsets, vec![0, 1]); // disc at 0, payload at 1
    }

    #[test]
    fn test_result_i32_i32() {
        // result<i32, i32>: disc(u8, 1 byte) + 3 pad + max(4, 4) = 8 bytes, align 4
        let result = generic_type("Result", vec![named_type("i32"), named_type("i32")]);
        assert_eq!(cm_size(&result), 8);
        assert_eq!(cm_align(&result), 4);

        let layout = layout_result(&named_type("i32"), &named_type("i32"));
        assert_eq!(layout.size, 8);
        assert_eq!(layout.align, 4);
        assert_eq!(layout.offsets, vec![0, 4]);
    }

    #[test]
    fn test_result_resource_enum() {
        // result<Resource, ErrorCode>: disc(u8) + 3 pad + max(4, 4) = 8 bytes, align 4
        // Resource and ErrorCode are both i32 handles/discriminants
        let result = generic_type(
            "Result",
            vec![named_type("IncomingBody"), named_type("ErrorCode")],
        );
        assert_eq!(cm_size(&result), 8);
        assert_eq!(cm_align(&result), 4);
    }

    #[test]
    fn test_result_string_i32() {
        // result<string, i32>: disc(u8) + 3 pad + max(string=8, i32=4) = 12 bytes, align 4
        let result = generic_type("Result", vec![named_type("String"), named_type("i32")]);
        assert_eq!(cm_size(&result), 12);
        assert_eq!(cm_align(&result), 4);
    }

    #[test]
    fn test_tuple_i32_i32() {
        // tuple<i32, i32>: 4 + 4 = 8 bytes, align 4
        let tuple = Type::Tuple(vec![named_type("i32"), named_type("i32")]);
        assert_eq!(cm_size(&tuple), 8);
        assert_eq!(cm_align(&tuple), 4);

        let layout = layout_tuple(&[named_type("i32"), named_type("i32")]);
        assert_eq!(layout.size, 8);
        assert_eq!(layout.align, 4);
        assert_eq!(layout.offsets, vec![0, 4]);
    }

    #[test]
    fn test_tuple_string_string() {
        // tuple<string, string>: string(8) + string(8) = 16 bytes, align 4
        let layout = layout_tuple(&[named_type("String"), named_type("String")]);
        assert_eq!(layout.size, 16);
        assert_eq!(layout.align, 4);
        assert_eq!(layout.offsets, vec![0, 8]);
    }

    #[test]
    fn test_tuple_bool_i32() {
        // tuple<bool, i32>: bool(1) + 3 padding + i32(4) = 8 bytes, align 4
        let layout = layout_tuple(&[named_type("bool"), named_type("i32")]);
        assert_eq!(layout.size, 8);
        assert_eq!(layout.align, 4);
        assert_eq!(layout.offsets, vec![0, 4]);
    }

    #[test]
    fn test_tuple_i32_bool() {
        // tuple<i32, bool>: i32(4) + bool(1) = 5, padded to 8 bytes, align 4
        let layout = layout_tuple(&[named_type("i32"), named_type("bool")]);
        assert_eq!(layout.size, 8);
        assert_eq!(layout.align, 4);
        assert_eq!(layout.offsets, vec![0, 4]);
    }

    #[test]
    fn test_tuple_u8_u16_u32() {
        // tuple<u8, u16, u32>: u8(1) + 1 padding + u16(2) + u32(4) = 8 bytes, align 4
        let layout = layout_tuple(&[named_type("u8"), named_type("u16"), named_type("u32")]);
        assert_eq!(layout.size, 8);
        assert_eq!(layout.align, 4);
        assert_eq!(layout.offsets, vec![0, 2, 4]);
    }

    #[test]
    fn test_list_option_string() {
        // list<option<string>> is still just (ptr, len) = 8 bytes
        let inner = generic_type("Option", vec![named_type("String")]);
        let list = generic_type("Array", vec![inner]);
        assert_eq!(cm_size(&list), 8);
        assert_eq!(cm_align(&list), 4);
    }

    #[test]
    fn test_option_list_u8() {
        // option<list<u8>>: disc(1) + 3 padding + list(8) = 12 bytes, align 4
        let inner = generic_type("Array", vec![named_type("u8")]);
        let opt = generic_type("Option", vec![inner]);
        assert_eq!(cm_size(&opt), 12);
        assert_eq!(cm_align(&opt), 4);
    }

    #[test]
    fn test_result_option_string_i32() {
        // result<option<string>, i32>:
        //   option<string> size = 12, align = 4
        //   i32 size = 4, align = 4
        //   disc(4) + max(12, 4) = 16 bytes, align 4
        let opt_string = generic_type("Option", vec![named_type("String")]);
        let result = generic_type("Result", vec![opt_string, named_type("i32")]);
        assert_eq!(cm_size(&result), 16);
        assert_eq!(cm_align(&result), 4);
    }

    #[test]
    fn test_flat_primitives() {
        assert_eq!(cm_flat_types(&named_type("i32")), vec![CmValType::I32]);
        assert_eq!(cm_flat_types(&named_type("i64")), vec![CmValType::I64]);
        assert_eq!(cm_flat_types(&named_type("f32")), vec![CmValType::F32]);
        assert_eq!(cm_flat_types(&named_type("f64")), vec![CmValType::F64]);
        assert_eq!(cm_flat_types(&named_type("bool")), vec![CmValType::I32]);
        assert_eq!(cm_flat_types(&named_type("char")), vec![CmValType::I32]);
        assert_eq!(cm_flat_types(&named_type("u8")), vec![CmValType::I32]);
    }

    #[test]
    fn test_flat_string() {
        assert_eq!(
            cm_flat_types(&named_type("String")),
            vec![CmValType::I32, CmValType::I32]
        );
    }

    #[test]
    fn test_flat_list() {
        let list = generic_type("Array", vec![named_type("i32")]);
        assert_eq!(cm_flat_types(&list), vec![CmValType::I32, CmValType::I32]);
    }

    #[test]
    fn test_flat_unit() {
        assert_eq!(cm_flat_types(&named_type("()")), vec![]);
        assert_eq!(cm_flat_types(&Type::Tuple(vec![])), vec![]);
    }

    #[test]
    fn test_flat_tuple() {
        let tuple = Type::Tuple(vec![named_type("i32"), named_type("f64")]);
        assert_eq!(cm_flat_types(&tuple), vec![CmValType::I32, CmValType::F64]);
    }

    #[test]
    fn test_record_simple() {
        // record { x: i32, y: i32 }
        let layout = layout_record(&[("x", &named_type("i32")), ("y", &named_type("i32"))]);
        assert_eq!(layout.size, 8);
        assert_eq!(layout.align, 4);
        assert_eq!(layout.offsets, vec![0, 4]);
    }

    #[test]
    fn test_record_mixed_alignment() {
        // record { flag: bool, value: i64 }
        // flag(1) + 7 padding + i64(8) = 16 bytes, align 8
        let bool_ty = named_type("bool");
        let i64_ty = named_type("i64");
        let layout = layout_record(&[("flag", &bool_ty), ("value", &i64_ty)]);
        assert_eq!(layout.size, 16);
        assert_eq!(layout.align, 8);
        assert_eq!(layout.offsets, vec![0, 8]);
    }

    #[test]
    fn test_record_with_string() {
        // record { name: string, age: i32 }
        // string(8) + i32(4) = 12 bytes, align 4
        let str_ty = named_type("String");
        let i32_ty = named_type("i32");
        let layout = layout_record(&[("name", &str_ty), ("age", &i32_ty)]);
        assert_eq!(layout.size, 12);
        assert_eq!(layout.align, 4);
        assert_eq!(layout.offsets, vec![0, 8]);
    }

    #[test]
    fn test_consistency_string_return() {
        // CmCallConvention uses outptr_alloc: Some((8, 4)) for String return
        assert_eq!(cm_size(&named_type("String")), 8);
        assert_eq!(cm_align(&named_type("String")), 4);
    }

    #[test]
    fn test_consistency_option_string_return() {
        // CmCallConvention uses outptr_alloc: Some((12, 4)) for option<string>
        let layout = layout_option(&named_type("String"));
        assert_eq!(layout.size, 12);
        assert_eq!(layout.align, 4);
    }

    #[test]
    fn test_consistency_option_own_resource() {
        // CmCallConvention uses outptr_alloc: Some((8, 4)) for option<own<resource>>
        let own_resource = generic_type("Own", vec![named_type("Fields")]);
        let layout = layout_option(&own_resource);
        assert_eq!(layout.size, 8);
        assert_eq!(layout.align, 4);
    }

    #[test]
    fn test_consistency_result_resource_enum() {
        // CmCallConvention uses outptr_alloc: Some((8, 4)) for result<resource, enum>
        let layout = layout_result(&named_type("IncomingBody"), &named_type("ErrorCode"));
        assert_eq!(layout.size, 8);
        assert_eq!(layout.align, 4);
    }

    #[test]
    fn test_consistency_list_return() {
        // CmCallConvention uses outptr_alloc: Some((8, 4)) for list<T>
        let list = generic_type("Array", vec![named_type("String")]);
        assert_eq!(cm_size(&list), 8);
        assert_eq!(cm_align(&list), 4);
    }

    #[test]
    fn test_consistency_tuple_return() {
        // CmCallConvention::for_tuple_return computes size/align for tuple<i32, u64>
        // i32(4) + 4 padding + u64(8) = 16 bytes, align 8
        let layout = layout_tuple(&[named_type("i32"), named_type("u64")]);
        assert_eq!(layout.size, 16);
        assert_eq!(layout.align, 8);
        assert_eq!(layout.offsets, vec![0, 8]);
    }
}
