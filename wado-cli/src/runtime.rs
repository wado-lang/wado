use anyhow::Result;
use wasmtime::component::{Linker, ResourceTable};
use wasmtime::{Config, Engine, OptLevel, ProfilingStrategy, Store};
use wasmtime_wasi::filesystem::WasiFilesystemCtx;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p3::{WasiHttpCtxView, WasiHttpView};
use wasmtime_wasi_tls::{WasiTlsCtx, WasiTlsCtxBuilder, WasiTlsCtxView, WasiTlsView};

use crate::http_hooks::WadoHttpHooks;

/// Install the rustls process-level `CryptoProvider` exactly once. The
/// workspace pulls in multiple rustls feature combinations through wasmtime's
/// dependency graph, so the auto-detect path used by `WasiTlsCtxBuilder::new`
/// would otherwise panic with "could not automatically determine the
/// process-level `CryptoProvider`".
fn install_rustls_provider() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        // Ignore the result: another caller may have raced us, in which case
        // a provider is already in place and that is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub struct WasiState {
    ctx: WasiCtx,
    table: ResourceTable,
    http: WasiHttpCtx,
    http_hooks: WadoHttpHooks,
    tls: WasiTlsCtx,
}

impl WasiState {
    /// Create a new WASI state with preopened directories and program arguments.
    /// `preopened_dirs`: `(host_path, guest_path)` pairs.
    /// `args`: arguments passed to the guest program via `wasi:cli/environment.get-arguments`.
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
        Self::build(preopened_dirs, args, true)
    }

    /// Like [`Self::new`], but reuses pre-opened directories from
    /// [`Preopens`] and does NOT inherit the host process's environment.
    /// Used by `wado serve`, which builds a fresh `WasiState` per request:
    /// the preopens are opened once at server startup, then attached to
    /// every per-request state via this constructor — avoiding one `openat`
    /// syscall per preopen per request.
    ///
    /// Skipping `inherit_env` matches the policy expected of long-running
    /// services: an HTTP server's env typically holds secrets (DB
    /// credentials, API tokens) that must not leak into per-request handler
    /// components.
    pub fn new_no_inherit_env_with_preopens(preopens: &Preopens, args: &[String]) -> Self {
        let mut builder = WasiCtx::builder();
        builder.inherit_stdio();
        builder.args(args);
        let mut ctx = builder.build();
        // Replace the (empty) filesystem ctx with a clone of the cached one.
        // The clone is cheap: each preopen's underlying `cap_std::fs::Dir`
        // is held behind an `Arc`, so we share fds rather than re-opening.
        *ctx.filesystem() = preopens.0.clone();

        let table = ResourceTable::new();
        let http = WasiHttpCtx::new();
        let http_hooks = WadoHttpHooks::new();
        install_rustls_provider();
        let tls = WasiTlsCtxBuilder::new().build();
        Self {
            ctx,
            table,
            http,
            http_hooks,
            tls,
        }
    }

    fn build(
        preopened_dirs: &[(String, String)],
        args: &[String],
        inherit_env: bool,
    ) -> Result<Self> {
        let mut builder = WasiCtx::builder();
        builder.inherit_stdio();
        if inherit_env {
            builder.inherit_env();
        }
        builder.args(args);
        for (host_path, guest_path) in preopened_dirs {
            builder.preopened_dir(host_path, guest_path, DirPerms::all(), FilePerms::all())?;
        }
        // `wado run` and `wado test` are invoked directly by the developer,
        // so mirror native CLI tools by letting `wasi:sockets`/`wasi:tls`
        // reach the host network. Long-running services go through
        // [`Self::new_no_inherit_env_with_preopens`], which keeps the
        // default deny-all stance — operators opt in explicitly there.
        // Hermetic test harnesses (compiler `tests/common.rs`) build their
        // own `WasiCtx` and are unaffected.
        builder.inherit_network();
        builder.allow_ip_name_lookup(true);
        let ctx = builder.build();
        let table = ResourceTable::new();
        let http = WasiHttpCtx::new();
        let http_hooks = WadoHttpHooks::new();
        install_rustls_provider();
        let tls = WasiTlsCtxBuilder::new().build();
        Ok(Self {
            ctx,
            table,
            http,
            http_hooks,
            tls,
        })
    }
}

/// Pre-opened directories that can be reused across many `WasiState`s.
///
/// `WasiCtxBuilder::preopened_dir` opens the host path with one `openat`
/// syscall per call. For one-shot processes (`wado run`, `wado test`) that
/// is fine, but `wado serve` builds a fresh `WasiState` per request, and
/// re-opening the same dirs on every request is wasted work. A `Preopens`
/// is opened once (at server startup) and then attached to per-request
/// states via [`WasiState::new_no_inherit_env_with_preopens`].
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
            builder.preopened_dir(host_path, guest_path, DirPerms::all(), FilePerms::all())?;
        }
        // We only need the filesystem half of the WasiCtx; the rest is
        // rebuilt fresh per request. `WasiFilesystemCtx` is `Clone` (with
        // the heavy `Dir` handles refcounted internally), so taking it by
        // clone here pays the open cost once and lets every subsequent
        // request reuse the open fds.
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
    wasmtime_wasi_tls::p3::add_to_linker(&mut linker)?;
    crate::timezone_host::add_to_linker(&mut linker)?;
    Ok(linker)
}
