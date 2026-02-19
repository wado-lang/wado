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

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use wasmtime::component::{Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use wado_compiler::{Bail, CompileError, CompilerHost, Diagnostic, OptLevel, SourceError};

/// A filesystem-based CompilerHost for tests that need to load files
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
    ) -> impl std::future::Future<Output = Result<String, SourceError>> + Send {
        let full_path = self.base_path.join(path);
        async move {
            std::fs::read_to_string(&full_path).map_err(|e| SourceError::IoError {
                path: full_path.to_string_lossy().to_string(),
                message: e.to_string(),
            })
        }
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic);
    }
}

/// An in-memory CompilerHost for tests that don't need file loading
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
    ) -> impl std::future::Future<Output = Result<String, SourceError>> + Send {
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
        let fname = filename.map(String::from).or_else(|| {
            d.span.as_ref().and_then(|s| {
                if s.file.is_empty() {
                    None
                } else {
                    Some(s.file.clone())
                }
            })
        });
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
            }
        } else {
            // All other errors
            let all_messages: Vec<String> = errors.iter().map(|d| d.message.clone()).collect();
            CompileError::Analyzer {
                message: all_messages.join("; "),
                filename: fname,
            }
        }
    } else {
        CompileError::Analyzer {
            message: "compilation failed".to_string(),
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

/// Shared wasmtime Engine for CLI tests (initialized once)
static CLI_ENGINE: OnceLock<Engine> = OnceLock::new();

/// Get or initialize the shared wasmtime Engine for CLI (wasi:cli) tests
pub fn cli_engine() -> &'static Engine {
    CLI_ENGINE.get_or_init(|| {
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
        config.wasm_gc(true);
        config.wasm_function_references(true);
        // Use minimal optimization for faster compilation in tests
        config.cranelift_opt_level(wasmtime::OptLevel::None);
        Engine::new(&config).expect("Failed to create wasmtime Engine")
    })
}

/// Shared wasmtime Engine for HTTP (wasi:http/service) tests (initialized once)
static HTTP_ENGINE: OnceLock<Engine> = OnceLock::new();

/// Get or initialize the shared wasmtime Engine for HTTP tests
pub fn http_engine() -> &'static Engine {
    HTTP_ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.async_support(true);
        config.wasm_component_model(true);
        config.wasm_component_model_async(true);
        config.wasm_component_model_async_stackful(true);
        config.wasm_component_model_gc(true);
        config.wasm_gc(true);
        config.wasm_function_references(true);
        config.wasm_wide_arithmetic(true);
        config.wasm_threads(true);
        config.wasm_backtrace_details(wasmtime::WasmBacktraceDetails::Enable);
        // Use minimal optimization for faster compilation in tests
        config.cranelift_opt_level(wasmtime::OptLevel::None);
        Engine::new(&config).expect("Failed to create wasmtime Engine")
    })
}

/// WASI state for CLI tests
pub struct CliWasiState {
    pub ctx: WasiCtx,
    pub table: ResourceTable,
}

impl WasiView for CliWasiState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl CliWasiState {
    /// Create a new CLI WASI state with captured stdout/stderr
    pub fn new_with_pipes(
        stdout: wasmtime_wasi::p2::pipe::MemoryOutputPipe,
        stderr: wasmtime_wasi::p2::pipe::MemoryOutputPipe,
    ) -> Self {
        let ctx = WasiCtxBuilder::new().stdout(stdout).stderr(stderr).build();
        let table = ResourceTable::new();
        Self { ctx, table }
    }

    /// Create a basic CLI WASI state (no I/O capture)
    pub fn new() -> Self {
        let ctx = WasiCtxBuilder::new().build();
        let table = ResourceTable::new();
        Self { ctx, table }
    }
}

impl Default for CliWasiState {
    fn default() -> Self {
        Self::new()
    }
}

/// Set up a linker for CLI (wasi:cli) tests
pub fn cli_linker(engine: &Engine) -> anyhow::Result<Linker<CliWasiState>> {
    let mut linker: Linker<CliWasiState> = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    Ok(linker)
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
        .map_err(|_: Bail| bail_to_compile_error(&host.diagnostics(), None))
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
    let base_path = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let host = FilesystemHost::new(base_path);
    let filename = path.to_string_lossy();

    runtime()
        .block_on(wado_compiler::compile_with_host(
            source,
            &host,
            Some(&filename),
            opt_level,
        ))
        .map_err(|_: Bail| bail_to_compile_error(&host.diagnostics(), Some(&filename)))
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
    let base_path = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
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
        .map_err(|_: Bail| bail_to_compile_error(&host.diagnostics(), Some(&filename)))
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

    let base_path = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let host = FilesystemHost::new(base_path);
    let filename = path.to_string_lossy();

    wado_compiler::compile_with_host(&source, &host, Some(&filename), opt_level)
        .await
        .map_err(|_: Bail| bail_to_compile_error(&host.diagnostics(), Some(&filename)))
}

/// Extract __DATA__ section from source file content
pub fn extract_data_section(source: &str) -> Option<&str> {
    let marker = "\n__DATA__\n";
    if let Some(pos) = source.find(marker) {
        Some(&source[pos + marker.len()..])
    } else if source.starts_with("__DATA__\n") {
        Some(&source["__DATA__\n".len()..])
    } else {
        None
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
    use wasmtime::component::Component;
    use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

    let rt = runtime();
    let engine = cli_engine();

    rt.block_on(async {
        let component = Component::new(engine, &wasm)?;

        let linker = cli_linker(engine)?;

        let stdout_pipe = MemoryOutputPipe::new(65536);
        let stdout_clone = stdout_pipe.clone();
        let stderr_pipe = MemoryOutputPipe::new(65536);
        let stderr_clone = stderr_pipe.clone();

        let state = CliWasiState::new_with_pipes(stdout_pipe, stderr_pipe);
        let mut store = Store::new(engine, state);

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
