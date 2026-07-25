//! Return-buffer reclamation for synchronously-lifted `--lib` exports.
//!
//! A non-`async` `--lib` export is lifted with `sync_lift` (see
//! `WorldExportPlan::sync_lift`). When such an export returns a memory-backed
//! value — `string`, `list<T>`, a composite via outptr — the guest allocates the
//! payload with `realloc` and hands the host a pointer. The Canonical ABI's only
//! mechanism for telling the guest that the host has finished reading, so the
//! payload can be freed, is the `post-return` canonical option:
//!
//! > The `(post-return ...)` option may only be present in `canon lift` when
//! > `async` is not present and specifies a core function to be called with the
//! > original return values after they have finished being read, allowing memory
//! > to be deallocated and destructors called.
//!
//! Codegen never emits it, so nothing frees the payload. The library world
//! defaults to the `freelist` allocator precisely because "a library is consumed
//! by a long-running host; reclaim memory" (`select_allocator`), and this is the
//! one buffer it cannot reclaim.
//!
//! The test caps guest memory well below the total payload it returns, so a
//! per-call leak exhausts the cap while correct reclamation stays flat.

#![allow(unused_crate_dependencies)]

mod common;

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};
use wasmtime::component::{Component, Val};
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, Store};

/// FQ of the synthesized library world. Mirrors `cm_catalog.rs`.
const LIB_WORLD_FQ: &str = "wado-lang:cm-catalog/cm-catalog@0.0.23";

/// Doubling `n` times from a 16-byte seed: `chunk(16)` returns 1 MiB.
const SOURCE: &str = r#"
export fn chunk(n: u32) -> String {
    let mut s = "0123456789abcdef";
    for let mut i = 0; i < n as i32; i += 1 {
        s = s + s;
    }
    return s;
}
"#;

const DOUBLINGS: u32 = 16;
const PAYLOAD: usize = 16 << DOUBLINGS;
const CALLS: usize = 48;
/// Enough for a few live payloads, far below `CALLS * PAYLOAD` (48 MiB).
const MEMORY_CAP: usize = 12 << 20;

/// An engine matching `common::engine`'s feature set, with a hard cap on guest
/// linear memory so an unreclaimed payload cannot hide in address space.
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
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(pooling));
    Engine::new(&config).expect("build capped engine")
}

fn compile_lib(opt_level: OptLevel) -> Vec<u8> {
    let options = CompilerOptions {
        opt_level,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        // The library world's own default. `bump` never frees anything, so it
        // cannot distinguish a leak from correct behavior.
        allocator: Some("freelist".to_string()),
        ..Default::default()
    };
    common::compile_source_with_compiler_options(Path::new("lib.wado"), SOURCE, options)
        .expect("library failed to compile")
        .wasm
}

fn run(opt_level: OptLevel) {
    let engine = capped_engine();
    let wasm = compile_lib(opt_level);
    let component = Component::new(&engine, &wasm).expect("component failed to load");

    common::runtime().block_on(async {
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
            .and_then(|i| instance.get_export(&mut store, Some(&i), "chunk"))
            .or_else(|| instance.get_export(&mut store, None, "chunk"))
            .expect("`chunk` export not found");
        let func = instance
            .get_func(&mut store, func_idx)
            .expect("`chunk` is not a func");

        for call in 0..CALLS {
            let mut results = vec![Val::Bool(false)];
            func.call_async(&mut store, &[Val::U32(DOUBLINGS)], &mut results)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "[{opt_level:?}] call {call} of {CALLS} failed after \
                         {leaked} MiB of returned payload, with guest memory \
                         capped at {cap} MiB: {e:#}\n\
                         Each synchronously-lifted return leaks its payload \
                         buffer because `canon lift` carries no `post-return`.",
                        leaked = (call * PAYLOAD) >> 20,
                        cap = MEMORY_CAP >> 20,
                    )
                });
            match results.into_iter().next() {
                Some(Val::String(s)) => assert_eq!(s.len(), PAYLOAD, "payload size"),
                other => panic!("[{opt_level:?}] expected a string result, got {other:?}"),
            }
        }
    });
}

// Both tests are ignored because codegen emits no `post-return`: they trap on
// call 8 of 48, the payload having grown 1 MiB per call — one unreclaimed buffer
// each. Remove the attributes with the fix.

#[test]
#[ignore = "known leak: sync-lifted `--lib` exports never free their return buffer"]
fn lib_sync_lift_return_buffer_is_reclaimed_o0() {
    run(OptLevel::O0);
}

#[test]
#[ignore = "known leak: sync-lifted `--lib` exports never free their return buffer"]
fn lib_sync_lift_return_buffer_is_reclaimed_o2() {
    run(OptLevel::O2);
}
