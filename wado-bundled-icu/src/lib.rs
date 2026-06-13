//! Technical-validation wrapper that exposes a slice of ICU4X over a Wasm
//! Component Model interface. Built no_std for wasm32-unknown-unknown so the
//! resulting module imports nothing; a post-build `wasm-tools component new`
//! wraps it into a component. See `wit/world.wit` for the exported surface.

#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};

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

wit_bindgen::generate!({
    world: "icu",
    path: "wit",
});

use exports::wado::icu::casemap::Guest as CasemapGuest;
use exports::wado::icu::locale::{
    Guest as LocaleGuest, GuestLocale, Locale as LocaleHandle, LocaleBorrow,
};

use icu::casemap::CaseMapper;
use icu::locale::Locale as IcuLocale;

struct Component;

/// Guest representation behind the `locale` resource handle.
struct LocaleRes {
    inner: IcuLocale,
}

impl GuestLocale for LocaleRes {
    fn parse(tag: String) -> Result<LocaleHandle, String> {
        match tag.parse::<IcuLocale>() {
            Ok(inner) => Ok(LocaleHandle::new(LocaleRes { inner })),
            Err(e) => Err(e.to_string()),
        }
    }

    fn to_string(&self) -> String {
        self.inner.to_string()
    }
}

impl LocaleGuest for Component {
    type Locale = LocaleRes;
}

impl CasemapGuest for Component {
    fn uppercase(text: String, loc: LocaleBorrow<'_>) -> String {
        let cm = CaseMapper::new();
        cm.uppercase_to_string(&text, &loc.get::<LocaleRes>().inner.id)
            .into_owned()
    }

    fn lowercase(text: String, loc: LocaleBorrow<'_>) -> String {
        let cm = CaseMapper::new();
        cm.lowercase_to_string(&text, &loc.get::<LocaleRes>().inner.id)
            .into_owned()
    }

    fn uppercase_in(text: String, tag: String) -> Result<String, String> {
        let locale: IcuLocale = tag.parse().map_err(|e: icu::locale::ParseError| e.to_string())?;
        let cm = CaseMapper::new();
        Ok(cm.uppercase_to_string(&text, &locale.id).into_owned())
    }
}

export!(Component);
