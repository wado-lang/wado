//! Data-free normalizer feature component (marker-dedup demo). Loads NFC/NFD
//! data from the SAME shared blob the collator uses; bakes none.

#![no_std]
extern crate alloc;
use alloc::string::{String, ToString};
use core::cell::UnsafeCell;

#[global_allocator]
static ALLOC: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

wit_bindgen::generate!({ world: "normalizer-feature", path: "../cn-wit" });

use exports::wado::icu_cn::normalizer::Guest;
use wado::icu_cn::data::get_blob;

use icu_normalizer::{ComposingNormalizer, DecomposingNormalizer};
use icu_provider_blob::BlobDataProvider;

struct Holder(UnsafeCell<Option<BlobDataProvider>>);
unsafe impl Sync for Holder {}
static PROVIDER: Holder = Holder(UnsafeCell::new(None));

fn provider() -> Result<&'static BlobDataProvider, String> {
    let slot = unsafe { &mut *PROVIDER.0.get() };
    if slot.is_none() {
        *slot = Some(
            BlobDataProvider::try_new_from_blob(get_blob().into_boxed_slice())
                .map_err(|e| e.to_string())?,
        );
    }
    Ok(slot.as_ref().unwrap())
}

struct Component;
impl Guest for Component {
    fn nfc(text: String) -> Result<String, String> {
        let n = ComposingNormalizer::try_new_nfc_with_buffer_provider(provider()?)
            .map_err(|e| e.to_string())?;
        Ok(n.as_borrowed().normalize(&text).into_owned())
    }
    fn nfd(text: String) -> Result<String, String> {
        let n = DecomposingNormalizer::try_new_nfd_with_buffer_provider(provider()?)
            .map_err(|e| e.to_string())?;
        Ok(n.as_borrowed().normalize(&text).into_owned())
    }
}
export!(Component);
