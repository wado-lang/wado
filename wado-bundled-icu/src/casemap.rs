//! Full case mapping. See `wit-properties/world.wit`.

use alloc::string::String;

use icu::casemap::CaseMapper;
use icu::locale::LanguageIdentifier;
use icu::locale::subtags::language;

use crate::locale;
use crate::properties::Component;
use crate::properties::exports::wado::icu::casemap::Guest;

/// The language identifier the mapping reads: the language alone.
fn langid(language_raw: u32) -> LanguageIdentifier {
    LanguageIdentifier::from(locale::language(language_raw))
}

fn fold(text: &str, language_raw: u32) -> alloc::borrow::Cow<'_, str> {
    let lang = locale::language(language_raw);
    if lang == language!("tr") || lang == language!("az") {
        CaseMapper::new().fold_turkic_string(text)
    } else {
        CaseMapper::new().fold_string(text)
    }
}

impl Guest for Component {
    fn uppercase(text: String, language: u32) -> String {
        CaseMapper::new().uppercase_to_string(&text, &langid(language)).into_owned()
    }

    fn lowercase(text: String, language: u32) -> String {
        CaseMapper::new().lowercase_to_string(&text, &langid(language)).into_owned()
    }

    fn fold(text: String, language: u32) -> String {
        fold(&text, language).into_owned()
    }

    fn eq_ignore_case(left: String, right: String, language: u32) -> bool {
        fold(&left, language) == fold(&right, language)
    }
}
