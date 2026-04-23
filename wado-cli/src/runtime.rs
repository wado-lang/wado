use anyhow::Result;
use wasmtime::component::{Linker, ResourceTable};
use wasmtime::{Config, Engine, OptLevel, ProfilingStrategy, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p3::{WasiHttpCtxView, WasiHttpView};

pub struct WasiState {
    ctx: WasiCtx,
    table: ResourceTable,
    http: WasiHttpCtx,
}

impl WasiState {
    /// Create a new WASI state with preopened directories and program arguments.
    /// `preopened_dirs`: `(host_path, guest_path)` pairs.
    /// `args`: arguments passed to the guest program via `wasi:cli/environment.get-arguments`.
    ///
    /// # Errors
    ///
    /// Returns an error if a preopened directory cannot be opened.
    pub fn new(preopened_dirs: &[(String, String)], args: &[String]) -> Result<Self> {
        let mut builder = WasiCtx::builder();
        builder.inherit_stdio();
        builder.inherit_env();
        builder.args(args);
        for (host_path, guest_path) in preopened_dirs {
            builder.preopened_dir(host_path, guest_path, DirPerms::all(), FilePerms::all())?;
        }
        let ctx = builder.build();
        let table = ResourceTable::new();
        let http = WasiHttpCtx::new();
        Ok(Self { ctx, table, http })
    }
}

impl WasiView for WasiState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for WasiState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

/// Profiling mode for wasmtime execution.
#[derive(Clone, Debug)]
pub enum ProfileMode {
    /// No profiling.
    None,
    /// Linux perf jitdump profiling. Use with `perf record -k mono`.
    JitDump,
    /// Linux perf map profiling. Use with `perf record -k mono`.
    PerfMap,
    /// Cross-platform guest profiling. Produces Firefox Profiler JSON.
    Guest {
        /// Output file path.
        path: String,
        /// Sampling interval in milliseconds.
        interval_ms: u64,
    },
}

/// Create a wasmtime Config with all required Wasm features enabled.
#[must_use]
pub fn create_config(opt_level: OptLevel, profile: &ProfileMode) -> Config {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_gc(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_simd(true);
    config.wasm_wide_arithmetic(true);
    config.wasm_threads(true);
    // config.wasm_stack_switching(true); // Not supported on macOS
    config.wasm_gc(true);
    config.wasm_function_references(true);

    config.cranelift_opt_level(opt_level);

    match profile {
        ProfileMode::None => {}
        ProfileMode::JitDump => {
            config.profiler(ProfilingStrategy::JitDump);
        }
        ProfileMode::PerfMap => {
            config.profiler(ProfilingStrategy::PerfMap);
        }
        ProfileMode::Guest { .. } => {
            config.epoch_interruption(true);
        }
    }

    config
}

/// Create a wasmtime Engine with the standard configuration.
///
/// # Errors
///
/// Returns an error if the engine cannot be created with the given configuration.
pub fn create_engine(opt_level: OptLevel, profile: &ProfileMode) -> Result<Engine> {
    Ok(Engine::new(&create_config(opt_level, profile))?)
}

/// Create a wasmtime Engine tuned for Kiln generator execution.
///
/// Differs from [`create_engine`] by enabling fuel consumption so the Kiln
/// runtime can enforce [`crate::kiln_runtime::KilnRunPolicy::fuel`]. Without
/// this, `Store::set_fuel` is rejected by the runtime.
///
/// # Errors
///
/// Returns an error if the engine cannot be created with the given configuration.
pub fn create_kiln_engine(opt_level: OptLevel) -> Result<Engine> {
    let mut config = create_config(opt_level, &ProfileMode::None);
    config.consume_fuel(true);
    Ok(Engine::new(&config)?)
}

/// Create a wasmtime Engine for test execution with epoch interruption enabled.
///
/// # Errors
///
/// Returns an error if the engine cannot be created with the given configuration.
pub fn create_test_engine(opt_level: OptLevel) -> Result<Engine> {
    let mut config = create_config(opt_level, &ProfileMode::None);
    config.epoch_interruption(true);
    Ok(Engine::new(&config)?)
}

/// Create a Store with WASI state, preopened directories, and program arguments.
/// `preopened_dirs`: `(host_path, guest_path)` pairs.
/// `args`: arguments passed to the guest via `wasi:cli/environment.get-arguments`.
///
/// # Errors
///
/// Returns an error if a preopened directory cannot be opened.
pub fn create_store(
    engine: &Engine,
    preopened_dirs: &[(String, String)],
    args: &[String],
) -> Result<Store<WasiState>> {
    Ok(Store::new(engine, WasiState::new(preopened_dirs, args)?))
}

/// Create a Linker with WASI P3 and HTTP bindings.
///
/// # Errors
///
/// Returns an error if WASI bindings cannot be added to the linker.
pub fn create_linker(engine: &Engine) -> Result<Linker<WasiState>> {
    let mut linker: Linker<WasiState> = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    Ok(linker)
}
