//! A library export's parameter crosses the boundary under its kebab-case CM
//! name, as the export itself does. A Wado parameter is `snake_case`, which the
//! Component Model rejects as a name.

use wado_compiler::OptLevel;
use wasmtime::Store;
use wasmtime::component::{Component, Val};

use crate::common::{WasiState, compile_lib_world, engine, lib_func, limit_store, linker, runtime};

/// FQ of the synthesized library world, a name of this test's own.
const LIB_WORLD_FQ: &str = "test:snake-param/snake-param@0.1.0";

/// Bounds runaway guest work, as every metered test does.
const TIMEOUT_MS: u64 = 5_000;

const SOURCE: &str = r#"
export fn add_one(the_value: i32) -> i32 {
    return the_value + 1;
}
"#;

#[test]
fn a_snake_case_parameter_crosses_under_its_kebab_name() {
    let engine = engine();
    let wasm = compile_lib_world(SOURCE, LIB_WORLD_FQ, OptLevel::O0, None);
    let component = Component::new(engine, &wasm).expect("component failed to load");

    runtime().block_on(async {
        let linker = linker(engine).expect("build linker");
        let mut store = Store::new(engine, WasiState::new());
        limit_store(&mut store, TIMEOUT_MS);
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");
        let add_one = lib_func(&mut store, &instance, LIB_WORLD_FQ, "add-one");
        let mut results = vec![Val::S32(0)];
        add_one
            .call_async(&mut store, &[Val::S32(41)], &mut results)
            .await
            .expect("call add-one");
        assert_eq!(results[0], Val::S32(42));
    });
}
