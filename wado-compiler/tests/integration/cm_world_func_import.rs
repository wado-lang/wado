//! Phase 9: importing a component whose export is a **bare world-level
//! function** (not grouped under a WIT interface). `sub/hlwf.wasm` exports the
//! world-level function `highlight(source) -> string`. A consumer imports it as
//! a free function and calls it; the dependency composes in via `wasm-compose`
//! (`dep.export[highlight] -> program.import[highlight]`), so the produced
//! component is standalone and runs the round-trip.
//!
//! See `docs/wep-2026-06-26-wasm-cm-component-import.md` (Phase 9).

use std::path::Path;
use wado_compiler::{CompilerOptions, OptLevel};

#[test]
fn world_level_function_export_imports_as_a_free_function() {
    let consumer_src = r#"
use { highlight } from "./sub/hlwf.wasm" with { type: "wasm" };
export fn go(source: String) -> String {
    return highlight(source);
}
"#;
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/world_func_consumer.wado"
    );
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some("test:consumer/consumer@0.1.0".to_string()),
        ..Default::default()
    };
    let composed =
        crate::common::compile_source_with_compiler_options(Path::new(path), consumer_src, options)
            .expect("consumer imports a world-level function export and composes it in")
            .wasm;

    // The dependency is composed in, so the world-level function is no longer imported.
    let decoded = wit_component::decode(&composed).expect("decode composed");
    let wit_component::DecodedWasm::Component(resolve, world) = decoded else {
        panic!("expected a component");
    };
    let still_imports = resolve.worlds[world].imports.iter().any(
        |(_, item)| matches!(item, wit_parser::WorldItem::Function(f) if f.name == "highlight"),
    );
    assert!(
        !still_imports,
        "provider/dep should satisfy the highlight import"
    );

    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    let engine = wasmtime::Engine::new(&config).unwrap();
    let component =
        wasmtime::component::Component::new(&engine, &composed).expect("composed validates");
    let linker = wasmtime::component::Linker::new(&engine);
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = linker
        .instantiate(&mut store, &component)
        .expect("instantiate");
    let go = instance
        .get_typed_func::<(&str,), (String,)>(&mut store, "go")
        .expect("go export (source) -> string");
    let (text,) = go
        .call(&mut store, ("x",))
        .expect("go -> hlwf.highlight round-trip");
    assert_eq!(
        text, "hl:x",
        "round-trip through the composed world-level function"
    );
}

/// A multi-word world-level function (`render_html`, WIT `render-html`) must
/// also compose: the plan keys it by the Wado (snake) name while the dependency
/// exports and the program imports the CM (kebab) name, so composition must
/// translate between them. A single-word name hides the mismatch.
#[test]
fn multi_word_world_level_function_composes() {
    let consumer_src = r#"
use { render_html } from "./sub/hlwf_mw.wasm" with { type: "wasm" };
export fn go(source: String) -> String {
    return render_html(source);
}
"#;
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/world_func_mw_consumer.wado"
    );
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some("test:consumer/consumer@0.1.0".to_string()),
        ..Default::default()
    };
    let composed =
        crate::common::compile_source_with_compiler_options(Path::new(path), consumer_src, options)
            .expect("multi-word world-level function composes")
            .wasm;

    let decoded = wit_component::decode(&composed).expect("decode composed");
    let wit_component::DecodedWasm::Component(resolve, world) = decoded else {
        panic!("expected a component");
    };
    let still_imports = resolve.worlds[world].imports.iter().any(
        |(_, item)| matches!(item, wit_parser::WorldItem::Function(f) if f.name == "render-html"),
    );
    assert!(
        !still_imports,
        "render-html must be composed away, not left imported"
    );

    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    let engine = wasmtime::Engine::new(&config).unwrap();
    let component =
        wasmtime::component::Component::new(&engine, &composed).expect("composed validates");
    let linker = wasmtime::component::Linker::new(&engine);
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = linker
        .instantiate(&mut store, &component)
        .expect("instantiate");
    let go = instance
        .get_typed_func::<(&str,), (String,)>(&mut store, "go")
        .expect("go export");
    let (text,) = go
        .call(&mut store, ("x",))
        .expect("go -> render-html round-trip");
    assert_eq!(
        text, "<x>",
        "round-trip through the composed multi-word function"
    );
}
