//! Tests for `check_default_purity_semantic` — the Semantics-based purity
//! checker (Design B). Parameter defaults, struct-field defaults and global
//! initializers must be pure: they may not call an effectful function, dispatch
//! an interface operation, or install a handler. Runs on the LSP analysis
//! result (no TIR), so violations surface even without reify.

use crate::common::InMemoryHost;
use wado_compiler::semantics::semantics;
use wado_compiler::{Impurity, PureContext, check_default_purity_semantic};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// Purity callees reported for `source`, whatever the position.
fn violations(source: &str) -> Vec<String> {
    reported(source)
        .into_iter()
        .filter_map(|(_, impurity)| match impurity {
            Impurity::Call(callee) => Some(callee),
            Impurity::HandlerInstall => None,
        })
        .collect()
}

/// Every purity violation for `source`, with the position it was found in.
fn reported(source: &str) -> Vec<(PureContext, Impurity)> {
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));
    check_default_purity_semantic(&sem)
        .into_iter()
        .map(|e| (e.context, e.impurity))
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
fn effectful_call_in_global_initializer_is_reported() {
    let source = r#"
use { println, Stdout } from "core:cli";

fn noisy() -> i32 with Stdout {
    println("side effect");
    return 1;
}

global A: i32 = noisy();

export fn run() with Stdout {
    println(`${A}`);
}
"#;
    let reported = reported(source);
    assert!(
        reported.iter().any(|(context, impurity)| *context
            == PureContext::GlobalInitializer
            && matches!(impurity, Impurity::Call(callee) if callee == "noisy")),
        "expected `noisy` flagged in a global initializer, got {reported:?}"
    );
}

#[test]
fn interface_operation_in_global_initializer_is_reported() {
    let source = r#"
interface Counter {
    fn next() -> i32 {
        return 41;
    }
}

global A: i32 = Counter::next();

export fn run() {
    assert A == 41;
}
"#;
    let reported = reported(source);
    assert!(
        reported.iter().any(|(context, impurity)| *context
            == PureContext::GlobalInitializer
            && matches!(impurity, Impurity::Call(callee) if callee == "next")),
        "expected the operation `next` flagged in a global initializer, got {reported:?}"
    );
}

#[test]
fn handler_install_in_global_initializer_is_reported() {
    let source = r#"
interface Counter {
    fn next() -> i32 {
        return 41;
    }
}

struct Tally {
    value: i32,
}

impl Counter for Tally {
    fn next(&mut self) -> i32 {
        self.value += 1;
        resume self.value
    }
}

global A: i32 = hold: {
    let mut tally = Tally { value: 10 };
    with Counter => &mut tally do {
        break hold: Counter::next()
    }
    break hold: 0
};

export fn run() {
    assert A == 11;
}
"#;
    let reported = reported(source);
    assert!(
        reported.iter().any(|(context, impurity)| *context
            == PureContext::GlobalInitializer
                && matches!(impurity, Impurity::HandlerInstall)),
        "expected the handler install flagged in a global initializer, got {reported:?}"
    );
}

#[test]
fn pure_global_initializer_is_not_reported() {
    let source = r#"
fn pure_value() -> i32 {
    return 7;
}

global BASE: String = "https://example.com";
global DOCS: String = `${BASE}/docs`;
global N: i32 = pure_value() * 2;

export fn run() {
    assert N == 14;
    assert DOCS == "https://example.com/docs";
}
"#;
    let reported = reported(source);
    assert!(
        reported.is_empty(),
        "a pure global initializer must not be flagged: {reported:?}"
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
