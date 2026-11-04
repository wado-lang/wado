//! An `#[ambient]` effect is imported wherever a call reaches its body, and a
//! `--lib` build's synthesized world is no exception: a library whose only
//! stderr write is an assertion's message imports `wasi:cli/stderr`. Its
//! consumer forwards that import without holding `Stderr`, which only an
//! export declaring it demands.

use std::path::Path;

use wado_compiler::{CompilerOptions, OptLevel};
use wasmtime::component::Component;

use crate::common::{compile_source_with_compiler_options, engine, run_wasm};

const LIB_WORLD_FQ: &str = "test:ambient/ambient@0.1.0";

const ASSERTING_LIB: &str = r"
export fn positive(x: i32) -> i32 {
    assert x > 0;
    return x;
}
";

fn compile_lib(source: &str) -> Vec<u8> {
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        lib_world: Some(LIB_WORLD_FQ.to_string()),
        ..Default::default()
    };
    compile_source_with_compiler_options(Path::new("lib.wado"), source, options)
        .expect("the library compiles")
        .wasm
}

/// Compile `consumer` against `lib` compiled as `./lib.wasm`.
fn compile_consumer(lib: &str, consumer: &str) -> Result<Vec<u8>, String> {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("lib.wasm"), compile_lib(lib)).expect("write lib.wasm");
    compile_source_with_compiler_options(
        &dir.path().join("main.wado"),
        consumer,
        CompilerOptions {
            opt_level: OptLevel::O2,
            ..Default::default()
        },
    )
    .map(|output| output.wasm)
    .map_err(|err| format!("{err}"))
}

#[test]
fn a_library_keeps_its_assertion_message() {
    let engine = engine();
    let component =
        Component::new(engine, compile_lib(ASSERTING_LIB)).expect("the library is a component");
    let imports: Vec<String> = component
        .component_type()
        .imports(engine)
        .map(|(name, _)| name.to_string())
        .collect();
    assert!(
        imports.iter().any(|name| name == "wasi:cli/stderr@0.3.0"),
        "imports: {imports:?}"
    );
}

#[test]
fn its_consumer_holds_no_stderr_for_it() {
    let wasm = compile_consumer(
        ASSERTING_LIB,
        r#"
use { positive } from "./lib.wasm" with { type: "wasm" };
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println(`${positive(7)}`);
}
"#,
    )
    .expect("the consumer compiles without `Stderr`");
    let result = run_wasm(wasm).expect("the composed component runs");
    assert!(!result.trapped, "trapped: {result:?}");
    assert_eq!(result.stdout, "7\n");
}

#[test]
fn an_export_declaring_stderr_demands_it_of_its_consumer() {
    let err = compile_consumer(
        r#"
use { eprintln, Stderr } from "core:cli";

export fn warn(x: i32) with Stderr {
    eprintln(`${x}`);
}
"#,
        r#"
use { warn } from "./lib.wasm" with { type: "wasm" };

export fn run() {
    warn(1);
}
"#,
    )
    .expect_err("the consumer must hold `Stderr`");
    assert!(err.contains("missing effect 'Stderr'"), "{err}");
}
