//! Common test utilities shared across test files
//!
//! This module provides shared utilities for:
//! - Compiler host implementations (filesystem and in-memory)
//! - Wasmtime engine configuration
//! - WASI context setup
//! - Test fixture parsing (__DATA__ sections)
//! - Tokio runtime management

// Each test file includes this module but only uses a subset of functions.
#![allow(dead_code)]

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use wasmtime::component::{Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p3::{WasiHttpCtxView, WasiHttpHooks, WasiHttpView};
use wasmtime_wasi_tls::{
    Error as WasiTlsError, TlsProvider, TlsStream, TlsTransport, WasiTlsCtx, WasiTlsCtxBuilder,
    WasiTlsCtxView, WasiTlsView,
};

/// Install the rustls process-level `CryptoProvider` exactly once. See the
/// matching helper in `wado-cli/src/runtime.rs` for the rationale; the test
/// harness needs the same setup so `WasiTlsCtxBuilder::new()` does not panic
/// on the rustls auto-detect path.
pub fn install_rustls_provider_for_tests() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

use wado_compiler::{
    CompileError, CompileFailure, CompilerHost, Diagnostic, OptLevel, SourceError,
};

/// A filesystem-based `CompilerHost` for tests that need to load files
pub struct FilesystemHost {
    base_path: PathBuf,
    diagnostics: Mutex<Vec<Diagnostic>>,
}

impl FilesystemHost {
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            diagnostics: Mutex::new(Vec::new()),
        }
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.lock().unwrap().clone()
    }
}

impl CompilerHost for FilesystemHost {
    fn load_source(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, SourceError>> + Send {
        let full_path = self.base_path.join(path);
        async move {
            std::fs::read(&full_path).map_err(|e| SourceError::IoError {
                path: full_path.to_string_lossy().to_string(),
                message: e.to_string(),
            })
        }
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic);
    }
}

/// An in-memory `CompilerHost` for tests that don't need file loading
pub struct InMemoryHost {
    diagnostics: Mutex<Vec<Diagnostic>>,
}

impl InMemoryHost {
    pub fn new() -> Self {
        Self {
            diagnostics: Mutex::new(Vec::new()),
        }
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.lock().unwrap().clone()
    }
}

impl Default for InMemoryHost {
    fn default() -> Self {
        Self::new()
    }
}

impl CompilerHost for InMemoryHost {
    fn load_source(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, SourceError>> + Send {
        let path = path.to_string();
        async move { Err(SourceError::NotFound { path }) }
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic);
    }
}

/// Convert a `Bail` + host diagnostics into a `CompileError` for test backward compat
pub fn bail_to_compile_error(diagnostics: &[Diagnostic], filename: Option<&str>) -> CompileError {
    use wado_compiler::{Code, Severity};

    // Filter to only error/fatal diagnostics (skip span tracking and debug messages)
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| matches!(d.severity, Severity::Error | Severity::Fatal))
        .collect();

    if let Some(d) = errors.first() {
        // Prefer the diagnostic's span file (the actual error location) over
        // the passed-in filename (which is the entry module).
        let fname = d
            .span
            .as_ref()
            .and_then(|s| {
                if s.file.is_empty() {
                    None
                } else {
                    Some(s.file.clone())
                }
            })
            .or_else(|| filename.map(String::from));
        let code = d.code;
        let message = d.message.clone();
        let line = d.span.as_ref().map_or(0, |s| s.line);
        let column = d.span.as_ref().map_or(0, |s| s.column);

        // Map diagnostic code to CompileError variant
        if code == Code::InvalidSyntax && message.starts_with("lexer error:") {
            CompileError::Lexer {
                message: message
                    .strip_prefix("lexer error: ")
                    .unwrap_or(&message)
                    .to_string(),
                line,
                column,
                filename: fname,
            }
        } else if code == Code::InvalidSyntax && message.starts_with("parse error:") {
            CompileError::Parser {
                message: message
                    .strip_prefix("parse error: ")
                    .unwrap_or(&message)
                    .to_string(),
                line,
                column,
                filename: fname,
                is_todo_module: false,
            }
        } else {
            // All other errors
            let all_messages: Vec<String> = errors.iter().map(|d| d.message.clone()).collect();
            CompileError::Analyzer {
                message: all_messages.join("; "),
                line,
                column,
                filename: fname,
            }
        }
    } else {
        CompileError::Analyzer {
            message: "compilation failed".to_string(),
            line: 0,
            column: 0,
            filename: filename.map(String::from),
        }
    }
}

/// Shared tokio runtime for all tests (initialized once)
static TOKIO_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Get or initialize the shared tokio runtime
pub fn runtime() -> &'static tokio::runtime::Runtime {
    TOKIO_RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime")
    })
}

/// Create a new single-threaded tokio runtime (for tests that need isolation)
pub fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime")
}

/// Shared wasmtime Engine for all tests (initialized once)
static ENGINE: OnceLock<Engine> = OnceLock::new();

/// Default timeout for e2e test execution (5 seconds).
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;

/// Epoch tick interval for wasmtime's epoch-based interruption.
/// Coarser interval (1s) reduces thread wake-ups; up to 1s of overshoot is acceptable
/// since timeouts only fire on runaway tests.
const EPOCH_INTERVAL_MS: u64 = 1000;

/// Get or initialize the shared wasmtime Engine for all tests.
/// The engine has epoch interruption enabled; a background thread increments
/// the epoch every `EPOCH_INTERVAL_MS` to enforce test timeouts.
pub fn engine() -> &'static Engine {
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.wasm_component_model_gc(true);
        config.wasm_component_model_async(true);
        config.wasm_component_model_more_async_builtins(true);
        config.wasm_component_model_async_stackful(true);
        config.wasm_component_model_error_context(true);
        config.wasm_wide_arithmetic(true);
        config.wasm_gc(true);
        config.wasm_function_references(true);
        // Honor the `metadata.code.branch_hint` custom section the compiler
        // emits for `builtin::likely`/`builtin::unlikely`, matching the wado
        // CLI runtime so the e2e suite validates the hints it produces.
        config.wasm_branch_hinting(true);
        // Match the wado CLI's default collector so the e2e suite exercises
        // the collector users actually get (see `runtime::DEFAULT_COLLECTOR`).
        config.collector(wasmtime::Collector::Copying);
        config.wasm_backtrace_details(wasmtime::WasmBacktraceDetails::Enable);
        // Use minimal optimization for faster compilation in tests
        config.cranelift_opt_level(wasmtime::OptLevel::None);
        // Enable epoch-based interruption for timeout enforcement
        config.epoch_interruption(true);
        let engine = Engine::new(&config).expect("Failed to create wasmtime Engine");

        // Start background epoch ticker thread
        let engine_clone = engine.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(EPOCH_INTERVAL_MS));
                engine_clone.increment_epoch();
            }
        });

        engine
    })
}

/// Backward-compat aliases
pub fn cli_engine() -> &'static Engine {
    engine()
}

pub fn http_engine() -> &'static Engine {
    engine()
}

/// Mock spec for a single outgoing HTTP response
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OutgoingMockResponse {
    /// HTTP status code (default: 200)
    #[serde(default = "default_status_200")]
    pub status: u16,

    /// Response body as UTF-8 string (default: empty)
    #[serde(default)]
    pub body: String,

    /// Response headers as `[[name, value], ...]`
    #[serde(default)]
    pub headers: Vec<[String; 2]>,
}

fn default_status_200() -> u16 {
    200
}

impl Default for OutgoingMockResponse {
    fn default() -> Self {
        Self {
            status: 200,
            body: String::new(),
            headers: Vec::new(),
        }
    }
}

/// HTTP context for tests that supports outgoing request mocking.
///
/// Mock entries are keyed by URL pattern (matched against request URI).
pub struct TestHttpCtx {
    /// Mock responses keyed by URL (exact match against full request URI)
    pub mocks: indexmap::IndexMap<String, OutgoingMockResponse>,
}

impl Default for TestHttpCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl TestHttpCtx {
    pub fn new() -> Self {
        Self {
            mocks: indexmap::IndexMap::new(),
        }
    }
}

impl WasiHttpHooks for TestHttpCtx {
    fn send_request(
        &mut self,
        request: http::Request<
            http_body_util::combinators::UnsyncBoxBody<
                bytes::Bytes,
                wasmtime_wasi_http::p3::bindings::http::types::ErrorCode,
            >,
        >,
        _options: Option<wasmtime_wasi_http::p3::RequestOptions>,
        _fut: Box<
            dyn Future<
                    Output = Result<(), wasmtime_wasi_http::p3::bindings::http::types::ErrorCode>,
                > + Send,
        >,
    ) -> Box<
        dyn Future<
                Output = Result<
                    (
                        http::Response<
                            http_body_util::combinators::UnsyncBoxBody<
                                bytes::Bytes,
                                wasmtime_wasi_http::p3::bindings::http::types::ErrorCode,
                            >,
                        >,
                        Box<
                            dyn Future<
                                    Output = Result<
                                        (),
                                        wasmtime_wasi_http::p3::bindings::http::types::ErrorCode,
                                    >,
                                > + Send,
                        >,
                    ),
                    wasmtime_wasi::TrappableError<
                        wasmtime_wasi_http::p3::bindings::http::types::ErrorCode,
                    >,
                >,
            > + Send,
    > {
        use http_body_util::BodyExt;
        use wasmtime_wasi_http::p3::bindings::http::types::ErrorCode;

        let uri = request.uri().to_string();

        // Try exact URI match first, then try matching by path only
        let mock = self
            .mocks
            .get(&uri)
            .or_else(|| {
                let path = request
                    .uri()
                    .path_and_query()
                    .map(http::uri::PathAndQuery::as_str)
                    .unwrap_or("/");
                self.mocks.get(path)
            })
            .cloned();

        match mock {
            Some(mock_resp) => {
                let mut builder = http::Response::builder().status(mock_resp.status);
                for [name, value] in &mock_resp.headers {
                    builder = builder.header(name.as_str(), value.as_str());
                }
                let body =
                    http_body_util::Full::new(bytes::Bytes::from(mock_resp.body.into_bytes()))
                        .map_err(|_: std::convert::Infallible| -> ErrorCode { unreachable!() })
                        .boxed_unsync();
                let resp = builder.body(body).unwrap();
                Box::new(async {
                    Ok((
                        resp,
                        Box::new(async { Ok(()) }) as Box<dyn Future<Output = _> + Send>,
                    ))
                })
            }
            None => Box::new(async move {
                Err(wasmtime_wasi::TrappableError::trap(wasmtime::Error::msg(
                    format!("no mock configured for outgoing HTTP request to {uri}"),
                )))
            }),
        }
    }
}

/// Mock spec for a single `wasi:tls` handshake.
///
/// Keys in `tls_mocks` are matched against the `server_name` argument the
/// guest passes to `Connector::connect(this, server_name)`.
#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct TlsMockResponse {
    /// Cleartext bytes the mock delivers to the guest's `Connector::receive`
    /// stream as if they were decrypted from the server. Read-side closes
    /// (EOFs) once exhausted. Default: empty.
    #[serde(default)]
    pub recv: String,

    /// If set, the handshake fails with this error message and the guest's
    /// `Connector::connect()` future resolves to `Err`.
    #[serde(default)]
    pub error: Option<String>,
}

/// In-memory `TlsStream` used by `MockTlsProvider`.
///
/// Reads drain `recv_buf` (canned cleartext from the spec) and EOF once empty.
/// Writes are captured into `sent` so tests could later assert on what the
/// guest sent — currently we only need the read side, but the buffer is kept
/// in case fixtures want to inspect it via a host hook in the future.
struct MockTlsStream {
    recv_buf: std::io::Cursor<Vec<u8>>,
    sent: Vec<u8>,
}

impl MockTlsStream {
    fn new(recv: Vec<u8>) -> Self {
        Self {
            recv_buf: std::io::Cursor::new(recv),
            sent: Vec::new(),
        }
    }
}

impl tokio::io::AsyncRead for MockTlsStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let remaining = self.recv_buf.get_ref().len() as u64 - self.recv_buf.position();
        if remaining == 0 {
            return std::task::Poll::Ready(Ok(())); // EOF
        }
        let want = buf.remaining().min(remaining as usize);
        let pos = self.recv_buf.position() as usize;
        let slice = &self.recv_buf.get_ref()[pos..pos + want];
        buf.put_slice(slice);
        self.recv_buf.set_position((pos + want) as u64);
        std::task::Poll::Ready(Ok(()))
    }
}

impl tokio::io::AsyncWrite for MockTlsStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.sent.extend_from_slice(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

impl TlsStream for MockTlsStream {}

/// `TlsProvider` that returns canned in-memory streams keyed by `server_name`.
///
/// Mirrors the `TestHttpCtx` convention: any handshake whose `server_name` is
/// not in the mock table fails closed with a clear error so tests can't
/// silently reach the real network.
pub struct MockTlsProvider {
    mocks: indexmap::IndexMap<String, TlsMockResponse>,
}

impl MockTlsProvider {
    pub fn new(mocks: indexmap::IndexMap<String, TlsMockResponse>) -> Self {
        Self { mocks }
    }
}

impl TlsProvider for MockTlsProvider {
    fn connect(
        &self,
        server_name: String,
        _transport: Box<dyn TlsTransport>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Box<dyn TlsStream>, WasiTlsError>> + Send>>
    {
        let mock = self.mocks.get(&server_name).cloned();
        Box::pin(async move {
            match mock {
                Some(spec) => {
                    if let Some(err) = spec.error {
                        return Err(WasiTlsError::msg(err));
                    }
                    Ok(Box::new(MockTlsStream::new(spec.recv.into_bytes())) as Box<dyn TlsStream>)
                }
                None => Err(WasiTlsError::msg(format!(
                    "no TLS mock configured for server '{server_name}'"
                ))),
            }
        })
    }
}

/// Build a `WasiTlsCtx` whose provider is the mock provider seeded with `mocks`.
/// Always installs the rustls crypto provider first (idempotent).
pub fn build_tls_ctx(mocks: indexmap::IndexMap<String, TlsMockResponse>) -> WasiTlsCtx {
    install_rustls_provider_for_tests();
    WasiTlsCtxBuilder::new()
        .provider(Box::new(MockTlsProvider::new(mocks)))
        .build()
}

/// Unified WASI state for all test worlds (CLI, test, HTTP service)
pub struct WasiState {
    pub ctx: WasiCtx,
    pub table: ResourceTable,
    pub http_ctx: WasiHttpCtx,
    pub http_hooks: TestHttpCtx,
    pub tls_ctx: WasiTlsCtx,
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
            ctx: &mut self.http_ctx,
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

impl WasiTlsView for WasiState {
    fn tls(&mut self) -> WasiTlsCtxView<'_> {
        WasiTlsCtxView {
            ctx: &mut self.tls_ctx,
            table: &mut self.table,
        }
    }
}

impl WasiState {
    /// Create a new state with captured stdout/stderr
    pub fn new_with_pipes(
        stdout: wasmtime_wasi::p2::pipe::MemoryOutputPipe,
        stderr: wasmtime_wasi::p2::pipe::MemoryOutputPipe,
    ) -> Self {
        let ctx = WasiCtxBuilder::new().stdout(stdout).stderr(stderr).build();
        Self {
            ctx,
            table: ResourceTable::new(),
            http_ctx: WasiHttpCtx::new(),
            http_hooks: TestHttpCtx::new(),
            tls_ctx: build_tls_ctx(indexmap::IndexMap::new()),
        }
    }

    /// Create a basic state (no I/O capture)
    pub fn new() -> Self {
        let ctx = WasiCtxBuilder::new().build();
        Self {
            ctx,
            table: ResourceTable::new(),
            http_ctx: WasiHttpCtx::new(),
            http_hooks: TestHttpCtx::new(),
            tls_ctx: build_tls_ctx(indexmap::IndexMap::new()),
        }
    }
}

impl Default for WasiState {
    fn default() -> Self {
        Self::new()
    }
}

/// Backward-compat alias
pub type CliWasiState = WasiState;

/// Set up a linker for all test worlds (WASI + HTTP + TLS)
pub fn linker(engine: &Engine) -> anyhow::Result<Linker<WasiState>> {
    let mut linker: Linker<WasiState> = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_tls::p3::add_to_linker(&mut linker)?;
    timezone_host::add_to_linker(&mut linker)?;
    Ok(linker)
}

/// Host implementation for `wasi:clocks/timezone`. Mirrors
/// `wado_cli::timezone_host` (we cannot depend on `wado-cli` from the
/// compiler tests because of the dependency direction).
mod timezone_host {
    use wasmtime::component::{HasData, Linker};
    use wasmtime_wasi::p3::bindings::clocks::timezone::{self, Host, Instant, LinkOptions};

    pub struct WadoTimezone;

    impl HasData for WadoTimezone {
        type Data<'a> = TimezoneCtx;
    }

    pub struct TimezoneCtx;

    impl Host for TimezoneCtx {
        fn iana_id(&mut self) -> wasmtime::Result<Option<String>> {
            Ok(iana_time_zone::get_timezone().ok())
        }

        fn utc_offset(&mut self, when: Instant) -> wasmtime::Result<Option<i64>> {
            Ok(local_offset_nanos(when.seconds))
        }

        fn to_debug_string(&mut self) -> wasmtime::Result<String> {
            Ok(match iana_time_zone::get_timezone() {
                Ok(timezone) => timezone,
                Err(err) => format!("timezone unavailable: {err}"),
            })
        }
    }

    pub fn add_to_linker<T: 'static>(linker: &mut Linker<T>) -> anyhow::Result<()> {
        let mut options = LinkOptions::default();
        options.clocks_timezone(true);
        timezone::add_to_linker::<T, WadoTimezone>(linker, &options, |_| TimezoneCtx)?;
        Ok(())
    }

    #[cfg(unix)]
    fn local_offset_nanos(secs: i64) -> Option<i64> {
        use std::mem::MaybeUninit;
        // `libc::time_t` is `i64` on 64-bit Linux/macOS but `i32` on some
        // platforms; keep the conversion fallible for portability even where
        // clippy would call it useless on the current target.
        #[allow(clippy::useless_conversion)]
        let t: libc::time_t = secs.try_into().ok()?;
        let mut tm: MaybeUninit<libc::tm> = MaybeUninit::uninit();
        let ret = unsafe { libc::localtime_r(&raw const t, tm.as_mut_ptr()) };
        if ret.is_null() {
            return None;
        }
        let tm = unsafe { tm.assume_init() };
        Some(tm.tm_gmtoff * 1_000_000_000)
    }

    #[cfg(not(unix))]
    fn local_offset_nanos(_secs: i64) -> Option<i64> {
        None
    }
}

/// Backward-compat alias
pub fn cli_linker(engine: &Engine) -> anyhow::Result<Linker<WasiState>> {
    linker(engine)
}

/// Compile source string using in-memory host
pub fn compile_source(source: &str) -> Result<wado_compiler::CompileResult, CompileError> {
    let host = InMemoryHost::new();
    runtime()
        .block_on(wado_compiler::compile_with_host(
            source,
            &host,
            None,
            OptLevel::default(),
        ))
        .map_err(|_: CompileFailure| bail_to_compile_error(&host.diagnostics(), None))
}

/// Compile a file using filesystem host
pub fn compile_file(path: &std::path::Path) -> Result<wado_compiler::CompileResult, CompileError> {
    compile_file_with_opts(path, OptLevel::default())
}

/// Compile a file with specific optimization level
pub fn compile_file_with_opts(
    path: &std::path::Path,
    opt_level: OptLevel,
) -> Result<wado_compiler::CompileResult, CompileError> {
    let source = std::fs::read_to_string(path).map_err(|e| CompileError::Io {
        path: path.to_string_lossy().to_string(),
        message: e.to_string(),
    })?;

    compile_source_with_opts(path, &source, opt_level)
}

/// Compile source code with a file path for module resolution
pub fn compile_source_with_opts(
    path: &std::path::Path,
    source: &str,
    opt_level: OptLevel,
) -> Result<wado_compiler::CompileResult, CompileError> {
    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let host = FilesystemHost::new(base_path);
    let filename = path.to_string_lossy();

    runtime()
        .block_on(wado_compiler::compile_with_host(
            source,
            &host,
            Some(&filename),
            opt_level,
        ))
        .map_err(|_: CompileFailure| bail_to_compile_error(&host.diagnostics(), Some(&filename)))
}

/// Compile source code with full compiler options (including WIR backend flag)
pub fn compile_source_with_compiler_options(
    path: &std::path::Path,
    source: &str,
    options: wado_compiler::CompilerOptions,
) -> Result<wado_compiler::CompileResult, CompileError> {
    compile_source_with_compiler_options_and_filename(path, source, options, None)
}

/// Compile source code with full compiler options and an explicit display filename.
///
/// When `display_filename` is provided, it is used as the filename passed to the compiler
/// (for `#file` and assertion messages) instead of `path`. The `path` is still used for
/// module resolution (its parent directory becomes the filesystem host's base path).
pub fn compile_source_with_compiler_options_and_filename(
    path: &std::path::Path,
    source: &str,
    options: wado_compiler::CompilerOptions,
    display_filename: Option<&str>,
) -> Result<wado_compiler::CompileResult, CompileError> {
    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let host = FilesystemHost::new(base_path);
    let filename = display_filename
        .map(std::borrow::Cow::Borrowed)
        .unwrap_or_else(|| path.to_string_lossy());

    runtime()
        .block_on(wado_compiler::compile_with_options(
            source,
            &host,
            Some(&filename),
            options,
        ))
        .map_err(|_| bail_to_compile_error(&host.diagnostics(), Some(&filename)))
}

/// Compile a file asynchronously (for use within async context)
pub async fn compile_file_async(
    path: &std::path::Path,
    opt_level: OptLevel,
) -> Result<wado_compiler::CompileResult, CompileError> {
    let source = std::fs::read_to_string(path).map_err(|e| CompileError::Io {
        path: path.to_string_lossy().to_string(),
        message: e.to_string(),
    })?;

    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let host = FilesystemHost::new(base_path);
    let filename = path.to_string_lossy();

    wado_compiler::compile_with_host(&source, &host, Some(&filename), opt_level)
        .await
        .map_err(|_: CompileFailure| bail_to_compile_error(&host.diagnostics(), Some(&filename)))
}

/// Extract __DATA__ section from source file content
pub fn extract_data_section(source: &str) -> Option<&str> {
    let marker = "\n__DATA__\n";
    if let Some(pos) = source.find(marker) {
        Some(&source[pos + marker.len()..])
    } else {
        source.strip_prefix("__DATA__\n")
    }
}

/// Parse JSON from __DATA__ section with helpful error message
pub fn parse_data_section<T: serde::de::DeserializeOwned>(data_section: &str, context: &str) -> T {
    serde_json::from_str(data_section).unwrap_or_else(|e| {
        panic!(
            "[{context}] Failed to parse __DATA__ section as JSON: {e}\nContent:\n{data_section}"
        );
    })
}

/// Get human-readable name for optimization level
pub fn opt_level_name(opt: OptLevel) -> &'static str {
    match opt {
        OptLevel::O0 => "O0",
        OptLevel::O1 => "O1",
        OptLevel::O2 => "O2",
        OptLevel::O3 => "O3",
        OptLevel::Os => "Os",
    }
}

/// Result of running a Wasm component with captured output
#[derive(Debug)]
pub struct WasmRunResult {
    /// Captured stdout
    pub stdout: String,
    /// Captured stderr
    pub stderr: String,
    /// Whether the component trapped (e.g., from unreachable)
    pub trapped: bool,
}

/// Run a compiled Wasm component and capture its output
pub fn run_wasm(wasm: Vec<u8>) -> anyhow::Result<WasmRunResult> {
    run_wasm_with_options(wasm, &[], None)
}

/// Run a compiled Wasm component with preopened directories and capture its output.
/// `dirs` is a list of `(host_path, guest_path)` pairs.
pub fn run_wasm_with_dirs(
    wasm: Vec<u8>,
    dirs: &[(String, String)],
) -> anyhow::Result<WasmRunResult> {
    run_wasm_with_options(wasm, dirs, None)
}

/// Run a compiled Wasm component with full options (dirs, stdin) and capture its output.
pub fn run_wasm_with_options(
    wasm: Vec<u8>,
    dirs: &[(String, String)],
    stdin_data: Option<&str>,
) -> anyhow::Result<WasmRunResult> {
    run_wasm_with_full_options(
        wasm,
        dirs,
        stdin_data,
        indexmap::IndexMap::new(),
        indexmap::IndexMap::new(),
    )
}

/// Run a compiled Wasm component with full options including outgoing HTTP and TLS mocks.
pub fn run_wasm_with_full_options(
    wasm: Vec<u8>,
    dirs: &[(String, String)],
    stdin_data: Option<&str>,
    outgoing_mocks: indexmap::IndexMap<String, OutgoingMockResponse>,
    tls_mocks: indexmap::IndexMap<String, TlsMockResponse>,
) -> anyhow::Result<WasmRunResult> {
    use wasmtime::component::Component;
    use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};

    let rt = runtime();
    let engine = engine();

    rt.block_on(async {
        let component = Component::new(engine, &wasm)?;
        let linker = linker(engine)?;

        let stdout_pipe = MemoryOutputPipe::new(65536);
        let stdout_clone = stdout_pipe.clone();
        let stderr_pipe = MemoryOutputPipe::new(65536);
        let stderr_clone = stderr_pipe.clone();

        let mut builder = WasiCtxBuilder::new();
        builder.stdout(stdout_pipe).stderr(stderr_pipe);
        if let Some(data) = stdin_data {
            builder.stdin(MemoryInputPipe::new(data.to_owned()));
        }
        if !dirs.is_empty() {
            builder.allow_blocking_current_thread(true);
        }
        for (host_path, guest_path) in dirs {
            builder.preopened_dir(host_path, guest_path, DirPerms::all(), FilePerms::all())?;
        }
        let ctx = builder.build();
        let state = WasiState {
            ctx,
            table: ResourceTable::new(),
            http_ctx: WasiHttpCtx::new(),
            http_hooks: TestHttpCtx {
                mocks: outgoing_mocks,
            },
            tls_ctx: build_tls_ctx(tls_mocks),
        };
        let mut store = Store::new(engine, state);
        // Set epoch deadline for timeout enforcement
        let deadline_ticks = (DEFAULT_TIMEOUT_MS / EPOCH_INTERVAL_MS).max(1);
        store.set_epoch_deadline(deadline_ticks);

        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")?;

        let (trapped, trap_msg) = match run_func.call_async(&mut store, ()).await {
            Ok((result,)) => (result.is_err(), String::new()),
            Err(e) => (true, format!("{e:#}")),
        };

        let stdout = String::from_utf8(stdout_clone.contents().to_vec())?;
        let mut stderr = String::from_utf8(stderr_clone.contents().to_vec())?;
        if !trap_msg.is_empty() {
            if !stderr.is_empty() {
                stderr.push('\n');
            }
            stderr.push_str(&trap_msg);
        }

        Ok(WasmRunResult {
            stdout,
            stderr,
            trapped,
        })
    })
}
