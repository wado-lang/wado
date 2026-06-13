#![no_std]
extern crate alloc;
#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { core::arch::wasm32::unreachable() }
wit_bindgen::generate!({ world: "bare", path: "../wit" });
use exports::wado::infra::bare_iface::Guest;
struct C;
impl Guest for C { fn ping() -> u32 { 42 } }
export!(C);
