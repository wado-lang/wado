//! Tests for `check_stores_semantic` — the Semantics-based stores checker
//! (Design B). It runs on the LSP analysis result (no TIR), so a reference
//! parameter that escapes without a `stores[...]` declaration is reported even
//! when the function is never called (immune to dead-code gating).

#![allow(unused_crate_dependencies)]

mod common;

use common::InMemoryHost;
use wado_compiler::check_stores_semantic;
use wado_compiler::semantics::semantics;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// Stores-violation messages reported for `source`.
fn violations(source: &str) -> Vec<String> {
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));
    check_stores_semantic(&sem)
        .into_iter()
        .map(|e| e.message)
        .collect()
}

#[test]
fn returning_ref_param_without_stores_is_reported_even_in_dead_function() {
    // `bad_return` is never called, so reify gating would drop it — but the
    // stores violation must still surface from `Semantics`.
    let source = r#"
struct Data {
    value: i32,
}

fn bad_return(data: &Data) -> &Data {
    return data;
}

export fn run() {}
"#;
    let v = violations(source);
    assert!(
        v.iter()
            .any(|m| m.contains("returning reference parameter 'data'") && m.contains("stores[data]")),
        "expected a returning-ref violation for `bad_return`, got {v:?}"
    );
}

#[test]
fn storing_ref_param_in_struct_field_is_reported() {
    let source = r#"
struct Data {
    value: i32,
}

struct Container {
    data: &Data,
}

fn bad_store(data: &Data) -> Container {
    return Container { data };
}

export fn run() {}
"#;
    let v = violations(source);
    assert!(
        v.iter().any(|m| m
            .contains("storing reference parameter 'data' in struct field")
            && m.contains("stores[data]")),
        "expected a struct-field stores violation, got {v:?}"
    );
}

#[test]
fn storing_ref_param_in_global_is_reported() {
    let source = r#"
struct Data {
    value: i32,
}

global mut SAVED: &Data = &Data { value: 0 };

fn bad_global(data: &Data) {
    SAVED = data;
}

export fn run() {}
"#;
    let v = violations(source);
    assert!(
        v.iter().any(|m| m.contains("storing reference parameter 'data' in global 'SAVED'")
            && m.contains("stores[data]")),
        "expected a global stores violation, got {v:?}"
    );
}

#[test]
fn declared_stores_is_not_reported() {
    let source = r#"
struct Data {
    value: i32,
}

fn ok_return(data: &Data) -> &Data with stores[data] {
    return data;
}

export fn run() {}
"#;
    assert!(
        violations(source).is_empty(),
        "`stores[data]` covers the escape: {:?}",
        violations(source)
    );
}

#[test]
fn non_reference_param_is_not_reported() {
    let source = r#"
struct Data {
    value: i32,
}

fn passthrough(data: Data) -> Data {
    return data;
}

export fn run() {}
"#;
    assert!(
        violations(source).is_empty(),
        "value parameters are not references and never trigger stores: {:?}",
        violations(source)
    );
}
