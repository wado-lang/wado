//! What a guest reaches, whichever command hosts it: WASI, `wasi:http`,
//! `wasi:tls`, `wasi:clocks/timezone` and `core:process`, under the runtime
//! knobs of the command line. A host adds what it alone serves by embedding a
//! [`GuestState`] in its store data and naming that field to [`guest_views`].

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use anyhow::Result;
use wasmtime::component::{Linker, ResourceTable};
use wasmtime::{AsContext, Config, Engine, OptLevel, ProfilingStrategy, Store};
use wasmtime_wasi::filesystem::WasiFilesystemCtx;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder};
pub use wasmtime_wasi::{WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
pub use wasmtime_wasi_http::{WasiHttpCtxView, WasiHttpView};
use wasmtime_wasi_tls::{
    Error as WasiTlsError, TlsProvider, TlsStream, TlsTransport, WasiTlsCtx, WasiTlsCtxBuilder,
};
pub use wasmtime_wasi_tls::{WasiTlsCtxView, WasiTlsView};

use crate::engine::{default_config, guest_store};
use crate::fuel::meter_fuel;
use crate::http_hooks::WadoHttpHooks;
use crate::options::{ProfileMode, RuntimeKnobs};
use crate::process_host::{
    GuestStdio, ProcessCtx, ProcessGrant, ProcessView, Spawning,
    add_to_linker as add_process_to_linker,
};
use crate::random;
use crate::timezone::add_to_linker as add_timezone_to_linker;
use crate::tls_trust::{build_root_cert_store, install_default_crypto_provider};

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

/// Per-guest stdout/stderr capacity for [`GuestState::new_capturing_stdio`].
/// Overflow traps the guest (see that constructor's doc) rather than
/// silently truncating, so this just needs to be generous enough that
/// realistic test output (assertion diagnostics, a benchmark's own
/// printed stats) never gets anywhere near it.
pub const CAPTURED_STDIO_CAPACITY: usize = 1024 * 1024;

/// Selects where a [`GuestState`]'s stdout/stderr go.
enum Stdio {
    /// Pass through to the host's real stdout/stderr.
    Inherit,
    /// Redirect into in-memory pipes a caller reads back later.
    Captured {
        stdout: MemoryOutputPipe,
        stderr: MemoryOutputPipe,
    },
}

/// What a [`GuestState`]'s `wasi:random/insecure` and `insecure-seed` answer.
enum InsecureRandom {
    /// The host's randomness. `insecure-seed` seeds a hash map's protection
    /// against denial of service, so a guest serving untrusted input needs it
    /// unpredictable.
    Host,
    /// Zeros, so a test's outcome, and what its run measures (fuel, coverage, a
    /// profile), is a function of the program: `HashMap::new` hashes under
    /// `insecure-seed`, and a seed changes how far its probes go.
    Zeroed,
}

/// The store data every guest has.
pub struct GuestState {
    ctx: WasiCtx,
    /// Whatever a host adds shares this table with WASI's resources.
    pub table: ResourceTable,
    http: WasiHttpCtx,
    http_hooks: WadoHttpHooks,
    tls: WasiTlsCtx,
    process: ProcessCtx,
    /// The fuel [`new_store`] filled the store with, where its engine meters.
    fuel_start: Option<u64>,
}

impl GuestState {
    /// Create a new guest with preopened directories and program arguments.
    /// `preopened_dirs`: `(host_path, guest_path)` pairs.
    /// `args`: what `wasi:cli/environment.get-arguments` answers, the program name first.
    ///
    /// Inherits the host's environment so guest CLI/test programs can read
    /// `PATH`, `HOME`, etc. — appropriate for `wado run`, `wado test` and the
    /// runners, where the user is launching the guest from their own shell.
    /// Long-running services should use
    /// [`Self::new_no_inherit_env_with_preopens`] instead so secrets from
    /// the server's environment are not exposed to handlers.
    ///
    /// # Errors
    ///
    /// Returns an error if a preopened directory cannot be opened.
    pub fn new(
        preopened_dirs: &[(String, String)],
        args: &[String],
        process: ProcessGrant,
    ) -> Result<Self> {
        let preopens = Preopens::open(preopened_dirs, process)?;
        Ok(Self::build(
            &preopens,
            args,
            true,
            Stdio::Inherit,
            InsecureRandom::Host,
        ))
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
    /// A test serves no untrusted input, so its insecure randomness is
    /// [`InsecureRandom::Zeroed`].
    ///
    /// # Errors
    ///
    /// Returns an error if a preopened directory cannot be opened.
    pub fn new_capturing_stdio(
        preopened_dirs: &[(String, String)],
        args: &[String],
        process: ProcessGrant,
    ) -> Result<(Self, MemoryOutputPipe, MemoryOutputPipe)> {
        let stdout = MemoryOutputPipe::new(CAPTURED_STDIO_CAPACITY);
        let stderr = MemoryOutputPipe::new(CAPTURED_STDIO_CAPACITY);
        let state = Self::build(
            &Preopens::open(preopened_dirs, process)?,
            args,
            true,
            Stdio::Captured {
                stdout: stdout.clone(),
                stderr: stderr.clone(),
            },
            InsecureRandom::Zeroed,
        );
        Ok((state, stdout, stderr))
    }

    /// Like [`Self::new`], but reuses pre-opened directories from
    /// [`Preopens`] and does NOT inherit the host process's environment.
    /// Used by `wado serve`, which builds a state for each worker instance and
    /// again whenever it recycles one: the preopens are opened once at server
    /// startup, then attached to every state via this constructor — avoiding
    /// one `openat` syscall per preopen per state.
    ///
    /// Skipping `inherit_env` matches the policy expected of long-running
    /// services: an HTTP server's env typically holds secrets (DB
    /// credentials, API tokens) that must not leak into handler components.
    #[must_use]
    pub fn new_no_inherit_env_with_preopens(preopens: &Preopens, args: &[String]) -> Self {
        Self::build(preopens, args, false, Stdio::Inherit, InsecureRandom::Host)
    }

    /// The one place that decides what a guest reaches.
    fn build(
        preopens: &Preopens,
        args: &[String],
        inherit_env: bool,
        stdio: Stdio,
        insecure_random: InsecureRandom,
    ) -> Self {
        let mut builder = WasiCtx::builder();
        match insecure_random {
            InsecureRandom::Host => {}
            InsecureRandom::Zeroed => {
                random::zeroed(&mut builder);
            }
        }
        let guest_stdio = match stdio {
            Stdio::Inherit => {
                builder.inherit_stdio();
                GuestStdio::Host
            }
            Stdio::Captured { stdout, stderr } => {
                builder.stdout(stdout.clone());
                builder.stderr(stderr.clone());
                GuestStdio::Captured { stdout, stderr }
            }
        };
        if inherit_env {
            builder.inherit_env();
        }
        builder.args(args);
        grant_host_network(&mut builder);
        let mut ctx = builder.build();
        *ctx.filesystem() = preopens.filesystem.clone();
        Self {
            ctx,
            table: ResourceTable::new(),
            http: WasiHttpCtx::new(),
            http_hooks: WadoHttpHooks::new(),
            tls: build_wasi_tls_ctx(),
            process: ProcessCtx::new(preopens.spawning.clone(), guest_stdio),
            fuel_start: None,
        }
    }

    /// The store's handle on `core:process`.
    pub fn process_view(&mut self) -> ProcessView<'_> {
        ProcessView {
            ctx: &mut self.process,
            table: &mut self.table,
        }
    }
}

/// Pre-opened directories that can be reused across many [`GuestState`]s.
///
/// `WasiCtxBuilder::preopened_dir` opens the host path with one `openat`
/// syscall per call. `wado serve` builds a state per worker instance and
/// again on every recycle, so it opens a `Preopens` once at startup and
/// attaches it to each state via
/// [`GuestState::new_no_inherit_env_with_preopens`].
///
/// The underlying `cap_std::fs::Dir` handles are reference-counted inside
/// wasmtime's `Dir` (`Arc<cap_std::fs::Dir>`), so cloning a `Preopens`
/// only bumps refcounts.
pub struct Preopens {
    filesystem: WasiFilesystemCtx,
    /// Whether a guest granted these directories may spawn.
    spawning: Spawning,
}

impl Preopens {
    /// Open the given `(host_path, guest_path)` pairs once.
    ///
    /// # Errors
    ///
    /// Returns an error if any host path cannot be opened.
    pub fn open(preopened_dirs: &[(String, String)], process: ProcessGrant) -> Result<Self> {
        let mut builder = WasiCtx::builder();
        for (host_path, guest_path) in preopened_dirs {
            builder.preopened_dir(host_path, guest_path, FsPerms::ReadWrite)?;
        }
        let mut ctx = builder.build();
        Ok(Self {
            filesystem: ctx.filesystem().clone(),
            spawning: Spawning::new(preopened_dirs, process)?,
        })
    }
}

impl WasiView for GuestState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for GuestState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

impl WasiTlsView for GuestState {
    fn tls(&mut self) -> WasiTlsCtxView<'_> {
        WasiTlsCtxView {
            ctx: &mut self.tls,
            table: &mut self.table,
        }
    }
}

/// Store data that holds a [`GuestState`]. [`guest_views`] implements it, and
/// the views the WASI bindings ask of the store data along with it.
pub trait Guest: WasiView + WasiHttpView + WasiTlsView + Send + 'static {
    fn guest(&self) -> &GuestState;
    fn guest_mut(&mut self) -> &mut GuestState;
}

impl Guest for GuestState {
    fn guest(&self) -> &GuestState {
        self
    }

    fn guest_mut(&mut self) -> &mut GuestState {
        self
    }
}

/// Implements [`Guest`] and its views for `$host`, through its [`GuestState`]
/// `$field`. A macro since each view is a foreign trait, which only the crate
/// declaring `$host` may implement for it.
#[macro_export]
macro_rules! guest_views {
    ($host:ty, $field:ident) => {
        impl $crate::guest::WasiView for $host {
            fn ctx(&mut self) -> $crate::guest::WasiCtxView<'_> {
                $crate::guest::WasiView::ctx(&mut self.$field)
            }
        }

        impl $crate::guest::WasiHttpView for $host {
            fn http(&mut self) -> $crate::guest::WasiHttpCtxView<'_> {
                $crate::guest::WasiHttpView::http(&mut self.$field)
            }
        }

        impl $crate::guest::WasiTlsView for $host {
            fn tls(&mut self) -> $crate::guest::WasiTlsCtxView<'_> {
                $crate::guest::WasiTlsView::tls(&mut self.$field)
            }
        }

        impl $crate::guest::Guest for $host {
            fn guest(&self) -> &$crate::guest::GuestState {
                &self.$field
            }

            fn guest_mut(&mut self) -> &mut $crate::guest::GuestState {
                &mut self.$field
            }
        }
    };
}

/// [`default_config`] under the runtime knobs and profiling mode a run chose.
#[must_use]
pub fn create_config(opt_level: OptLevel, profile: &ProfileMode, knobs: &RuntimeKnobs) -> Config {
    let mut config = default_config(opt_level);
    config.collector(knobs.collector);
    config.gc_heap_initial_size(knobs.gc_heap_initial_size);
    if knobs.report_fuel {
        meter_fuel(&mut config);
    }

    match profile {
        ProfileMode::None => {}
        ProfileMode::JitDump => {
            config.profiler(ProfilingStrategy::JitDump);
        }
        ProfileMode::PerfMap => {
            config.profiler(ProfilingStrategy::PerfMap);
        }
        ProfileMode::Guest(_) => {
            config.epoch_interruption(true);
        }
    }

    config
}

/// A store over `state`. On a metering engine it holds fuel it can never
/// exhaust, so `--report-fuel` observes the guest without stopping it.
#[must_use]
pub fn new_store<T: Guest>(engine: &Engine, state: T) -> Store<T> {
    let mut store = guest_store(engine, state);
    if engine.get_consume_fuel() {
        store.set_fuel(u64::MAX).expect("the engine meters fuel");
        store.data_mut().guest_mut().fuel_start = Some(u64::MAX);
    }
    store
}

/// The fuel `store` has spent since [`new_store`] filled it, or `None` when
/// its engine does not meter.
pub fn fuel_spent<T: Guest>(store: impl AsContext<Data = T>) -> Option<u64> {
    let store = store.as_context();
    let start = store.data().guest().fuel_start?;
    Some(start - store.get_fuel().expect("the engine meters fuel"))
}

fn process_view<T: Guest>(state: &mut T) -> ProcessView<'_> {
    state.guest_mut().process_view()
}

/// A linker holding what every guest may reach.
///
/// # Errors
///
/// Returns an error if a binding cannot be added to the linker.
pub fn host_linker<T: Guest>(engine: &Engine) -> Result<Linker<T>> {
    let mut linker: Linker<T> = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_tls::p3::add_to_linker(&mut linker)?;
    add_timezone_to_linker(&mut linker)?;
    add_process_to_linker(&mut linker, process_view::<T>)?;
    Ok(linker)
}
