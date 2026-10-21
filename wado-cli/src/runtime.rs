use anyhow::Result;
use std::fs::File;
use std::future::Future;
use std::io::{BufWriter, Write};
use std::pin::Pin;
use std::sync::{Arc, Condvar, LazyLock, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{
    AsContext, Collector, Config, Engine, GuestProfiler, InstanceAllocationStrategy, OptLevel,
    PoolingAllocationConfig, ProfilingStrategy, Store,
};
use wasmtime_wasi::filesystem::WasiFilesystemCtx;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};
use wasmtime_wasi_tls::{
    Error as WasiTlsError, TlsProvider, TlsStream, TlsTransport, WasiTlsCtx, WasiTlsCtxBuilder,
    WasiTlsCtxView, WasiTlsView,
};

use crate::args::CliExit;
use crate::coverage_host::{CoverageHits, add_to_linker as add_coverage_to_linker};
use crate::eval_host::{EvalSession, add_to_linker as add_eval_to_linker};
use crate::http_hooks::WadoHttpHooks;
use crate::knobs::RuntimeKnobs;
use wado_host::engine::default_config;
use wado_host::sync::lock;
use wado_host::timezone::add_to_linker;
use wado_host::tls_trust::{build_root_cert_store, install_default_crypto_provider};

/// Let `wasi:sockets` and `wasi:tls` reach the host network, as a native tool
/// can: a guest is confined by what its effects reach, and one that names
/// `TcpSocket` asked for the network. A deployment that must not give it one
/// confines the host process instead.
fn grant_host_network(builder: &mut WasiCtxBuilder) {
    builder.inherit_network();
    // wasmtime denies creating a socket by default; `inherit_network`
    // relaxes only the per-address check that comes after it.
    builder.allow_tcp(true);
    builder.allow_udp(true);
    builder.allow_ip_name_lookup(true);
}

/// Build a [`WasiTlsCtx`] backed by [`WadoTlsProvider`] so the raw
/// `wasi:tls` connector and [`WadoHttpHooks`] share the same trust store.
fn build_wasi_tls_ctx() -> WasiTlsCtx {
    WasiTlsCtxBuilder::new()
        .provider(Box::new(WadoTlsProvider::shared()))
        .build()
}

/// `TlsProvider` for `wasi:tls`. The `ClientConfig` is process-cached.
struct WadoTlsProvider {
    client_config: Arc<rustls::ClientConfig>,
}

impl WadoTlsProvider {
    fn shared() -> Self {
        static CONFIG: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
            install_default_crypto_provider();
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(build_root_cert_store())
                .with_no_client_auth();
            Arc::new(config)
        });
        Self {
            client_config: Arc::clone(&CONFIG),
        }
    }
}

impl TlsProvider for WadoTlsProvider {
    fn connect(
        &self,
        server_name: String,
        transport: Box<dyn TlsTransport>,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn TlsStream>, WasiTlsError>> + Send>> {
        let client_config = Arc::clone(&self.client_config);
        Box::pin(async move {
            let domain = rustls::pki_types::ServerName::try_from(server_name)
                .map_err(|_| WasiTlsError::msg("invalid server name"))?;
            let stream = tokio_rustls::TlsConnector::from(client_config)
                .connect(domain, transport)
                .await
                .map_err(|e| WasiTlsError::msg(e.to_string()))?;
            Ok(Box::new(stream) as Box<dyn TlsStream>)
        })
    }
}

pub struct WasiState {
    ctx: WasiCtx,
    table: ResourceTable,
    http: WasiHttpCtx,
    http_hooks: WadoHttpHooks,
    tls: WasiTlsCtx,
    /// `wado test` only: the store's handle on `core:eval`.
    eval: Option<EvalSession>,
    /// `wado test` only: the coverage regions this store reported.
    coverage: CoverageHits,
    /// The fuel [`new_store`] filled the store with, where its engine meters.
    fuel_start: Option<u64>,
}

/// Per-guest stdout/stderr capacity for [`WasiState::new_capturing_stdio`].
/// Overflow traps the guest (see that constructor's doc) rather than
/// silently truncating, so this just needs to be generous enough that
/// realistic test output (assertion diagnostics, a benchmark's own
/// printed stats) never gets anywhere near it.
pub(crate) const CAPTURED_STDIO_CAPACITY: usize = 1024 * 1024;

/// Selects where a [`WasiState`]'s stdout/stderr go.
enum Stdio {
    /// Pass through to the host's real stdout/stderr.
    Inherit,
    /// Redirect into in-memory pipes a caller reads back later.
    Captured {
        stdout: MemoryOutputPipe,
        stderr: MemoryOutputPipe,
    },
}

impl WasiState {
    /// Create a new WASI state with preopened directories and program arguments.
    /// `preopened_dirs`: `(host_path, guest_path)` pairs.
    /// `args`: what `wasi:cli/environment.get-arguments` answers, the program name first.
    ///
    /// Inherits the host's environment so guest CLI/test programs can read
    /// `PATH`, `HOME`, etc. — appropriate for `wado run` and `wado test`,
    /// where the user is launching the guest from their own shell. Long-
    /// running services should use
    /// [`Self::new_no_inherit_env_with_preopens`] instead so secrets from
    /// the server's environment are not exposed to handlers.
    ///
    /// # Errors
    ///
    /// Returns an error if a preopened directory cannot be opened.
    pub fn new(preopened_dirs: &[(String, String)], args: &[String]) -> Result<Self> {
        let preopens = Preopens::open(preopened_dirs)?;
        Ok(Self::build(&preopens, args, true, Stdio::Inherit, None))
    }

    /// Like [`Self::new`], but captures the guest's stdout/stderr into
    /// in-memory pipes instead of inheriting the host's, so a caller can
    /// read back what the guest printed after it exits. Used by `wado
    /// test` to attribute a test's own output to its result instead of
    /// letting it race directly onto the host's real stdout/stderr
    /// alongside every other concurrently-running test.
    ///
    /// Capacity-bounded (`CAPTURED_STDIO_CAPACITY`): a guest that writes
    /// past the cap traps rather than blocking, so a runaway test can't
    /// deadlock the caller, who only reads the pipes back after the
    /// guest's `Store` is done with them.
    ///
    /// `eval` is the test's handle on `core:eval`, which only a test links.
    ///
    /// # Errors
    ///
    /// Returns an error if a preopened directory cannot be opened.
    pub fn new_capturing_stdio(
        preopened_dirs: &[(String, String)],
        args: &[String],
        eval: EvalSession,
    ) -> Result<(Self, MemoryOutputPipe, MemoryOutputPipe)> {
        let stdout = MemoryOutputPipe::new(CAPTURED_STDIO_CAPACITY);
        let stderr = MemoryOutputPipe::new(CAPTURED_STDIO_CAPACITY);
        let state = Self::build(
            &Preopens::open(preopened_dirs)?,
            args,
            true,
            Stdio::Captured {
                stdout: stdout.clone(),
                stderr: stderr.clone(),
            },
            Some(eval),
        );
        Ok((state, stdout, stderr))
    }

    /// Like [`Self::new`], but reuses pre-opened directories from
    /// [`Preopens`] and does NOT inherit the host process's environment.
    /// Used by `wado serve`, which builds a `WasiState` for each worker
    /// instance and again whenever it recycles one: the preopens are opened
    /// once at server startup, then attached to every state via this
    /// constructor — avoiding one `openat` syscall per preopen per state.
    ///
    /// Skipping `inherit_env` matches the policy expected of long-running
    /// services: an HTTP server's env typically holds secrets (DB
    /// credentials, API tokens) that must not leak into handler components.
    pub fn new_no_inherit_env_with_preopens(preopens: &Preopens, args: &[String]) -> Self {
        Self::build(preopens, args, false, Stdio::Inherit, None)
    }

    /// The one place that decides what a guest reaches.
    fn build(
        preopens: &Preopens,
        args: &[String],
        inherit_env: bool,
        stdio: Stdio,
        eval: Option<EvalSession>,
    ) -> Self {
        let mut builder = WasiCtx::builder();
        match stdio {
            Stdio::Inherit => {
                builder.inherit_stdio();
            }
            Stdio::Captured { stdout, stderr } => {
                builder.stdout(stdout);
                builder.stderr(stderr);
            }
        }
        if inherit_env {
            builder.inherit_env();
        }
        builder.args(args);
        grant_host_network(&mut builder);
        let mut ctx = builder.build();
        // The clone shares the open directories: each preopen's
        // `cap_std::fs::Dir` sits behind an `Arc`.
        *ctx.filesystem() = preopens.0.clone();
        Self {
            ctx,
            table: ResourceTable::new(),
            http: WasiHttpCtx::new(),
            http_hooks: WadoHttpHooks::new(),
            tls: build_wasi_tls_ctx(),
            eval,
            coverage: CoverageHits::default(),
            fuel_start: None,
        }
    }

    /// The coverage regions this store reported.
    pub fn coverage_hits(&mut self) -> &mut CoverageHits {
        &mut self.coverage
    }

    /// The test's handle on `core:eval`.
    pub fn eval_session(&mut self) -> &mut EvalSession {
        self.eval
            .as_mut()
            .expect("only a test links core:eval, and every test store carries a session")
    }
}

/// Pre-opened directories that can be reused across many `WasiState`s.
///
/// `WasiCtxBuilder::preopened_dir` opens the host path with one `openat`
/// syscall per call. `wado serve` builds a `WasiState` per worker instance and
/// again on every recycle, so it opens a `Preopens` once at startup and
/// attaches it to each state via
/// [`WasiState::new_no_inherit_env_with_preopens`].
///
/// The underlying `cap_std::fs::Dir` handles are reference-counted inside
/// wasmtime's `Dir` (`Arc<cap_std::fs::Dir>`), so cloning a `Preopens`
/// only bumps refcounts.
pub struct Preopens(WasiFilesystemCtx);

impl Preopens {
    /// Open the given `(host_path, guest_path)` pairs once.
    ///
    /// # Errors
    ///
    /// Returns an error if any host path cannot be opened.
    pub fn open(preopened_dirs: &[(String, String)]) -> Result<Self> {
        let mut builder = WasiCtx::builder();
        for (host_path, guest_path) in preopened_dirs {
            builder.preopened_dir(host_path, guest_path, FsPerms::ReadWrite)?;
        }
        let mut ctx = builder.build();
        Ok(Self(ctx.filesystem().clone()))
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
            hooks: &mut self.http_hooks,
        }
    }
}

impl WasiTlsView for WasiState {
    fn tls(&mut self) -> WasiTlsCtxView<'_> {
        WasiTlsCtxView {
            ctx: &mut self.tls,
            table: &mut self.table,
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

/// `profiler`'s samples written to `file`. Flushed here: a `BufWriter` dropped
/// with its last block unwritten reports no error, so a profile that never
/// reached the disk would read as written.
pub fn write_guest_profile(profiler: GuestProfiler, file: File) -> Result<()> {
    let mut out = BufWriter::new(file);
    profiler.finish(&mut out)?;
    out.flush()?;
    Ok(())
}

/// Parse a `--profile` argument value. Shared by `run` and `serve`.
///
/// # Errors
///
/// Returns an error if the mode name or the interval is unparseable.
pub fn parse_profile(s: &str) -> Result<ProfileMode, CliExit> {
    if s == "jitdump" {
        return Ok(ProfileMode::JitDump);
    }
    if s == "perfmap" {
        return Ok(ProfileMode::PerfMap);
    }
    if s == "guest" {
        return Ok(ProfileMode::Guest {
            path: "profile.json".to_owned(),
            interval_ms: 10,
        });
    }
    if let Some(rest) = s.strip_prefix("guest,") {
        let parts: Vec<&str> = rest.splitn(2, ',').collect();
        let path = if parts[0].is_empty() {
            "profile.json".to_owned()
        } else {
            parts[0].to_owned()
        };
        let interval_ms = if parts.len() > 1 {
            parts[1]
                .parse::<u64>()
                .map_err(|_| CliExit::error(format!("invalid profiling interval '{}'", parts[1])))?
        } else {
            10
        };
        return Ok(ProfileMode::Guest { path, interval_ms });
    }
    Err(CliExit::error(format!(
        "unknown profile mode '{s}'. Use guest, jitdump, or perfmap"
    )))
}

/// Parse a `--collector` argument value into a wasmtime [`Collector`].
///
/// # Errors
///
/// Returns an error message if the collector name is unrecognized.
pub fn parse_collector(s: &str) -> Result<Collector, String> {
    match s {
        "copying" => Ok(Collector::Copying),
        "drc" => Ok(Collector::DeferredReferenceCounting),
        "null" => Ok(Collector::Null),
        other => Err(format!(
            "unknown collector '{other}'. Use copying, drc, or null"
        )),
    }
}

/// Parse a `--gc-heap-initial` value: a byte count, or one suffixed `k`, `m`
/// or `g` for binary multiples.
///
/// # Errors
///
/// Returns an error message if the value is not a size.
pub fn parse_gc_heap_size(s: &str) -> Result<u64, String> {
    let lower = s.trim().to_ascii_lowercase();
    let suffixed = |shift: u32| (&lower[..lower.len() - 1], 1u64 << shift);
    let (digits, multiple) = match lower.as_bytes().last() {
        Some(b'k') => suffixed(10),
        Some(b'm') => suffixed(20),
        Some(b'g') => suffixed(30),
        _ => (lower.as_str(), 1),
    };
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("invalid GC heap size '{s}'. Use bytes, or a k / m / g suffix"))?;
    value
        .checked_mul(multiple)
        .ok_or_else(|| format!("GC heap size '{s}' does not fit in 64 bits"))
}

/// [`default_config`] under the runtime knobs and profiling mode a run chose.
#[must_use]
pub fn create_config(opt_level: OptLevel, profile: &ProfileMode, knobs: RuntimeKnobs) -> Config {
    let mut config = default_config(opt_level);
    config.collector(knobs.collector);
    config.gc_heap_initial_size(knobs.gc_heap_initial_size);
    config.consume_fuel(knobs.report_fuel);

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
pub fn create_engine(
    opt_level: OptLevel,
    profile: &ProfileMode,
    knobs: RuntimeKnobs,
) -> Result<Engine> {
    Ok(Engine::new(&create_config(opt_level, profile, knobs))?)
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
    knobs: RuntimeKnobs,
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
    knobs: RuntimeKnobs,
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

/// A store over `state`. On a metering engine it holds fuel it can never
/// exhaust, so `--report-fuel` observes the guest without stopping it.
#[must_use]
pub fn new_store(engine: &Engine, state: WasiState) -> Store<WasiState> {
    let mut store = Store::new(engine, state);
    if engine.get_consume_fuel() {
        store.set_fuel(u64::MAX).expect("the engine meters fuel");
        store.data_mut().fuel_start = Some(u64::MAX);
    }
    store
}

/// The fuel `store` has spent since [`new_store`] filled it, or `None` when
/// its engine does not meter.
pub fn fuel_spent(store: impl AsContext<Data = WasiState>) -> Option<u64> {
    let store = store.as_context();
    let start = store.data().fuel_start?;
    Some(start - store.get_fuel().expect("the engine meters fuel"))
}

/// Create a Store with WASI state, preopened directories, and program arguments.
/// `preopened_dirs`: `(host_path, guest_path)` pairs.
/// `args`: what `wasi:cli/environment.get-arguments` answers, the program name first.
///
/// # Errors
///
/// Returns an error if a preopened directory cannot be opened.
pub fn create_store(
    engine: &Engine,
    preopened_dirs: &[(String, String)],
    args: &[String],
) -> Result<Store<WasiState>> {
    Ok(new_store(engine, WasiState::new(preopened_dirs, args)?))
}

/// Like [`create_store`], but the guest's stdout/stderr are captured into
/// in-memory pipes instead of inherited — used by `wado test` so a
/// test's own prints can be attributed to its result instead of racing
/// directly onto the host's real stdout/stderr.
///
/// # Errors
///
/// Returns an error if a preopened directory cannot be opened.
pub fn create_test_store(
    engine: &Engine,
    preopened_dirs: &[(String, String)],
    program: &str,
    eval: EvalSession,
) -> Result<(Store<WasiState>, MemoryOutputPipe, MemoryOutputPipe)> {
    let (state, stdout, stderr) =
        WasiState::new_capturing_stdio(preopened_dirs, &[program.to_owned()], eval)?;
    Ok((new_store(engine, state), stdout, stderr))
}

/// Create a Linker for `component`: WASI P3, HTTP and TLS, and a trap for every
/// other import.
///
/// # Errors
///
/// Returns an error if WASI bindings cannot be added to the linker, or if an
/// import is a module or a component.
pub fn create_linker(component: &Component) -> Result<Linker<WasiState>> {
    let mut linker = host_linker(component)?;
    linker.define_unknown_imports_as_traps(component)?;
    Ok(linker)
}

/// Like [`create_linker`], plus `core:eval` and `core:coverage`, which only a
/// test may reach.
///
/// # Errors
///
/// Returns an error if a binding cannot be added to the linker, or if an import
/// is a module or a component.
pub fn create_test_linker(component: &Component) -> Result<Linker<WasiState>> {
    let mut linker = host_linker(component)?;
    add_eval_to_linker(&mut linker, WasiState::eval_session)?;
    add_coverage_to_linker(&mut linker, WasiState::coverage_hits)?;
    linker.define_unknown_imports_as_traps(component)?;
    Ok(linker)
}

fn host_linker(component: &Component) -> Result<Linker<WasiState>> {
    let mut linker: Linker<WasiState> = Linker::new(component.engine());
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_tls::p3::add_to_linker(&mut linker)?;
    add_to_linker(&mut linker)?;
    Ok(linker)
}

/// An OS thread calling `tick` every `interval` until the ticker drops.
/// Dropping wakes the thread, so it never waits out the interval in flight.
pub struct Ticker {
    stop: Arc<(Mutex<bool>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl Ticker {
    pub fn start(interval: Duration, mut tick: impl FnMut() + Send + 'static) -> Self {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let handle = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                let (stopped, wake) = &*stop;
                let mut stopped = lock(stopped);
                while !*stopped {
                    let (guard, wait) = wake
                        .wait_timeout_while(stopped, interval, |stopped| !*stopped)
                        .unwrap_or_else(PoisonError::into_inner);
                    stopped = guard;
                    if wait.timed_out() {
                        tick();
                    }
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        let (stopped, wake) = &*self.stop;
        *lock(stopped) = true;
        wake.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn dropping_a_ticker_does_not_wait_out_its_interval() {
        let ticker = Ticker::start(Duration::from_secs(60), || {});
        // Dropped before its thread first runs, a ticker stops at the loop
        // head and the wait is never exercised.
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        drop(ticker);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
