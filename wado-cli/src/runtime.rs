use anyhow::Result;
use wado_compiler::coverage::TestReport;
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, InstanceAllocationStrategy, OptLevel, PoolingAllocationConfig, Store};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

use crate::coverage_host::add_to_linker as add_coverage_to_linker;
use crate::eval_host::{EvalSession, add_to_linker as add_eval_to_linker};
use crate::fuel_host::{FuelRegion, add_to_linker as add_fuel_to_linker};
use crate::knobs::RuntimeKnobs;
use wado_host::guest::{GuestState, create_config, host_linker};
pub use wado_host::guest::{Preopens, fuel_spent, new_store};
pub use wado_host::options::{ProfileMode, parse_profile};
use wado_host::process_host::ProcessGrant;

/// A guest under `wado`: what every host serves, plus what only a test reaches.
pub struct WasiState {
    guest: GuestState,
    /// `wado test` only: the store's handle on `core:eval`.
    eval: Option<EvalSession>,
    /// `wado test` only: what this store reported through `core:coverage`.
    coverage: TestReport,
    /// `wado test` only: the regions this store measured through `core:fuel`.
    fuel_regions: Vec<FuelRegion>,
}

wado_host::guest_views!(WasiState, guest);

impl From<GuestState> for WasiState {
    fn from(guest: GuestState) -> Self {
        Self {
            guest,
            eval: None,
            coverage: TestReport::default(),
            fuel_regions: Vec::new(),
        }
    }
}

impl WasiState {
    /// See [`GuestState::new_no_inherit_env_with_preopens`].
    #[must_use]
    pub fn new_no_inherit_env_with_preopens(preopens: &Preopens, args: &[String]) -> Self {
        GuestState::new_no_inherit_env_with_preopens(preopens, args).into()
    }

    pub fn coverage_report(&mut self) -> &mut TestReport {
        &mut self.coverage
    }

    /// The regions this store measured through `core:fuel`.
    pub fn fuel_regions(&mut self) -> &mut Vec<FuelRegion> {
        &mut self.fuel_regions
    }

    /// The test's handle on `core:eval`.
    pub fn eval_session(&mut self) -> &mut EvalSession {
        self.eval
            .as_mut()
            .expect("only a test links core:eval, and every test store carries a session")
    }
}

/// Create a wasmtime Engine for test execution with epoch interruption enabled,
/// under the profiling mode `wado test --profile` selected.
///
/// # Errors
///
/// Returns an error if the engine cannot be created with the given configuration.
pub fn create_test_engine(
    opt_level: OptLevel,
    profile: &ProfileMode,
    knobs: &RuntimeKnobs,
) -> Result<Engine> {
    let mut config = create_config(opt_level, profile, knobs);
    config.epoch_interruption(true);
    Ok(Engine::new(&config)?)
}

/// Create a wasmtime Engine for `wado serve`, backed by the pooling
/// instance allocator.
///
/// `wado serve` keeps a fixed set of long-lived component instances and
/// recycles them periodically. The pooling allocator keeps the OS memory
/// slots (linear memories, tables, GC heaps, async fiber stacks)
/// reserved up front and reuses them across (re-)instantiations, so a
/// recycle costs no `mmap`/`munmap` round-trip.
///
/// `max_instances` bounds how many component instances may be live at
/// once (workers plus recycle head-room), `max_stacks` how many async
/// fiber stacks. Both translate into a fixed up-front virtual
/// address-space reservation.
///
/// Epoch interruption is enabled so `wado serve` can push the per-store
/// deadline out of reach on its long-lived stores.
///
/// # Errors
///
/// Returns an error if the engine cannot be created — notably if the
/// host cannot reserve the pool's virtual address space.
pub fn create_serve_engine(
    opt_level: OptLevel,
    max_instances: u32,
    max_stacks: u32,
    knobs: &RuntimeKnobs,
) -> Result<Engine> {
    let mut config = create_config(opt_level, &ProfileMode::None, knobs);
    config.epoch_interruption(true);

    // Generous per-instance multipliers: a single component instantiation
    // expands into several core instances / memories / tables / GC heaps.
    // `max_instances` is small (workers + head-room), so the headroom
    // costs little address space.
    let mut pool = PoolingAllocationConfig::new();
    pool.total_component_instances(max_instances);
    pool.total_core_instances(max_instances.saturating_mul(8));
    pool.total_memories(max_instances.saturating_mul(4));
    pool.total_tables(max_instances.saturating_mul(8));
    pool.total_gc_heaps(max_instances.saturating_mul(4));
    pool.total_stacks(max_stacks.max(1));
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));

    Ok(Engine::new(&config)?)
}

/// A store whose guest's stdout/stderr are captured into in-memory pipes instead
/// of inherited — used by `wado test` so a test's own prints can be attributed
/// to its result instead of racing directly onto the host's real stdout/stderr.
/// `eval` is the test's handle on `core:eval`, which only a test links.
///
/// # Errors
///
/// Returns an error if a preopened directory cannot be opened.
pub fn create_test_store(
    engine: &Engine,
    preopened_dirs: &[(String, String)],
    program: &str,
    eval: EvalSession,
    process: ProcessGrant,
) -> Result<(Store<WasiState>, MemoryOutputPipe, MemoryOutputPipe)> {
    let (guest, stdout, stderr) =
        GuestState::new_capturing_stdio(preopened_dirs, &[program.to_owned()], process)?;
    let state = WasiState {
        eval: Some(eval),
        ..guest.into()
    };
    Ok((new_store(engine, state), stdout, stderr))
}

/// Create a Linker for `component`: what every guest reaches, and a trap for
/// every other import.
///
/// # Errors
///
/// Returns an error if a binding cannot be added to the linker, or if an import
/// is a module or a component.
pub fn create_linker(component: &Component) -> Result<Linker<WasiState>> {
    let mut linker = host_linker(component.engine())?;
    linker.define_unknown_imports_as_traps(component)?;
    Ok(linker)
}

/// Like [`create_linker`], plus `core:eval`, `core:coverage` and `core:fuel`,
/// which only a test may reach.
///
/// # Errors
///
/// Returns an error if a binding cannot be added to the linker, or if an import
/// is a module or a component.
pub fn create_test_linker(component: &Component) -> Result<Linker<WasiState>> {
    let mut linker = host_linker(component.engine())?;
    add_eval_to_linker(&mut linker, WasiState::eval_session)?;
    add_coverage_to_linker(&mut linker, WasiState::coverage_report)?;
    add_fuel_to_linker(&mut linker)?;
    linker.define_unknown_imports_as_traps(component)?;
    Ok(linker)
}
