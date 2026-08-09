//! Tests for `wit_bundle` — the `component-type` embedding (WEP
//! `wep-2026-05-02-wit-interoperability.md`, Phase 2).
//!
//! These pin down the Phase 2 design decisions, including the central finding:
//! a Wado component is *already* self-describing, so `wit_parser::decode` (what
//! `wasm-tools component wit` uses) reconstructs WIT from the component itself,
//! and the embedded `component-type` section is additive full-fidelity metadata
//! decodable as a standalone WIT package.

use crate::common::InMemoryHost;
use wado_compiler::semantics::semantics;
use wado_compiler::wit_bundle::{embed_component_type, encode_component_type};
use wado_compiler::wit_emit;
use wado_compiler::{OptLevel, dump_with_host_and_world};

use wit_parser::decoding::{DecodedWasm, decode};

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// Faithful world import set (the WIR-level plan), as the CLI feeds the emitter.
fn import_plan(source: &str, world_fq: &str) -> Vec<String> {
    let host = InMemoryHost::new();
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

/// Compile `source` to component bytes at the given world.
fn compile(source: &str, world_fq: &str) -> Vec<u8> {
    let host = InMemoryHost::new();
    let mut options = wado_compiler::CompilerOptions {
        opt_level: OptLevel::O2,
        target_world: Some(world_fq.to_string()),
        ..Default::default()
    };
    options.log_level = Some(wado_compiler::LogLevel::Error);
    block_on(wado_compiler::compile_with_options(
        source,
        &host,
        Some("entry.wado"),
        options,
    ))
    .map(|r| r.wasm)
    .unwrap_or_else(|_| panic!("compile failed:\n{:#?}", host.diagnostics()))
}

/// Set the WIT contract on `sem` for `world_fq`, matching the CLI's setup.
fn with_contract(
    mut sem: wado_compiler::semantics::Semantics,
    world_fq: &str,
) -> wado_compiler::semantics::Semantics {
    sem.set_wit_contract(wit_emit::wit_contract(Some(world_fq), None, Some("entry")));
    sem
}

const CLI_WORLD: &str = "wasi:cli/command";

/// A CLI command: `run` with the `Stdout` effect (the canonical hello shape).
const HELLO: &str = r#"
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("Hello, world!");
}
"#;

/// A pure-compute command: a `run` entry with no effects, exercising embedding
/// with a near-empty import set.
const PURE: &str = r#"
export fn run() {
    let x = 1 + 2;
    assert x == 3;
}
"#;

/// The un-embedded Wado component already self-describes: `decode` reconstructs
/// a `Component` from the component's own CM type structure, no custom section.
#[test]
fn component_is_self_describing_without_embedding() {
    let wasm = compile(HELLO, CLI_WORLD);
    let DecodedWasm::Component(resolve, world) = decode(&wasm).expect("decode component") else {
        panic!("expected a Component, not a WIT package");
    };
    // The reconstructed world carries the real CM imports/exports.
    let w = &resolve.worlds[world];
    let import_names: Vec<String> = w
        .imports
        .keys()
        .map(|k| resolve.name_world_key(k))
        .collect();
    let export_names: Vec<String> = w
        .exports
        .keys()
        .map(|k| resolve.name_world_key(k))
        .collect();
    assert!(
        import_names
            .iter()
            .any(|n| n.starts_with("wasi:cli/stdout")),
        "imports: {import_names:?}"
    );
    assert!(
        export_names.iter().any(|n| n.starts_with("wasi:cli/run")),
        "exports: {export_names:?}"
    );
}

/// Embedding is byte-additive: it appends a `component-type` custom section,
/// leaving the original component bytes (and thus its intrinsic type) untouched.
#[test]
fn embedding_appends_section_and_preserves_component() {
    let wasm = compile(HELLO, CLI_WORLD);
    let host = InMemoryHost::new();
    let sem = with_contract(
        block_on(semantics(HELLO, &host, Some("entry.wado"))),
        CLI_WORLD,
    );
    assert!(sem.is_complete());

    let embedded = embed_component_type(&wasm, &sem, &import_plan(HELLO, CLI_WORLD))
        .expect("embed_component_type");

    assert!(embedded.len() > wasm.len(), "section should add bytes");
    assert_eq!(
        &embedded[..wasm.len()],
        &wasm[..],
        "original bytes preserved"
    );

    // `wasm-tools component wit` still reads the intrinsic component type — the
    // embedded section does not flip the artifact to a WIT package.
    assert!(
        matches!(decode(&embedded), Ok(DecodedWasm::Component(..))),
        "embedded artifact must still decode as a Component"
    );

    // Exactly one `component-type` custom section was added.
    let count = wasmparser::Parser::new(0)
        .parse_all(&embedded)
        .filter(|p| matches!(p, Ok(wasmparser::Payload::CustomSection(r)) if r.name() == "component-type"))
        .count();
    assert_eq!(count, 1, "exactly one component-type section");
}

/// The encoded payload is a standalone WIT package (the full-fidelity contract),
/// for `wkg` / `wasm-tools metadata` and relink flows.
#[test]
fn encoded_payload_decodes_as_wit_package() {
    let host = InMemoryHost::new();
    let sem = with_contract(
        block_on(semantics(HELLO, &host, Some("entry.wado"))),
        CLI_WORLD,
    );
    let payload =
        encode_component_type(&sem, &import_plan(HELLO, CLI_WORLD)).expect("encode_component_type");
    assert!(
        matches!(decode(&payload), Ok(DecodedWasm::WitPackage(..))),
        "payload must decode as a standalone WIT package"
    );
}

/// A minimal-import command embeds and round-trips just like the hello shape.
#[test]
fn pure_compute_world_embeds() {
    let wasm = compile(PURE, CLI_WORLD);
    let host = InMemoryHost::new();
    let sem = with_contract(
        block_on(semantics(PURE, &host, Some("entry.wado"))),
        CLI_WORLD,
    );
    let embedded = embed_component_type(&wasm, &sem, &import_plan(PURE, CLI_WORLD)).expect("embed");
    assert!(matches!(decode(&embedded), Ok(DecodedWasm::Component(..))));

    let payload = encode_component_type(&sem, &import_plan(PURE, CLI_WORLD)).expect("encode");
    assert!(matches!(decode(&payload), Ok(DecodedWasm::WitPackage(..))));
}
