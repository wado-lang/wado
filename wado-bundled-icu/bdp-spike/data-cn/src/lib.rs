//! Shared data component for the collator+normalizer demo: bakes the shared
//! blob (collator + normalizer markers) and serves it to both features.

#![no_std]
extern crate alloc;
use alloc::vec::Vec;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

wit_bindgen::generate!({ world: "cn-provider", path: "../cn-wit" });
use exports::wado::icu_cn::data::Guest;

const BLOB: &[u8] = include_bytes!("../shared.blob");

struct Component;
impl Guest for Component {
    fn get_blob() -> Vec<u8> {
        BLOB.into()
    }
}
export!(Component);
