//! `wado compile --lib` must preserve a local newtype (`type Meters = f64`) as a
//! named CM type alias in the compiled component's *structural* type, matching
//! what `wado wit` renders (issue #1456). `wit_component::decode` recovers WIT
//! from the component's own types, so a decoded `id-newtype` that reads
//! `func(v: f64) -> f64` (with no `meters` type) is the drift this guards.

#![allow(unused_crate_dependencies)]

mod common;

use std::path::Path;
use wado_compiler::{CompilerOptions, OptLevel};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/cm_catalog.wado"
);
const LIB_WORLD_FQ: &str = "wado-lang:cm-catalog/cm-catalog@0.1.0";

fn compile_lib() -> Vec<u8> {
    let source = std::fs::read_to_string(FIXTURE).unwrap();
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    common::compile_source_with_compiler_options(Path::new(FIXTURE), &source, options)
        .expect("compile catalog as --lib")
        .wasm
}

#[test]
fn lib_structural_type_preserves_newtype_alias() {
    let wasm = compile_lib();
    let decoded = wit_component::decode(&wasm).expect("decode component structural type");
    let resolve = decoded.resolve();

    // The `meters` alias must survive as a named type in the component's own type.
    let has_meters = resolve
        .types
        .iter()
        .any(|(_, t)| t.name.as_deref() == Some("meters"));
    assert!(
        has_meters,
        "compiled --lib component dropped the `meters` newtype alias; named types = {:?}",
        resolve
            .types
            .iter()
            .filter_map(|(_, t)| t.name.as_deref())
            .collect::<Vec<_>>()
    );

    // `id-newtype` must reference the named `meters` type, not bare `f64`.
    let iface_fn = resolve
        .interfaces
        .iter()
        .find_map(|(_, i)| i.functions.get("id-newtype"))
        .expect("id-newtype export present");
    let param_ty = iface_fn.params.first().expect("id-newtype has a param").ty;
    match param_ty {
        wit_parser::Type::Id(id) => {
            assert_eq!(
                resolve.types[id].name.as_deref(),
                Some("meters"),
                "id-newtype param is a named type but not `meters`"
            );
        }
        other => panic!("id-newtype param erased to a bare primitive: {other:?}"),
    }
}
