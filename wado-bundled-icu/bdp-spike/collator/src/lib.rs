//! Data-free collator feature component (marker-dedup demo). Loads collation +
//! NFD-normalization data from the shared blob via BlobDataProvider; bakes none.

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

wit_bindgen::generate!({ world: "collator-feature", path: "../cn-wit" });

use exports::wado::icu_cn::collator::{Guest, Ordering};
use wado::icu_cn::data::get_blob;

use icu_collator::{Collator, options::CollatorOptions, CollatorPreferences};
use icu_locale_core::Locale;
use icu_provider_blob::BlobDataProvider;

// The blob provider is built once from the imported blob and cached.
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
    fn compare(left: String, right: String, langid: String) -> Result<Ordering, String> {
        let loc: Locale = langid.parse().map_err(|e: icu_locale_core::ParseError| e.to_string())?;
        let prefs = CollatorPreferences::from(&loc);
        let coll = Collator::try_new_with_buffer_provider(provider()?, prefs, CollatorOptions::default())
            .map_err(|e| e.to_string())?;
        Ok(match coll.as_borrowed().compare(&left, &right) {
            core::cmp::Ordering::Less => Ordering::Less,
            core::cmp::Ordering::Equal => Ordering::Equal,
            core::cmp::Ordering::Greater => Ordering::Greater,
        })
    }
}
export!(Component);
