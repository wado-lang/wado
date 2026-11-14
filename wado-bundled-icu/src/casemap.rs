//! Case mapping, full and simple. See `wit-properties/world.wit`.

mod fold_classes;

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use icu::casemap::CaseMapper;
use icu::collections::codepointinvlist::CodePointInversionListBuilder;
use icu::locale::subtags::{Language, language};

use crate::locale;
use crate::properties::{Component, ranges_of};
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

    fn simple_lowercase(ch: char) -> char {
        CaseMapper::new().simple_lowercase(ch)
    }

    fn simple_uppercase(ch: char) -> char {
        CaseMapper::new().simple_uppercase(ch)
    }

    fn simple_titlecase(ch: char) -> char {
        CaseMapper::new().simple_titlecase(ch)
    }

    fn simple_fold(ch: char) -> char {
        CaseMapper::new().simple_fold(ch)
    }

    fn simple_fold_closure(ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
        let mut builder = CodePointInversionListBuilder::new();
        for (lo, hi) in ranges {
            builder.add_range32(lo..=hi);
        }
        let input = builder.build();
        let mut closure = CodePointInversionListBuilder::new();
        closure.add_set(&input);
        for class in fold_classes::CLASSES {
            if class.iter().any(|&c| input.contains32(c)) {
                class.iter().for_each(|&c| closure.add32(c));
            }
        }
        ranges_of(closure.build().iter_ranges())
    }
}
