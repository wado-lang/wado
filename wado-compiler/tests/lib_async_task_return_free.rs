//! Buffer reclamation at the async-lifted `--lib` boundary.
//!
//! An `async` export returns through `task.return`, which lifts eagerly: the
//! Canonical ABI has read the whole value by the time the builtin returns, so
//! the guest owns the buffers it lowered into and must give them back on the
//! next instruction. `post-return` is not available here — it is illegal
//! alongside `async` on `canon lift` — so freeing after `task.return` is the
//! only mechanism there is.
//!
//! The mirror of `lib_sync_lift_post_return.rs`, and capped the same way: guest
//! memory is held far below the total payload moved, so a per-call leak
//! exhausts the cap while correct reclamation stays flat. `freelist` — the
//! library world's own default — traps on double-free, so an over-eager free
//! fails here too.
//!
//! Covered: a bare `string` result, a `result<string, string>` (whose payload
//! shares joined flat slots with the other case), and a `list<string>` (whose
//! element buffers hang off the element array, out of the outer pointer's
//! reach).

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
const DOUBLINGS: u32 = 16;
const CALLS: usize = 48;
/// Enough for a few live payloads, far below the 48 MiB the string cases move.
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

/// Call `export` `CALLS` times under the memory cap, checking each result with
/// `check`. The panic message names the leak, since exhausting the cap is how
/// every unreclaimed buffer shows up here.
fn run_calls(
    source: &str,
    export: &str,
    doublings: u32,
    opt_level: OptLevel,
    check: impl Fn(&Val, usize),
) {
    let payload = 16usize << doublings;
    let engine = capped_engine();
    let wasm = compile_lib(source, opt_level);
    let component = Component::new(&engine, &wasm).expect("component failed to load");

    common::runtime().block_on(async {
        let linker = common::linker(&engine).expect("build linker");
        let mut store = Store::new(&engine, common::WasiState::new());
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");
        let func = lib_func(&mut store, &instance, export);

        for call in 0..CALLS {
            let mut results = vec![Val::Bool(false)];
            func.call_async(&mut store, &[Val::U32(doublings)], &mut results)
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "[{opt_level:?}] `{export}` call {call} of {CALLS} failed \
                         after at least {leaked} MiB of returned payload, with \
                         guest memory capped at {cap} MiB: {e:#}\n\
                         The `task.return` free walk is not reclaiming the \
                         lowered payload buffer (out-of-bounds), or is \
                         reclaiming it twice (freelist double-free trap).",
                        leaked = (call * payload) >> 20,
                        cap = MEMORY_CAP >> 20,
                    )
                });
            check(&results[0], payload);
        }
    });
}

/// The bare case:one `(ptr, len)` pair straight into `task.return`.
const STRING_SOURCE: &str = r#"
export async fn chunk(n: u32) -> String {
    let mut s = "0123456789abcdef";
    for let mut i = 0; i < n as i32; i += 1 {
        s = s + s;
    }
    task return s;
}
"#;

fn expect_string(val: &Val, payload: usize) {
    match val {
        Val::String(s) => assert_eq!(s.len(), payload, "payload size"),
        other => panic!("expected a string result, got {other:?}"),
    }
}

#[test]
fn async_task_return_string_buffer_is_reclaimed_o0() {
    run_calls(STRING_SOURCE, "chunk", DOUBLINGS, OptLevel::O0, expect_string);
}

#[test]
fn async_task_return_string_buffer_is_reclaimed_o2() {
    run_calls(STRING_SOURCE, "chunk", DOUBLINGS, OptLevel::O2, expect_string);
}

/// The payload shares its flat slots with the other case, so freeing has to
/// read the discriminant before touching them — an unconditional free would
/// treat an `Err` message's slots as the `Ok` buffer.
const RESULT_SOURCE: &str = r#"
export async fn chunk_result(n: u32) -> Result<String, String> {
    let mut s = "0123456789abcdef";
    for let mut i = 0; i < n as i32; i += 1 {
        s = s + s;
    }
    task return Result::<String, String>::Ok(s);
}
"#;

fn expect_ok_string(val: &Val, payload: usize) {
    match val {
        Val::Result(Ok(Some(inner))) => expect_string(inner, payload),
        other => panic!("expected `Ok(string)`, got {other:?}"),
    }
}

#[test]
fn async_task_return_result_payload_is_reclaimed_o0() {
    run_calls(
        RESULT_SOURCE,
        "chunk-result",
        DOUBLINGS,
        OptLevel::O0,
        expect_ok_string,
    );
}

#[test]
fn async_task_return_result_payload_is_reclaimed_o2() {
    run_calls(
        RESULT_SOURCE,
        "chunk-result",
        DOUBLINGS,
        OptLevel::O2,
        expect_ok_string,
    );
}

/// `ELEMENTS` payloads per call, each behind an element of the list's own
/// buffer: freeing the outer `(ptr, len)` alone still leaks every element.
///
/// Value semantics copy each element on `push`, so this case trades payload
/// size for element count — the GC heap, not the capped linear memory, is what
/// a 1 MiB element would exhaust first.
const LIST_SOURCE: &str = r#"
export async fn chunk_list(n: u32) -> List<String> {
    let mut s = "0123456789abcdef";
    for let mut i = 0; i < n as i32; i += 1 {
        s = s + s;
    }
    let mut out: List<String> = [];
    for let mut i = 0; i < 4; i += 1 {
        out.push(s);
    }
    task return out;
}
"#;

const LIST_DOUBLINGS: u32 = 14;
const ELEMENTS: usize = 4;

fn expect_string_list(val: &Val, payload: usize) {
    match val {
        Val::List(items) => {
            assert_eq!(items.len(), ELEMENTS, "element count");
            for item in items {
                expect_string(item, payload);
            }
        }
        other => panic!("expected a list result, got {other:?}"),
    }
}

#[test]
fn async_task_return_list_element_buffers_are_reclaimed_o0() {
    run_calls(
        LIST_SOURCE,
        "chunk-list",
        LIST_DOUBLINGS,
        OptLevel::O0,
        expect_string_list,
    );
}

#[test]
fn async_task_return_list_element_buffers_are_reclaimed_o2() {
    run_calls(
        LIST_SOURCE,
        "chunk-list",
        LIST_DOUBLINGS,
        OptLevel::O2,
        expect_string_list,
    );
}
