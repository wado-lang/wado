use std::process;

use anyhow::Result;
use lexopt::Arg::{Long, Value};
use wasmtime::component::Component;

use crate::args::{next_arg, reject_multiple_inputs, require_input, unexpected_arg};
use crate::compile;
use crate::runtime;

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

async fn run_component(wasm: &[u8]) -> Result<()> {
    let engine = runtime::create_engine()?;
    let component = Component::new(&engine, wasm)?;
    let linker = runtime::create_linker(&engine)?;
    let mut store = runtime::create_store(&engine);

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")?;

    let (result,) = run_func.call_async(&mut store, ()).await?;
    result.map_err(|()| anyhow::anyhow!("Component returned error"))?;

    Ok(())
}

pub async fn run(opts: RunOptions) {
    let wasm = compile::compile(&opts.input).await;

    if let Err(e) = run_component(&wasm).await {
        eprintln!("Runtime error: {e}");
        process::exit(1);
    }
}
