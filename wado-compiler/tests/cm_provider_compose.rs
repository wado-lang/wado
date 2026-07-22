//! Provider composition: satisfy a dependency component's guest-effect import
//! with a sibling provider component, via `wasm-compose` (the acyclic shape the
//! Component Model sanctions — UseCases #8 dependency injection). See
//! `docs/research-cm-boundary-callbacks.md`.
//!
//! `sub/hlc.wasm` imports `test:hlc/highlight@0.1.0` and exports
//! `test:hlc/hlc` (`wrap(code, lang) -> Wrapped`). A provider is compiled from
//! source with `--implement test:hlc/highlight@0.1.0`. We compose
//! provider.export → hlc.import, export hlc's `wrap`, then call it: the host
//! calls `wrap`, hlc calls its `highlight` import, the provider runs.

#![allow(unused_crate_dependencies)]

mod common;

use std::path::Path;
use wado_compiler::{CompilerOptions, OptLevel};

const HLC_WASM: &[u8] = include_bytes!("fixtures/sub/hlc.wasm");
const GUEST_IFACE_FQ: &str = "test:hlc/highlight@0.1.0";
const DEP_EXPORT_FQ: &str = "test:hlc/hlc@0.1.0";

fn compile_provider() -> Vec<u8> {
    let source = r#"
export fn highlight(code: String, lang: String) -> String {
    return `${lang}:${code}`;
}
"#;
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(GUEST_IFACE_FQ.to_string()),
        lib_interface_export: true,
        ..Default::default()
    };
    common::compile_source_with_compiler_options(Path::new("provider.wado"), source, options)
        .expect("compile provider with --implement")
        .wasm
}

fn compose(provider: &[u8], dep: &[u8]) -> Vec<u8> {
    use wasm_compose::graph::{Component, CompositionGraph, EncodeOptions};
    use wasmparser::Validator;

    let mut validator = Validator::new_with_features(wasmparser::WasmFeatures::all());
    let mut graph = CompositionGraph::new();

    let dep_c = Component::from_bytes(&mut validator, "dep", dep.to_vec()).expect("dep decodes");
    let dep_id = graph.add_component(dep_c).unwrap();
    let dep_inst = graph.instantiate(dep_id).unwrap();

    let prov_c =
        Component::from_bytes(&mut validator, "provider", provider.to_vec()).expect("provider");
    let prov_id = graph.add_component(prov_c).unwrap();
    let prov_inst = graph.instantiate(prov_id).unwrap();

    // provider.export[highlight] -> dep.import[highlight]
    let prov_export = graph
        .get_component(prov_id)
        .and_then(|c| c.export_by_name(GUEST_IFACE_FQ))
        .map(|(idx, _, _)| idx)
        .expect("provider exports the interface");
    let dep_import = graph
        .get_component(dep_id)
        .and_then(|c| c.import_by_name(GUEST_IFACE_FQ))
        .map(|(idx, _)| idx)
        .expect("dep imports the interface");
    graph
        .connect(prov_inst, Some(prov_export), dep_inst, dep_import)
        .expect("sibling provider->dep wiring is acyclic and accepted");

    graph
        .encode(EncodeOptions {
            define_components: true,
            export: Some(dep_inst),
            validate: false,
        })
        .expect("compose encodes")
}

#[test]
fn provider_satisfies_dep_import_and_roundtrips() {
    let composed = compose(&compile_provider(), HLC_WASM);

    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    let engine = wasmtime::Engine::new(&config).unwrap();
    let component = wasmtime::component::Component::new(&engine, &composed)
        .expect("composed component validates");
    let linker = wasmtime::component::Linker::new(&engine);
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &component).expect("instantiate");

    use wasmtime::component::Val;

    // Look up `wrap` inside the exported `test:hlc/hlc` interface instance.
    let (_, iface) = instance
        .get_export(&mut store, None, DEP_EXPORT_FQ)
        .expect("hlc interface export");
    let (_, wrap_idx) = instance
        .get_export(&mut store, Some(&iface), "wrap")
        .expect("wrap export");
    let wrap = instance.get_func(&mut store, &wrap_idx).expect("wrap func");

    let mut results = [Val::Bool(false)];
    wrap.call(
        &mut store,
        &[Val::String("x".into()), Val::String("wado".into())],
        &mut results,
    )
    .expect("host->dep.wrap->dep.highlight-import->provider round-trip runs");

    let Val::Record(fields) = &results[0] else {
        panic!("wrap returns a record, got {:?}", results[0]);
    };
    let text = fields
        .iter()
        .find(|(k, _)| k == "text")
        .map(|(_, v)| v)
        .expect("Wrapped.text");
    assert_eq!(*text, Val::String("[wado:x]".into()), "round-trip value");
}

/// The compiler-integrated path: a consumer that imports `sub/hlc.wasm` and
/// calls `Hlc::wrap` with **no handler** compiles when a provider is supplied
/// (`CompilerOptions::providers` discharges the reconstructed effect and
/// `compose_dependency_components` wires the provider into the dependency).
/// The produced component is standalone and runs the round-trip.
#[test]
fn provider_option_discharges_and_composes_through_codegen() {
    let provider = compile_provider();

    let consumer_src = r#"
use { Hlc } from "./sub/hlc.wasm" with { type: "wasm" };
export fn go(code: String, lang: String) -> String {
    return Hlc::wrap(code, lang).text;
}
"#;
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/provider_consumer.wado");
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some("test:consumer/consumer@0.1.0".to_string()),
        providers: vec![wado_compiler::ProviderComponent {
            import_fq: GUEST_IFACE_FQ.to_string(),
            bytes: provider,
        }],
        ..Default::default()
    };
    let composed = common::compile_source_with_compiler_options(Path::new(path), consumer_src, options)
        .expect("consumer compiles: provider discharges Highlight and composes in")
        .wasm;

    // The provider satisfies the import, so the composed component no longer
    // imports the highlight interface.
    let decoded = wit_component::decode(&composed).expect("decode composed");
    let wit_component::DecodedWasm::Component(resolve, world) = decoded else {
        panic!("expected a component");
    };
    let still_imports_highlight = resolve.worlds[world].imports.iter().any(|(_, item)| {
        matches!(item, wit_parser::WorldItem::Interface { id, .. }
            if resolve.interfaces[*id].name.as_deref() == Some("highlight"))
    });
    assert!(
        !still_imports_highlight,
        "provider should have satisfied the highlight import"
    );

    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    let engine = wasmtime::Engine::new(&config).unwrap();
    let component =
        wasmtime::component::Component::new(&engine, &composed).expect("composed validates");
    let linker = wasmtime::component::Linker::new(&engine);
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &component).expect("instantiate");

    let go = instance
        .get_typed_func::<(&str, &str), (String,)>(&mut store, "go")
        .expect("go export (code, lang) -> string");
    let (text,) = go
        .call(&mut store, ("y", "wado"))
        .expect("consumer.go -> hlc.wrap -> hlc.highlight-import -> provider");
    assert_eq!(text, "[wado:y]", "round-trip through the composed provider");
}
