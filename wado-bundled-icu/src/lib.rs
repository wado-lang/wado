//! ICU4X bundled as a self-contained Wasm Component Model component. The
//! default build is the `core:icu` surface, `--features spike` the wider one.

#![no_std]

extern crate alloc;

// ICU4X allocates on the heap; provide the allocator it manages inside the
// component's own linear memory.
#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

// Wado has no exceptions: a panic is a programming error, so map it straight to
// a Wasm trap (`unreachable`) instead of dragging in unwinding/formatting.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

#[cfg(not(feature = "spike"))]
mod properties;
#[cfg(feature = "spike")]
mod spike;
