//! Technical-validation wrapper exposing the string-oriented slice of ICU4X
//! (plus character properties) over a Wasm Component Model interface. Built
//! no_std for wasm32-unknown-unknown so the resulting module imports nothing; a
//! post-build `wasm-tools component new` wraps it into a component. See
//! `wit/world.wit` for the exported surface.

#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

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
use exports::wado::icu::collator::{
    Collator as CollatorHandle, Guest as CollatorGuest, GuestCollator, Ordering as WitOrdering,
};
use exports::wado::icu::locale::{
    Guest as LocaleGuest, GuestLocale, Locale as LocaleHandle, LocaleBorrow,
};
use exports::wado::icu::normalizer::Guest as NormalizerGuest;
use exports::wado::icu::properties::{GeneralCategory as WitGc, Guest as PropertiesGuest};
use exports::wado::icu::segmenter::Guest as SegmenterGuest;

use icu::casemap::{CaseMapper, TitlecaseMapper, options::TitlecaseOptions};
use icu::collator::{Collator, CollatorBorrowed, CollatorPreferences, options::CollatorOptions};
use icu::locale::Locale as IcuLocale;
use icu::normalizer::{ComposingNormalizer, DecomposingNormalizer};
use icu::properties::props::{
    Alphabetic, Emoji, GeneralCategory as IcuGc, Lowercase, Script, Uppercase, WhiteSpace,
};
use icu::properties::{CodePointMapData, CodePointSetData, PropertyNamesShort};
use icu::segmenter::{
    GraphemeClusterSegmenter, LineSegmenter, SentenceSegmenter, WordSegmenter,
    options::{LineBreakOptions, SentenceBreakInvariantOptions, WordBreakInvariantOptions},
};

struct Component;

// --- locale -----------------------------------------------------------------

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

// --- casemap ----------------------------------------------------------------

impl CasemapGuest for Component {
    fn uppercase(text: String, loc: LocaleBorrow<'_>) -> String {
        CaseMapper::new()
            .uppercase_to_string(&text, &loc.get::<LocaleRes>().inner.id)
            .into_owned()
    }

    fn lowercase(text: String, loc: LocaleBorrow<'_>) -> String {
        CaseMapper::new()
            .lowercase_to_string(&text, &loc.get::<LocaleRes>().inner.id)
            .into_owned()
    }

    fn titlecase(text: String, loc: LocaleBorrow<'_>) -> String {
        TitlecaseMapper::new()
            .titlecase_segment_to_string(
                &text,
                &loc.get::<LocaleRes>().inner.id,
                TitlecaseOptions::default(),
            )
            .into_owned()
    }

    fn fold(text: String) -> String {
        CaseMapper::new().fold_string(&text).into_owned()
    }
}

// --- collator ---------------------------------------------------------------

struct CollatorRes {
    inner: CollatorBorrowed<'static>,
}

impl GuestCollator for CollatorRes {
    fn create(loc: LocaleBorrow<'_>) -> Result<CollatorHandle, String> {
        let prefs = CollatorPreferences::from(&loc.get::<LocaleRes>().inner);
        match Collator::try_new(prefs, CollatorOptions::default()) {
            Ok(inner) => Ok(CollatorHandle::new(CollatorRes { inner })),
            Err(e) => Err(e.to_string()),
        }
    }

    fn compare(&self, left: String, right: String) -> WitOrdering {
        match self.inner.compare(&left, &right) {
            core::cmp::Ordering::Less => WitOrdering::Less,
            core::cmp::Ordering::Equal => WitOrdering::Equal,
            core::cmp::Ordering::Greater => WitOrdering::Greater,
        }
    }
}

impl CollatorGuest for Component {
    type Collator = CollatorRes;
}

// --- normalizer -------------------------------------------------------------

impl NormalizerGuest for Component {
    fn nfc(text: String) -> String {
        ComposingNormalizer::new_nfc().normalize(&text).into_owned()
    }

    fn nfd(text: String) -> String {
        DecomposingNormalizer::new_nfd().normalize(&text).into_owned()
    }

    fn nfkc(text: String) -> String {
        ComposingNormalizer::new_nfkc().normalize(&text).into_owned()
    }

    fn nfkd(text: String) -> String {
        DecomposingNormalizer::new_nfkd().normalize(&text).into_owned()
    }

    fn is_nfc(text: String) -> bool {
        ComposingNormalizer::new_nfc().is_normalized(&text)
    }
}

// --- segmenter --------------------------------------------------------------

impl SegmenterGuest for Component {
    fn graphemes(text: String) -> Vec<u32> {
        GraphemeClusterSegmenter::new()
            .segment_str(&text)
            .map(|i| i as u32)
            .collect()
    }

    fn words(text: String) -> Vec<u32> {
        WordSegmenter::new_auto(WordBreakInvariantOptions::default())
            .segment_str(&text)
            .map(|i| i as u32)
            .collect()
    }

    fn sentences(text: String) -> Vec<u32> {
        SentenceSegmenter::new(SentenceBreakInvariantOptions::default())
            .segment_str(&text)
            .map(|i| i as u32)
            .collect()
    }

    fn lines(text: String) -> Vec<u32> {
        LineSegmenter::new_auto(LineBreakOptions::default())
            .segment_str(&text)
            .map(|i| i as u32)
            .collect()
    }
}

// --- properties -------------------------------------------------------------

impl PropertiesGuest for Component {
    fn category(ch: char) -> WitGc {
        match CodePointMapData::<IcuGc>::new().get(ch) {
            IcuGc::Unassigned => WitGc::Unassigned,
            IcuGc::UppercaseLetter => WitGc::UppercaseLetter,
            IcuGc::LowercaseLetter => WitGc::LowercaseLetter,
            IcuGc::TitlecaseLetter => WitGc::TitlecaseLetter,
            IcuGc::ModifierLetter => WitGc::ModifierLetter,
            IcuGc::OtherLetter => WitGc::OtherLetter,
            IcuGc::NonspacingMark => WitGc::NonspacingMark,
            IcuGc::SpacingMark => WitGc::SpacingMark,
            IcuGc::EnclosingMark => WitGc::EnclosingMark,
            IcuGc::DecimalNumber => WitGc::DecimalNumber,
            IcuGc::LetterNumber => WitGc::LetterNumber,
            IcuGc::OtherNumber => WitGc::OtherNumber,
            IcuGc::SpaceSeparator => WitGc::SpaceSeparator,
            IcuGc::LineSeparator => WitGc::LineSeparator,
            IcuGc::ParagraphSeparator => WitGc::ParagraphSeparator,
            IcuGc::Control => WitGc::Control,
            IcuGc::Format => WitGc::Format,
            IcuGc::PrivateUse => WitGc::PrivateUse,
            IcuGc::Surrogate => WitGc::Surrogate,
            IcuGc::DashPunctuation => WitGc::DashPunctuation,
            IcuGc::OpenPunctuation => WitGc::OpenPunctuation,
            IcuGc::ClosePunctuation => WitGc::ClosePunctuation,
            IcuGc::ConnectorPunctuation => WitGc::ConnectorPunctuation,
            IcuGc::InitialPunctuation => WitGc::InitialPunctuation,
            IcuGc::FinalPunctuation => WitGc::FinalPunctuation,
            IcuGc::OtherPunctuation => WitGc::OtherPunctuation,
            IcuGc::MathSymbol => WitGc::MathSymbol,
            IcuGc::CurrencySymbol => WitGc::CurrencySymbol,
            IcuGc::ModifierSymbol => WitGc::ModifierSymbol,
            IcuGc::OtherSymbol => WitGc::OtherSymbol,
        }
    }

    fn script(ch: char) -> String {
        let script = CodePointMapData::<Script>::new().get(ch);
        PropertyNamesShort::<Script>::new()
            .get(script)
            .unwrap_or("Zzzz")
            .to_string()
    }

    fn alphabetic(ch: char) -> bool {
        CodePointSetData::new::<Alphabetic>().contains(ch)
    }

    fn white_space(ch: char) -> bool {
        CodePointSetData::new::<WhiteSpace>().contains(ch)
    }

    fn uppercase(ch: char) -> bool {
        CodePointSetData::new::<Uppercase>().contains(ch)
    }

    fn lowercase(ch: char) -> bool {
        CodePointSetData::new::<Lowercase>().contains(ch)
    }

    fn emoji(ch: char) -> bool {
        CodePointSetData::new::<Emoji>().contains(ch)
    }
}

export!(Component);
