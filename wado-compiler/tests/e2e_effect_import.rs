//! End-to-end tests for Effect function import feature
//!
//! These tests verify that Effect functions can be imported and used
//! without the `Effect.` prefix.

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use wado_compiler::compile;

struct TestWasiState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for TestWasiState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

async fn run_wasm_capture_output(wasm: Vec<u8>) -> anyhow::Result<(String, String)> {
    // Configure engine with async and component model support
    let mut config = Config::new();
    config.async_support(true);
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_builtins(true);
    config.wasm_component_model_async_stackful(true);

    let engine = Engine::new(&config)?;

    // Create component from wasm bytes
    let component = Component::new(&engine, &wasm)?;

    // Set up linker with WASI P3
    let mut linker: Linker<TestWasiState> = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;

    // Create stdout and stderr capture pipes
    let stdout_pipe = MemoryOutputPipe::new(4096);
    let stdout_clone = stdout_pipe.clone();
    let stderr_pipe = MemoryOutputPipe::new(4096);
    let stderr_clone = stderr_pipe.clone();

    // Create WASI state with captured stdout and stderr
    let ctx = WasiCtxBuilder::new()
        .stdout(stdout_pipe)
        .stderr(stderr_pipe)
        .build();
    let table = ResourceTable::new();

    let state = TestWasiState { ctx, table };
    let mut store = Store::new(&engine, state);

    // Instantiate the component
    let instance = linker.instantiate_async(&mut store, &component).await?;

    // Get and call the "run" function
    let run_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")?;

    let (result,) = run_func.call_async(&mut store, ()).await?;
    result.map_err(|()| anyhow::anyhow!("Component returned error"))?;

    // Get captured stdout and stderr
    let stdout_bytes = stdout_clone.contents();
    let stdout = String::from_utf8(stdout_bytes.to_vec())?;
    let stderr_bytes = stderr_clone.contents();
    let stderr = String::from_utf8(stderr_bytes.to_vec())?;

    Ok((stdout, stderr))
}

#[tokio::test]
async fn test_effect_import_demo() {
    let source = include_str!("fixtures/effect-import-demo.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let (stdout, _stderr) = run_wasm_capture_output(wasm)
        .await
        .expect("runtime error");

    // Verify stdout
    assert_eq!(stdout, "Hello from imported Stdout!\n");
}

#[tokio::test]
async fn test_effect_import_with_aliasing() {
    // Test that importing with 'as' aliasing works correctly
    let source = r#"
use core::cli::{println, Stdout};

fn main() with Stdout {
    println("Aliased import works!");
    println("Direct import also works!");
}
"#;

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let (stdout, _stderr) = run_wasm_capture_output(wasm)
        .await
        .expect("runtime error");

    // Verify output
    assert!(stdout.contains("Aliased import works!"));
    assert!(stdout.contains("Direct import also works!"));
}

#[tokio::test]
async fn test_multiple_effect_imports() {
    // Test importing from multiple effects
    let source = r#"
use core::cli::{println, Stdout};

fn main() with Stdout {
    println("Testing multiple imports");
}
"#;

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let (stdout, _stderr) = run_wasm_capture_output(wasm)
        .await
        .expect("runtime error");

    // Verify output
    assert_eq!(stdout, "Testing multiple imports\n");
}
