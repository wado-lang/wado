//! The wasmtime configuration every host of Wado's output shares, so a program
//! runs under the e2e harness as it does under `wado`.

use wasmtime::{Collector, Config};

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
pub fn enable_wasm_features(config: &mut Config) {
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
