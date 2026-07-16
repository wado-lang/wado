//! wasm32 wrapper exposing the Wado compiler for an in-browser playground.
//!
//! The C ABI is intentionally minimal so the browser side can drive it with
//! plain `WebAssembly` (no wasm-bindgen runtime):
//!
//! - `wado_alloc(len)` reserves `len` bytes the host fills with UTF-8 source.
//! - `wado_compile(ptr, len)` compiles that source and returns a pointer to a
//!   result buffer laid out as `[status:u32 LE][len:u32 LE][payload…]`.
//!   `status == 1`: payload is the component Wasm. `status == 0`: payload is
//!   UTF-8 diagnostics text. It **consumes** the input buffer (frees it), so
//!   the host must not reuse `ptr` afterwards.
//! - `wado_free(ptr, len)` releases a buffer. The host calls it on the result
//!   pointer (with `len == 8 + payload_len`) once it has copied the bytes out.
//!
//! Every buffer is allocated with an exact `[u8; len]` layout and freed with
//! the same layout, so alloc/dealloc always agree (no `Vec` capacity slack).

use std::alloc::{Layout, alloc, dealloc};
use std::future::Future;
use std::task::{Context, Poll, Waker};

use wado_compiler::compiler_host::InMemoryCompilerHost;
use wado_compiler::{CompilerOptions, OptLevel, compile_with_options};

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
/// # Safety
/// `ptr..ptr+len` must be a valid buffer previously returned by [`wado_alloc`]
/// and filled with `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wado_compile(ptr: *mut u8, len: usize) -> *const u8 {
    let source = unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned() };
    unsafe { wado_free(ptr, len) };

    let host = InMemoryCompilerHost::new();
    let options = CompilerOptions {
        opt_level: OptLevel::O2,
        // V8/browsers do not implement the wide-arithmetic proposal.
        codegen_flags: vec!["no-wide-arithmetic".to_string()],
        ..CompilerOptions::default()
    };

    let fut = compile_with_options(&source, &host, Some("playground.wado"), options);
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

/// Allocate a result buffer `[status:u32 LE][len:u32 LE][payload…]` and return
/// it for the host to read and then release with [`wado_free`].
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

/// Drive a future to completion. `InMemoryCompilerHost` never truly suspends
/// (every `load_source` resolves immediately), so the noop waker suffices; a
/// `Pending` would mean that invariant broke, so fail loudly instead of
/// spinning forever.
fn block_on<F: Future>(fut: F) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut fut = Box::pin(fut);
    let Poll::Ready(v) = fut.as_mut().poll(&mut cx) else {
        panic!("compile future suspended, but InMemoryCompilerHost never yields");
    };
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a source string through the C ABI, exercising the exact
    /// alloc → compile → free path the browser uses.
    fn compile_str(src: &str) -> (u32, Vec<u8>) {
        let bytes = src.as_bytes();
        let ptr = wado_alloc(bytes.len());
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) };

        let out = unsafe { wado_compile(ptr, bytes.len()) };
        let header = unsafe { std::slice::from_raw_parts(out, 8) };
        let status = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let plen = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let payload = unsafe { std::slice::from_raw_parts(out.add(8), plen).to_vec() };
        unsafe { wado_free(out.cast_mut(), 8 + plen) };
        (status, payload)
    }

    #[test]
    fn compiles_hello_to_a_component() {
        let src = "use { println, Stdout } from \"core:cli\";\n\nexport fn run() with Stdout {\n    println(\"hi\");\n}\n";
        let (status, payload) = compile_str(src);
        assert_eq!(status, 1, "expected success");
        assert_eq!(&payload[0..4], b"\0asm", "Wasm magic");
        assert_eq!(&payload[6..8], &[0x01, 0x00], "component layer");
    }

    #[test]
    fn reports_diagnostics_on_error() {
        let (status, payload) = compile_str("this is not valid wado");
        assert_eq!(status, 0, "expected failure");
        assert!(!payload.is_empty(), "diagnostics text present");
        assert!(std::str::from_utf8(&payload).is_ok(), "diagnostics are UTF-8");
    }

    #[test]
    fn handles_empty_source() {
        let (status, _) = compile_str("");
        assert_eq!(status, 0, "empty source is an error, not a crash");
    }
}
