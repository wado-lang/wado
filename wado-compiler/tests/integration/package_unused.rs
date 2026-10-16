//! Unused diagnostics decided over a whole package
//! (`wado_compiler::package_unused_diagnostics`): the modules its entries reach
//! are production code, every other member is test code.

use crate::common::{in_memory_host, runtime};
use wado_compiler::{Code, Lints, PackageSources, kiln::InvocationIndex};

/// The unused diagnostics of a package made of `files`, the first of which is
/// its one entry, as `(code, message)` pairs.
fn package_unused(files: &[(&str, &str)]) -> Vec<(Code, String)> {
    package_unused_in(files, Lints::default())
}

/// [`package_unused`], reported in the modules `lints` names.
fn package_unused_in(files: &[(&str, &str)], lints: Lints) -> Vec<(Code, String)> {
    let rooted: Vec<(String, &str)> = files
        .iter()
        .map(|(path, source)| (format!("./{path}"), *source))
        .collect();
    let rooted: Vec<(&str, &str)> = rooted.iter().map(|(p, s)| (p.as_str(), *s)).collect();
    let host = in_memory_host(&rooted);
    let package = PackageSources {
        root: ".".to_string(),
        members: files.iter().map(|(path, _)| (*path).to_string()).collect(),
        entries: vec![files[0].0.to_string()],
    };
    let diagnostics = runtime()
        .block_on(wado_compiler::package_unused_diagnostics(
            &host,
            &package,
            InvocationIndex::default(),
            lints,
            wado_compiler::LogLevel::default(),
        ))
        .unwrap_or_else(|| panic!("the package must elaborate: {:?}", host.diagnostics()));
    diagnostics
        .into_iter()
        .map(|d| (d.code, d.message))
        .collect()
}

const LIB: &str = r#"
pub use { offered } from "./impl.wado";
use { helper } from "./impl.wado";

pub fn api() -> i32 { return helper(); }
"#;

const IMPL: &str = r#"
internal fn helper() -> i32 { return 1; }
internal fn unused_internal() -> i32 { return 2; }
internal fn tested_only() -> i32 { return 3; }
pub fn offered() -> i32 { return 4; }
pub fn unoffered() -> i32 { return 5; }
"#;

const IMPL_TEST: &str = r#"
use { tested_only, unoffered } from "./impl.wado";

fn check(n: i32) -> bool { return n > 0; }
fn stale_helper() -> i32 { return 6; }

test "impl" {
    assert check(tested_only());
    assert check(unoffered());
}
"#;

const SCRIPT: &str = r#"
export fn run() { step(); }
fn step() { }
"#;

fn diagnostics() -> Vec<(Code, String)> {
    package_unused(&[
        ("lib.wado", LIB),
        ("impl.wado", IMPL),
        ("impl_test.wado", IMPL_TEST),
        ("scripts/script.wado", SCRIPT),
    ])
}

fn reported(diags: &[(Code, String)], code: Code, message: &str) -> bool {
    diags.iter().any(|(c, m)| *c == code && m == message)
}

#[test]
fn an_internal_item_another_member_uses_is_not_reported() {
    let diags = diagnostics();
    assert!(
        !diags.iter().any(|(_, m)| m.contains("`helper`")),
        "{diags:?}"
    );
}

#[test]
fn an_internal_item_no_member_uses_is_never_used() {
    let diags = diagnostics();
    assert!(
        reported(
            &diags,
            Code::DeadFunction,
            "function `unused_internal` is never used"
        ),
        "{diags:?}"
    );
}

#[test]
fn production_code_only_a_test_reaches_is_only_used_by_tests() {
    let diags = diagnostics();
    assert!(
        reported(
            &diags,
            Code::TestOnlyFunction,
            "function `tested_only` is only used by tests"
        ),
        "{diags:?}"
    );
}

#[test]
fn a_pub_item_no_entry_offers_is_not_a_root() {
    let diags = diagnostics();
    assert!(
        reported(
            &diags,
            Code::TestOnlyFunction,
            "function `unoffered` is only used by tests"
        ),
        "{diags:?}"
    );
}

#[test]
fn what_an_entry_offers_is_a_root() {
    let diags = diagnostics();
    assert!(
        !diags
            .iter()
            .any(|(_, m)| m.contains("`offered`") || m.contains("`api`")),
        "{diags:?}"
    );
}

#[test]
fn a_program_beside_the_entries_is_its_own_root() {
    let diags = diagnostics();
    assert!(
        !diags
            .iter()
            .any(|(_, m)| m.contains("`run`") || m.contains("`step`")),
        "{diags:?}"
    );
}

#[test]
fn a_test_helper_a_test_reaches_is_not_reported() {
    let diags = diagnostics();
    assert!(
        !diags.iter().any(|(_, m)| m.contains("`check`")),
        "{diags:?}"
    );
}

#[test]
fn a_generated_module_keeps_what_it_calls_used() {
    let diags = package_unused(&[
        (
            "lib.wado",
            r#"use { parse } from "./parser.wado"; pub fn api() -> i32 { return parse(); }"#,
        ),
        (
            "parser.wado",
            r#"#![generated(by = "test")]
use { shift } from "./runtime.wado";
pub fn parse() -> i32 { return shift(); }"#,
        ),
        ("runtime.wado", "internal fn shift() -> i32 { return 1; }"),
    ]);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn a_generated_module_importing_only_the_stdlib_keeps_its_declarations() {
    let diags = package_unused(&[
        (
            "lib.wado",
            r#"use { parse } from "./parser.wado"; pub fn api() -> i32 { return parse(); }"#,
        ),
        (
            "parser.wado",
            r#"#![generated(by = "test")]
use { TreeMap } from "core:collections";
pub fn parse() -> i32 {
    let braces = `{${"}"}`;
    if braces.len() > 0 && '{' != '}' { return TreeMap::<i32, i32>::new().len(); }
    return 0;
}"#,
        ),
    ]);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn a_generated_module_reports_only_when_its_lints_are_asked_for() {
    let files = [
        (
            "lib.wado",
            r#"use { parse } from "./parser.wado"; pub fn api() -> i32 { return parse(); }"#,
        ),
        (
            "parser.wado",
            r#"#![generated(by = "test")]
pub fn parse() -> i32 { return 1; }
fn unused_rule() -> i32 { return 2; }"#,
        ),
    ];
    let dead = |diags: &[(Code, String)]| {
        reported(
            diags,
            Code::DeadFunction,
            "function `unused_rule` is never used",
        )
    };
    assert!(!dead(&package_unused(&files)));
    assert!(dead(&package_unused_in(&files, Lints::Generated)));
}

#[test]
fn a_test_helper_nothing_reaches_is_never_used() {
    let diags = diagnostics();
    assert!(
        reported(
            &diags,
            Code::DeadFunction,
            "function `stale_helper` is never used"
        ),
        "{diags:?}"
    );
}
