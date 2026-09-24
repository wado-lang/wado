//! A library export taking or returning an unrestricted handle. The handle is
//! an `f64` on both sides of the boundary, bare and inside an `option`.

use wado_compiler::OptLevel;
use wasmtime::Store;
use wasmtime::component::{Component, Val};

use crate::common::{WasiState, compile_lib_world, engine, lib_func, limit_store, linker, runtime};

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

/// Class 1, index 7.
const HANDLE: f64 = 137_438_953_472.0 + 7.0;

fn handles_cross_as_f64(opt_level: OptLevel) {
    let engine = engine();
    let wasm = compile_lib_world(SOURCE, LIB_WORLD_FQ, opt_level, None);
    let component = Component::new(engine, &wasm).expect("component failed to load");

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
    handles_cross_as_f64(OptLevel::O0);
}

#[test]
fn handles_cross_as_f64_o2() {
    handles_cross_as_f64(OptLevel::O2);
}
