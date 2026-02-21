use anyhow::Result;
use wasmtime::component::{Linker, ResourceTable};
use wasmtime::{Config, Engine, OptLevel, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxView, WasiView};

pub struct WasiState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiState {
    /// Create a new WASI state with preopened directories and program arguments.
    /// `preopened_dirs`: `(host_path, guest_path)` pairs.
    /// `args`: arguments passed to the guest program via `wasi:cli/environment.get-arguments`.
    pub fn new(preopened_dirs: &[(String, String)], args: &[String]) -> Result<Self> {
        let mut builder = WasiCtx::builder();
        builder.inherit_stdio();
        builder.args(args);
        for (host_path, guest_path) in preopened_dirs {
            builder.preopened_dir(host_path, guest_path, DirPerms::all(), FilePerms::all())?;
        }
        let ctx = builder.build();
        let table = ResourceTable::new();
        Ok(Self { ctx, table })
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

/// Create a wasmtime Config with all required Wasm features enabled.
pub fn create_config(opt_level: OptLevel) -> Config {
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
    // config.wasm_stack_switching(true); // Not supported on macOS
    config.wasm_gc(true);
    config.wasm_function_references(true);

    config.cranelift_opt_level(opt_level);

    config
}

/// Create a wasmtime Engine with the standard configuration.
pub fn create_engine(opt_level: OptLevel) -> Result<Engine> {
    Engine::new(&create_config(opt_level))
}

/// Create a Store with WASI state, preopened directories, and program arguments.
/// `preopened_dirs`: `(host_path, guest_path)` pairs.
/// `args`: arguments passed to the guest via `wasi:cli/environment.get-arguments`.
pub fn create_store(
    engine: &Engine,
    preopened_dirs: &[(String, String)],
    args: &[String],
) -> Result<Store<WasiState>> {
    Ok(Store::new(engine, WasiState::new(preopened_dirs, args)?))
}

/// Create a Linker with WASI P3 bindings.
pub fn create_linker(engine: &Engine) -> Result<Linker<WasiState>> {
    let mut linker: Linker<WasiState> = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    Ok(linker)
}
