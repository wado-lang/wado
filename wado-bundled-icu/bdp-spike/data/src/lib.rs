//! Shared data component: serves the sliced casemap postcard blob over the
//! `data` interface. This is the "shared part as its own component" — the data
//! lives here once, and feature components import it. See `wit/world.wit`.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

wit_bindgen::generate!({
    world: "provider",
    path: "wit",
});

use exports::wado::icu_bdp::data::Guest as DataGuest;

// The casemap data, baked once into this component.
const BLOB: &[u8] = include_bytes!("../casemap.blob");

struct Component;

impl DataGuest for Component {
    fn get_casemap_blob() -> Vec<u8> {
        BLOB.into()
    }
}

export!(Component);
