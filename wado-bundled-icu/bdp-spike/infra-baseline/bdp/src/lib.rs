#![no_std]
extern crate alloc;
use alloc::vec::Vec;
#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { core::arch::wasm32::unreachable() }
wit_bindgen::generate!({ world: "bdp", path: "../wit" });
use exports::wado::infra::bdp_iface::Guest;
use icu_provider_blob::BlobDataProvider;
struct C;
impl Guest for C {
    fn check(blob: Vec<u8>) -> bool {
        BlobDataProvider::try_new_from_blob(blob.into_boxed_slice()).is_ok()
    }
}
export!(C);
