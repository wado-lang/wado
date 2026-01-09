use std::process;

use anyhow::Result;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::compile;

pub struct RunOptions {
    pub input: String,
}

pub fn print_usage() {
    eprintln!("Usage: wado run [options] <file.wado>");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --help, -h  Show this help message");
}

pub fn parse_args(args: &[String]) -> RunOptions {
    let mut input: Option<String> = None;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_usage();
                process::exit(0);
            }
            arg if arg.starts_with('-') => {
                eprintln!("Error: unknown option '{arg}'");
                print_usage();
                process::exit(1);
            }
            arg => {
                if input.is_some() {
                    eprintln!("Error: multiple input files not supported");
                    process::exit(1);
                }
                input = Some(arg.to_string());
                i += 1;
            }
        }
    }

    let input = match input {
        Some(f) => f,
        None => {
            eprintln!("Error: no input file specified");
            print_usage();
            process::exit(1);
        }
    };

    RunOptions { input }
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
    // Configure engine with async and component model support
    let mut config = Config::new();
    config.async_support(true);
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_builtins(true);
    config.wasm_component_model_async_stackful(true); // stack switching
    config.wasm_gc(true); // Enable GC for GC string arrays

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

pub fn run(opts: RunOptions) {
    let wasm = compile::compile(&opts.input);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    if let Err(e) = runtime.block_on(run_wasm(wasm)) {
        eprintln!("Runtime error: {e}");
        process::exit(1);
    }
}
