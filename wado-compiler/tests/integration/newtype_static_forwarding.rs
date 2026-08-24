//! A newtype answers for its base's static methods, and reaches the base
//! through the type rather than its spelling: `type Headers = Fields` makes
//! `Headers::new()` resolve in a module that imported `Headers` alone.

use crate::common::InMemoryHost;
use wado_compiler::semantics::semantics;

fn diagnostics(source: &str) -> Vec<String> {
    let host = InMemoryHost::new();
    let _ = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(semantics(source, &host, Some("entry.wado")));
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
