//! A library export taking or returning an unrestricted handle. The handle is
//! an `f64` on both sides of the boundary, bare and inside an `option`.

use std::path::Path;

use wado_compiler::ast::HandleClasses;
use wado_compiler::{CompilerOptions, OptLevel};
use wasmtime::Store;
use wasmtime::component::{Component, Val};

use crate::common::{
    WasiState, compile_lib_world, compile_source_with_compiler_options, engine, lib_func,
    limit_store, linker, runtime,
};

const LIB_WORLD_FQ: &str = "test:handles/handles@0.1.0";

const TIMEOUT_MS: u64 = 5_000;

const SOURCE: &str = r#"
#[cm("wasi:demo/events@0.1.0#node", linearity = "unrestricted", classes = "0..=1")]
pub resource Node {}

export fn make(x: f64) -> Node {
    return x as Node;
}

export fn take(n: Node) -> f64 {
    return n as f64;
}

export fn echo(n: Option<Node>) -> Option<Node> {
    return n;
}
"#;

/// The same exports over a handle another module declares, imported under an
/// alias.
const IMPORTED_SOURCE: &str = r#"
use { Node as Handle } from "./sub/lib_unrestricted_handle_node.wado";

export fn make(x: f64) -> Handle {
    return x as Handle;
}

export fn take(n: Handle) -> f64 {
    return n as f64;
}

export fn echo(n: Option<Handle>) -> Option<Handle> {
    return n;
}
"#;

/// Class 1, index 7.
const HANDLE: f64 = HandleClasses::STRIDE + 7.0;

fn compile_imported(opt_level: OptLevel) -> Vec<u8> {
    let options = CompilerOptions {
        opt_level,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lib.wado");
    compile_source_with_compiler_options(&path, IMPORTED_SOURCE, options)
        .expect("library failed to compile")
        .wasm
}

fn handles_cross_as_f64(wasm: &[u8]) {
    let engine = engine();
    let component = Component::new(engine, wasm).expect("component failed to load");

    runtime().block_on(async {
        let linker = linker(engine).expect("build linker");
        let mut store = Store::new(engine, WasiState::new());
        limit_store(&mut store, TIMEOUT_MS);
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");

        for (name, arg) in [
            ("make", Val::Float64(HANDLE)),
            ("take", Val::Float64(HANDLE)),
            ("echo", Val::Option(Some(Box::new(Val::Float64(HANDLE))))),
            ("echo", Val::Option(None)),
        ] {
            let func = lib_func(&mut store, &instance, LIB_WORLD_FQ, name);
            let mut results = vec![Val::Bool(false)];
            func.call_async(&mut store, std::slice::from_ref(&arg), &mut results)
                .await
                .unwrap_or_else(|e| panic!("call {name}: {e}"));
            assert_eq!(results[0], arg, "{name} hands the handle back unchanged");
        }
    });
}

#[test]
fn handles_cross_as_f64_o0() {
    handles_cross_as_f64(&compile_lib_world(SOURCE, LIB_WORLD_FQ, OptLevel::O0, None));
}

#[test]
fn handles_cross_as_f64_o2() {
    handles_cross_as_f64(&compile_lib_world(SOURCE, LIB_WORLD_FQ, OptLevel::O2, None));
}

#[test]
fn imported_handles_cross_as_f64_o0() {
    handles_cross_as_f64(&compile_imported(OptLevel::O0));
}

#[test]
fn imported_handles_cross_as_f64_o2() {
    handles_cross_as_f64(&compile_imported(OptLevel::O2));
}
