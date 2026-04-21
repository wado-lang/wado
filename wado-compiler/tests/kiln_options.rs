//! Tests for [`wado_compiler::kiln::extract_options_descriptor`].

#![allow(unused_crate_dependencies)]

mod common;

use common::InMemoryHost;
use wado_compiler::ModuleSource;
use wado_compiler::annotate;
use wado_compiler::kiln::{CanonicalValue, OptionsType, extract_options_descriptor};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

fn entry() -> ModuleSource {
    ModuleSource::EntryPoint {
        filename: "entry.wado".to_string(),
    }
}

#[test]
fn descriptor_primitive_fields_and_defaults() {
    let source = r#"
pub struct Options {
    pub highlight: bool = false,
    pub depth: i32 = 3,
    pub name: String,
}

pub fn generate() {}
"#;
    let host = InMemoryHost::new();
    let annotated = block_on(annotate(source, &host, Some("entry.wado"))).unwrap();
    let desc = extract_options_descriptor(&annotated, &entry()).unwrap();
    assert_eq!(desc.fields.len(), 3);

    assert_eq!(desc.fields[0].name, "highlight");
    assert!(matches!(desc.fields[0].ty, OptionsType::Bool));
    assert_eq!(desc.fields[0].default, Some(CanonicalValue::Bool(false)));

    assert_eq!(desc.fields[1].name, "depth");
    assert!(matches!(desc.fields[1].ty, OptionsType::I32));
    assert_eq!(desc.fields[1].default, Some(CanonicalValue::I64(3)));

    assert_eq!(desc.fields[2].name, "name");
    assert!(matches!(desc.fields[2].ty, OptionsType::String));
    assert!(desc.fields[2].default.is_none());
}

#[test]
fn descriptor_option_string_accepts_null_default() {
    let source = r#"
pub struct Options {
    pub rule: Option<String> = null,
}

pub fn generate() {}
"#;
    let host = InMemoryHost::new();
    let annotated = block_on(annotate(source, &host, Some("entry.wado"))).unwrap();
    let desc = extract_options_descriptor(&annotated, &entry()).unwrap();
    assert_eq!(desc.fields.len(), 1);
    match &desc.fields[0].ty {
        OptionsType::Option(inner) => assert!(matches!(inner.as_ref(), OptionsType::String)),
        other => panic!("expected Option<String>, got {other:?}"),
    }
    assert_eq!(desc.fields[0].default, Some(CanonicalValue::None));
}

#[test]
fn descriptor_enum_field_extracts_variants() {
    let source = r#"
pub enum Style {
    Rpc,
    Oneway,
}

pub struct Options {
    pub style: Style = Style::Rpc,
}

pub fn generate() {}
"#;
    let host = InMemoryHost::new();
    let annotated = block_on(annotate(source, &host, Some("entry.wado"))).unwrap();
    let desc = extract_options_descriptor(&annotated, &entry()).unwrap();
    assert_eq!(desc.fields.len(), 1);
    match &desc.fields[0].ty {
        OptionsType::Enum { name, variants } => {
            assert_eq!(name, "Style");
            assert_eq!(variants, &["Rpc".to_string(), "Oneway".to_string()]);
        }
        other => panic!("expected Enum, got {other:?}"),
    }
    assert_eq!(
        desc.fields[0].default,
        Some(CanonicalValue::Enum("Rpc".to_string()))
    );
}

#[test]
fn descriptor_nested_struct_descends_and_cycles_reject() {
    let source = r#"
pub struct Inner {
    pub flag: bool = true,
    pub count: i64 = 7,
}

pub struct Options {
    pub inner: Inner,
}

pub fn generate() {}
"#;
    let host = InMemoryHost::new();
    let annotated = block_on(annotate(source, &host, Some("entry.wado"))).unwrap();
    let desc = extract_options_descriptor(&annotated, &entry()).unwrap();
    assert_eq!(desc.fields.len(), 1);
    let OptionsType::Struct { name, descriptor } = &desc.fields[0].ty else {
        panic!("expected nested Struct field");
    };
    assert_eq!(name, "Inner");
    assert_eq!(descriptor.fields.len(), 2);
    assert_eq!(descriptor.fields[0].name, "flag");
    assert!(matches!(descriptor.fields[0].ty, OptionsType::Bool));
    assert_eq!(
        descriptor.fields[0].default,
        Some(CanonicalValue::Bool(true))
    );
    assert_eq!(descriptor.fields[1].name, "count");
    assert_eq!(descriptor.fields[1].default, Some(CanonicalValue::I64(7)));
}

#[test]
fn descriptor_missing_options_struct_errors() {
    let source = r#"
pub fn generate() {}
"#;
    let host = InMemoryHost::new();
    let annotated = block_on(annotate(source, &host, Some("entry.wado"))).unwrap();
    let err = extract_options_descriptor(&annotated, &entry()).unwrap_err();
    assert!(
        err.iter()
            .any(|d| d.message.contains("does not declare `pub struct Options`"))
    );
}

#[test]
fn descriptor_missing_generate_fn_errors() {
    let source = r#"
pub struct Options {
    pub foo: bool = false,
}
"#;
    let host = InMemoryHost::new();
    let annotated = block_on(annotate(source, &host, Some("entry.wado"))).unwrap();
    let err = extract_options_descriptor(&annotated, &entry()).unwrap_err();
    assert!(
        err.iter()
            .any(|d| d.message.contains("does not declare `generate` function"))
    );
}
