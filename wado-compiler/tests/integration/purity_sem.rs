//! Tests for `check_purity_semantic` — the Semantics-based purity checker
//! (Design B). It runs on the LSP analysis result (no TIR), so an effect in a
//! default expression or a global initializer surfaces even without reify.

use crate::common::{InMemoryHost, block_on};
use wado_compiler::semantics::semantics;
use wado_compiler::{INDIRECT_CALLEE, Impurity, PureContext, check_purity_semantic};

/// Purity callees reported for `source`, whatever the position.
fn violations(source: &str) -> Vec<String> {
    reported(source)
        .into_iter()
        .map(|(_, impurity)| match impurity {
            Impurity::Call(callee) | Impurity::Dispatch(callee) => callee,
        })
        .collect()
}

/// Purity violations reported against a global initializer.
fn in_global(source: &str) -> Vec<Impurity> {
    reported(source)
        .into_iter()
        .filter(|(context, _)| *context == PureContext::GlobalInitializer)
        .map(|(_, impurity)| impurity)
        .collect()
}

/// Every purity violation for `source`, with the position it was found in.
fn reported(source: &str) -> Vec<(PureContext, Impurity)> {
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));
    check_purity_semantic(&sem)
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
    let found = in_global(source);
    assert!(
        found
            .iter()
            .any(|i| matches!(i, Impurity::Call(callee) if callee == "noisy")),
        "expected `noisy` flagged in a global initializer, got {found:?}"
    );
}

/// A user-defined effect's operation demands nothing of the position: an
/// installed handler answers it, and a dispatch with none traps, in an
/// initializer as in a function body.
#[test]
fn interface_operation_in_global_initializer_is_not_reported() {
    let source = r#"
interface Counter {
    fn next() -> i32;
}

global A: i32 = Counter::next();

export fn run() {
    assert A == 41;
}
"#;
    let found = in_global(source);
    assert!(
        found.is_empty(),
        "an unhandled dispatch traps rather than failing to compile: {found:?}"
    );
}

/// A `with … do` grants the effect it installs to its body, and nothing beyond
/// it: `tick` declares `with Counter`, so the same call is answered inside the
/// install and unanswered outside it.
#[test]
fn handler_install_grants_only_its_own_body() {
    let source = r#"
interface Counter {
    fn next() -> i32;
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

fn tick() -> i32 with Counter {
    return Counter::next();
}

global INSIDE: i32 = hold: {
    let mut tally = Tally { value: 10 };
    with Counter => &mut tally do {
        break hold: tick()
    }
    break hold: 0
};

global OUTSIDE: i32 = tick();

export fn run() {
    assert INSIDE + OUTSIDE > 0;
}
"#;
    let found = in_global(source);
    assert_eq!(
        found.len(),
        1,
        "only the call outside the install is unanswered: {found:?}"
    );
    assert!(
        matches!(&found[0], Impurity::Call(callee) if callee == "tick"),
        "expected `tick` flagged outside the install, got {found:?}"
    );
}

/// A closure literal's body is read where it is written, as it is inside a
/// function: a `fn() with Stdout` annotation grants the body nothing.
#[test]
fn effectful_closure_literal_in_global_initializer_is_reported() {
    let source = r#"
use { println, Stdout } from "core:cli";

global CB: fn() with Stdout = || { println("hi"); };

export fn run() with Stdout {
    CB();
}
"#;
    let found = in_global(source);
    assert!(
        found
            .iter()
            .any(|i| matches!(i, Impurity::Call(callee) if callee == "println")),
        "expected `println` flagged in the closure body, got {found:?}"
    );
}

/// A closure whose body performs nothing is a value like any other.
#[test]
fn pure_closure_literal_in_global_initializer_is_not_reported() {
    let source = r#"
global DOUBLER: fn(i32) -> i32 = |n| n * 2;

export fn run() {
    assert DOUBLER(21) == 42;
}
"#;
    let found = in_global(source);
    assert!(
        found.is_empty(),
        "a pure closure performs nothing: {found:?}"
    );
}

/// An operation declaring a default body needs no handler — the default is what
/// a dispatch with none installed runs.
#[test]
fn defaulted_operation_in_global_initializer_is_not_reported() {
    let source = r#"
interface Log {
    fn level() -> i32 {
        return 3;
    }
}

global A: i32 = Log::level();

export fn run() {
    assert A == 3;
}
"#;
    let found = in_global(source);
    assert!(found.is_empty(), "the default body runs: {found:?}");
}

/// An `effect E` bound through a named argument, where no closure body is
/// written to walk into: only resolving `E` against the argument's type sees it.
#[test]
fn bound_effect_parameter_in_global_initializer_is_reported() {
    let source = r#"
use { println, Stdout } from "core:cli";

fn noisy(n: i32) -> i32 with Stdout {
    println("side effect");
    return n + 1;
}

fn apply<T, effect E>(f: fn(T) -> T with E, x: T) -> T with E {
    return f(x);
}

global A: i32 = apply(noisy, 41);

export fn run() with Stdout {
    println(`${A}`);
}
"#;
    let found = in_global(source);
    assert!(
        found
            .iter()
            .any(|i| matches!(i, Impurity::Call(callee) if callee == "apply")),
        "expected `apply` flagged: `E` binds to Stdout here, got {found:?}"
    );
}

/// An `effect E` that bound to no concrete effect demands nothing, the way
/// `SemEffectWalker::report_missing` reads it.
#[test]
fn unbound_effect_parameter_in_global_initializer_is_not_reported() {
    let source = r#"
fn apply<T, effect E>(f: fn(T) -> T with E, x: T) -> T with E {
    return f(x);
}

global A: i32 = apply(|n| n + 1, 41);

export fn run() {
    assert A == 42;
}
"#;
    let found = in_global(source);
    assert!(
        found.is_empty(),
        "an unbound effect parameter is not an effect: {found:?}"
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
    let found = reported(source);
    assert!(
        found.is_empty(),
        "a pure global initializer must not be flagged: {found:?}"
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
    let v = violations(source);
    assert!(v.is_empty(), "a pure default must not be flagged: {v:?}");
}

#[test]
fn effectful_tagged_template_in_global_initializer_is_reported() {
    let source = r#"
use { println, Stdout } from "core:cli";

fn shout<T: ReflectTemplate<Holes = [..V]>, ..V: Display>(t: T) -> i32 with Stdout {
    println("tag");
    return 1;
}

global TAGGED: i32 = shout`hello ${1}`;

export fn run() with Stdout {
    println(`${TAGGED}`);
}
"#;
    let found = in_global(source);
    assert!(
        found
            .iter()
            .any(|i| matches!(i, Impurity::Call(callee) if callee == "shout")),
        "a tag is a call, so its effects reach the initializer: {found:?}"
    );
}

#[test]
fn effectful_indirect_call_in_global_initializer_is_reported() {
    let source = r#"
use { println, Stdout } from "core:cli";

fn noisy() -> i32 with Stdout {
    println("noisy");
    return 7;
}

global INDIRECT: i32 = hold: {
    let f: fn() -> i32 with Stdout = noisy;
    break hold: f()
};

export fn run() with Stdout {
    println(`${INDIRECT}`);
}
"#;
    let found = in_global(source);
    assert!(
        found
            .iter()
            .any(|i| matches!(i, Impurity::Call(callee) if callee == INDIRECT_CALLEE)),
        "a call through a function-typed value performs what its type declares: {found:?}"
    );
}

#[test]
fn host_operation_in_global_initializer_is_reported_as_a_dispatch() {
    let source = r#"
use { println, Stdout } from "core:cli";
use { MonotonicClock, Mark } from "wasi:clocks";

global STARTED: Mark = MonotonicClock::now();

export fn run() with Stdout {
    println(`${STARTED:?}`);
}
"#;
    let found = in_global(source);
    assert!(
        found
            .iter()
            .any(|i| matches!(i, Impurity::Dispatch(op) if op == "now")),
        "a host-backed operation demands a capability, not a handler: {found:?}"
    );
}
