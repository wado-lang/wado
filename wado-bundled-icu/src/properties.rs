//! The shippable `core:icu` stage-1 surface: Unicode character properties as
//! code point ranges. See `wit-properties/world.wit`.

use alloc::string::String;
use alloc::vec::Vec;

wit_bindgen::generate!({
    world: "icu-properties",
    path: "wit-properties",
});

use exports::wado::icu::properties::Guest as PropertiesGuest;

use icu::properties::props::{
    Alnum, Alphabetic, AsciiHexDigit, BidiClass, BidiControl, BidiMirrored, Blank,
    CanonicalCombiningClass, CaseIgnorable, Cased, ChangesWhenCasefolded,
    ChangesWhenCasemapped, ChangesWhenLowercased, ChangesWhenNfkcCasefolded, ChangesWhenTitlecased,
    ChangesWhenUppercased, Dash, DefaultIgnorableCodePoint, Deprecated, Diacritic, EastAsianWidth,
    Emoji, EmojiComponent, EmojiModifier, EmojiModifierBase, EmojiPresentation, ExtendedPictographic,
    Extender, FullCompositionExclusion, GeneralCategory, GeneralCategoryGroup, Graph, GraphemeBase,
    GraphemeClusterBreak, GraphemeExtend, GraphemeLink, HangulSyllableType, HexDigit, Hyphen,
    IdContinue, IdStart, IdCompatMathContinue, IdCompatMathStart, Ideographic, IdsBinaryOperator,
    IdsTrinaryOperator, IdsUnaryOperator, IndicConjunctBreak, IndicSyllabicCategory, JoinControl,
    JoiningGroup, JoiningType, LineBreak, LogicalOrderException, Lowercase, Math,
    ModifierCombiningMark, NoncharacterCodePoint,
    NumericType, PatternSyntax, PatternWhiteSpace, PrependedConcatenationMark, Print, QuotationMark,
    Radical, RegionalIndicator, Script, SentenceBreak, SentenceTerminal, SoftDotted,
    TerminalPunctuation, UnifiedIdeograph, Uppercase, VariationSelector, VerticalOrientation,
    WhiteSpace, WordBreak, Xdigit, XidContinue, XidStart,
};
use icu::properties::{
    CodePointMapData, CodePointSetData, CodePointSetDataBorrowed, PropertyParser,
    props::{BinaryProperty, EnumeratedProperty, ParseableEnumeratedProperty},
};

struct Component;

/// UAX #44 loose matching: case, `_`, `-` and spaces are not significant.
fn fold(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_' && *c != '-' && *c != ' ')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn set_ranges(set: CodePointSetDataBorrowed<'static>) -> Vec<(u32, u32)> {
    set.iter_ranges().map(|r| (*r.start(), *r.end())).collect()
}

fn binary_ranges<P: BinaryProperty>() -> Option<Vec<(u32, u32)>> {
    Some(set_ranges(CodePointSetData::new::<P>()))
}

fn binary_contains<P: BinaryProperty>(ch: char) -> Option<bool> {
    Some(CodePointSetData::new::<P>().contains(ch))
}

fn value_ranges<T>(value: &str) -> Option<Vec<(u32, u32)>>
where
    T: EnumeratedProperty + ParseableEnumeratedProperty,
{
    let value = PropertyParser::<T>::new().get_loose(value)?;
    Some(
        CodePointMapData::<T>::new()
            .iter_ranges_for_value(value)
            .map(|r| (*r.start(), *r.end()))
            .collect(),
    )
}

fn value_contains<T>(value: &str, ch: char) -> Option<bool>
where
    T: EnumeratedProperty + ParseableEnumeratedProperty + PartialEq,
{
    let value = PropertyParser::<T>::new().get_loose(value)?;
    Some(CodePointMapData::<T>::new().get(ch) == value)
}

/// General_Category, whose value may name a group (`L`) as well as a single
/// category (`Lu`); ICU parses both into one mask.
fn category_group(value: &str) -> Option<GeneralCategoryGroup> {
    PropertyParser::<GeneralCategoryGroup>::new().get_loose(value)
}

fn category_ranges(value: &str) -> Option<Vec<(u32, u32)>> {
    let group = category_group(value)?;
    Some(
        CodePointMapData::<GeneralCategory>::new()
            .iter_ranges_for_group(group)
            .map(|r| (*r.start(), *r.end()))
            .collect(),
    )
}

fn category_contains(value: &str, ch: char) -> Option<bool> {
    let group = category_group(value)?;
    Some(group.contains(CodePointMapData::<GeneralCategory>::new().get(ch)))
}

/// Every binary property this build answers, under its folded long and short
/// names. The UCD's contributory `Other_*` properties are deliberately absent,
/// as are the ICU-internal ones that are not UCD properties at all
/// (`*_Inert`, `Segment_Starter`, `Case_Sensitive`).
macro_rules! binary_properties {
    ($($ty:ty => [$($alias:literal),+]),+ $(,)?) => {
        fn binary_property_ranges(key: &str) -> Option<Vec<(u32, u32)>> {
            match key {
                $($($alias)|+ => binary_ranges::<$ty>(),)+
                _ => None,
            }
        }

        fn binary_property_contains(key: &str, ch: char) -> Option<bool> {
            match key {
                $($($alias)|+ => binary_contains::<$ty>(ch),)+
                _ => None,
            }
        }
    };
}

/// Every enumerated property this build answers, under its folded long and
/// short names. `Decomposition_Type` is absent because ICU4X has no data for
/// it.
macro_rules! enumerated_properties {
    ($($ty:ty => [$($alias:literal),+]),+ $(,)?) => {
        fn enumerated_property_ranges(key: &str, value: &str) -> Option<Vec<(u32, u32)>> {
            match key {
                $($($alias)|+ => value_ranges::<$ty>(value),)+
                _ => None,
            }
        }

        fn enumerated_property_contains(key: &str, value: &str, ch: char) -> Option<bool> {
            match key {
                $($($alias)|+ => value_contains::<$ty>(value, ch),)+
                _ => None,
            }
        }
    };
}

binary_properties! {
    Alnum => ["alnum"],
    Alphabetic => ["alphabetic", "alpha"],
    AsciiHexDigit => ["asciihexdigit", "ahex"],
    BidiControl => ["bidicontrol", "bidic"],
    BidiMirrored => ["bidimirrored", "bidim"],
    Blank => ["blank"],
    CaseIgnorable => ["caseignorable", "ci"],
    Cased => ["cased"],
    ChangesWhenCasefolded => ["changeswhencasefolded", "cwcf"],
    ChangesWhenCasemapped => ["changeswhencasemapped", "cwcm"],
    ChangesWhenLowercased => ["changeswhenlowercased", "cwl"],
    ChangesWhenNfkcCasefolded => ["changeswhennfkccasefolded", "cwkcf"],
    ChangesWhenTitlecased => ["changeswhentitlecased", "cwt"],
    ChangesWhenUppercased => ["changeswhenuppercased", "cwu"],
    Dash => ["dash"],
    DefaultIgnorableCodePoint => ["defaultignorablecodepoint", "di"],
    Deprecated => ["deprecated", "dep"],
    Diacritic => ["diacritic", "dia"],
    Emoji => ["emoji"],
    EmojiComponent => ["emojicomponent", "ecomp"],
    EmojiModifier => ["emojimodifier", "emod"],
    EmojiModifierBase => ["emojimodifierbase", "ebase"],
    EmojiPresentation => ["emojipresentation", "epres"],
    ExtendedPictographic => ["extendedpictographic", "extpict"],
    Extender => ["extender", "ext"],
    FullCompositionExclusion => ["fullcompositionexclusion", "compex"],
    Graph => ["graph"],
    GraphemeBase => ["graphemebase", "grbase"],
    GraphemeExtend => ["graphemeextend", "grext"],
    GraphemeLink => ["graphemelink", "grlink"],
    HexDigit => ["hexdigit", "hex"],
    Hyphen => ["hyphen"],
    IdCompatMathContinue => ["idcompatmathcontinue"],
    IdCompatMathStart => ["idcompatmathstart"],
    IdContinue => ["idcontinue", "idc"],
    IdStart => ["idstart", "ids"],
    Ideographic => ["ideographic", "ideo"],
    IdsBinaryOperator => ["idsbinaryoperator", "idsb"],
    IdsTrinaryOperator => ["idstrinaryoperator", "idst"],
    IdsUnaryOperator => ["idsunaryoperator", "idsu"],
    JoinControl => ["joincontrol", "joinc"],
    LogicalOrderException => ["logicalorderexception", "loe"],
    Lowercase => ["lowercase", "lower"],
    Math => ["math"],
    ModifierCombiningMark => ["modifiercombiningmark", "mcm"],
    NoncharacterCodePoint => ["noncharactercodepoint", "nchar"],
    PatternSyntax => ["patternsyntax", "patsyn"],
    PatternWhiteSpace => ["patternwhitespace", "patws"],
    PrependedConcatenationMark => ["prependedconcatenationmark", "pcm"],
    Print => ["print"],
    QuotationMark => ["quotationmark", "qmark"],
    Radical => ["radical"],
    RegionalIndicator => ["regionalindicator", "ri"],
    SentenceTerminal => ["sentenceterminal", "sterm"],
    SoftDotted => ["softdotted", "sd"],
    TerminalPunctuation => ["terminalpunctuation", "term"],
    UnifiedIdeograph => ["unifiedideograph", "uideo"],
    Uppercase => ["uppercase", "upper"],
    VariationSelector => ["variationselector", "vs"],
    WhiteSpace => ["whitespace", "wspace", "space"],
    Xdigit => ["xdigit"],
    XidContinue => ["xidcontinue", "xidc"],
    XidStart => ["xidstart", "xids"],
}

enumerated_properties! {
    BidiClass => ["bidiclass", "bc"],
    CanonicalCombiningClass => ["canonicalcombiningclass", "ccc"],
    EastAsianWidth => ["eastasianwidth", "ea"],
    GraphemeClusterBreak => ["graphemeclusterbreak", "gcb"],
    HangulSyllableType => ["hangulsyllabletype", "hst"],
    IndicConjunctBreak => ["indicconjunctbreak", "incb"],
    IndicSyllabicCategory => ["indicsyllabiccategory", "insc"],
    JoiningGroup => ["joininggroup", "jg"],
    JoiningType => ["joiningtype", "jt"],
    LineBreak => ["linebreak", "lb"],
    NumericType => ["numerictype", "nt"],
    Script => ["script", "sc"],
    SentenceBreak => ["sentencebreak", "sb"],
    VerticalOrientation => ["verticalorientation", "vo"],
    WordBreak => ["wordbreak", "wb"],
}

impl PropertiesGuest for Component {
    fn ranges(property: String, value: Option<String>) -> Option<Vec<(u32, u32)>> {
        let key = fold(&property);
        match value {
            None => binary_property_ranges(&key),
            Some(value) => {
                let value = fold(&value);
                match key.as_str() {
                    "generalcategory" | "gc" => category_ranges(&value),
                    _ => enumerated_property_ranges(&key, &value),
                }
            }
        }
    }

    fn contains(property: String, value: Option<String>, ch: char) -> Option<bool> {
        let key = fold(&property);
        match value {
            None => binary_property_contains(&key, ch),
            Some(value) => {
                let value = fold(&value);
                match key.as_str() {
                    "generalcategory" | "gc" => category_contains(&value, ch),
                    _ => enumerated_property_contains(&key, &value, ch),
                }
            }
        }
    }
}

export!(Component);
