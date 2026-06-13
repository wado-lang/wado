//! Technical-validation wrapper that exposes a slice of ICU4X over a Wasm
//! Component Model interface. Built as a wasm32-wasip2 component; see
//! `wit/world.wit` for the exported surface.

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
}

export!(Component);
