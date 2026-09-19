//! The wasmtime host: WASI P3 plus `wasi:webgpu`, backed by wgpu.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use wasi_webgpu_wasmtime::{WasiWebGpuCtx, WasiWebGpuCtxView, WasiWebGpuOptions};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p3::bindings::Command;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};
use wgpu_core::global::Global;
use wgpu_types::{Backends, InstanceDescriptor};

use crate::args::Args;

/// The wgpu instance every `wasi:webgpu` call draws on.
pub type Gpu = Arc<Global>;

struct Host {
    ctx: WasiCtx,
    table: ResourceTable,
    instance: Gpu,
    options: WasiWebGpuOptions,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl WasiWebGpuCtxView for Host {
    fn webgpu_ctx(&mut self) -> WasiWebGpuCtx<'_> {
        WasiWebGpuCtx {
            instance: &self.instance,
            table: &mut self.table,
            options: &self.options,
        }
    }
}

/// Run a component that exports `wasi:cli/run`, on the GPU already found.
pub async fn run(component: &Path, args: &Args, gpu: Gpu) -> Result<()> {
    let engine = Engine::new(&engine_config())?;
    let component = Component::from_file(&engine, component)?;

    let mut linker: Linker<Host> = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasi_webgpu_wasmtime::add_to_linker(&mut linker)?;

    let mut store = Store::new(&engine, host(args, gpu)?);
    let command = Command::instantiate_async(&mut store, &component, &linker).await?;
    let result = store
        .run_concurrent(async |accessor| command.wasi_cli_run().call_run(accessor).await)
        .await??;
    match result {
        Ok(()) => Ok(()),
        Err(()) => bail!("the program exited with an error"),
    }
}

fn engine_config() -> Config {
    let mut config = Config::new();
    config.wasm_component_model_gc(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_wide_arithmetic(true);
    config.wasm_branch_hinting(true);
    config
}

fn host(args: &Args, gpu: Gpu) -> Result<Host> {
    let mut builder = WasiCtx::builder();
    builder.inherit_stdio().inherit_env();

    let mut argv = vec![
        args.input
            .file_name()
            .unwrap_or(args.input.as_os_str())
            .to_string_lossy()
            .into_owned(),
    ];
    argv.extend(args.program_args.iter().cloned());
    builder.args(&argv);

    for dir in &args.preopens {
        let guest = dir.to_string_lossy().into_owned();
        builder.preopened_dir(dir, &guest, FsPerms::ReadWrite)?;
    }

    Ok(Host {
        ctx: builder.build(),
        table: ResourceTable::new(),
        instance: gpu,
        options: WasiWebGpuOptions::default(),
    })
}

/// The wgpu instance, refused when the machine offers no adapter: the guest
/// would otherwise see `request-adapter` answer `none` and have nothing to say
/// about why.
pub fn gpu() -> Result<Gpu> {
    let global = Global::new(
        "wado-run-webgpu",
        InstanceDescriptor::new_without_display_handle_from_env(),
        None,
    );
    if global
        .instance
        .enumerate_adapters(Backends::all())
        .is_empty()
    {
        bail!(
            "no GPU adapter found. wasi:webgpu needs one, and a driver supplies it: \
             on Linux install mesa-vulkan-drivers for the lavapipe software adapter, \
             or set WGPU_BACKEND to pick another backend"
        );
    }
    Ok(Arc::new(global))
}
