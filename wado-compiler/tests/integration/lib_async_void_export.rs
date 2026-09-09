//! A void `export async fn` delivers task completion.
//!
//! An export declaring no result has nothing to hand back, but the host task
//! still has to be told it is done. The delivery is a `task.return` carrying no
//! value; without it the call never completes and the guest's effects are
//! unobservable.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};
use wasmtime::Store;
use wasmtime::component::{Component, Func, Instance, Val};

use crate::common::{
    WasiState, compile_source_with_compiler_options, engine, limit_store, linker, runtime,
};

/// FQ of the synthesized library world; any stable name works.
const LIB_WORLD_FQ: &str = "wado-lang:cm-catalog/cm-catalog@0.0.23";

/// Budget for the two calls; an undelivered task burns it rather than hanging.
const TIMEOUT_MS: u64 = 5_000;

/// A void async export beside a sync reader, so the effect the void export had
/// is observable from the host.
const SOURCE: &str = r#"
global mut hits: i32 = 0;

export async fn ping() {
    hits = hits + 1;
    task return ();
}

export fn hit_count() -> i32 {
    return hits;
}
"#;

fn compile_lib(opt_level: OptLevel) -> Vec<u8> {
    let options = CompilerOptions {
        opt_level,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    compile_source_with_compiler_options(Path::new("lib.wado"), SOURCE, options)
        .expect("library failed to compile")
        .wasm
}

fn lib_func(store: &mut Store<WasiState>, instance: &Instance, name: &str) -> Func {
    let iface = instance
        .get_export(&mut *store, None, LIB_WORLD_FQ)
        .map(|(_, idx)| idx);
    let (_, func_idx) = iface
        .and_then(|i| instance.get_export(&mut *store, Some(&i), name))
        .or_else(|| instance.get_export(&mut *store, None, name))
        .unwrap_or_else(|| panic!("`{name}` export not found"));
    instance
        .get_func(&mut *store, func_idx)
        .unwrap_or_else(|| panic!("`{name}` is not a func"))
}

fn void_async_export_completes(opt_level: OptLevel) {
    let engine = engine();
    let wasm = compile_lib(opt_level);
    let component = Component::new(engine, &wasm).expect("component failed to load");

    runtime().block_on(async {
        let linker = linker(engine).expect("build linker");
        let mut store = Store::new(engine, WasiState::new());
        limit_store(&mut store, TIMEOUT_MS);
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");

        let ping = lib_func(&mut store, &instance, "ping");
        ping.call_async(&mut store, &[], &mut [])
            .await
            .expect("the void async export never delivered task completion");

        let hit_count = lib_func(&mut store, &instance, "hit-count");
        let mut results = vec![Val::Bool(false)];
        hit_count
            .call_async(&mut store, &[], &mut results)
            .await
            .expect("read the hit count");
        assert_eq!(results[0], Val::S32(1), "the export's effect is observable");
    });
}

#[test]
fn void_async_export_completes_o0() {
    void_async_export_completes(OptLevel::O0);
}

#[test]
fn void_async_export_completes_o2() {
    void_async_export_completes(OptLevel::O2);
}
