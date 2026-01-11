//! Float-to-string conversion for Wado using ryu.
//!
//! This crate compiles to Wasm P1 format for static linking with Wado-generated code.
//! The functions write formatted floats to a caller-provided buffer in linear memory.
//!
//! Key design: No internal memory allocation. The caller provides a buffer pointer.
//! This makes it easy to integrate with the Wado compiler's memory model.

// Only use no_std on wasm32 target
#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
use core::panic::PanicInfo;

/// Copy bytes to a destination pointer in linear memory.
///
/// # Safety
/// The destination must have enough space for `src.len()` bytes.
unsafe fn copy_to_ptr(dest_ptr: i32, src: &[u8]) {
    let dest = dest_ptr as *mut u8;
    for (i, &byte) in src.iter().enumerate() {
        // SAFETY: Caller guarantees dest has enough space
        unsafe { dest.add(i).write(byte) };
    }
}

/// Formats an f64 value to the provided buffer, returns the length
///
/// # Safety
/// The buffer must be at least 24 bytes (ryu's max output for f64)
#[unsafe(no_mangle)]
pub extern "C" fn f64_to_buffer(value: f64, buffer_ptr: i32) -> i32 {
    let mut ryu_buffer = ryu::Buffer::new();
    let formatted = ryu_buffer.format(value);
    unsafe { copy_to_ptr(buffer_ptr, formatted.as_bytes()) };
    formatted.len() as i32
}

/// Formats an f32 value to the provided buffer, returns the length
///
/// # Safety
/// The buffer must be at least 16 bytes (ryu's max output for f32)
#[unsafe(no_mangle)]
pub extern "C" fn f32_to_buffer(value: f32, buffer_ptr: i32) -> i32 {
    let mut ryu_buffer = ryu::Buffer::new();
    let formatted = ryu_buffer.format(value);
    unsafe { copy_to_ptr(buffer_ptr, formatted.as_bytes()) };
    formatted.len() as i32
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    core::arch::wasm32::unreachable();
}
