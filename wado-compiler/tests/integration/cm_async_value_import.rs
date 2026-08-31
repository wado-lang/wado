//! A `stream<T>` / `future<T>` across an import: the consumer creates the pair,
//! hands the readable end over, and drains what comes back — a channel between
//! two components. See `docs/wep-2026-06-26-wasm-cm-component-import.md`.

use std::path::Path;
use wado_compiler::{CompilerOptions, OptLevel};

/// The dependency's `async func(stream<u32>) -> stream<u32>`. It `task return`s
/// before copying, so the caller's writes rendezvous instead of deadlocking.
const DOUBLE_STREAM: &str = r#"
export async fn double_stream(v: Stream<u32>) -> Stream<u32> {
    let [rx, tx] = Stream::<u32>::new();
    task return rx;
    let mut out: List<u32> = [];
    for let x of v.read_to_end() {
        out.push(x * 2);
    }
    tx.write_all(out);
    v.drop();
    tx.drop();
}
"#;

/// Compile `source` as a library world and return the component bytes.
fn compile_dep(source: &str) -> Vec<u8> {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some("test:dep/dep@0.1.0".to_string()),
        ..Default::default()
    };
    crate::common::compile_source_with_compiler_options(Path::new("dep.wado"), source, options)
        .expect("dependency compiles as a library world")
        .wasm
}

/// Build `dep_source` as a component, compile `consumer_source` against it, run
/// the composed result, and return what it printed.
fn run_against_dep(dep_source: &str, consumer_source: &str) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("dep.wasm"), compile_dep(dep_source)).expect("write dep.wasm");

    let wasm = crate::common::compile_source_with_compiler_options(
        &dir.path().join("main.wado"),
        consumer_source,
        CompilerOptions {
            opt_level: OptLevel::O2,
            ..Default::default()
        },
    )
    .expect("consumer compiles against the imported component")
    .wasm;

    let result = crate::common::run_wasm(wasm).expect("composed component runs");
    assert!(!result.trapped, "composed component trapped: {result:?}");
    result.stdout
}

/// A dependency that also exports a type groups its exports into a WIT
/// interface, so the stream crosses the boundary as an interface method.
#[test]
fn stream_round_trips_through_an_imported_interface() {
    let stdout = run_against_dep(
        &format!(
            r#"
export struct Pair {{
    pub a: u32,
    pub b: u32,
}}

export fn add(p: Pair) -> u32 {{
    return p.a + p.b;
}}
{DOUBLE_STREAM}"#
        ),
        r#"
use { Dep } from "./dep.wasm" with { type: "wasm" };
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    let [rx, tx] = Stream::<u32>::new();
    let call = Dep::double_stream(rx);
    tx.write_all([1, 2, 3]);
    tx.drop();
    let out = call.wait();
    let got = out.read_to_end();
    out.drop();
    println(`${got:?}`);
}
"#,
    );
    assert_eq!(stdout, "[2, 4, 6]\n");
}

/// A dependency exporting only functions exports them at the world level, which
/// the consumer imports as free functions.
#[test]
fn stream_round_trips_through_an_imported_world_function() {
    let stdout = run_against_dep(
        DOUBLE_STREAM,
        r#"
use { double_stream } from "./dep.wasm" with { type: "wasm" };
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    let [rx, tx] = Stream::<u32>::new();
    let call = double_stream(rx);
    tx.write_all([5, 6]);
    tx.drop();
    let out = call.wait();
    let got = out.read_to_end();
    out.drop();
    println(`${got:?}`);
}
"#,
    );
    assert_eq!(stdout, "[10, 12]\n");
}

/// A `#[cm]` call inside a generic body binds per instance: the payload is a
/// type parameter until monomorphize mints `read_once<u32>`, so the helper it
/// calls is synthesized after that — see
/// `docs/wep-2026-08-30-stream-copy-result.md`.
#[test]
fn a_generic_body_binds_its_stream_read_per_instance() {
    let stdout = run_against_dep(
        DOUBLE_STREAM,
        r#"
use { double_stream } from "./dep.wasm" with { type: "wasm" };
use { println, Stdout } from "core:cli";

fn read_once<T>(s: &Stream<T>) -> StreamChunk<T> {
    return s.read(16);
}

export fn run() with Stdout {
    let [rx, tx] = Stream::<u32>::new();
    let call = double_stream(rx);
    tx.write_all([4, 5]);
    tx.drop();
    let out = call.wait();
    let got = read_once::<u32>(&out).items;
    out.drop();
    println(`${got:?}`);
}
"#,
    );
    assert_eq!(stdout, "[8, 10]\n");
}

/// The `future<T>` half of the same surface, through a sync export.
#[test]
fn future_round_trips_through_an_imported_world_function() {
    let stdout = run_against_dep(
        r#"
export fn triple_future(v: Future<u32>) -> Future<u32> {
    let value = v.read();
    v.drop();
    let [rx, tx] = Future::<u32>::new();
    if let Some(x) = value {
        tx.write(x * 3);
    }
    return rx;
}
"#,
        r#"
use { triple_future } from "./dep.wasm" with { type: "wasm" };
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    let [rx, tx] = Future::<u32>::new();
    tx.write(7);
    let out = triple_future(rx);
    let got = out.read();
    out.drop();
    println(`${got:?}`);
}
"#,
    );
    assert_eq!(stdout, "Option::Some(21)\n");
}
