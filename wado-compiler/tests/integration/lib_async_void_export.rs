//! A void `export async fn` delivers task completion. An export declaring no
//! result has nothing to hand back, but the host task still has to be told it
//! is done, and without that delivery the call never completes.

use wado_compiler::OptLevel;
use wasmtime::Store;
use wasmtime::component::{Component, Val};

use crate::common::{WasiState, compile_lib_world, engine, lib_func, limit_store, linker, runtime};

/// FQ of the synthesized library world; any stable name works.
const LIB_WORLD_FQ: &str = "wado-lang:cm-catalog/cm-catalog@0.0.23";

/// Bounds runaway guest work, as every metered test does. An undelivered task
/// does not reach it: the async lift traps the call.
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

fn void_async_export_completes(opt_level: OptLevel) {
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

        let ping = lib_func(&mut store, &instance, LIB_WORLD_FQ, "ping");
        ping.call_async(&mut store, &[], &mut [])
            .await
            .expect("the void async export never delivered task completion");

        let hit_count = lib_func(&mut store, &instance, LIB_WORLD_FQ, "hit-count");
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
