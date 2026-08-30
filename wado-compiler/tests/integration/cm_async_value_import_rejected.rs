//! Importing a component whose exported signature carries an async value type
//! (`stream<T>` / `future<T>`) is not supported yet — see the "Not yet
//! supported" list in `docs/wep-2026-06-26-wasm-cm-component-import.md`. The
//! decoder leaves that one export out and the `use` clause naming it says why,
//! rather than building a `Stream<T>` binding that codegen cannot encode.

use std::path::Path;
use wado_compiler::{CompilerOptions, OptLevel};

/// Compile `source` as a library world and return the component bytes.
fn compile_lib(source: &str) -> Vec<u8> {
    let options = CompilerOptions {
        opt_level: OptLevel::O0,
        lib_world: Some("test:dep/dep@0.1.0".to_string()),
        ..Default::default()
    };
    crate::common::compile_source_with_compiler_options(Path::new("dep.wado"), source, options)
        .expect("dependency compiles as a library world")
        .wasm
}

/// Compile `consumer_source` against `dep_source` built as a component next to
/// it, and return the compile error, if any.
fn compile_against_dep(dep_source: &str, consumer_source: &str) -> Result<(), String> {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("dep.wasm"), compile_lib(dep_source)).expect("write dep.wasm");

    let options = CompilerOptions {
        opt_level: OptLevel::O0,
        lib_world: Some("test:consumer/consumer@0.1.0".to_string()),
        ..Default::default()
    };
    crate::common::compile_source_with_compiler_options(
        &dir.path().join("main.wado"),
        consumer_source,
        options,
    )
    .map(|_| ())
    .map_err(|err| format!("{err:?}"))
}

fn import_error(dep_source: &str, consumer_source: &str) -> String {
    let Err(err) = compile_against_dep(dep_source, consumer_source) else {
        panic!("importing an async value type must be rejected");
    };
    err
}

const STREAM_DEP: &str = r#"
export fn add(a: u32, b: u32) -> u32 {
    return a + b;
}
export async fn double_stream(v: Stream<u32>) -> Stream<u32> {
    let [rx, tx] = Stream::<u32>::new();
    task return rx;
    loop {
        let chunk = v.read(16);
        if chunk.len() == 0 {
            break;
        }
        let mut out: List<u32> = [];
        for let x of chunk {
            out.push(x * 2);
        }
        tx.write(out);
    }
    v.drop();
    tx.drop();
}
"#;

#[test]
fn stream_in_an_imported_signature_is_rejected() {
    let err = import_error(
        STREAM_DEP,
        r#"
use { double_stream } from "./dep.wasm" with { type: "wasm" };
export fn go() -> u32 {
    return 0;
}
"#,
    );
    assert!(
        err.contains("double_stream") && err.contains("`stream`"),
        "the error must name the export and the unsupported shape: {err}"
    );
}

/// One unsupported export must not cost the rest of the component: the
/// decoder skips that function alone, and `cm-catalog` — which mixes streams
/// with the whole value-type surface — stays importable.
#[test]
fn a_value_export_still_imports_from_a_component_that_also_streams() {
    compile_against_dep(
        STREAM_DEP,
        r#"
use { add } from "./dep.wasm" with { type: "wasm" };
export fn go() -> u32 {
    return add(1, 2);
}
"#,
    )
    .expect("the stream export does not block the value-type export");
}

#[test]
fn future_in_an_imported_signature_is_rejected() {
    let err = import_error(
        r#"
export fn id_future_u32(v: Future<u32>) -> Future<u32> {
    let value = v.read();
    v.drop();
    let [rx, tx] = Future::<u32>::new();
    if let Some(x) = value {
        tx.write(x);
    }
    return rx;
}
"#,
        r#"
use { id_future_u32 } from "./dep.wasm" with { type: "wasm" };
export fn go() -> u32 {
    return 0;
}
"#,
    );
    assert!(
        err.contains("id_future_u32") && err.contains("`future`"),
        "the error must name the export and the unsupported shape: {err}"
    );
}
