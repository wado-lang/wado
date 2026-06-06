//! Tests for `check_effects_semantic` — the Semantics-based effect checker
//! (Design B, Phase 1b). It runs on the LSP analysis result (no TIR), so a
//! function with a missing effect is reported even when it is never called.

#![allow(unused_crate_dependencies)]

mod common;

use common::InMemoryHost;
use wado_compiler::check_effects_semantic;
use wado_compiler::semantics::semantics;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// Effect violations as `"callee: missing_effect"` strings.
fn violations(source: &str) -> Vec<String> {
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));
    check_effects_semantic(&sem)
        .into_iter()
        .map(|e| format!("{}: {}", e.callee, e.missing_effect))
        .collect()
}

#[test]
fn free_call_missing_effect_is_reported_even_in_dead_function() {
    // `bad` is never called, so reify gating would drop it — but the effect
    // violation must still surface from `Semantics`.
    let source = r#"
use { println, Stdout } from "core:cli";

fn greet(name: String) with Stdout {
    println(`Hello, {name}!`);
}

fn bad() {
    greet("Bob");
}

export fn run() with Stdout {
    println("ok");
}
"#;
    let v = violations(source);
    assert!(
        v.iter().any(|s| s.contains("Stdout")),
        "expected a missing-Stdout violation for `bad`, got {v:?}"
    );
}

#[test]
fn caller_with_effect_is_not_reported() {
    let source = r#"
use { println, Stdout } from "core:cli";

fn greet(name: String) with Stdout {
    println(`Hello, {name}!`);
}

export fn run() with Stdout {
    greet("Bob");
}
"#;
    assert!(
        violations(source).is_empty(),
        "caller declares `with Stdout`, so the call is covered: {:?}",
        violations(source)
    );
}

#[test]
fn static_method_call_missing_effect_is_reported() {
    let source = r#"
use { println, Stdout } from "core:cli";

struct Logger {}

impl Logger {
    fn log(message: String) with Stdout {
        println(message);
    }
}

fn bad() {
    Logger::log("Hello");
}

export fn run() with Stdout {
    println("ok");
}
"#;
    let v = violations(source);
    assert!(
        v.iter().any(|s| s.contains("Stdout")),
        "expected a missing-Stdout violation for the static call in `bad`, got {v:?}"
    );
}

#[test]
fn signature_resource_is_not_required_again() {
    // `use_counter` takes `&Counter` and calls a Counter method. Because the
    // resource already appears in the signature, no explicit `with Counter` is
    // needed — `signature_resources` must admit it.
    let source = r#"
pub resource Counter {
    fn bump(&self);
}

fn use_counter(c: &Counter) {
    c.bump();
}

export fn run() {}
"#;
    assert!(
        violations(source).is_empty(),
        "Counter is in the signature, so the call is covered: {:?}",
        violations(source)
    );
}

#[test]
fn effect_param_resolves_from_closure_argument() {
    // `wrapper<effect E>(f: fn() with E) with E` is called from an effect-free
    // function with a closure that needs Stdout, so E resolves to Stdout and
    // the missing effect must be reported.
    let source = r#"
use { println, Stdout } from "core:cli";

fn wrapper<effect E>(f: fn() with E) with E {
    f();
}

fn bad() {
    wrapper(|| {
        println("needs stdout");
    });
}

export fn run() with Stdout {
    println("ok");
}
"#;
    let v = violations(source);
    assert!(
        v.iter().any(|s| s.contains("Stdout")),
        "E should resolve to Stdout from the closure, got {v:?}"
    );
}

#[test]
fn benign_effect_is_admitted_in_body() {
    // `#[benign(Stdout)]` lets the body use Stdout without a `with Stdout`
    // clause, so no violation should be reported.
    let source = r#"
use { println, Stdout } from "core:cli";

#[benign(Stdout)]
fn quiet_log() {
    println("admitted");
}

export fn run() {}
"#;
    assert!(
        violations(source).is_empty(),
        "#[benign(Stdout)] admits Stdout in the body: {:?}",
        violations(source)
    );
}

#[test]
fn indirect_closure_call_missing_effect_is_reported() {
    // Calling a closure value whose body needs Stdout from an effect-free
    // function: the indirect call's function type carries the effect.
    let source = r#"
use { println, Stdout } from "core:cli";

fn bad() {
    let f = || {
        println("needs stdout");
    };
    f();
}

export fn run() with Stdout {
    println("ok");
}
"#;
    let v = violations(source);
    assert!(
        v.iter().any(|s| s.contains("Stdout")),
        "indirect call should require Stdout, got {v:?}"
    );
}

#[test]
fn effect_free_program_has_no_violations() {
    let source = r#"
fn add(a: i32, b: i32) -> i32 {
    return a + b;
}

export fn run() {
    let _x = add(1, 2);
}
"#;
    assert!(
        violations(source).is_empty(),
        "no effects in play: {:?}",
        violations(source)
    );
}
