//! Unused diagnostics tests (WEP `wep-2026-05-16-unused-diagnostics.md`,
//! Stage 6 of `wep-2026-05-26-elaborator-rearchitecture.md`).
//!
//! Slice 1 covers `DeadFunction` / `DeadGlobal` for free functions and
//! globals: an item never reachable from the export boundary is reported;
//! a reachable one (and the export root itself) is not. The `--no-unused`
//! toggle (`CompilerOptions::unused_diagnostics`) silences the lints.

mod common;

use common::{InMemoryHost, runtime};
use wado_compiler::{Code, CompilerOptions, OptLevel};

/// Compile `source` and return the unused-diagnostic `(code, message)` pairs.
fn unused_for(source: &str) -> Vec<(Code, String)> {
    unused_for_with(source, true)
}

fn unused_for_with(source: &str, unused_diagnostics: bool) -> Vec<(Code, String)> {
    let host = InMemoryHost::new();
    let options = CompilerOptions {
        opt_level: OptLevel::O0,
        unused_diagnostics,
        ..CompilerOptions::default()
    };
    let _ = runtime().block_on(wado_compiler::compile_with_options(
        source,
        &host,
        Some("test.wado"),
        options,
    ));
    host.diagnostics()
        .into_iter()
        .filter(|d| {
            matches!(
                d.code,
                Code::DeadFunction
                    | Code::DeadGlobal
                    | Code::TestOnlyFunction
                    | Code::TestOnlyGlobal
            )
        })
        .map(|d| (d.code, d.message))
        .collect()
}

fn has_dead_function(diags: &[(Code, String)], name: &str) -> bool {
    diags
        .iter()
        .any(|(code, msg)| matches!(code, Code::DeadFunction) && msg.contains(name))
}

fn has_dead_global(diags: &[(Code, String)], name: &str) -> bool {
    diags
        .iter()
        .any(|(code, msg)| matches!(code, Code::DeadGlobal) && msg.contains(name))
}

fn has_test_only_function(diags: &[(Code, String)], name: &str) -> bool {
    diags
        .iter()
        .any(|(code, msg)| matches!(code, Code::TestOnlyFunction) && msg.contains(name))
}

#[test]
fn dead_private_function_is_reported() {
    let diags = unused_for(
        r#"
fn helper() -> i32 { return 1; }

export fn run() {}
"#,
    );
    assert!(
        has_dead_function(&diags, "helper"),
        "expected DeadFunction for `helper`, got {diags:?}"
    );
}

#[test]
fn used_function_is_not_reported() {
    let diags = unused_for(
        r#"
fn helper() -> i32 { return 1; }

export fn run() {
    let _x = helper();
}
"#,
    );
    assert!(
        !has_dead_function(&diags, "helper"),
        "`helper` is called and must not be reported, got {diags:?}"
    );
}

#[test]
fn export_root_is_not_reported() {
    let diags = unused_for(
        r#"
export fn run() {}
"#,
    );
    assert!(
        !has_dead_function(&diags, "run"),
        "the `run` export root must not be reported, got {diags:?}"
    );
}

#[test]
fn dead_global_is_reported() {
    let diags = unused_for(
        r#"
global UNUSED: i32 = 42;

export fn run() {}
"#,
    );
    assert!(
        has_dead_global(&diags, "UNUSED"),
        "expected DeadGlobal for `UNUSED`, got {diags:?}"
    );
}

#[test]
fn used_global_is_not_reported() {
    let diags = unused_for(
        r#"
global USED: i32 = 42;

export fn run() {
    let _x = USED;
}
"#,
    );
    assert!(
        !has_dead_global(&diags, "USED"),
        "`USED` is read and must not be reported, got {diags:?}"
    );
}

#[test]
fn transitively_dead_function_is_reported() {
    // `a` is called only by `b`, and `b` is never called: both are dead.
    let diags = unused_for(
        r#"
fn a() -> i32 { return 1; }

fn b() -> i32 { return a(); }

export fn run() {}
"#,
    );
    assert!(
        has_dead_function(&diags, "a") && has_dead_function(&diags, "b"),
        "both `a` and `b` are unreachable, got {diags:?}"
    );
}

#[test]
fn no_unused_flag_silences_diagnostics() {
    let diags = unused_for_with(
        r#"
fn helper() -> i32 { return 1; }

export fn run() {}
"#,
        false,
    );
    assert!(
        diags.is_empty(),
        "`--no-unused` must silence the lints, got {diags:?}"
    );
}

#[test]
fn function_used_only_by_test_is_test_only_not_dead() {
    // `helper` is reachable from a `test` block but never from production
    // (`run`), so it is `TestOnly`, not `DeadFunction` (and not silent).
    let diags = unused_for(
        r#"
fn helper() -> i32 { return 1; }

export fn run() {}

test "uses helper" {
    assert helper() == 1;
}
"#,
    );
    assert!(
        has_test_only_function(&diags, "helper"),
        "`helper` is reached only by a test → TestOnly, got {diags:?}"
    );
    assert!(
        !has_dead_function(&diags, "helper"),
        "`helper` must not also be reported dead, got {diags:?}"
    );
}

#[test]
fn function_used_by_production_and_test_is_not_reported() {
    // Reached from `run` (production) as well as a test → live, no diagnostic.
    let diags = unused_for(
        r#"
fn helper() -> i32 { return 1; }

export fn run() {
    let _ = helper();
}

test "also uses helper" {
    assert helper() == 1;
}
"#,
    );
    assert!(
        !has_test_only_function(&diags, "helper") && !has_dead_function(&diags, "helper"),
        "`helper` is used by production, so it is live: {diags:?}"
    );
}

#[test]
fn allow_dead_code_attr_silences_dead_function() {
    // `#[allow(dead_code)]` on an otherwise-dead function suppresses the lint,
    // matching Rust's `dead_code` lint name.
    let diags = unused_for(
        r#"
#[allow(dead_code)]
fn helper() -> i32 { return 1; }

export fn run() {}
"#,
    );
    assert!(
        !has_dead_function(&diags, "helper"),
        "`#[allow(dead_code)]` must silence DeadFunction, got {diags:?}"
    );
}

#[test]
fn allow_dead_code_attr_silences_dead_global() {
    let diags = unused_for(
        r#"
#[allow(dead_code)]
global UNUSED: i32 = 42;

export fn run() {}
"#,
    );
    assert!(
        !has_dead_global(&diags, "UNUSED"),
        "`#[allow(dead_code)]` must silence DeadGlobal, got {diags:?}"
    );
}

#[test]
fn allow_dead_code_attr_silences_test_only_function() {
    // The attribute covers the test-only classification too: an item the user
    // has explicitly marked dead-code-allowed should raise no unused lint at
    // all, whether it is reached by tests or by no one.
    let diags = unused_for(
        r#"
#[allow(dead_code)]
fn helper() -> i32 { return 1; }

export fn run() {}

test "uses helper" {
    assert helper() == 1;
}
"#,
    );
    assert!(
        !has_test_only_function(&diags, "helper") && !has_dead_function(&diags, "helper"),
        "`#[allow(dead_code)]` must silence the test-only lint, got {diags:?}"
    );
}

#[test]
fn generated_module_is_not_linted() {
    // `#![generated]` marks machine-emitted code (Gale's parser output, etc.).
    // It is never hand-edited, so linting it is pure noise — no unused
    // diagnostic fires regardless of reachability.
    let diags = unused_for(
        r#"
#![generated(by = "gale", sources = ["X.g4"])]

fn helper() -> i32 { return 1; }
global UNUSED: i32 = 42;

export fn run() {}
"#,
    );
    assert!(
        diags.is_empty(),
        "a `#![generated]` module must not be linted, got {diags:?}"
    );
}

#[test]
fn module_allow_dead_code_silences_every_item() {
    // A file-level `#![allow(dead_code)]` suppresses the lint for every item in
    // the module — the idiom for test-helper files whose functions exist only
    // to support `test` blocks.
    let diags = unused_for(
        r#"
#![allow(dead_code)]

fn helper() -> i32 { return 1; }
global UNUSED: i32 = 42;

export fn run() {}
"#,
    );
    assert!(
        diags.is_empty(),
        "module-level `#![allow(dead_code)]` silences all dead diagnostics, got {diags:?}"
    );
}

#[test]
fn function_used_by_neither_is_dead_not_test_only() {
    // No production and no test reference → `DeadFunction`.
    let diags = unused_for(
        r#"
fn helper() -> i32 { return 1; }

export fn run() {}

test "unrelated" {
    assert 1 == 1;
}
"#,
    );
    assert!(
        has_dead_function(&diags, "helper") && !has_test_only_function(&diags, "helper"),
        "`helper` is used by neither → Dead, got {diags:?}"
    );
}
