//! A newtype answers for its base's static methods, and reaches the base
//! through the type rather than its spelling: `type Headers = Fields` makes
//! `Headers::new()` resolve in a module that imported `Headers` alone.

use crate::common::InMemoryHost;
use wado_compiler::semantics::semantics;

fn diagnostics(source: &str) -> Vec<String> {
    let host = InMemoryHost::new();
    let _ = tokio::runtime::Runtime::new().unwrap().block_on(semantics(
        source,
        &host,
        Some("entry.wado"),
    ));
    host.diagnostics()
        .into_iter()
        .map(|d| format!("{:?}: {}", d.code, d.message))
        .collect()
}

#[test]
fn a_newtype_over_a_resource_forwards_its_base_static() {
    let diags = diagnostics(
        r#"
use { Headers } from "wasi:http";

fn make() -> Headers {
    return Headers::new();
}
"#,
    );
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn the_forwarded_static_yields_the_newtype_not_the_base() {
    let diags = diagnostics(
        r#"
use { Headers, Fields } from "wasi:http";

fn make() -> Fields {
    return Headers::new();
}
"#,
    );
    assert!(
        diags
            .iter()
            .any(|d| d.contains("expected 'Fields', found 'Headers'")),
        "{diags:?}"
    );
}

#[test]
fn the_base_is_not_the_newtype() {
    let diags = diagnostics(
        r#"
use { Headers, Fields } from "wasi:http";

fn make() -> Headers {
    return Fields::new();
}
"#,
    );
    assert!(
        diags
            .iter()
            .any(|d| d.contains("expected 'Headers', found 'Fields'")),
        "{diags:?}"
    );
}

#[test]
fn the_newtype_replaces_the_base_wherever_it_stands_in_the_return() {
    // `Fields::from_list` returns `Result<Fields, HeaderError>`, so through
    // the newtype it returns `Result<Headers, HeaderError>`.
    let diags = diagnostics(
        r#"
use { Headers, Fields, FieldName, FieldValue, HeaderError } from "wasi:http";

fn make(entries: List<[FieldName, FieldValue]>) -> Result<Fields, HeaderError> {
    return Headers::from_list(entries);
}
"#,
    );
    assert!(
        diags.iter().any(|d| d.contains(
            "expected 'Result<Fields, HeaderError>', found 'Result<Headers, HeaderError>'"
        )),
        "{diags:?}"
    );
}

#[test]
fn a_newtype_forwards_its_bases_auto_derived_default() {
    let diags = diagnostics(
        r#"
struct Config {
    a: i32 = 1,
}

type Wrapped = Config;

fn make() -> Wrapped {
    return Wrapped::default();
}
"#,
    );
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn a_newtype_forwards_a_trait_defaulted_static() {
    let diags = diagnostics(
        r#"
trait Factory {
    fn make() -> u32 {
        return 42
    }
}

struct Base {
    x: u32,
}

impl Factory for Base {}

type Wrapped = Base;

fn go() -> u32 {
    return Wrapped::make();
}
"#,
    );
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn a_wrong_argument_to_a_forwarded_static_is_reported() {
    // Left unchecked, the mismatch reached codegen as an invalid module.
    let diags = diagnostics(
        r#"
use { Headers } from "wasi:http";

fn make() -> Headers {
    return Headers::from_list("not a list").unwrap();
}
"#,
    );
    assert!(
        diags
            .iter()
            .any(|d| d.contains("expected 'List<[FieldName, FieldValue]>', found 'String'")),
        "{diags:?}"
    );
}

#[test]
fn a_forwarded_static_substitutes_the_bases_type_arguments() {
    // `type Bytes = List<u8>` makes `List::filled`'s `element: T` a `u8`; the
    // bare type parameter would measure the literal against `T` and fail.
    let diags = diagnostics(
        r#"
type Bytes = List<u8>;

fn make() -> Bytes {
    return Bytes::filled(4, 255);
}
"#,
    );
    assert!(diags.is_empty(), "{diags:?}");
}
