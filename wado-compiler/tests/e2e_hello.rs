//! End-to-end test for hello world program
//!
//! This test compiles a hello world Wado program and runs it with wasmtime,
//! verifying the output matches "Hello, world!\n".

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

async fn run_wasm_capture_stdout(wasm: Vec<u8>) -> anyhow::Result<String> {
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

    // Create stdout capture pipe
    let stdout_pipe = MemoryOutputPipe::new(4096);
    let stdout_clone = stdout_pipe.clone();

    // Create WASI state with captured stdout
    let ctx = WasiCtxBuilder::new().stdout(stdout_pipe).build();
    let table = ResourceTable::new();

    let state = TestWasiState { ctx, table };
    let mut store = Store::new(&engine, state);

    // Instantiate the component
    let instance = linker.instantiate_async(&mut store, &component).await?;

    // Get and call the "run" function
    let run_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")?;

    let (result,) = run_func.call_async(&mut store, ()).await?;
    result.map_err(|()| anyhow::anyhow!("Component returned error"))?;

    // Get captured stdout using contents() method
    let output_bytes = stdout_clone.contents();
    let output = String::from_utf8(output_bytes.to_vec())?;

    Ok(output)
}

#[tokio::test]
async fn test_hello_world() {
    let source = include_str!("fixtures/hello.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output
    assert_eq!(output, "Hello, world!\n");
}

#[tokio::test]
async fn test_multiple_println() {
    let source = include_str!("fixtures/multiple_println.wado");

    // Compile the source
    let wasm = compile(source).expect("compilation failed");

    // Run and capture output
    let output = run_wasm_capture_stdout(wasm).await.expect("runtime error");

    // Verify output
    assert_eq!(output, "Line 1\nLine 2\nLine 3\n");
}
