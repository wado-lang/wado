//! Data-free casemap feature component for the BlobDataProvider (BDP)
//! separation experiment. It bakes NO Unicode data; on first use it pulls the
//! casemap postcard blob from the imported `data` interface and loads it through
//! icu's `BlobDataProvider`. See `wit/world.wit`.

#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use core::cell::UnsafeCell;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

wit_bindgen::generate!({
    world: "feature",
    path: "wit",
});

use exports::wado::icu_bdp::casemap::Guest as CasemapGuest;
use wado::icu_bdp::data::get_casemap_blob;

use icu_casemap::CaseMapper;
use icu_locale_core::LanguageIdentifier;
use icu_provider_blob::BlobDataProvider;

// The case mapper is built once (lazily) from the imported blob and cached here.
// Wasm guest exports run single-threaded, so a hand-rolled cell is sound; the
// `Sync` bound only exists to satisfy `static`.
struct Holder(UnsafeCell<Option<CaseMapper>>);
unsafe impl Sync for Holder {}
static MAPPER: Holder = Holder(UnsafeCell::new(None));

/// Returns the cached mapper, building it from the imported blob on first call.
fn mapper() -> Result<&'static CaseMapper, String> {
    // SAFETY: single-threaded guest; no aliasing access during this call.
    let slot = unsafe { &mut *MAPPER.0.get() };
    if slot.is_none() {
        let blob = get_casemap_blob();
        let provider = BlobDataProvider::try_new_from_blob(blob.into_boxed_slice())
            .map_err(|e| e.to_string())?;
        let cm =
            CaseMapper::try_new_with_buffer_provider(&provider).map_err(|e| e.to_string())?;
        *slot = Some(cm);
    }
    Ok(slot.as_ref().unwrap())
}

struct Component;

impl CasemapGuest for Component {
    fn fold(text: String) -> Result<String, String> {
        Ok(mapper()?.as_borrowed().fold_string(&text).into_owned())
    }

    fn uppercase(text: String, langid: String) -> Result<String, String> {
        let id = langid
            .parse::<LanguageIdentifier>()
            .map_err(|e| e.to_string())?;
        Ok(mapper()?
            .as_borrowed()
            .uppercase_to_string(&text, &id)
            .into_owned())
    }
}

export!(Component);
