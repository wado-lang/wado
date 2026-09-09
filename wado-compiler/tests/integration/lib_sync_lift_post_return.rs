//! Buffer reclamation at the synchronously-lifted `--lib` boundary.
//!
//! A non-`async` `--lib` export returning a value wider than one core value
//! hands the host a pointer into guest memory. The Canonical ABI's only channel
//! for telling the guest the host has finished reading is the `post-return`
//! option of `canon lift` (`CanonicalABI.md`).
//!
//! The runtime tests cap guest memory well below the total payload they move, so
//! a per-call leak exhausts the cap while correct reclamation stays flat. They
//! run under `freelist` — the library world's own default — which also traps on
//! double-free, so an over-eager free fails here too.
//!
//! Covered: the returned payload, an incoming `string` parameter, and the
//! canonical option itself, which tracks the indirect return.

use wado_compiler::OptLevel;
use wasmtime::Store;
use wasmtime::component::{Component, Val};

use crate::common::{WasiState, capped_engine, compile_lib_world, lib_func, linker, runtime};

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

fn run(opt_level: OptLevel) {
    let engine = capped_engine(MEMORY_CAP);
    let wasm = compile_lib_world(SOURCE, LIB_WORLD_FQ, opt_level, Some("freelist"));
    let component = Component::new(&engine, &wasm).expect("component failed to load");

    runtime().block_on(async {
        let linker = linker(&engine).expect("build linker");
        let mut store = Store::new(&engine, WasiState::new());
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");
        let func = lib_func(&mut store, &instance, LIB_WORLD_FQ, "chunk");

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
    let engine = capped_engine(MEMORY_CAP);
    let wasm = compile_lib_world(PARAM_SOURCE, LIB_WORLD_FQ, opt_level, Some("freelist"));
    let component = Component::new(&engine, &wasm).expect("component failed to load");
    let arg = "x".repeat(PAYLOAD);

    runtime().block_on(async {
        let linker = linker(&engine).expect("build linker");
        let mut store = Store::new(&engine, WasiState::new());
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("instantiate library component");
        let func = lib_func(&mut store, &instance, LIB_WORLD_FQ, "measure");

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

/// Every direction of the canonical option in one component: a result that owns
/// a buffer, one that owns nothing but is still returned indirectly, and one
/// returned in a core result with no allocation at all.
const OPTION_SOURCE: &str = r#"
struct Pair {
    a: u32,
    b: u32,
}

export fn owns_memory(n: u32) -> List<u32> {
    return [n];
}

export fn indirect_scalars(n: u32) -> Pair {
    return Pair { a: n, b: n };
}

export fn direct(n: u32) -> u32 {
    return n;
}
"#;

/// `post-return` tracks the indirect return, not memory ownership.
///
/// Anything wider than one core value comes back through a guest-allocated area,
/// and that area leaks without the option even when nothing hangs off it. A
/// result that fits in a core value allocates nothing, so its lift keeps the
/// option off and its component stays as it was before `post-return` existed.
#[test]
fn post_return_tracks_the_indirect_return() {
    let wasm = compile_lib_world(OPTION_SOURCE, LIB_WORLD_FQ, OptLevel::O0, Some("freelist"));
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

    let indirect = lift_of("indirect-scalars");
    assert!(
        indirect.contains("post-return"),
        "a record of two `u32` owns no memory, but still comes back through a \
         guest-allocated area that leaks without `post-return`:\n{indirect}"
    );

    let direct = lift_of("direct");
    assert!(
        !direct.contains("post-return"),
        "a `u32` result is returned in a core result and allocates nothing, so \
         its lift must carry no `post-return`:\n{direct}"
    );
}
