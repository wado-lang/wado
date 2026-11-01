//! A locale as `wit-properties/world.wit` passes it: each subtag's ASCII bytes
//! in a `u32`, first byte lowest, 0 when absent.

use icu::locale::LanguageIdentifier;
use icu::locale::subtags::{Language, Region, Script, Variants};

pub fn language(raw: u32) -> Language {
    Language::try_from_raw(subtag(raw)).expect("core:icu passes a checked language subtag")
}

pub fn langid(language_raw: u32, script: u32, region: u32) -> LanguageIdentifier {
    LanguageIdentifier {
        language: language(language_raw),
        script: (script != 0).then(|| {
            Script::try_from_raw(script.to_le_bytes())
                .expect("core:icu passes a checked script subtag")
        }),
        region: (region != 0).then(|| {
            Region::try_from_raw(subtag(region)).expect("core:icu passes a checked region subtag")
        }),
        variants: Variants::new(),
    }
}

fn subtag(raw: u32) -> [u8; 3] {
    let [a, b, c, high] = raw.to_le_bytes();
    assert!(high == 0, "a three-byte subtag leaves the high byte clear");
    [a, b, c]
}
