//! The shippable `core:icu` stage-1 surface: Unicode character properties.
//! See `wit-properties/world.wit`.

use alloc::string::String;
use alloc::vec::Vec;
use core::ops::RangeInclusive;

wit_bindgen::generate!({
    world: "icu-properties",
    path: "wit-properties",
});

use exports::wado::icu::properties::Guest as PropertiesGuest;

use icu::properties::props::{
    Alnum, Alphabetic, AsciiHexDigit, BidiClass, BidiControl, BidiMirrored, Blank,
    CanonicalCombiningClass, CaseIgnorable, Cased, ChangesWhenCasefolded, ChangesWhenCasemapped,
    ChangesWhenLowercased, ChangesWhenNfkcCasefolded, ChangesWhenTitlecased, ChangesWhenUppercased,
    Dash, DefaultIgnorableCodePoint, Deprecated, Diacritic, EastAsianWidth, Emoji, EmojiComponent,
    EmojiModifier, EmojiModifierBase, EmojiPresentation, ExtendedPictographic, Extender,
    FullCompositionExclusion, GeneralCategory, GeneralCategoryGroup, Graph, GraphemeBase,
    GraphemeClusterBreak, GraphemeExtend, GraphemeLink, HangulSyllableType, HexDigit, Hyphen,
    IdCompatMathContinue, IdCompatMathStart, IdContinue, IdStart, Ideographic, IdsBinaryOperator,
    IdsTrinaryOperator, IdsUnaryOperator, IndicConjunctBreak, IndicSyllabicCategory, JoinControl,
    JoiningGroup, JoiningType, LineBreak, LogicalOrderException, Lowercase, Math,
    ModifierCombiningMark, NoncharacterCodePoint, NumericType, PatternSyntax, PatternWhiteSpace,
    PrependedConcatenationMark, Print, QuotationMark, Radical, RegionalIndicator, Script,
    SentenceBreak, SentenceTerminal, SoftDotted, TerminalPunctuation, UnifiedIdeograph, Uppercase,
    VariationSelector, VerticalOrientation, WhiteSpace, WordBreak, Xdigit, XidContinue, XidStart,
};
use icu::properties::{
    CodePointMapData, CodePointSetData, PropertyParser,
    props::{BinaryProperty, EnumeratedProperty},
};

struct Component;

/// UAX #44 loose matching: case, `_`, `-` and spaces are not significant.
fn fold(name: &str) -> String {
    name.chars()
        .filter(|c| *c != '_' && *c != '-' && *c != ' ')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn ranges_of(iter: impl Iterator<Item = RangeInclusive<u32>>) -> Vec<(u32, u32)> {
    iter.map(|r| (*r.start(), *r.end())).collect()
}

/// A question put to a property once its name has resolved, asked the same way
/// whatever kind the property turned out to be.
trait Query {
    type Out;

    fn binary<P: BinaryProperty>(&self) -> Self::Out;
    fn enumerated<T: EnumeratedProperty + PartialEq>(&self, value: T) -> Self::Out;
    fn category(&self, group: GeneralCategoryGroup) -> Self::Out;
}

struct Ranges;

impl Query for Ranges {
    type Out = Vec<(u32, u32)>;

    fn binary<P: BinaryProperty>(&self) -> Self::Out {
        ranges_of(CodePointSetData::new::<P>().iter_ranges())
    }

    fn enumerated<T: EnumeratedProperty + PartialEq>(&self, value: T) -> Self::Out {
        ranges_of(CodePointMapData::<T>::new().iter_ranges_for_value(value))
    }

    fn category(&self, group: GeneralCategoryGroup) -> Self::Out {
        ranges_of(CodePointMapData::<GeneralCategory>::new().iter_ranges_for_group(group))
    }
}

struct Contains(char);

impl Query for Contains {
    type Out = bool;

    fn binary<P: BinaryProperty>(&self) -> Self::Out {
        CodePointSetData::new::<P>().contains(self.0)
    }

    fn enumerated<T: EnumeratedProperty + PartialEq>(&self, value: T) -> Self::Out {
        CodePointMapData::<T>::new().get(self.0) == value
    }

    fn category(&self, group: GeneralCategoryGroup) -> Self::Out {
        group.contains(CodePointMapData::<GeneralCategory>::new().get(self.0))
    }
}

macro_rules! binary_properties {
    ($($ty:ty => [$($alias:literal),+]),+ $(,)?) => {
        fn binary_property<Q: Query>(q: &Q, key: &str) -> Option<Q::Out> {
            match key {
                $($($alias)|+ => Some(q.binary::<$ty>()),)+
                _ => None,
            }
        }
    };
}

macro_rules! enumerated_properties {
    ($($ty:ty => [$($alias:literal),+]),+ $(,)?) => {
        fn enumerated_property<Q: Query>(q: &Q, key: &str, value: &str) -> Option<Q::Out> {
            match key {
                $($($alias)|+ => {
                    Some(q.enumerated(PropertyParser::<$ty>::new().get_loose(value)?))
                })+
                _ => None,
            }
        }
    };
}

// The UCD's contributory `Other_*` properties are deliberately absent, as are
// the ICU-internal ones that are not UCD properties at all (`*_Inert`,
// `Segment_Starter`, `Case_Sensitive`).
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

// `Decomposition_Type` is absent because ICU4X has no data for it.
// `General_Category` is not here either: its value may name a group, which
// takes a parser and an iterator of its own.
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

/// Resolve the name, then put `q` to what it named. `None` for a name this
/// build does not answer, never an approximation.
fn query<Q: Query>(q: Q, property: &str, value: Option<String>) -> Option<Q::Out> {
    let key = fold(property);
    let Some(value) = value else {
        return binary_property(&q, &key);
    };
    let value = fold(&value);
    match key.as_str() {
        "generalcategory" | "gc" => {
            Some(q.category(PropertyParser::<GeneralCategoryGroup>::new().get_loose(&value)?))
        }
        _ => enumerated_property(&q, &key, &value),
    }
}

impl PropertiesGuest for Component {
    fn ranges(property: String, value: Option<String>) -> Option<Vec<(u32, u32)>> {
        query(Ranges, &property, value)
    }

    fn contains(property: String, value: Option<String>, ch: char) -> Option<bool> {
        query(Contains(ch), &property, value)
    }
}

export!(Component);
