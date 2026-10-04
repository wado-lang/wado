//! The wasmtime host: WASI P3 plus `wasi:webgpu`, backed by wgpu.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use wasi_webgpu_wasmtime::wasi::webgpu::webgpu::{
    Gpu as GuestGpu, GpuPowerPreference, GpuRequestAdapterOptions,
};
use wasi_webgpu_wasmtime::{
    Adapter, WasiWebGpuCtx, WasiWebGpuCtxView, WasiWebGpuOptions,
    add_to_linker as add_webgpu_to_linker,
};
use wasmtime::component::{Accessor, Component, Linker, Resource, ResourceTable};
use wasmtime::{Collector, Config, Engine, Store};
use wasmtime_wasi::p3::add_to_linker as add_wasi_to_linker;
use wasmtime_wasi::p3::bindings::Command;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};
use wgpu_core::global::Global;
use wgpu_core::id::AdapterId;
use wgpu_core::instance::RequestAdapterOptions;
use wgpu_types::{AdapterInfo, Backends, InstanceDescriptor, PowerPreference, RequestAdapterError};

use crate::args::{Args, LogLevel, OptLevel};

/// The interface `request-adapter` belongs to, as the host's bindings name it.
const WEBGPU_INTERFACE: &str = "wasi:webgpu/webgpu@0.3.0-rc.2";

/// The wgpu instance every `wasi:webgpu` call draws on, and the adapter
/// `--gpu-adapter` hands every `request-adapter`.
pub struct Gpu {
    instance: Arc<Global>,
    pinned: Option<Adapter>,
}

struct Host {
    ctx: WasiCtx,
    table: ResourceTable,
    instance: Arc<Global>,
    options: WasiWebGpuOptions,
    pinned: Option<Adapter>,
    log_level: LogLevel,
}

impl Host {
    /// What the host's own `request-adapter` answers, unless `--gpu-adapter`
    /// pinned one. Replaced here so `--log-level info` can say which it was.
    fn request_adapter(
        &mut self,
        options: Option<&GpuRequestAdapterOptions>,
    ) -> wasmtime::Result<Option<Resource<Adapter>>> {
        let adapter = self.choose_adapter(options)?;
        if self.log_level >= LogLevel::Info {
            let answer = adapter.as_ref().map_or_else(
                || "none".to_owned(),
                |id| describe(&self.instance.adapter_get_info(**id)),
            );
            eprintln!("wado-run-webgpu: info: request-adapter: {answer}");
        }
        Ok(match adapter {
            Some(adapter) => Some(self.table.push(adapter)?),
            None => None,
        })
    }

    fn choose_adapter(
        &self,
        options: Option<&GpuRequestAdapterOptions>,
    ) -> wasmtime::Result<Option<Adapter>> {
        if let Some(pinned) = &self.pinned {
            return Ok(Some(Arc::clone(pinned)));
        }
        let options = options.map_or_else(RequestAdapterOptions::default, core_options);
        match self
            .instance
            .request_adapter(&options, Backends::all(), None)
        {
            Ok(id) => Ok(Some(Arc::new(id))),
            Err(RequestAdapterError::NotFound { .. }) => Ok(None),
            // `RequestAdapterError` is `#[non_exhaustive]`.
            Err(error) => Err(wasmtime::Error::msg(error.to_string())),
        }
    }
}

/// What the host's bindings make of the options, `feature-level` and
/// `xr-compatible` ignored as they are there.
fn core_options(options: &GpuRequestAdapterOptions) -> RequestAdapterOptions {
    RequestAdapterOptions {
        power_preference: match options.power_preference {
            None => PowerPreference::None,
            Some(GpuPowerPreference::LowPower) => PowerPreference::LowPower,
            Some(GpuPowerPreference::HighPerformance) => PowerPreference::HighPerformance,
        },
        force_fallback_adapter: options.force_fallback_adapter.unwrap_or(false),
        compatible_surface: None,
    }
}

fn describe(info: &AdapterInfo) -> String {
    let driver = format!("{} {}", info.driver, info.driver_info);
    format!(
        "{} ({:?}, {:?}, {})",
        info.name,
        info.backend,
        info.device_type,
        driver.trim()
    )
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
    let engine = Engine::new(&engine_config(args.opt_level))?;
    let component = Component::from_file(&engine, component)?;

    let mut linker: Linker<Host> = Linker::new(&engine);
    add_wasi_to_linker(&mut linker)?;
    add_webgpu_to_linker(&mut linker)?;
    linker.allow_shadowing(true);
    linker.instance(WEBGPU_INTERFACE)?.func_wrap_concurrent(
        "[method]gpu.request-adapter",
        |accessor: &Accessor<Host>,
         (_gpu, options): (Resource<GuestGpu>, Option<GpuRequestAdapterOptions>)| {
            Box::pin(async move {
                accessor.with(|mut access| Ok((access.get().request_adapter(options.as_ref())?,)))
            })
        },
    )?;

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

/// What `wado run` configures, so a program behaves the same under either
/// runner: the same features, the same collector, the same Cranelift level.
fn engine_config(opt_level: OptLevel) -> Config {
    let mut config = Config::new();
    config.wasm_component_model_gc(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_wide_arithmetic(true);
    config.wasm_branch_hinting(true);
    config.collector(Collector::Copying);
    config.cranelift_opt_level(cranelift_opt_level(opt_level));
    config
}

fn cranelift_opt_level(opt_level: OptLevel) -> wasmtime::OptLevel {
    match opt_level {
        OptLevel::O0 => wasmtime::OptLevel::None,
        OptLevel::O1 | OptLevel::O2 | OptLevel::O3 => wasmtime::OptLevel::Speed,
        OptLevel::Os => wasmtime::OptLevel::SpeedAndSize,
    }
}

fn host(args: &Args, gpu: Gpu) -> Result<Host> {
    let mut builder = WasiCtx::builder();
    builder.inherit_stdio().inherit_env();

    builder.arg(args.input.to_string_lossy());
    builder.args(&args.program_args);

    for dir in &args.preopens {
        let guest = dir.to_string_lossy().into_owned();
        builder.preopened_dir(dir, &guest, FsPerms::ReadWrite)?;
    }

    Ok(Host {
        ctx: builder.build(),
        table: ResourceTable::new(),
        instance: gpu.instance,
        options: WasiWebGpuOptions::default(),
        pinned: gpu.pinned,
        log_level: args.log_level,
    })
}

/// The wgpu instance, refused when the machine offers no adapter: the guest
/// would otherwise see `request-adapter` answer `none` and have nothing to say
/// about why. `--gpu-adapter` is settled here too, before anything compiles.
pub fn gpu(args: &Args) -> Result<Gpu> {
    let global = Global::new(
        "wado-run-webgpu",
        InstanceDescriptor::new_without_display_handle_from_env(),
        None,
    );
    let adapters = global.enumerate_adapters(Backends::all());
    if adapters.is_empty() {
        bail!(
            "no GPU adapter found. wasi:webgpu needs one, and a driver supplies it: \
             on Linux install mesa-vulkan-drivers for the lavapipe software adapter, \
             or set WGPU_BACKEND to pick another backend"
        );
    }
    let pinned = match &args.gpu_adapter {
        Some(name) => Some(pin(&global, &adapters, name)?),
        None => None,
    };
    for id in adapters {
        if pinned != Some(id) {
            global.adapter_drop(id);
        }
    }
    Ok(Gpu {
        instance: Arc::new(global),
        pinned: pinned.map(Arc::new),
    })
}

/// The one adapter whose name contains `name`, ignoring case.
fn pin(global: &Global, adapters: &[AdapterId], name: &str) -> Result<AdapterId> {
    let needle = name.to_lowercase();
    let described: Vec<(AdapterId, AdapterInfo)> = adapters
        .iter()
        .map(|&id| (id, global.adapter_get_info(id)))
        .collect();
    let matches: Vec<&(AdapterId, AdapterInfo)> = described
        .iter()
        .filter(|(_, info)| info.name.to_lowercase().contains(&needle))
        .collect();
    match matches.as_slice() {
        [(id, _)] => Ok(*id),
        [] => bail!(
            "no GPU adapter matches '{name}'. The adapters here:{}",
            listed(described.iter())
        ),
        [..] => bail!(
            "'{name}' matches more than one GPU adapter:{}",
            listed(matches.into_iter())
        ),
    }
}

fn listed<'a>(adapters: impl Iterator<Item = &'a (AdapterId, AdapterInfo)>) -> String {
    adapters
        .map(|(_, info)| format!("\n  {}", describe(info)))
        .collect::<Vec<_>>()
        .concat()
}
