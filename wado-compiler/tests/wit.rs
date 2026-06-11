//! Tests for `wit_emit::emit_wit_text` — the WIT producer (WEP
//! `wep-2026-05-02-wit-interoperability.md`, Phase 1).
//!
//! Each case asserts the rendered WIT text and re-parses it with `wit-parser`
//! to confirm the output is syntactically valid WIT.

#![allow(unused_crate_dependencies)]

mod common;

use common::InMemoryHost;
use wado_compiler::semantics::semantics;
use wado_compiler::wit_emit::{WitEmitOptions, WitScope, emit_wit_text};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// Emit WIT for `source` targeting the default CLI world, under `scope`.
fn emit_scope(source: &str, scope: WitScope) -> String {
    let host = InMemoryHost::new();
    let sem = block_on(semantics(source, &host, Some("entry.wado")));
    assert!(sem.is_complete(), "semantics did not complete for source");
    let opts = WitEmitOptions {
        scope,
        world_fq: "wasi:cli/command".to_string(),
        default_interface_name: "entry".to_string(),
    };
    emit_wit_text(&sem, &opts).expect("emit_wit_text failed")
}

/// Emit WIT for `source` under `local` scope (no inlined nested packages).
fn emit(source: &str) -> String {
    emit_scope(source, WitScope::Local)
}

/// Assert the emitted WIT equals `expected` and is valid WIT.
fn check(source: &str, expected: &str) {
    let text = emit(source);
    assert_eq!(
        text.trim_end(),
        expected.trim_end(),
        "\n--- emitted ---\n{text}"
    );
    let mut resolve = wit_parser::Resolve::new();
    resolve
        .push_str("emitted.wit", &text)
        .expect("emitted WIT failed to re-parse");
}

#[test]
fn empty_world_when_no_exports() {
    check(
        "fn helper() -> i32 { return 42; }",
        "package root:component;\n\nworld command {\n}",
    );
}

#[test]
fn functions_only_become_direct_world_exports() {
    check(
        "export fn add(a: i32, b: i32) -> i32 { return a + b; }",
        "package root:component;\n\nworld command {\n  export add: func(a: s32, b: s32) -> s32;\n}",
    );
}

#[test]
fn record_export_groups_into_default_interface() {
    check(
        "pub struct Point { x: f64, y: f64 }\n\
         export fn midpoint(a: Point, b: Point) -> Point { return a; }",
        "package root:component;\n\n\
         interface entry {\n  \
           record point {\n    x: f64,\n    y: f64,\n  }\n  \
           midpoint: func(a: point, b: point) -> point;\n\
         }\n\n\
         world command {\n  export entry;\n}",
    );
}

#[test]
fn cli_program_emits_faithful_world_imports_and_run_export() {
    // A `run` entry with `with Stdout` maps to the standard `wasi:cli/run`
    // export and imports the used `wasi:cli/stdout` interface by FQ.
    let text = emit(
        "use { println } from \"core:cli\";\n\
         export fn run() with Stdout { println(\"hi\"); }",
    );
    assert!(text.contains("world command {"), "\n{text}");
    assert!(text.contains("import wasi:cli/stdout@"), "\n{text}");
    // Transitive: stdout's signature references `ErrorCode` from wasi:cli/types,
    // so the faithful import set includes it too.
    assert!(text.contains("import wasi:cli/types@"), "\n{text}");
    assert!(text.contains("export wasi:cli/run@"), "\n{text}");
    // `run` is not a bare function export under the faithful mapping.
    assert!(!text.contains("export run:"), "\n{text}");
}

#[test]
fn full_scope_inlines_referenced_interfaces_and_reparses() {
    // `full` scope inlines the referenced WASI interfaces as nested packages,
    // producing a self-describing document that re-parses without a registry.
    let text = emit_scope(
        "use { println } from \"core:cli\";\n\
         export fn run() with Stdout { println(\"hi\"); }",
        WitScope::Full,
    );
    assert!(text.contains("package wasi:cli@"), "\n{text}");
    assert!(text.contains("interface stdout {"), "\n{text}");
    assert!(text.contains("enum error-code {"), "\n{text}");
    assert!(text.contains("use types.{"), "\n{text}");

    let mut resolve = wit_parser::Resolve::new();
    resolve
        .push_str("full.wit", &text)
        .expect("full-scope WIT failed to re-parse");
}

#[test]
fn string_and_list_and_option_map_to_wit() {
    let text = emit("export fn lookup(keys: List<String>, maybe: Option<String>, n: u32) { }");
    assert!(
        text.contains("export lookup: func(keys: list<string>, maybe: option<string>, n: u32);"),
        "\n{text}"
    );
    let mut resolve = wit_parser::Resolve::new();
    resolve.push_str("emitted.wit", &text).expect("valid WIT");
}
