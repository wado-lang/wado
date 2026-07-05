//! Map a generator's [`OptionsDescriptor`] to a JSON Schema (Draft 2020-12
//! subset) and encode it as CBOR — the payload of the `describe-options`
//! export a prebuilt (registry) generator carries so a consumer can validate
//! and encode options without the generator's source.
//!
//! The **shape** is [Jade](../../../docs/wep-2026-06-13-jade.md)'s `Schema`
//! document model (capability A); the **wire encoding** is CBOR, the same
//! codec option *values* use (see `kiln::cache`), so schema and values share
//! one format. Jade's type→schema derivation (capability B) waits on
//! `Reflect`; until then the compiler performs the mapping here, in Rust, from
//! the source-extracted descriptor.
//!
//! Mapping (WEP 2026-04-12 §"Options introspection"): the `Options` struct is
//! an `object` with `additionalProperties: false`; a field with a default is
//! optional and carries a JSON Schema `default`; a field without one lands in
//! `required`; a no-payload `enum` becomes a string `enum`. Primitive mappings
//! follow `package-jade`'s constructors (`boolean` / `integer` / `number` /
//! `string`). An `Option<T>` is the nullable form: its `type` becomes a union
//! with `"null"` (the `TypeSet::Many` shape `package-jade` models).

use crate::kiln::import_check::DESCRIBE_OPTIONS_FN;
use crate::kiln::options::{CanonicalValue, OptionsDescriptor, OptionsType};
use crate::tir::{TirExprKind, TirModule, TirStmtKind};

/// The `$schema` dialect Jade emits (mirrors `package-jade`'s `Schema::DRAFT`).
pub const DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

/// A JSON Schema `type`: one name (`"string"`) or a union including `"null"`
/// (`["string", "null"]`).
#[derive(Debug, Clone, PartialEq)]
enum TypeSet {
    One(&'static str),
    Many(Vec<&'static str>),
}

/// A scalar in `default` / `enum` positions.
#[derive(Debug, Clone, PartialEq)]
enum SchemaValue {
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(String),
    Null,
}

/// The JSON Schema subset Jade needs to describe an `Options` struct. Absent
/// fields are omitted on encode, so the emitted schema stays idiomatic.
#[derive(Debug, Clone, PartialEq)]
struct Schema {
    dialect: Option<&'static str>,
    type_: Option<TypeSet>,
    properties: Vec<(String, Schema)>,
    required: Vec<String>,
    additional_properties: Option<bool>,
    enum_: Vec<SchemaValue>,
    default: Option<SchemaValue>,
}

impl Schema {
    fn empty() -> Self {
        Schema {
            dialect: None,
            type_: None,
            properties: Vec::new(),
            required: Vec::new(),
            additional_properties: None,
            enum_: Vec::new(),
            default: None,
        }
    }

    fn of_type(name: &'static str) -> Self {
        Schema {
            type_: Some(TypeSet::One(name)),
            ..Schema::empty()
        }
    }

    /// Make this schema nullable: fold `"null"` into its `type` and, for an
    /// enum, into its allowed values.
    fn into_nullable(mut self) -> Self {
        self.type_ = Some(match self.type_ {
            Some(TypeSet::One(t)) => TypeSet::Many(vec![t, "null"]),
            Some(TypeSet::Many(mut ts)) => {
                if !ts.contains(&"null") {
                    ts.push("null");
                }
                TypeSet::Many(ts)
            }
            None => TypeSet::One("null"),
        });
        if !self.enum_.is_empty() && !self.enum_.contains(&SchemaValue::Null) {
            self.enum_.push(SchemaValue::Null);
        }
        self
    }
}

/// Encode `descriptor` as a CBOR-encoded JSON Schema — the `describe-options`
/// payload for a generator whose `Options` this describes.
#[must_use]
pub fn describe_options_cbor(descriptor: &OptionsDescriptor) -> Vec<u8> {
    let mut enc = minicbor::Encoder::new(Vec::new());
    encode_schema(&mut enc, &object_schema(descriptor, true));
    enc.into_writer()
}

/// Replace the placeholder `BytesLiteral` in the injected `describe_options`
/// function body (see `import_check::inject_describe_options_export`) with the
/// CBOR-encoded options schema. Returns `true` when the function was found and
/// patched. The injected body is exactly `{ task return b""; }`, so the single
/// task-return-value `BytesLiteral` is the one patched.
#[must_use]
pub fn patch_describe_options(module: &TirModule, bytes: Vec<u8>) -> bool {
    let Some(func) = module.find_function(DESCRIBE_OPTIONS_FN) else {
        return false;
    };
    let mut func = func.borrow_mut();
    let Some(body) = func.body.as_mut() else {
        return false;
    };
    for stmt in &mut body.stmts {
        if let TirStmtKind::TaskReturn { value } = &mut stmt.kind
            && let TirExprKind::BytesLiteral(existing) = &mut value.kind
        {
            *existing = bytes;
            return true;
        }
    }
    false
}

/// Build the object schema for a descriptor. `is_root` stamps `$schema`.
fn object_schema(descriptor: &OptionsDescriptor, is_root: bool) -> Schema {
    let mut schema = Schema::of_type("object");
    if is_root {
        schema.dialect = Some(DRAFT);
    }
    for field in &descriptor.fields {
        let mut field_schema = type_schema(&field.ty);
        match &field.default {
            Some(default) => field_schema.default = Some(canonical_value(default)),
            None => schema.required.push(field.name.clone()),
        }
        schema.properties.push((field.name.clone(), field_schema));
    }
    schema.additional_properties = Some(false);
    schema
}

fn type_schema(ty: &OptionsType) -> Schema {
    match ty {
        OptionsType::Bool => Schema::of_type("boolean"),
        OptionsType::I32 | OptionsType::I64 | OptionsType::U32 | OptionsType::U64 => {
            Schema::of_type("integer")
        }
        OptionsType::F32 | OptionsType::F64 => Schema::of_type("number"),
        OptionsType::String => Schema::of_type("string"),
        OptionsType::Enum { variants, .. } => {
            let mut schema = Schema::of_type("string");
            schema.enum_ = variants.iter().map(|v| SchemaValue::Str(v.clone())).collect();
            schema
        }
        OptionsType::Struct { descriptor, .. } => object_schema(descriptor, false),
        OptionsType::Option(inner) => type_schema(inner).into_nullable(),
    }
}

fn canonical_value(value: &CanonicalValue) -> SchemaValue {
    match value {
        CanonicalValue::Bool(b) => SchemaValue::Bool(*b),
        CanonicalValue::I64(n) => SchemaValue::I64(*n),
        CanonicalValue::U64(n) => SchemaValue::U64(*n),
        CanonicalValue::F64(f) => SchemaValue::F64(*f),
        CanonicalValue::String(s) | CanonicalValue::Enum(s) => SchemaValue::Str(s.clone()),
        CanonicalValue::None => SchemaValue::Null,
        CanonicalValue::Some(inner) => canonical_value(inner),
        // A struct default is not representable as a single JSON Schema
        // `default` scalar here; the descriptor extractor only admits scalar
        // defaults on scalar fields, so this arm is unreachable in practice.
        CanonicalValue::Struct(_) => SchemaValue::Null,
    }
}

type EncVec = minicbor::Encoder<Vec<u8>>;

/// Writing into a `Vec<u8>` cannot fail (`minicbor`'s `Write` for `Vec` has
/// `Error = Infallible`), so every encode result is unwrapped with this.
const INFALLIBLE: &str = "vec writer is infallible";

/// One keyword slot: present iff its schema field is set. Emitted in canonical
/// CBOR map order (shorter encoded key first, then bytewise), matching
/// `kiln::cache::encode_options_canonical`.
fn encode_schema(enc: &mut EncVec, schema: &Schema) {
    let mut keys: Vec<&'static str> = Vec::new();
    if schema.dialect.is_some() {
        keys.push("$schema");
    }
    if schema.type_.is_some() {
        keys.push("type");
    }
    if !schema.properties.is_empty() {
        keys.push("properties");
    }
    if !schema.required.is_empty() {
        keys.push("required");
    }
    if schema.additional_properties.is_some() {
        keys.push("additionalProperties");
    }
    if !schema.enum_.is_empty() {
        keys.push("enum");
    }
    if schema.default.is_some() {
        keys.push("default");
    }
    keys.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));

    enc.map(keys.len() as u64).expect(INFALLIBLE);
    for key in keys {
        enc.str(key).expect(INFALLIBLE);
        match key {
            "$schema" => {
                enc.str(schema.dialect.unwrap()).expect(INFALLIBLE);
            }
            "type" => encode_type(enc, schema.type_.as_ref().unwrap()),
            "properties" => encode_properties(enc, &schema.properties),
            "required" => {
                enc.array(schema.required.len() as u64).expect(INFALLIBLE);
                for name in &schema.required {
                    enc.str(name).expect(INFALLIBLE);
                }
            }
            "additionalProperties" => {
                enc.bool(schema.additional_properties.unwrap())
                    .expect(INFALLIBLE);
            }
            "enum" => {
                enc.array(schema.enum_.len() as u64).expect(INFALLIBLE);
                for value in &schema.enum_ {
                    encode_value(enc, value);
                }
            }
            "default" => encode_value(enc, schema.default.as_ref().unwrap()),
            _ => unreachable!(),
        }
    }
}

fn encode_type(enc: &mut EncVec, type_: &TypeSet) {
    match type_ {
        TypeSet::One(name) => {
            enc.str(name).expect(INFALLIBLE);
        }
        TypeSet::Many(names) => {
            enc.array(names.len() as u64).expect(INFALLIBLE);
            for name in names {
                enc.str(name).expect(INFALLIBLE);
            }
        }
    }
}

/// A `properties` sub-schema map. Keys are property names in canonical CBOR
/// order (shorter encoded key first, then bytewise).
fn encode_properties(enc: &mut EncVec, properties: &[(String, Schema)]) {
    let mut ordered: Vec<&(String, Schema)> = properties.iter().collect();
    ordered.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then_with(|| a.0.cmp(&b.0)));
    enc.map(ordered.len() as u64).expect(INFALLIBLE);
    for (name, schema) in ordered {
        enc.str(name).expect(INFALLIBLE);
        encode_schema(enc, schema);
    }
}

fn encode_value(enc: &mut EncVec, value: &SchemaValue) {
    match value {
        SchemaValue::Bool(b) => {
            enc.bool(*b).expect(INFALLIBLE);
        }
        SchemaValue::I64(n) => {
            enc.i64(*n).expect(INFALLIBLE);
        }
        SchemaValue::U64(n) => {
            enc.u64(*n).expect(INFALLIBLE);
        }
        SchemaValue::F64(f) => {
            let f = if *f == 0.0 { 0.0 } else { *f };
            enc.f64(f).expect(INFALLIBLE);
        }
        SchemaValue::Str(s) => {
            enc.str(s).expect(INFALLIBLE);
        }
        SchemaValue::Null => {
            enc.null().expect(INFALLIBLE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiln::options::{OptionsField, OptionsType};
    use crate::token::Span;

    fn field(name: &str, ty: OptionsType, default: Option<CanonicalValue>) -> OptionsField {
        OptionsField {
            name: name.to_string(),
            ty,
            default,
            span: Span::default(),
        }
    }

    /// A decoded CBOR value, rich enough for the schema assertions.
    #[derive(Debug, PartialEq)]
    enum Cbor {
        Bool(bool),
        Int(i64),
        Float(f64),
        Text(String),
        Null,
        Array(Vec<Cbor>),
        Map(Vec<(String, Cbor)>),
    }

    fn decode(bytes: &[u8]) -> Cbor {
        let mut dec = minicbor::Decoder::new(bytes);
        let value = decode_value(&mut dec);
        assert_eq!(dec.position(), bytes.len(), "trailing bytes");
        value
    }

    fn decode_value(dec: &mut minicbor::Decoder<'_>) -> Cbor {
        use minicbor::data::Type;
        match dec.datatype().expect("valid CBOR datatype") {
            Type::Bool => Cbor::Bool(dec.bool().unwrap()),
            Type::U8 | Type::U16 | Type::U32 | Type::U64 => Cbor::Int(dec.u64().unwrap() as i64),
            Type::I8 | Type::I16 | Type::I32 | Type::I64 => Cbor::Int(dec.i64().unwrap()),
            Type::F16 | Type::F32 | Type::F64 => Cbor::Float(dec.f64().unwrap()),
            Type::String => Cbor::Text(dec.str().unwrap().to_string()),
            Type::Null => {
                dec.null().unwrap();
                Cbor::Null
            }
            Type::Array => {
                let len = dec.array().unwrap().unwrap();
                Cbor::Array((0..len).map(|_| decode_value(dec)).collect())
            }
            Type::Map => {
                let len = dec.map().unwrap().unwrap();
                let mut entries = Vec::new();
                for _ in 0..len {
                    let key = dec.str().unwrap().to_string();
                    entries.push((key, decode_value(dec)));
                }
                Cbor::Map(entries)
            }
            other => panic!("unexpected CBOR type {other:?}"),
        }
    }

    fn get<'a>(map: &'a Cbor, key: &str) -> Option<&'a Cbor> {
        match map {
            Cbor::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => panic!("not a map"),
        }
    }

    /// The gale `Options` case from the WEP: `{ highlight: bool (required),
    /// trace: bool = false }`.
    #[test]
    fn gale_options_describe_as_the_wep_schema() {
        let descriptor = OptionsDescriptor {
            fields: vec![
                field("highlight", OptionsType::Bool, None),
                field("trace", OptionsType::Bool, Some(CanonicalValue::Bool(false))),
            ],
        };
        let schema = decode(&describe_options_cbor(&descriptor));

        assert_eq!(get(&schema, "$schema"), Some(&Cbor::Text(DRAFT.to_string())));
        assert_eq!(get(&schema, "type"), Some(&Cbor::Text("object".to_string())));
        assert_eq!(
            get(&schema, "additionalProperties"),
            Some(&Cbor::Bool(false))
        );
        assert_eq!(
            get(&schema, "required"),
            Some(&Cbor::Array(vec![Cbor::Text("highlight".to_string())]))
        );

        let props = get(&schema, "properties").unwrap();
        assert_eq!(
            get(props, "highlight"),
            Some(&Cbor::Map(vec![(
                "type".to_string(),
                Cbor::Text("boolean".to_string())
            )]))
        );
        let trace = get(props, "trace").unwrap();
        assert_eq!(get(trace, "type"), Some(&Cbor::Text("boolean".to_string())));
        assert_eq!(get(trace, "default"), Some(&Cbor::Bool(false)));
    }

    #[test]
    fn empty_options_is_an_object_with_no_properties() {
        let schema = decode(&describe_options_cbor(&OptionsDescriptor { fields: vec![] }));
        assert_eq!(get(&schema, "type"), Some(&Cbor::Text("object".to_string())));
        assert_eq!(
            get(&schema, "additionalProperties"),
            Some(&Cbor::Bool(false))
        );
        assert_eq!(get(&schema, "properties"), None);
        assert_eq!(get(&schema, "required"), None);
    }

    #[test]
    fn primitives_map_to_their_json_schema_types() {
        let descriptor = OptionsDescriptor {
            fields: vec![
                field("i", OptionsType::I32, None),
                field("u", OptionsType::U64, None),
                field("f", OptionsType::F64, None),
                field("s", OptionsType::String, None),
            ],
        };
        let schema = decode(&describe_options_cbor(&descriptor));
        let props = get(&schema, "properties").unwrap();
        for (name, expected) in [
            ("i", "integer"),
            ("u", "integer"),
            ("f", "number"),
            ("s", "string"),
        ] {
            assert_eq!(
                get(get(props, name).unwrap(), "type"),
                Some(&Cbor::Text(expected.to_string())),
                "field {name}"
            );
        }
    }

    #[test]
    fn no_payload_enum_becomes_a_string_enum() {
        let descriptor = OptionsDescriptor {
            fields: vec![field(
                "style",
                OptionsType::Enum {
                    name: "Style".to_string(),
                    variants: vec!["Rpc".to_string(), "Rest".to_string()],
                },
                None,
            )],
        };
        let schema = decode(&describe_options_cbor(&descriptor));
        let style = get(get(&schema, "properties").unwrap(), "style").unwrap();
        assert_eq!(get(style, "type"), Some(&Cbor::Text("string".to_string())));
        assert_eq!(
            get(style, "enum"),
            Some(&Cbor::Array(vec![
                Cbor::Text("Rpc".to_string()),
                Cbor::Text("Rest".to_string()),
            ]))
        );
    }

    #[test]
    fn nested_struct_is_an_object_without_its_own_schema_dialect() {
        let inner = OptionsDescriptor {
            fields: vec![field("width", OptionsType::U32, None)],
        };
        let descriptor = OptionsDescriptor {
            fields: vec![field(
                "layout",
                OptionsType::Struct {
                    name: "Layout".to_string(),
                    descriptor: inner,
                },
                None,
            )],
        };
        let schema = decode(&describe_options_cbor(&descriptor));
        let layout = get(get(&schema, "properties").unwrap(), "layout").unwrap();
        assert_eq!(get(layout, "$schema"), None, "nested gets no $schema");
        assert_eq!(get(layout, "type"), Some(&Cbor::Text("object".to_string())));
        assert_eq!(
            get(layout, "additionalProperties"),
            Some(&Cbor::Bool(false))
        );
        let width = get(get(layout, "properties").unwrap(), "width").unwrap();
        assert_eq!(get(width, "type"), Some(&Cbor::Text("integer".to_string())));
    }

    #[test]
    fn option_field_is_nullable_and_not_required() {
        let descriptor = OptionsDescriptor {
            fields: vec![field(
                "name",
                OptionsType::Option(Box::new(OptionsType::String)),
                Some(CanonicalValue::None),
            )],
        };
        let schema = decode(&describe_options_cbor(&descriptor));
        assert_eq!(get(&schema, "required"), None, "defaulted option not required");
        let name = get(get(&schema, "properties").unwrap(), "name").unwrap();
        assert_eq!(
            get(name, "type"),
            Some(&Cbor::Array(vec![
                Cbor::Text("string".to_string()),
                Cbor::Text("null".to_string()),
            ]))
        );
        assert_eq!(get(name, "default"), Some(&Cbor::Null));
    }
}
