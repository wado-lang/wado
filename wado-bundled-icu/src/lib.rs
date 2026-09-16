//! ICU4X bundled as a self-contained Wasm Component Model component. Built
//! no_std for wasm32-unknown-unknown so the resulting module imports nothing; a
//! post-build `wasm-tools component new` wraps it into a component.
//!
//! The default build is the shippable `core:icu` surface (`wit-properties/`);
//! `--features spike` builds the wider technical-validation one (`wit/`).

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
