//! wasm32 wrapper exposing the Wado compiler for an in-browser playground.
//!
//! The C ABI is intentionally minimal so the browser side can drive it with
//! plain `WebAssembly` (no wasm-bindgen runtime):
//!
//! - `wado_alloc(len)` reserves a buffer the host fills with UTF-8 source.
//! - `wado_compile(ptr, len)` compiles it and returns a pointer to a result
//!   buffer laid out as `[status:u32 LE][len:u32 LE][payload…]`.
//!   `status == 1`: payload is the component Wasm. `status == 0`: payload is
//!   UTF-8 diagnostics text.

use std::future::Future;
use std::task::{Context, Poll, Waker};

use wado_compiler::compiler_host::InMemoryCompilerHost;
use wado_compiler::{CompilerOptions, OptLevel, compile_with_options};

/// Reserve `len` bytes and return a pointer the host can write into.
///
/// # Safety
/// The returned pointer is owned by the caller until passed back to
/// [`wado_compile`]. Leaking here is fine: the playground compiles once per
/// call and the wasm instance is short-lived.
#[unsafe(no_mangle)]
pub extern "C" fn wado_alloc(len: usize) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// Compile the UTF-8 source at `ptr..ptr+len`.
///
/// # Safety
/// `ptr..ptr+len` must be a valid buffer previously returned by
/// [`wado_alloc`] and filled with `len` bytes of UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wado_compile(ptr: *mut u8, len: usize) -> *const u8 {
    let source = unsafe {
        let bytes = Vec::from_raw_parts(ptr, len, len);
        String::from_utf8_lossy(&bytes).into_owned()
    };

    let host = InMemoryCompilerHost::new();
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        // V8/browsers do not implement the wide-arithmetic proposal.
        codegen_flags: vec!["no-wide-arithmetic".to_string()],
        ..CompilerOptions::default()
    };

    let fut = compile_with_options(&source, &host, Some("playground.wado"), options);
    match block_on(fut) {
        Ok(result) => encode(1, result.wasm),
        Err(_) => {
            let text = host
                .diagnostics()
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            encode(0, text.into_bytes())
        }
    }
}

/// `[status:u32 LE][len:u32 LE][payload…]`, leaked for the host to read.
fn encode(status: u32, payload: Vec<u8>) -> *const u8 {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&status.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    let ptr = out.as_ptr();
    std::mem::forget(out);
    ptr
}

/// Drive a future to completion. `InMemoryCompilerHost` never truly suspends
/// (every `load_source` resolves immediately), so the noop waker suffices.
fn block_on<F: Future>(fut: F) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut fut = Box::pin(fut);
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
    }
}
