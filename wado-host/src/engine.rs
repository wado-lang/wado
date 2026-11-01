//! The wasmtime configuration every host of Wado's output shares, so a program
//! runs under the e2e harness as it does under `wado`. `wado-run-webgpu`, a
//! workspace of its own, cannot depend on it, so its `engine_config` restates
//! it and must change with it.

use wasmtime::{Collector, Config, Engine, OptLevel};

use crate::fuel::meter_fuel;

/// wado's default GC collector.
///
/// wasmtime's own default ([`Collector::Auto`]) still resolves to the deferred
/// reference-counting collector in this generation. We prefer the copying
/// collector: on allocation-heavy / small-live-set workloads it is several
/// times faster at the same resident set size, and it collects cycles.
pub const DEFAULT_COLLECTOR: Collector = Collector::Copying;

/// The GC heap every guest starts with, whatever its world. The copying
/// collector splits it in two, so a program allocates through half per cycle.
pub const DEFAULT_GC_HEAP_INITIAL_SIZE: u64 = 256 << 20;

/// Enable the Wasm features what Wado emits may use.
fn enable_wasm_features(config: &mut Config) {
    config.wasm_component_model_gc(true);
    config.wasm_component_model_more_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_error_context(true);
    config.wasm_component_model_map(true);
    // config.wasm_stack_switching(true); // Not supported on macOS
    // Honor the `metadata.code.branch_hint` custom section so Cranelift can
    // lay out hinted branches (from `builtin::cold_path()`) for the
    // predicted-taken path.
    config.wasm_branch_hinting(true);
}

/// A configuration with every Wasm feature [`enable_wasm_features`] lists and
/// the default GC, compiling at `opt_level`.
#[must_use]
pub fn default_config(opt_level: OptLevel) -> Config {
    let mut config = Config::new();
    enable_wasm_features(&mut config);
    config.collector(DEFAULT_COLLECTOR);
    // wasmtime collects before it grows, so a heap starting at zero doubles its
    // way up to the working set and pays a full trace at every rung. The 4 GiB
    // of address space is reserved either way: this commits, it does not map.
    config.gc_heap_initial_size(DEFAULT_GC_HEAP_INITIAL_SIZE);
    config.cranelift_opt_level(opt_level);
    config
}

/// An engine on [`default_config`] that consumes fuel, for a guest run under a
/// budget: a Kiln generator, or a program `core:eval` evaluates. Without fuel
/// consumption the runtime rejects `Store::set_fuel`.
///
/// # Errors
///
/// Fails when wasmtime rejects the configuration.
pub fn fuel_engine(opt_level: OptLevel) -> anyhow::Result<Engine> {
    let mut config = default_config(opt_level);
    meter_fuel(&mut config);
    Ok(Engine::new(&config)?)
}
