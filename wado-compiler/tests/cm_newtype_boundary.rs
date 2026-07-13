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

/// Name of a `wit_parser::Type`, or `None` for an unnamed primitive.
fn type_name(resolve: &wit_parser::Resolve, ty: &wit_parser::Type) -> Option<String> {
    match ty {
        wit_parser::Type::Id(id) => resolve.types[*id].name.clone(),
        _ => None,
    }
}

fn compile_lib_source(source: &str) -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nt_boundary.wado");
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    common::compile_source_with_compiler_options(Path::new(path), source, options)
        .expect("compile inline --lib source")
        .wasm
}

/// A local newtype is preserved in *nested* positions too — a record field, a
/// `list` element, an `option` payload — matching `wado wit` (issue #1456,
/// review finding #3/#4), not just a top-level export parameter.
#[test]
fn lib_structural_type_preserves_newtype_in_nested_positions() {
    let source = r#"
pub type Meters = f64;
pub struct Line {
    length: Meters,
}
export fn id_line(v: Line) -> Line {
    return v;
}
export fn id_list(v: List<Meters>) -> List<Meters> {
    return v;
}
test "shape compiles" {}
"#;
    let wasm = compile_lib_source(source);
    let decoded = wit_component::decode(&wasm).expect("decode structural type");
    let resolve = decoded.resolve();

    // The record field `length` must reference `meters`, not bare `f64`.
    let line = resolve
        .types
        .iter()
        .find_map(|(_, t)| match &t.kind {
            wit_parser::TypeDefKind::Record(r) if t.name.as_deref() == Some("line") => Some(r),
            _ => None,
        })
        .expect("record `line` present");
    let length = line
        .fields
        .iter()
        .find(|f| f.name == "length")
        .expect("field `length` present");
    assert_eq!(
        type_name(resolve, &length.ty).as_deref(),
        Some("meters"),
        "record field erased the newtype to its base"
    );

    // `id-list` param: `list<meters>` — the element must be the named alias.
    let list_fn = resolve
        .interfaces
        .iter()
        .find_map(|(_, i)| i.functions.get("id-list"))
        .expect("id-list export present");
    let wit_parser::Type::Id(list_id) = list_fn.params[0].ty else {
        panic!("id-list param is not a defined type");
    };
    let wit_parser::TypeDefKind::List(elem) = &resolve.types[list_id].kind else {
        panic!("id-list param is not a list");
    };
    assert_eq!(
        type_name(resolve, elem).as_deref(),
        Some("meters"),
        "list element erased the newtype to its base"
    );
}
