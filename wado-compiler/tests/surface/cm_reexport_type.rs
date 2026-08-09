//! Re-exporting a named type owned by a CM component dependency: a `--lib`
//! whose export signature references a type imported from another component
//! (e.g. `Point` from `sub/cm-catalog.wasm`) must resolve that type to the
//! dependency's interface, not panic ("unresolved CM named type reference").
//! Regression for the CM instance emitter's missing component-interface
//! fallback (it had `wasi:` / `core:kiln/` fallbacks only).

use std::path::Path;
use wado_compiler::{CompilerOptions, OptLevel};

const LIB_WORLD_FQ: &str = "test:reexp/reexp@0.1.0";

fn compile_reexport() -> Vec<u8> {
    // `./sub/cm-catalog.wasm` resolves relative to this fixtures path.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/reexp.wado");
    let source = r#"
use { CmCatalog, Point } from "./sub/cm-catalog.wasm" with { type: "wasm" };
export fn mk(v: Point) -> Point {
    return CmCatalog::id_record(v);
}
"#;
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    crate::common::compile_source_with_compiler_options(Path::new(path), source, options)
        .expect("re-exporting a component-owned type compiles (no unresolved-type ICE)")
        .wasm
}

#[test]
fn reexported_component_type_compiles_and_decodes() {
    let wasm = compile_reexport();
    // Was an ICE; now compiles and re-parses as a valid component whose export
    // signature references the (component-owned) point record.
    let decoded = wit_component::decode(&wasm).expect("decode composed component");
    let wit_component::DecodedWasm::Component(resolve, world) = decoded else {
        panic!("expected a component");
    };
    let mk = resolve.worlds[world]
        .exports
        .iter()
        .find_map(|(_, item)| match item {
            wit_parser::WorldItem::Interface { id, .. } => {
                resolve.interfaces[*id].functions.get("mk")
            }
            _ => None,
        })
        .expect("exported interface exposes `mk`");
    let wit_parser::Type::Id(param_id) = mk.params[0].ty else {
        panic!("mk param is not a named type");
    };
    assert_eq!(
        resolve.types[param_id].name.as_deref(),
        Some("point"),
        "mk's param is the point record"
    );
    // A malformed structural duplication would fail validation; `compile_reexport`
    // compiles with validation on, and the decode above re-parses the type.
}
