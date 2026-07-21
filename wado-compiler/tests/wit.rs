//! Tests for `wit_emit::emit_wit_text` — the WIT producer (WEP
//! `wep-2026-05-02-wit-interoperability.md`, Phase 1).
//!
//! Each case asserts the rendered WIT text and re-parses it with `wit-parser`
//! to confirm the output is syntactically valid WIT.

#![allow(unused_crate_dependencies)]

mod common;

use common::InMemoryHost;
use wado_compiler::semantics::{semantics, semantics_for_world};
use wado_compiler::wit_emit::{self, WitEmitOptions, WitScope, emit_wit_text};
use wado_compiler::{OptLevel, dump_with_host_and_world};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// The WIR-level import plan (`NirPackage::imported_cm_interfaces`) for
/// `source` under `world_fq`, the faithful world import set the emitter reads.
fn import_plan(source: &str, world_fq: &str) -> Vec<String> {
    let host = InMemoryHost::new();
    // Tolerant like the CLI's `resolve_world_imports`: a program that does not
    // compile to a full component (e.g. no world entry point) has no faithful
    // import set, which is the empty set for the emitter's purposes.
    match block_on(dump_with_host_and_world(
        source,
        &host,
        Some("entry.wado"),
        OptLevel::O2,
        Some(world_fq),
        None,
        None,
        None,
        &[],
        &wado_compiler::hashmap::IndexMap::default(),
        wado_compiler::param_resolution::ParamPolicy::default(),
        wado_compiler::kiln::InvocationIndex::default(),
    )) {
        Ok(dump) => dump
            .wir_package
            .map(|pkg| pkg.imported_cm_interfaces)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Emit WIT for `source` under `scope` targeting `world_fq`, feeding the
/// emitter the faithful import plan as the CLI does.
fn emit_world(source: &str, scope: WitScope, world_fq: &str) -> String {
    let host = InMemoryHost::new();
    let mut sem = block_on(semantics(source, &host, Some("entry.wado")));
    assert!(sem.is_complete(), "semantics did not complete for source");
    sem.set_wit_contract(wit_emit::wit_contract(Some(world_fq), None, Some("entry")));
    emit_wit_text(
        &sem,
        &WitEmitOptions { scope },
        &import_plan(source, world_fq),
    )
    .expect("emit_wit_text failed")
}

/// Emit WIT for `source` targeting the default CLI world, under `scope`.
fn emit_scope(source: &str, scope: WitScope) -> String {
    emit_world(source, scope, "wasi:cli/command")
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

/// Regression for issue #1478: WIT emission re-derives `Semantics` off the
/// codegen path, so it must apply the Kiln `Request<T>` adapter for the
/// generator world. Otherwise the `generate` export exposes the generic
/// `Request<Options>`, which is not representable in WIT, and the whole
/// component-type section is dropped. `semantics_for_world` runs the adapter,
/// leaving the representable revision-3 `generate(primary, inputs, options)`
/// signature with `options` typed to the generator's own `Options` record.
#[test]
fn kiln_generator_world_emits_typed_params_not_generic_request() {
    const GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";

pub struct Options {
    pub verbose: bool,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let _ = req.options.verbose;
    return Result::Ok(Response { files: [] });
}
"#;
    let host = InMemoryHost::new();
    let mut sem = block_on(semantics_for_world(
        GENERATOR,
        &host,
        Some("entry.wado"),
        Some("core:kiln/generator"),
        wado_compiler::kiln::InvocationIndex::new(),
    ));
    assert!(sem.is_complete(), "generator semantics did not complete");
    sem.set_wit_contract(wit_emit::wit_contract(
        Some("core:kiln/generator"),
        None,
        Some("entry"),
    ));
    let text = emit_wit_text(
        &sem,
        &WitEmitOptions {
            scope: WitScope::Local,
        },
        &[],
    )
    .expect("emit_wit_text must succeed for the generator world (issue #1478)");
    assert!(
        text.contains(
            "generate: func(primary: input-file, inputs: list<input-file>, options: options)"
        ),
        "generate must export the representable revision-3 typed params:\n{text}"
    );
    assert!(
        !text.contains("Request<")
            && !text.contains("request<options")
            && !text.contains("raw-request"),
        "neither the generic Request nor the retired raw-request may leak into WIT:\n{text}"
    );
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
fn full_scope_reconstructs_resource_methods_and_reparses() {
    // The HTTP service exercises resources (request/response/fields) with
    // methods, statics, and constructors, plus the `handle` entry point.
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../example/http_server.wado"
    ))
    .expect("read http_server example");
    let text = emit_world(&source, WitScope::Full, "wasi:http/service");

    assert!(text.contains("export wasi:http/handler@"), "\n{text}");
    assert!(text.contains("resource fields {"), "\n{text}");
    assert!(text.contains("constructor();"), "\n{text}");
    assert!(text.contains("static func"), "\n{text}");
    // The `self` parameter of instance methods is dropped.
    assert!(text.contains("get-method: func() ->"), "\n{text}");
    // No raw CM method markers leak into the WIT.
    assert!(!text.contains("[method]"), "\n{text}");

    let mut resolve = wit_parser::Resolve::new();
    resolve
        .push_str("service.wit", &text)
        .expect("resource WIT failed to re-parse");
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

#[test]
fn newtype_emits_alias_to_base_not_itself() {
    // Regression: a newtype must alias its base type, never `type x = x`.
    let text = emit(
        "type Meters = f64;\n\
         export fn id(v: Meters) -> Meters { return v; }",
    );
    assert!(text.contains("type meters = f64;"), "\n{text}");
    assert!(!text.contains("type meters = meters;"), "\n{text}");
    let mut resolve = wit_parser::Resolve::new();
    resolve.push_str("newtype.wit", &text).expect("valid WIT");
}

#[test]
fn result_unit_arms_collapse_to_wit_forms() {
    // A unit arm becomes the absent (`_`) arm, matching WIT's result sugar.
    let ok_only = emit("export fn id(v: Result<u32, ()>) -> Result<u32, ()> { return v; }");
    assert!(
        ok_only.contains("id: func(v: result<u32>) -> result<u32>;"),
        "\n{ok_only}"
    );
    let err_only = emit("export fn id(v: Result<(), String>) -> Result<(), String> { return v; }");
    assert!(
        err_only.contains("id: func(v: result<_, string>) -> result<_, string>;"),
        "\n{err_only}"
    );
    let both = emit("export fn id(v: Result<u32, String>) -> Result<u32, String> { return v; }");
    assert!(
        both.contains("id: func(v: result<u32, string>) -> result<u32, string>;"),
        "\n{both}"
    );
    let mut resolve = wit_parser::Resolve::new();
    resolve.push_str("result.wit", &both).expect("valid WIT");
}

#[test]
fn future_and_stream_map_to_wit() {
    let fut = emit("export fn id(v: Future<u32>) -> Future<u32> { return v; }");
    assert!(
        fut.contains("id: func(v: future<u32>) -> future<u32>;"),
        "\n{fut}"
    );
    let st = emit("export fn id(v: Stream<u8>) -> Stream<u8> { return v; }");
    assert!(
        st.contains("id: func(v: stream<u8>) -> stream<u8>;"),
        "\n{st}"
    );
}

/// Golden test for `package-cm-catalog`: re-emit the value-type catalog from
/// its source and assert it matches the committed `cm-catalog.wit`, so the
/// published artifact cannot drift from the emitter. Covers the whole
/// value-type surface (primitives, containers, all four `result` forms, named
/// types, nested compositions) in one fixture.
#[test]
fn cm_catalog_matches_committed_wit() {
    let source = include_str!("../../package-cm-catalog/src/lib.wado");
    let expected = include_str!("../../package-cm-catalog/cm-catalog.wit");
    let host = InMemoryHost::new();
    let mut sem = block_on(semantics(source, &host, Some("lib.wado")));
    assert!(sem.is_complete(), "catalog source did not analyze");
    sem.set_wit_contract(wit_emit::wit_contract(
        Some("wasi:cli/command"),
        None,
        Some("cm-catalog"),
    ));
    // Pure value types import no CM interface.
    let text = emit_wit_text(
        &sem,
        &WitEmitOptions {
            scope: WitScope::Full,
        },
        &[],
    )
    .expect("emit_wit_text failed");
    assert_eq!(
        text.trim_end(),
        expected.trim_end(),
        "cm-catalog.wit is stale; regenerate with `wado wit package-cm-catalog/src/lib.wado`\n--- emitted ---\n{text}"
    );
    let mut resolve = wit_parser::Resolve::new();
    resolve
        .push_str("cm-catalog.wit", &text)
        .expect("catalog WIT failed to re-parse");
}
