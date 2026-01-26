use anyhow::Result;
use wasmtime::component::{Linker, ResourceTable};
use wasmtime::{Config, Engine, OptLevel, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

pub struct WasiState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiState {
    pub fn new() -> Self {
        let ctx = WasiCtx::builder().inherit_stdio().build();
        let table = ResourceTable::new();
        Self { ctx, table }
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
///
/// Optimization is disabled by default for faster compilation during development.
pub fn create_config() -> Config {
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

    // Disable Cranelift optimizations for faster compilation
    config.cranelift_opt_level(OptLevel::None);

    config
}

/// Create a wasmtime Engine with the standard configuration.
pub fn create_engine() -> Result<Engine> {
    Engine::new(&create_config())
}

/// Create a Store with WASI state.
pub fn create_store(engine: &Engine) -> Store<WasiState> {
    Store::new(engine, WasiState::new())
}

/// Create a Linker with WASI P3 bindings.
pub fn create_linker(engine: &Engine) -> Result<Linker<WasiState>> {
    let mut linker: Linker<WasiState> = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    Ok(linker)
}
