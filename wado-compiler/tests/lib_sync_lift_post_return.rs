//! Return-buffer reclamation for synchronously-lifted `--lib` exports.
//!
//! A non-`async` `--lib` export is lifted with `sync_lift`. When it returns a
//! memory-backed value — `string`, `list<T>`, a composite via outptr — the guest
//! allocates the payload with `realloc` and hands the host a pointer. The
//! Canonical ABI's only mechanism for telling the guest that the host has
//! finished reading is the `post-return` option of `canon lift`
//! (`CanonicalABI.md`).
//!
//! The tests cap guest memory well below the total payload they move, so a
//! per-call leak exhausts the cap while correct reclamation stays flat. They run
//! under `freelist` — the library world's own default — which also traps on
//! double-free, so an over-eager free walk fails here too.
//!
//! Covered:
//!
//! - the returned payload, reclaimed through `post-return`;
//! - an incoming `string` parameter, whose buffer the caller allocated with the
//!   guest's `realloc` and which the export binding releases once it has copied
//!   it onto the GC heap;
//! - the canonical option itself: present when the result owns memory, absent
//!   when it does not.

#![allow(unused_crate_dependencies)]

mod common;

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};
use wasmtime::component::{Component, Val};
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, Store};

/// FQ of the synthesized library world; any stable name works, the compiler
/// only uses it to key the world it builds for `--lib`.
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

fn compile_lib(source: &str, opt_level: OptLevel) -> Vec<u8> {
    let options = CompilerOptions {
        opt_level,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        // The library world's own default. `bump` never frees anything, so it
        // cannot distinguish a leak from correct behavior, and `freelist` traps
        // on double-free, so it also catches an over-eager free.
        allocator: Some("freelist".to_string()),
        ..Default::default()
    };
    common::compile_source_with_compiler_options(Path::new("lib.wado"), source, options)
        .expect("library failed to compile")
        .wasm
}

/// Resolve `name` in the library world's instance export, falling back to a
/// bare top-level export.
fn lib_func(
    store: &mut Store<common::WasiState>,
    instance: &wasmtime::component::Instance,
    name: &str,
) -> wasmtime::component::Func {
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

fn run(opt_level: OptLevel) {
    let engine = capped_engine();
    let wasm = compile_lib(SOURCE, opt_level);
    let component = Component::new(&engine, &wasm).expect("component failed to load");

    common::runtime().block_on(async {
        let linker = common::linker(&engine).expect("build linker");
        let mut store = Store::new(&engine, common::WasiState::new());
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");
        let func = lib_func(&mut store, &instance, "chunk");

        for call in 0..CALLS {
            let mut results = vec![Val::Bool(false)];
            func.call_async(&mut store, &[Val::U32(DOUBLINGS)], &mut results)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "[{opt_level:?}] call {call} of {CALLS} failed after \
                         {leaked} MiB of returned payload, with guest memory \
                         capped at {cap} MiB: {e:#}\n\
                         The `post-return` free walk is not reclaiming the \
                         returned payload buffer (out-of-bounds), or is \
                         reclaiming it twice (freelist double-free trap).",
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

#[test]
fn lib_sync_lift_return_buffer_is_reclaimed_o0() {
    run(OptLevel::O0);
}

#[test]
fn lib_sync_lift_return_buffer_is_reclaimed_o2() {
    run(OptLevel::O2);
}

/// Takes a `string` and returns a scalar, so the only guest allocation in play
/// is the parameter buffer the caller lowered into guest memory.
const PARAM_SOURCE: &str = r#"
export fn measure(s: String) -> u32 {
    return s.len() as u32;
}
"#;

fn run_param(opt_level: OptLevel) {
    let engine = capped_engine();
    let wasm = compile_lib(PARAM_SOURCE, opt_level);
    let component = Component::new(&engine, &wasm).expect("component failed to load");
    let arg = "x".repeat(PAYLOAD);

    common::runtime().block_on(async {
        let linker = common::linker(&engine).expect("build linker");
        let mut store = Store::new(&engine, common::WasiState::new());
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");
        let func = lib_func(&mut store, &instance, "measure");

        for call in 0..CALLS {
            let mut results = vec![Val::Bool(false)];
            func.call_async(&mut store, &[Val::String(arg.clone())], &mut results)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "[{opt_level:?}] call {call} of {CALLS} failed after \
                         {leaked} MiB of passed-in payload, with guest memory \
                         capped at {cap} MiB: {e:#}\n\
                         The export binding is not releasing the caller-lowered \
                         `string` parameter buffer, or is releasing it twice.",
                        leaked = (call * PAYLOAD) >> 20,
                        cap = MEMORY_CAP >> 20,
                    )
                });
            match results.into_iter().next() {
                Some(Val::U32(n)) => assert_eq!(n as usize, PAYLOAD, "parameter length"),
                other => panic!("[{opt_level:?}] expected a u32 result, got {other:?}"),
            }
        }
    });
}

#[test]
fn lib_sync_lift_param_buffer_is_reclaimed_o0() {
    run_param(OptLevel::O0);
}

#[test]
fn lib_sync_lift_param_buffer_is_reclaimed_o2() {
    run_param(OptLevel::O2);
}

/// One memory-owning result and one that owns nothing, so the assertion below
/// pins both directions of the canonical option in a single component.
const OPTION_SOURCE: &str = r#"
export fn owns_memory(n: u32) -> List<u32> {
    return [n];
}

export fn owns_nothing(n: u32) -> u32 {
    return n;
}
"#;

/// `post-return` is emitted only where there is something to reclaim: a lift
/// whose result owns no linear memory keeps the option off, so components that
/// cannot leak stay byte-identical to what they were before it existed.
#[test]
fn post_return_is_emitted_only_for_memory_owning_results() {
    let wasm = compile_lib(OPTION_SOURCE, OptLevel::O0);
    let wat = wasmprinter::print_bytes(&wasm).expect("print component");

    let lift_of = |name: &str| -> String {
        wat.lines()
            .find(|line| line.contains("canon lift") && line.contains(&format!("${name} ")))
            .unwrap_or_else(|| panic!("no `canon lift` line for `{name}` in:\n{wat}"))
            .to_string()
    };

    let owning = lift_of("owns-memory");
    assert!(
        owning.contains("post-return"),
        "a `list<u32>` result owns its element buffer and has nothing else to \
         free it, so its lift needs `post-return`:\n{owning}"
    );

    let scalar = lift_of("owns-nothing");
    assert!(
        !scalar.contains("post-return"),
        "a `u32` result owns no memory, so its lift must carry no \
         `post-return`:\n{scalar}"
    );
}
