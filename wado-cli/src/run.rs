use std::process;

use anyhow::Result;
use lexopt::Arg::{Long, Value};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::args::{next_arg, reject_multiple_inputs, require_input, unexpected_arg};
use crate::compile;

pub struct RunOptions {
    pub input: String,
}

pub fn print_usage() {
    eprintln!("Usage: wado run [options] <file.wado>");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --help  Show this help message");
}

pub fn parse_args(mut parser: lexopt::Parser) -> RunOptions {
    let mut input: Option<String> = None;

    while let Some(arg) = next_arg(&mut parser) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Value(val) => {
                reject_multiple_inputs(&input);
                input = Some(val.to_string_lossy().into_owned());
            }
            _ => unexpected_arg(arg, print_usage),
        }
    }

    RunOptions {
        input: require_input(input, print_usage),
    }
}

struct WasiState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for WasiState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

async fn run_wasm(wasm: Vec<u8>) -> Result<()> {
    let mut config = Config::new();
    config.async_support(true);
    config.wasm_component_model(true);
    config.wasm_component_model_gc(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_simd(true);
    config.wasm_wide_arithmetic(true);
    config.wasm_threads(true);
    // config.wasm_stack_switching(true); // "runtime error: the wasm_stack_switching feature is not supported on this compiler configuration" on macos
    config.wasm_gc(true);
    config.wasm_function_references(true);

    let engine = Engine::new(&config)?;

    // Create component from wasm bytes
    let component = Component::new(&engine, &wasm)?;

    // Set up linker with WASI P3
    let mut linker: Linker<WasiState> = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;

    // Create WASI state
    let ctx = WasiCtx::builder().inherit_stdio().build();
    let table = ResourceTable::new();

    let state = WasiState { ctx, table };
    let mut store = Store::new(&engine, state);

    // Instantiate the component
    let instance = linker.instantiate_async(&mut store, &component).await?;

    // Get and call the "run" function
    // The function signature is: async func() -> result
    let run_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")?;

    let (result,) = run_func.call_async(&mut store, ()).await?;
    result.map_err(|()| anyhow::anyhow!("Component returned error"))?;

    Ok(())
}

pub async fn run(opts: RunOptions) {
    let wasm = compile::compile(&opts.input).await;

    if let Err(e) = run_wasm(wasm).await {
        eprintln!("Runtime error: {e}");
        process::exit(1);
    }
}
