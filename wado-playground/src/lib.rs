//! wasm32 wrapper exposing the Wado compiler for an in-browser playground.
//!
//! Minimal C ABI so the browser can drive it with plain `WebAssembly` (no
//! wasm-bindgen):
//!
//! - `wado_alloc(len)` reserves `len` bytes for the host to fill with UTF-8 source.
//! - `wado_compile(ptr, len)` compiles it, freeing the input buffer, and returns
//!   an owned result buffer `[status:u32 LE][len:u32 LE][payload…]` — `status 1`
//!   payload is the component Wasm, `status 0` payload is UTF-8 diagnostics.
//! - `wado_free(ptr, len)` releases a buffer (result buffers: `len == 8 + payload`).
//!
//! While compiling, the wrapper calls the host-supplied import `wado_phase(ptr,
//! len)` once per compiler phase (`parse`, `monomorphize`, `codegen`, …) so a
//! slow client — a phone especially — can show live progress instead of a
//! frozen button. This is the compiler's `--log-level debug` phase stream.
//!
//! The same cdylib also hosts the language server (`wado_lsp_*`), so the
//! browser gets the LSP engine without a second copy of the compiler. It drives
//! `wado_lsp::Engine` directly as a library — no WASI, no stdio loop — via:
//!
//! - `wado_lsp_new()` creates a session (document state + lifecycle).
//! - `wado_lsp_send(session, ptr, len)` feeds one JSON-RPC message (freeing the
//!   input) and returns an owned `[len:u32 LE][framed reply bytes…]` buffer of
//!   zero or more `Content-Length`-framed LSP messages (response + notifications).
//! - `wado_lsp_close(session)` frees the session.

use std::alloc::{Layout, alloc, dealloc};
use std::future::Future;
use std::task::{Context, Poll, Waker};

use wado_compiler::compiler_host::{CompilerHost, Diagnostic, InMemoryCompilerHost, SourceError};
use wado_compiler::{Code, CompilerOptions, LogLevel, OptLevel, Severity, compile_with_options};
use wado_lsp::Engine;
use wado_lsp::server::dispatch::{Lifecycle, dispatch};
use wado_lsp::server::rpc::{JsonRpcRequest, error_codes};
use wado_lsp::server::transport;

// Host-supplied progress import, called once per compiler phase with the phase
// name (`parse`, `codegen`, …) as a UTF-8 slice into this module's memory.
// Only wired up in the browser; native (test) builds report to nothing.
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn wado_phase(ptr: *const u8, len: usize);
}

/// Forward a phase name to the browser's `wado_phase` import. A no-op off wasm.
fn report_phase(name: &str) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        wado_phase(name.as_ptr(), name.len());
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = name;
}

/// A [`CompilerHost`] that streams phase progress while collecting diagnostics.
///
/// The compiler emits a `SpanStart` debug diagnostic when it enters a phase; we
/// forward those names to `on_phase` for live UI feedback and drop the rest of
/// the debug chatter (`SpanEnd`, timing logs), so `inner` collects only the
/// real errors and warnings that make up the failure text.
struct ProgressHost<F: Fn(&str) + Send + Sync> {
    inner: InMemoryCompilerHost,
    on_phase: F,
}

impl<F: Fn(&str) + Send + Sync> ProgressHost<F> {
    fn new(on_phase: F) -> Self {
        Self {
            inner: InMemoryCompilerHost::new(),
            on_phase,
        }
    }

    fn diagnostics(&self) -> Vec<Diagnostic> {
        self.inner.diagnostics()
    }
}

impl<F: Fn(&str) + Send + Sync> CompilerHost for ProgressHost<F> {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        self.inner.load_source(path).await
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        if diagnostic.code == Code::SpanStart {
            (self.on_phase)(&diagnostic.message);
        } else if diagnostic.severity != Severity::Debug {
            self.inner.emit_diagnostic(diagnostic);
        }
    }
}

/// Compiler options shared by the browser entry point and the tests, so the
/// playground and its coverage stay in lockstep. `report_phases` raises the log
/// level to `Debug` to turn on the per-phase `SpanStart` stream; when a caller
/// wants no progress it stays at `Info`, so the compiler never builds the ~170
/// span diagnostics per compile that nobody would read.
fn playground_options(report_phases: bool) -> CompilerOptions {
    CompilerOptions {
        opt_level: OptLevel::O2,
        // V8/browsers do not implement the wide-arithmetic proposal.
        codegen_flags: vec!["no-wide-arithmetic".to_string()],
        log_level: Some(if report_phases {
            LogLevel::Debug
        } else {
            LogLevel::Info
        }),
        ..CompilerOptions::default()
    }
}

/// Exact byte-buffer layout. `len == 0` is rounded to 1 so alloc never sees a
/// zero size; `wado_free` applies the same rounding, so the layouts match.
fn layout(len: usize) -> Layout {
    Layout::from_size_align(len.max(1), 1).expect("valid byte layout")
}

/// Reserve `len` bytes and return a pointer the host can write into.
#[unsafe(no_mangle)]
pub extern "C" fn wado_alloc(len: usize) -> *mut u8 {
    unsafe { alloc(layout(len)) }
}

/// Free a buffer previously returned by [`wado_alloc`] or [`wado_compile`].
///
/// # Safety
/// `ptr` must come from [`wado_alloc`]/[`wado_compile`] and `len` must be the
/// exact length it was allocated with (for a result buffer, `8 + payload_len`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wado_free(ptr: *mut u8, len: usize) {
    unsafe { dealloc(ptr, layout(len)) }
}

/// Compile the UTF-8 source at `ptr..ptr+len`, consuming (freeing) that buffer.
///
/// When `report_phases != 0`, each compiler phase is streamed through the
/// `wado_phase` import for live progress; pass `0` to skip that work entirely.
///
/// # Safety
/// `ptr..ptr+len` must be a valid buffer previously returned by [`wado_alloc`]
/// and filled with `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wado_compile(ptr: *mut u8, len: usize, report_phases: u32) -> *const u8 {
    let source =
        unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned() };
    unsafe { wado_free(ptr, len) };

    let host = ProgressHost::new(report_phase);

    let fut = compile_with_options(
        &source,
        &host,
        Some("playground.wado"),
        playground_options(report_phases != 0),
    );
    if let Ok(result) = block_on(fut) {
        encode(1, &result.wasm)
    } else {
        let text = host
            .diagnostics()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        encode(0, text.as_bytes())
    }
}

/// Allocate and fill an owned `[status:u32 LE][len:u32 LE][payload…]` buffer.
fn encode(status: u32, payload: &[u8]) -> *const u8 {
    let total = 8 + payload.len();
    let out = wado_alloc(total);
    unsafe {
        let buf = std::slice::from_raw_parts_mut(out, total);
        buf[0..4].copy_from_slice(&status.to_le_bytes());
        buf[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        buf[8..].copy_from_slice(payload);
    }
    out
}

/// Poll a future to completion. Every host here (`InMemoryCompilerHost`, the
/// LSP's `FilesystemCompilerHost`) resolves each `load_source` synchronously, so
/// one poll suffices; a `Pending` means that invariant broke — panic rather than
/// spin forever.
fn block_on<F: Future>(fut: F) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut fut = Box::pin(fut);
    let Poll::Ready(v) = fut.as_mut().poll(&mut cx) else {
        panic!("future suspended, but the compiler host never yields");
    };
    v
}

/// A live language-server session: document state plus LSP lifecycle, persisted
/// across `wado_lsp_send` calls. The compiler host is built per request inside
/// `dispatch`, so nothing here is tied to a filesystem.
pub struct LspSession {
    engine: Engine,
    lifecycle: Lifecycle,
}

/// Create an LSP session. Free it with [`wado_lsp_close`].
#[unsafe(no_mangle)]
pub extern "C" fn wado_lsp_new() -> *mut LspSession {
    Box::into_raw(Box::new(LspSession {
        engine: Engine::new(),
        lifecycle: Lifecycle::default(),
    }))
}

/// Free a session created by [`wado_lsp_new`].
///
/// # Safety
/// `session` must come from [`wado_lsp_new`] and not have been closed already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wado_lsp_close(session: *mut LspSession) {
    drop(unsafe { Box::from_raw(session) });
}

/// Feed one JSON-RPC message (`ptr..ptr+len`, consumed/freed) to `session` and
/// return an owned `[len:u32 LE][framed reply bytes…]` buffer — zero or more
/// `Content-Length`-framed LSP messages. Free it with `wado_free(ptr, 4 + len)`.
///
/// # Safety
/// `ptr..ptr+len` must be a valid buffer from [`wado_alloc`] filled with `len`
/// bytes, and `session` must come from [`wado_lsp_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wado_lsp_send(
    session: *mut LspSession,
    ptr: *mut u8,
    len: usize,
) -> *const u8 {
    let message =
        unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned() };
    unsafe { wado_free(ptr, len) };
    let session = unsafe { &mut *session };

    let mut out = Vec::<u8>::new();
    match serde_json::from_str::<JsonRpcRequest>(&message) {
        // `exit` is a stdio-server concept (`std::process::exit`); the browser
        // drops the session instead, so swallow it rather than aborting the wasm.
        Ok(request) if request.method == "exit" => {}
        Ok(request) => {
            // `dispatch` only errors when its writer fails; ours is a `Vec`,
            // which cannot. Surface a broken invariant instead of dropping it —
            // the worker's try/catch turns the trap into an `error` message.
            block_on(dispatch(
                &mut session.engine,
                &mut out,
                &mut session.lifecycle,
                request,
            ))
            .expect("LSP dispatch cannot fail writing to an in-memory buffer");
        }
        Err(err) => {
            // JSON-RPC 2.0 §5.1: malformed request → ParseError with id null.
            let _ = transport::send_error(
                &mut out,
                &serde_json::Value::Null,
                error_codes::PARSE_ERROR,
                err.to_string(),
            );
        }
    }
    encode_bytes(&out)
}

/// Allocate and fill an owned `[len:u32 LE][payload…]` buffer.
fn encode_bytes(payload: &[u8]) -> *const u8 {
    let total = 4 + payload.len();
    let out = wado_alloc(total);
    unsafe {
        let buf = std::slice::from_raw_parts_mut(out, total);
        buf[0..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        buf[4..].copy_from_slice(payload);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const HELLO: &str = "use { println, Stdout } from \"core:cli\";\n\nexport fn run() with Stdout {\n    println(\"hi\");\n}\n";

    /// Round-trip a source string through the C ABI, exercising the exact
    /// alloc → compile → free path the browser uses. `report_phases` mirrors the
    /// ABI flag that gates the progress stream.
    fn compile_str_reporting(src: &str, report_phases: u32) -> (u32, Vec<u8>) {
        let bytes = src.as_bytes();
        let ptr = wado_alloc(bytes.len());
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) };

        let out = unsafe { wado_compile(ptr, bytes.len(), report_phases) };
        let header = unsafe { std::slice::from_raw_parts(out, 8) };
        let status = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let plen = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let payload = unsafe { std::slice::from_raw_parts(out.add(8), plen).to_vec() };
        unsafe { wado_free(out.cast_mut(), 8 + plen) };
        (status, payload)
    }

    /// Compile with progress reporting on — the browser's usual path.
    fn compile_str(src: &str) -> (u32, Vec<u8>) {
        compile_str_reporting(src, 1)
    }

    #[test]
    fn compiles_hello_to_a_component() {
        let (status, payload) = compile_str(HELLO);
        assert_eq!(status, 1, "expected success");
        assert_eq!(&payload[0..4], b"\0asm", "Wasm magic");
        assert_eq!(&payload[6..8], &[0x01, 0x00], "component layer");
    }

    /// With progress off, a valid program still compiles to the same component.
    #[test]
    fn compiles_hello_without_progress() {
        let (status, payload) = compile_str_reporting(HELLO, 0);
        assert_eq!(status, 1, "expected success");
        assert_eq!(&payload[0..4], b"\0asm", "Wasm magic");
    }

    #[test]
    fn reports_diagnostics_on_error() {
        let (status, payload) = compile_str("this is not valid wado");
        assert_eq!(status, 0, "expected failure");
        assert!(!payload.is_empty(), "diagnostics text present");
        assert!(
            std::str::from_utf8(&payload).is_ok(),
            "diagnostics are UTF-8"
        );
    }

    #[test]
    fn handles_empty_source() {
        let (status, _) = compile_str("");
        assert_eq!(status, 0, "empty source is an error, not a crash");
    }

    /// A successful compile streams the compiler's phase names to `on_phase`, so
    /// the browser can show progress mid-compile. `parse` and `codegen` bracket
    /// the pipeline, so both must appear.
    #[test]
    fn streams_phase_progress() {
        let phases = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&phases);
        let host = ProgressHost::new(move |name: &str| sink.lock().unwrap().push(name.to_string()));

        let result = block_on(compile_with_options(
            HELLO,
            &host,
            Some("playground.wado"),
            playground_options(true),
        ));
        assert!(result.is_ok(), "hello compiles");

        let phases = phases.lock().unwrap();
        assert!(
            phases.iter().any(|p| p.starts_with("parse")),
            "saw parse: {phases:?}"
        );
        assert!(
            phases.iter().any(|p| p == "codegen"),
            "saw codegen: {phases:?}"
        );
    }

    /// With progress off (`Info` level) the compiler emits no `SpanStart`, so the
    /// phase callback never fires — the gate that avoids the ~170-diagnostic cost.
    #[test]
    fn no_progress_when_not_requested() {
        let phases = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&phases);
        let host = ProgressHost::new(move |name: &str| sink.lock().unwrap().push(name.to_string()));

        let result = block_on(compile_with_options(
            HELLO,
            &host,
            Some("playground.wado"),
            playground_options(false),
        ));
        assert!(result.is_ok(), "hello compiles");
        assert!(phases.lock().unwrap().is_empty(), "no phases streamed");
    }

    /// Phase names are progress, not diagnostics: the debug stream (`SpanStart`
    /// / `SpanEnd` / timing logs, all `Severity::Debug`) must not leak into the
    /// error text a failed compile returns.
    #[test]
    fn debug_stream_stays_out_of_error_text() {
        let (status, payload) = compile_str("this is not valid wado");
        assert_eq!(status, 0);
        let text = std::str::from_utf8(&payload).unwrap();
        assert!(!text.contains("debug:"), "no debug lines in errors: {text}");
    }

    /// Drive one JSON-RPC message through the LSP C ABI and return the framed
    /// reply bytes, exercising the exact alloc → send → free path the worker uses.
    fn lsp_send(session: *mut LspSession, msg: &serde_json::Value) -> Vec<u8> {
        let bytes = serde_json::to_vec(msg).unwrap();
        let ptr = wado_alloc(bytes.len());
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) };

        let out = unsafe { wado_lsp_send(session, ptr, bytes.len()) };
        let len = u32::from_le_bytes(
            unsafe { std::slice::from_raw_parts(out, 4) }
                .try_into()
                .unwrap(),
        ) as usize;
        let payload = unsafe { std::slice::from_raw_parts(out.add(4), len).to_vec() };
        unsafe { wado_free(out.cast_mut(), 4 + len) };
        payload
    }

    /// The bundled LSP answers a full initialize → didOpen → hover round-trip
    /// through the same C ABI the browser worker drives, with no WASI host.
    #[test]
    fn lsp_round_trip_over_the_abi() {
        let session = wado_lsp_new();

        let init = lsp_send(
            session,
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "processId": null, "rootUri": null, "capabilities": {} },
            }),
        );
        let init = String::from_utf8(init).unwrap();
        assert!(
            init.contains("\"name\":\"wado-lsp\""),
            "initialize result: {init}"
        );

        // Notifications produce no response to the client but must not error.
        lsp_send(
            session,
            &serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
        );

        let uri = "file:///playground.wado";
        let diags = lsp_send(
            session,
            &serde_json::json!({
                "jsonrpc": "2.0", "method": "textDocument/didOpen",
                "params": { "textDocument": { "uri": uri, "languageId": "wado", "version": 1, "text": HELLO } },
            }),
        );
        let diags = String::from_utf8(diags).unwrap();
        assert!(
            diags.contains("textDocument/publishDiagnostics"),
            "didOpen publishes diagnostics: {diags}"
        );

        let hover = lsp_send(
            session,
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "textDocument/hover",
                "params": { "textDocument": { "uri": uri }, "position": { "line": 3, "character": 4 } },
            }),
        );
        let hover = String::from_utf8(hover).unwrap();
        assert!(hover.contains("println"), "hover over println: {hover}");

        unsafe { wado_lsp_close(session) };
    }

    /// A malformed JSON-RPC message yields a framed `ParseError`, not a panic.
    #[test]
    fn lsp_reports_parse_error() {
        let session = wado_lsp_new();
        let reply =
            String::from_utf8(lsp_send(session, &serde_json::json!("not a request"))).unwrap();
        assert!(reply.contains("-32700"), "ParseError code present: {reply}");
        unsafe { wado_lsp_close(session) };
    }
}
