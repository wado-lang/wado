//! Allocation past the linear-memory limit must trap at the allocator.
//!
//! `grow_memory` discarded `memory.grow`'s `-1` failure result and advanced
//! `heap_offset` regardless, so an allocation the host refused to back returned
//! a pointer past the end of linear memory. The guest then wandered on and
//! faulted somewhere unrelated — typically inside allocator metadata, several
//! frames away from the allocation that actually failed. All three allocators
//! share `grow_memory` and so shared the bug.
//!
//! The request is driven from the host: lowering a `string` argument into the
//! guest calls the guest's `realloc` for the whole payload, so a large argument
//! against a small memory cap exercises the failure path without needing any
//! GC-heap headroom (the pooling allocator forces the GC heap and linear memory
//! to share one reservation, so the guest cannot build such a value itself).

#![allow(unused_crate_dependencies)]

mod common;

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};
use wasmtime::component::{Component, Val};
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, Store};

/// FQ of the synthesized library world; any stable name works, the compiler
/// only uses it to key the world it builds for `--lib`.
const LIB_WORLD_FQ: &str = "wado-lang:cm-catalog/cm-catalog@0.0.23";

const SOURCE: &str = r#"
export fn id_string(v: String) -> String {
    return v;
}
"#;

const MEMORY_CAP: usize = 8 << 20;
const PAYLOAD: usize = 32 << 20;

fn capped_engine() -> Engine {
    let mut pooling = PoolingAllocationConfig::default();
    pooling.max_memory_size(MEMORY_CAP);
    pooling.total_memories(16);
    pooling.total_core_instances(64);
    pooling.total_component_instances(16);

    let mut config = Config::new();
    config.wasm_component_model_gc(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_error_context(true);
    config.wasm_wide_arithmetic(true);
    config.collector(wasmtime::Collector::Copying);
    config.cranelift_opt_level(wasmtime::OptLevel::None);
    config.wasm_backtrace_details(wasmtime::WasmBacktraceDetails::Enable);
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(pooling));
    Engine::new(&config).expect("build capped engine")
}

fn run(opt_level: OptLevel, allocator: &str) {
    let engine = capped_engine();
    let options = CompilerOptions {
        opt_level,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        allocator: Some(allocator.to_string()),
        ..Default::default()
    };
    let wasm = common::compile_source_with_compiler_options(Path::new("lib.wado"), SOURCE, options)
        .expect("library failed to compile")
        .wasm;
    let component = Component::new(&engine, &wasm).expect("component failed to load");

    let err = common::runtime().block_on(async {
        let linker = common::linker(&engine).expect("build linker");
        let mut store = Store::new(&engine, common::WasiState::new());
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");
        let iface = instance
            .get_export(&mut store, None, LIB_WORLD_FQ)
            .map(|(_, idx)| idx);
        let (_, func_idx) = iface
            .and_then(|i| instance.get_export(&mut store, Some(&i), "id-string"))
            .or_else(|| instance.get_export(&mut store, None, "id-string"))
            .expect("`id-string` export not found");
        let func = instance
            .get_func(&mut store, func_idx)
            .expect("`id-string` is not a func");

        let payload = "x".repeat(PAYLOAD);
        let mut results = vec![Val::Bool(false)];
        func.call_async(&mut store, &[Val::String(payload)], &mut results)
            .await
            .expect_err("a 32 MiB argument cannot fit an 8 MiB memory cap")
    });

    let msg = format!("{err:#}");
    assert!(
        !msg.contains("out of bounds memory access"),
        "[{opt_level:?}/{allocator}] exhausting linear memory faulted on a \
         pointer the allocator handed out past the end of memory, instead of \
         trapping at the failed allocation:\n{msg}"
    );
}

#[test]
fn allocation_past_the_memory_limit_traps_at_the_allocator_bump() {
    run(OptLevel::O0, "bump");
    run(OptLevel::O2, "bump");
}

#[test]
fn allocation_past_the_memory_limit_traps_at_the_allocator_freelist() {
    run(OptLevel::O0, "freelist");
    run(OptLevel::O2, "freelist");
}

#[test]
fn allocation_past_the_memory_limit_traps_at_the_allocator_debug() {
    run(OptLevel::O0, "debug");
    run(OptLevel::O2, "debug");
}
