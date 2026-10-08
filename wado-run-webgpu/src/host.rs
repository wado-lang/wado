//! The wasmtime host: WASI P3 plus `wasi:webgpu`, backed by wgpu.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::task::spawn_blocking;
use wasi_webgpu_wasmtime::wasi::webgpu::webgpu::{
    Gpu as GuestGpu, GpuQueue as GuestGpuQueue, GpuRequestAdapterOptions,
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

use crate::args::{AdapterSelector, Args, LogLevel, OptLevel};

/// The wgpu instance every `wasi:webgpu` call draws on, and the adapter
/// `--gpu-adapter` hands every `request-adapter`.
pub struct Gpu {
    instance: Arc<Global>,
    pinned: Option<Adapter>,
}

struct Host {
    ctx: WasiCtx,
    table: ResourceTable,
    gpu: Gpu,
    options: WasiWebGpuOptions,
    log_level: LogLevel,
}

impl Host {
    /// What the host's own `request-adapter` answers, unless `--gpu-adapter`
    /// pinned one. Replaced here so the run can say which it was, or that
    /// there was none.
    fn request_adapter(
        &mut self,
        options: Option<&GpuRequestAdapterOptions>,
    ) -> wasmtime::Result<Option<Resource<Adapter>>> {
        let adapter = self.choose_adapter(options)?;
        match &adapter {
            Some(id) if self.log_level >= LogLevel::Info => eprintln!(
                "wado-run-webgpu: info: request-adapter: {}",
                describe(&self.gpu.instance.adapter_get_info(**id))
            ),
            None if self.log_level >= LogLevel::Warn => eprintln!(
                "wado-run-webgpu: warning: request-adapter: no adapter satisfies the options the program passed"
            ),
            Some(_) | None => {}
        }
        Ok(adapter
            .map(|adapter| self.table.push(adapter))
            .transpose()?)
    }

    fn choose_adapter(
        &self,
        options: Option<&GpuRequestAdapterOptions>,
    ) -> wasmtime::Result<Option<Adapter>> {
        if let Some(pinned) = &self.gpu.pinned {
            return Ok(Some(Arc::clone(pinned)));
        }
        let options = options.map_or_else(RequestAdapterOptions::default, core_options);
        match self
            .gpu
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
        power_preference: options
            .power_preference
            .map_or(PowerPreference::None, Into::into),
        force_fallback_adapter: options.force_fallback_adapter.unwrap_or(false),
        compatible_surface: None,
    }
}

/// The adapter's name, then its backend, device type and driver in brackets:
/// names and driver strings carry parentheses of their own (`llvmpipe (LLVM
/// 21.1.8, 256 bits)`), so the name ends at the first ` [`.
fn describe(info: &AdapterInfo) -> String {
    let driver = format!("{} {}", info.driver, info.driver_info);
    format!(
        "{} [{:?}, {:?}, {}]",
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
            instance: &self.gpu.instance,
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
    // Named by the guest's import rather than spelled here: a version the host
    // crate does not serve then fails to instantiate instead of leaving this
    // override on an interface nothing imports.
    if let Some(interface) = webgpu_import(&engine, &component) {
        linker.allow_shadowing(true);
        linker.instance(&interface)?.func_wrap_concurrent(
            "[method]gpu.request-adapter",
            |accessor: &Accessor<Host>,
             (_gpu, options): (Resource<GuestGpu>, Option<GpuRequestAdapterOptions>)| {
                Box::pin(async move {
                    accessor
                        .with(|mut access| Ok((access.get().request_adapter(options.as_ref())?,)))
                })
            },
        )?;
        // The host crate's version awaits a wgpu callback but polls no device,
        // so wgpu never fires it and the guest waits forever. A blocking poll
        // returns once every submission has finished, which is what the call
        // waits for. It runs off the store, whose other guest tasks it would
        // otherwise stall.
        linker.instance(&interface)?.func_wrap_concurrent(
            "[method]gpu-queue.on-submitted-work-done",
            |accessor: &Accessor<Host>, (_queue,): (Resource<GuestGpuQueue>,)| {
                Box::pin(async move {
                    let instance =
                        accessor.with(|mut access| Arc::clone(&access.get().gpu.instance));
                    spawn_blocking(move || instance.poll_all_devices(true)).await??;
                    Ok(())
                })
            },
        )?;
    }

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

/// The `wasi:webgpu/webgpu` interface the component imports, versioned as it
/// names it.
fn webgpu_import(engine: &Engine, component: &Component) -> Option<String> {
    component
        .component_type()
        .imports(engine)
        .map(|(name, _)| name)
        .find(|name| name.starts_with("wasi:webgpu/webgpu@"))
        .map(str::to_owned)
}

/// The GC heap `wado run` starts a guest with, `DEFAULT_GC_HEAP_INITIAL_SIZE`
/// in `wado-cli/src/runtime.rs`. wasmtime collects before it grows, so a heap
/// starting at zero pays a full trace at every doubling up to the working set.
const GC_HEAP_INITIAL_SIZE: u64 = 256 << 20;

/// What `wado run` configures, so a program behaves the same under either
/// runner: the same features, the same collector and initial GC heap, the same
/// Cranelift level.
fn engine_config(opt_level: OptLevel) -> Config {
    let mut config = Config::new();
    config.wasm_component_model_gc(true);
    config.wasm_component_model_more_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_map(true);
    config.wasm_branch_hinting(true);
    config.collector(Collector::Copying);
    config.gc_heap_initial_size(GC_HEAP_INITIAL_SIZE);
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
        gpu,
        options: WasiWebGpuOptions::default(),
        log_level: args.log_level,
    })
}

/// The wgpu instance, refused when the machine offers no adapter: the guest
/// would otherwise see `request-adapter` answer `none` and have nothing to say
/// about why. `--gpu-adapter` is settled here too, before anything compiles.
pub fn gpu(args: &Args) -> Result<Gpu> {
    let global = instance();
    let adapters = global.enumerate_adapters(Backends::all());
    if adapters.is_empty() {
        bail!("{NO_ADAPTER}");
    }
    let pinned = args
        .gpu_adapter
        .as_ref()
        .map(|selector| pin(&global, &adapters, selector))
        .transpose()?;
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

const NO_ADAPTER: &str = "no GPU adapter found. wasi:webgpu needs one, and a driver supplies it: \
     on Linux install mesa-vulkan-drivers for the lavapipe software adapter, \
     or set WGPU_BACKEND to pick another backend";

/// The wgpu instance over whichever backends the platform and `WGPU_BACKEND`
/// offer: Vulkan, Metal, D3D12 or GL.
fn instance() -> Global {
    Global::new(
        "wado-run-webgpu",
        InstanceDescriptor::new_without_display_handle_from_env(),
        None,
    )
}

fn adapter_infos(global: &Global, adapters: &[AdapterId]) -> Vec<AdapterInfo> {
    adapters
        .iter()
        .map(|&id| global.adapter_get_info(id))
        .collect()
}

/// This machine's adapters under the indices `--gpu-adapter` takes, as the
/// end of `--help`.
pub fn adapter_list() -> String {
    let global = instance();
    let infos = adapter_infos(&global, &global.enumerate_adapters(Backends::all()));
    if infos.is_empty() {
        return format!("GPU adapters: {NO_ADAPTER}\n");
    }
    format!(
        "GPU adapters (--gpu-adapter takes the index):\n  {}\n",
        listed(&infos, 0..infos.len())
    )
}

/// The one adapter `selector` names.
fn pin(global: &Global, adapters: &[AdapterId], selector: &AdapterSelector) -> Result<AdapterId> {
    let infos = adapter_infos(global, adapters);
    let all = || listed(&infos, 0..infos.len());
    match selector {
        AdapterSelector::Index(index) => match adapters.get(*index) {
            Some(&id) => Ok(id),
            None => bail!(
                "no GPU adapter at index {index}. The adapters here:\n  {}",
                all()
            ),
        },
        AdapterSelector::Name(name) => {
            let needle = name.to_lowercase();
            let matching: Vec<usize> = (0..infos.len())
                .filter(|&i| infos[i].name.to_lowercase().contains(&needle))
                .collect();
            match *matching.as_slice() {
                [only] => Ok(adapters[only]),
                [] => bail!(
                    "no GPU adapter matches '{name}'. The adapters here:\n  {}",
                    all()
                ),
                [..] => bail!(
                    "'{name}' matches more than one GPU adapter; name one by its index:\n  {}",
                    listed(&infos, matching)
                ),
            }
        }
    }
}

/// The adapters at `indices`, one per line, each under the index
/// `--gpu-adapter` takes.
fn listed(infos: &[AdapterInfo], indices: impl IntoIterator<Item = usize>) -> String {
    indices
        .into_iter()
        .map(|i| format!("{i}: {}", describe(&infos[i])))
        .collect::<Vec<_>>()
        .join("\n  ")
}
