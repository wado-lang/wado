//! Recover a Kiln generator's options descriptor from a *prebuilt* component.
//!
//! A path/source generator's [`OptionsDescriptor`] is extracted from its Wado
//! `pub struct Options` at compile time. A registry generator arrives as an
//! already-compiled Component Model artifact, so its options shape is read back
//! from the component's embedded WIT instead: the `generate` export takes an
//! `options: <record>` parameter (Kiln protocol revision 3, typed options), and
//! that record maps field-for-field onto an [`OptionsDescriptor`].
//!
//! Defaults do not survive the WIT boundary — a WIT `record` has no notion of an
//! optional-with-default field — so every recovered field is *required* unless
//! its type is `option<T>`. A consumer of a registry generator therefore
//! supplies every non-`option` field explicitly.

use wit_parser::{Type, TypeDefKind};

use wado_compiler::kiln::{OptionsDescriptor, OptionsField, OptionsType};
use wado_compiler::token::Span;

/// Read the `options` parameter type of the generator component's `generate`
/// export and describe it as an [`OptionsDescriptor`].
///
/// Returns `Ok(None)` when `generate` declares no `options` parameter (a
/// generator with no `Options` struct), matching the source path's empty
/// descriptor. Returns `Err` when the bytes are not a component, expose no
/// `generate` export, or use an options type Wado's options model cannot
/// represent.
pub fn options_descriptor_from_component(wasm: &[u8]) -> Result<Option<OptionsDescriptor>, String> {
    let decoded = wit_component::decode(wasm).map_err(|e| format!("decode component WIT: {e}"))?;
    let resolve = decoded.resolve();
    let world_id = match &decoded {
        wit_component::DecodedWasm::Component(_, world) => *world,
        wit_component::DecodedWasm::WitPackage(..) => {
            return Err("expected a component, got a WIT package".to_string());
        }
    };
    let world = &resolve.worlds[world_id];

    let generate = world
        .exports
        .values()
        .find_map(|item| match item {
            wit_parser::WorldItem::Interface { id, .. } => {
                resolve.interfaces[*id].functions.get("generate")
            }
            wit_parser::WorldItem::Function(f) if f.name == "generate" => Some(f),
            _ => None,
        })
        .ok_or_else(|| "component exports no `generate` function".to_string())?;

    let Some(options) = generate.params.iter().find(|p| p.name == "options") else {
        return Ok(None);
    };

    let descriptor = record_descriptor(resolve, &options.ty)
        .ok_or_else(|| "`generate` options parameter is not a record".to_string())??;
    Ok(Some(descriptor))
}

/// Describe a WIT record type as an [`OptionsDescriptor`]. `None` when `ty` is
/// not a record (the top-level options parameter must be one); `Some(Err(_))`
/// when a field's type is unrepresentable.
fn record_descriptor(
    resolve: &wit_parser::Resolve,
    ty: &Type,
) -> Option<Result<OptionsDescriptor, String>> {
    let Type::Id(id) = ty else { return None };
    let TypeDefKind::Record(record) = &resolve.types[*id].kind else {
        return None;
    };
    let mut fields = Vec::with_capacity(record.fields.len());
    for field in &record.fields {
        let ty = match options_type(resolve, &field.ty) {
            Ok(t) => t,
            Err(e) => return Some(Err(format!("field `{}`: {e}", field.name))),
        };
        fields.push(OptionsField {
            name: field.name.clone(),
            ty,
            // A WIT record carries no defaults; every field is required unless
            // it is `option<T>`, whose absence resolves to `None`.
            default: None,
            span: Span::default(),
        });
    }
    Some(Ok(OptionsDescriptor { fields }))
}

/// Map a WIT type onto the corresponding [`OptionsType`]. Errors on types the
/// options model does not cover (`char`, `list`, `tuple`, `variant`, …).
fn options_type(resolve: &wit_parser::Resolve, ty: &Type) -> Result<OptionsType, String> {
    Ok(match ty {
        Type::Bool => OptionsType::Bool,
        // Widths narrower than 32 bits have no distinct `OptionsType`; they
        // widen to the signed/unsigned 32-bit case, matching how the source
        // extractor treats small integer options.
        Type::S8 | Type::S16 | Type::S32 => OptionsType::I32,
        Type::U8 | Type::U16 | Type::U32 => OptionsType::U32,
        Type::S64 => OptionsType::I64,
        Type::U64 => OptionsType::U64,
        Type::F32 => OptionsType::F32,
        Type::F64 => OptionsType::F64,
        Type::String => OptionsType::String,
        Type::Id(id) => {
            let def = &resolve.types[*id];
            match &def.kind {
                TypeDefKind::Option(inner) => {
                    OptionsType::Option(Box::new(options_type(resolve, inner)?))
                }
                TypeDefKind::Enum(e) => OptionsType::Enum {
                    name: def.name.clone().unwrap_or_default(),
                    variants: e.cases.iter().map(|c| c.name.clone()).collect(),
                },
                TypeDefKind::Record(_) => {
                    let descriptor = record_descriptor(resolve, ty)
                        .expect("Type::Id over a record yields a record descriptor")?;
                    OptionsType::Struct {
                        name: def.name.clone().unwrap_or_default(),
                        descriptor,
                    }
                }
                // A named alias (`type foo = bar`) forwards to its aliasee.
                TypeDefKind::Type(inner) => options_type(resolve, inner)?,
                other => return Err(format!("unsupported options type: {other:?}")),
            }
        }
        other => return Err(format!("unsupported options type: {other:?}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wit_parser::Resolve;

    /// Parse a WIT package and return `(resolve, Type)` for the record type
    /// named `record-name`.
    fn record_type(wit: &str, record_name: &str) -> (Resolve, Type) {
        let mut resolve = Resolve::new();
        resolve.push_str("test.wit", wit).expect("parse WIT");
        let (id, _) = resolve
            .types
            .iter()
            .find(|(_, def)| def.name.as_deref() == Some(record_name))
            .expect("record type present");
        (resolve.clone(), Type::Id(id))
    }

    #[test]
    fn maps_scalar_option_enum_and_record_fields() {
        let wit = "\
package test:opts;
interface i {
    enum mode { fast, slow }
    record inner { n: u32 }
    record options {
        highlight: bool,
        count: u32,
        label: string,
        maybe: option<s64>,
        mode: mode,
        inner: inner,
    }
}
";
        let (resolve, ty) = record_type(wit, "options");
        let descriptor = record_descriptor(&resolve, &ty).unwrap().unwrap();
        let by_name = |n: &str| {
            descriptor
                .fields
                .iter()
                .find(|f| f.name == n)
                .unwrap_or_else(|| panic!("field {n}"))
        };
        assert_eq!(by_name("highlight").ty, OptionsType::Bool);
        assert_eq!(by_name("count").ty, OptionsType::U32);
        assert_eq!(by_name("label").ty, OptionsType::String);
        assert_eq!(
            by_name("maybe").ty,
            OptionsType::Option(Box::new(OptionsType::I64))
        );
        match &by_name("mode").ty {
            OptionsType::Enum { variants, .. } => assert_eq!(variants, &["fast", "slow"]),
            other => panic!("expected enum, got {other:?}"),
        }
        match &by_name("inner").ty {
            OptionsType::Struct { descriptor, .. } => {
                assert_eq!(descriptor.fields[0].name, "n");
                assert_eq!(descriptor.fields[0].ty, OptionsType::U32);
            }
            other => panic!("expected struct, got {other:?}"),
        }
        // No WIT field carries a default; all are required.
        assert!(descriptor.fields.iter().all(|f| f.default.is_none()));
    }

    #[test]
    fn rejects_unrepresentable_field_type() {
        let wit = "\
package test:bad;
interface i {
    record options { items: list<u32> }
}
";
        let (resolve, ty) = record_type(wit, "options");
        let err = record_descriptor(&resolve, &ty).unwrap().unwrap_err();
        assert!(err.contains("items"), "{err}");
    }
}
