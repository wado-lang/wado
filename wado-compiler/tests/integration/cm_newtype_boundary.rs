//! `wado compile --lib` must preserve a local newtype (`type Meters = f64`) as a
//! named CM type alias in the compiled component's *structural* type, matching
//! what `wado wit` renders (issue #1456). `wit_component::decode` recovers WIT
//! from the component's own types, so a decoded `id-newtype` that reads
//! `func(v: f64) -> f64` (with no `meters` type) is the drift this guards. A
//! stdlib newtype the signature names is carried the same way.

use crate::cm_catalog::{FIXTURE, LIB_WORLD_FQ};
use crate::common::{
    DEFAULT_TIMEOUT_MS, WasiState, compile_lib_world, compile_source_with_compiler_options, engine,
    lib_func, limit_store, linker, runtime,
};
use std::path::Path;
use wado_compiler::{CompilerOptions, OptLevel};
use wasmtime::Store;
use wasmtime::component::{Component, Val};

fn compile_lib() -> Vec<u8> {
    let source = std::fs::read_to_string(FIXTURE).unwrap();
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    compile_source_with_compiler_options(Path::new(FIXTURE), &source, options)
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
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/nt_boundary.wado");
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    compile_source_with_compiler_options(Path::new(path), source, options)
        .expect("compile inline --lib source")
        .wasm
}

/// Regression: the CM export adapter lifted an `Option<newtype>` payload as
/// `i32` (the fall-through default) instead of the newtype's base, producing
/// invalid core Wasm (`expected f64, found i32`). Compiling validates the
/// module, so a bad adapter panics in codegen.
#[test]
fn lib_option_newtype_compiles_to_valid_wasm() {
    let source = r#"
pub type Meters = f64;
export fn id_opt(v: Option<Meters>) -> Option<Meters> {
    return v;
}
test "shape compiles" {}
"#;
    let wasm = compile_lib_source(source);
    assert!(
        wit_component::decode(&wasm).is_ok(),
        "compiled Option<newtype> component does not decode"
    );
}

/// The flat-ABI newtype fix must cover every container the export adapter
/// flattens through, not just `Option` — `Result`, tuples, and nested
/// compositions all read the newtype's base flat slots.
#[test]
fn lib_newtype_containers_compile_to_valid_wasm() {
    let source = r#"
pub type Meters = f64;
export fn id_result(v: Result<Meters, u32>) -> Result<Meters, u32> {
    return v;
}
export fn id_tuple(v: [Meters, u32]) -> [Meters, u32] {
    return v;
}
export fn id_opt_list(v: Option<List<Meters>>) -> Option<List<Meters>> {
    return v;
}
test "shape compiles" {}
"#;
    let wasm = compile_lib_source(source);
    assert!(
        wit_component::decode(&wasm).is_ok(),
        "compiled newtype-container component does not decode"
    );
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

/// A stdlib newtype the library's types name is published beside them, yet it
/// is no API of the library's own: one whose only type is private still exports
/// nothing.
#[test]
fn lib_naming_only_a_stdlib_newtype_exports_nothing() {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    let Err(err) = compile_source_with_compiler_options(
        Path::new("lib.wado"),
        "struct Blob {\n    bytes: ByteList,\n}\n",
        options,
    ) else {
        panic!("a library with no public API compiled");
    };
    assert!(
        format!("{err:?}").contains("exports nothing"),
        "rejected for another reason: {err:?}"
    );
}

/// A library signature naming a stdlib newtype carries it as an alias, as it
/// does a local one: `ByteList` is `byte-list`, in and out, and inside an
/// `option`.
#[test]
fn lib_stdlib_newtype_crosses_as_its_alias() {
    let source = r#"
export fn reversed(b: ByteList) -> ByteList {
    let mut out: ByteList = [];
    for let i of 0..<b.len() {
        out.push(b[b.len() - 1 - i]);
    }
    return out;
}
export fn head(b: Option<ByteList>) -> Option<u8> {
    let Some(bytes) = b else {
        return null;
    };
    return if bytes.is_empty() { null } else { Option::Some(bytes[0]) };
}
"#;
    let wasm = compile_lib_world(source, LIB_WORLD_FQ, OptLevel::O2, None);
    let decoded = wit_component::decode(&wasm).expect("decode structural type");
    let resolve = decoded.resolve();
    let reversed = resolve
        .interfaces
        .iter()
        .find_map(|(_, i)| i.functions.get("reversed"))
        .expect("reversed export present");
    assert_eq!(
        type_name(resolve, &reversed.params[0].ty).as_deref(),
        Some("byte-list"),
        "reversed param erased the stdlib newtype to its base"
    );

    let component = Component::new(engine(), &wasm).expect("component failed to load");
    let bytes = |values: &[u8]| Val::List(values.iter().copied().map(Val::U8).collect());
    runtime().block_on(async {
        let linker = linker(engine()).expect("build linker");
        let mut store = Store::new(engine(), WasiState::new());
        limit_store(&mut store, DEFAULT_TIMEOUT_MS);
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");

        let reversed = lib_func(&mut store, &instance, LIB_WORLD_FQ, "reversed");
        let mut results = vec![Val::Bool(false)];
        reversed
            .call_async(&mut store, &[bytes(&[1, 2, 3])], &mut results)
            .await
            .expect("call `reversed`");
        assert_eq!(results[0], bytes(&[3, 2, 1]));

        let head = lib_func(&mut store, &instance, LIB_WORLD_FQ, "head");
        let mut results = vec![Val::Bool(false)];
        head.call_async(
            &mut store,
            &[Val::Option(Some(Box::new(bytes(&[7, 8]))))],
            &mut results,
        )
        .await
        .expect("call `head`");
        assert_eq!(results[0], Val::Option(Some(Box::new(Val::U8(7)))));
    });
}
