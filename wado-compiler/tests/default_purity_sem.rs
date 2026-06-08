//! Tests for `check_default_purity_semantic` — the Semantics-based default
//! value purity checker (Design B). Parameter and struct-field defaults must
//! be pure: they may not call an effectful function. Runs on the LSP analysis
//! result (no TIR), so violations surface even without reify.

#![allow(unused_crate_dependencies)]

mod common;

use common::InMemoryHost;
use wado_compiler::check_default_purity_semantic;
use wado_compiler::semantics::semantics;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// Default-purity callees reported for `source`.
fn violations(source: &str) -> Vec<String> {
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));
    check_default_purity_semantic(&sem)
        .into_iter()
        .map(|e| e.callee)
        .collect()
}

#[test]
fn effectful_parameter_default_is_reported() {
    let source = r#"
use { println, Stdout } from "core:cli";

fn noisy() -> i32 with Stdout {
    println("side effect");
    return 42;
}

fn greet(value: i32 = noisy()) -> i32 with Stdout {
    return value;
}

export fn run() with Stdout {
    let x = greet();
    assert x == 42;
}
"#;
    let v = violations(source);
    assert!(
        v.iter().any(|c| c == "noisy"),
        "expected `noisy` flagged as an impure parameter default, got {v:?}"
    );
}

#[test]
fn effectful_field_default_is_reported() {
    let source = r#"
use { println, Stdout } from "core:cli";

fn noisy() -> i32 with Stdout {
    println("side effect");
    return 99;
}

struct Config {
    value: i32 = noisy(),
}

export fn run() with Stdout {
    let c = Config {};
    assert c.value == 99;
}
"#;
    let v = violations(source);
    assert!(
        v.iter().any(|c| c == "noisy"),
        "expected `noisy` flagged as an impure field default, got {v:?}"
    );
}

#[test]
fn pure_default_is_not_reported() {
    let source = r#"
fn pure_value() -> i32 {
    return 7;
}

fn greet(value: i32 = pure_value()) -> i32 {
    return value;
}

export fn run() {
    let x = greet();
    assert x == 7;
}
"#;
    assert!(
        violations(source).is_empty(),
        "a pure default must not be flagged: {:?}",
        violations(source)
    );
}
