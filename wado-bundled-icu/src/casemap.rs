//! Full case mapping. See `wit-properties/world.wit`.

use alloc::borrow::Cow;
use alloc::string::String;

use icu::casemap::CaseMapper;
use icu::locale::subtags::{Language, language};

use crate::locale;
use crate::properties::Component;
use crate::properties::exports::wado::icu::casemap::Guest;

fn fold(text: &str, lang: Language) -> Cow<'_, str> {
    if lang == language!("tr") || lang == language!("az") {
        CaseMapper::new().fold_turkic_string(text)
    } else {
        CaseMapper::new().fold_string(text)
    }
}

impl Guest for Component {
    fn uppercase(text: String, language: u32) -> String {
        CaseMapper::new()
            .uppercase_to_string(&text, &locale::langid(language, 0, 0))
            .into_owned()
    }

    fn lowercase(text: String, language: u32) -> String {
        CaseMapper::new()
            .lowercase_to_string(&text, &locale::langid(language, 0, 0))
            .into_owned()
    }

    fn fold(text: String, language: u32) -> String {
        fold(&text, locale::language(language)).into_owned()
    }

    fn eq_ignore_case(left: String, right: String, language: u32) -> bool {
        let lang = locale::language(language);
        fold(&left, lang) == fold(&right, lang)
    }
}
