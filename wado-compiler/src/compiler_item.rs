//! Compiler-recognized stdlib items (the Wado compiler's analogue of
//! rustc's lang items).
//!
//! A `CompilerItem` identifies a specific stdlib symbol that the Wado
//! compiler — Rust-side — needs to reference directly: types it
//! instantiates (`Option<T>`, `Box<T>`), traits whose impls it
//! synthesises (`Default`, `From`, `Serialize`), or methods it lowers
//! into calls (`String::push_str`, `List::push`). Each item is bound
//! to its resolution by a `#[compiler_item("...")]` attribute on the
//! Wado-side declaration; the elaborator populates the
//! [`CompilerItems`] registry during the annotate phase and downstream
//! passes look symbols up through the registry instead of by string
//! name.
//!
//! Why this exists: pre-`CompilerItem`, the compiler hard-coded paths
//! and names (`LocalMethodName::new("String", None, "push_str")`,
//! `name == "Option"`) at every site. Renaming a stdlib item would
//! silently break those sites — failures showed up only at runtime as
//! "unresolved `MethodCall`" errors. With the registry, renames on the
//! Wado side are invisible to the Rust side as long as the
//! `#[compiler_item("...")]` value stays put; renaming the
//! `compiler_item` value, in turn, fails the compiler's own build by
//! breaking the enum variant ↔ string mapping in
//! [`CompilerItem::from_attr_name`].
//!
//! Scope: `#[compiler_item("...")]` is only meaningful inside
//! `core::*` modules. The elaborator rejects the attribute on user code.

use std::fmt;

use crate::module_source::ModuleSource;

/// The two fields of the `List` / `String` sequence containers, which share a
/// `{ repr: array<T>, used: i32 }` layout: an owned backing array plus the
/// count of valid elements (`used`, the value `len()` returns).
///
/// Passes that construct or match these structs name the fields through
/// [`SeqField::field_name`] instead of string literals, mirroring how
/// [`CompilerItem`] insulates the Rust side from stdlib renames — renaming a
/// container field on the Wado side is then a one-line change here.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SeqField {
    /// `repr` — the owned backing `array<T>`. Field index 0.
    Backing,
    /// `used` — the count of valid elements (the current length). Field index 1.
    Len,
}

impl SeqField {
    /// The Wado-side field name. The single source of truth for the `List` /
    /// `String` container field names across the compiler.
    #[must_use]
    pub const fn field_name(self) -> &'static str {
        match self {
            Self::Backing => "repr",
            Self::Len => "used",
        }
    }

    /// The field's positional index within the container struct.
    #[must_use]
    pub const fn index(self) -> u32 {
        match self {
            Self::Backing => 0,
            Self::Len => 1,
        }
    }
}

/// Every compiler-recognized stdlib item.
///
/// Each variant has a canonical `snake_case` name (see
/// [`CompilerItem::attr_name`]) that must match the argument of the
/// `#[compiler_item("...")]` attribute on the Wado-side declaration.
///
/// Naming convention: flat `snake_case`. Methods use
/// `<type>_<method>` (e.g. [`Self::StringPushStr`]); types and traits
/// use the lowercase type/trait name; variant cases use
/// `<variant>_<case>`. The convention is enforced socially, not by
/// the parser — the parser only checks that the argument names a
/// known [`CompilerItem`] variant.
#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CompilerItem {
    // ── Types (structs / generic structs) ────────────────────────────
    /// `List<T>` — the heap-backed sequence struct. Recognised by
    /// the CM lift, value-copy synthesis, and serde codegen so the
    /// compiler always points at the right concrete `List` struct.
    List,
    /// `Box<T>` — boxes primitive values into a struct that
    /// participates in GC tracing.
    Box,
    /// `i128` — the signed 128-bit wide-int struct.
    I128,
    /// `u128` — the unsigned 128-bit wide-int struct.
    U128,
    /// `RangeExclusive<T>` — `a..b` literals.
    RangeExclusive,
    /// `RangeInclusive<T>` — `a..=b` literals.
    RangeInclusive,
    /// `Request<T>` — the Kiln CM adapter wraps decoded options in
    /// this struct.
    KilnRequest,
    /// `String` — the GC-backed UTF-8 string struct. Synthesised
    /// codegen (CM binding, serde, format) refers to it through this
    /// item so the Rust side never hard-codes the struct name.
    String,

    // ── Variants (sum types) ──────────────────────────────────────────
    /// `Option<T>` — `Some(_)` / `None`.
    Option,
    /// `Result<T, E>` — `Ok(_)` / `Err(_)`.
    Result,

    // ── Enums (sum types without payload) ─────────────────────────────
    /// `Ordering` — return type of `Ord::cmp`. Recognised by operator
    /// dispatch and by the trait-synthesis pass that emits
    /// `T^Ord::cmp` bodies.
    Ordering,

    // ── Traits ────────────────────────────────────────────────────────
    /// `Default` — `Default::default()` synthesis anchor.
    Default,
    /// `Eq` — anchor for synthesised `==` / `!=` lowering and the
    /// auto-derive checks that decide whether a compound type
    /// (struct, variant, generic instance) implements `Eq`.
    Eq,
    /// `Ord` — anchor for synthesised `<` / `>` / `<=` / `>=`
    /// lowering and for auto-derived `T^Ord::cmp` bodies.
    Ord,
    /// `From<T>` — synthesised by the `From` synthesiser.
    From,
    /// `core:serde::Serialize` — anchor for `Serialize` impl synthesis.
    Serialize,
    /// `core:serde::Deserialize` — anchor for `Deserialize` impl
    /// synthesis and for the Kiln CM adapter's options decoding.
    Deserialize,
    /// `core:serde::Serializer` — supertrait bound on synthesised
    /// `serialize<S: Serializer>` methods.
    Serializer,
    /// `core:serde::Deserializer` — supertrait bound on synthesised
    /// `deserialize<D: Deserializer>` methods.
    Deserializer,
    /// `core:serde::SerializeStruct` — associated-type bound used when
    /// the synthesised struct serializer projects `S::StructSerializer`.
    SerializeStruct,
    /// `core:serde::SerializeSeq` — associated-type bound used when the
    /// synthesised array / tuple serializer projects `S::SeqSerializer`.
    SerializeSeq,
    /// `core:serde::SerializeVariant` — associated-type bound used when
    /// the synthesised variant serializer projects `S::VariantSerializer`.
    SerializeVariant,
    /// `core:serde::DeserializeStruct` — associated-type bound used when
    /// the synthesised struct deserializer projects `D::StructAccess`.
    DeserializeStruct,
    /// `core:serde::DeserializeSeq` — associated-type bound used when
    /// the synthesised array / tuple deserializer projects `D::SeqAccess`.
    DeserializeSeq,
    /// `core:serde::DeserializeVariant` — associated-type bound used when
    /// the synthesised variant deserializer projects `D::VariantAccess`.
    DeserializeVariant,
    /// `core:serde::FieldSchema` — bound on `DeserializeStruct::next_field`'s
    /// type parameter. The struct-deserialize synthesiser implements it for
    /// each deserializable struct, providing the static, allocation-free
    /// field-name → index `lookup` that self-describing formats invoke.
    FieldSchema,
    /// `core:prelude/traits::Display` — anchor for `{x}` template-string
    /// dispatch and the auto-derive Display→Inspect fallback.
    Display,
    /// `core:prelude/traits::DisplayAlt` — `{x:#}` dispatch.
    DisplayAlt,
    /// `core:prelude/traits::Inspect` — `{x:?}` dispatch.
    Inspect,
    /// `core:prelude/traits::InspectAlt` — `{x:#?}` dispatch.
    InspectAlt,
    /// `core:prelude/traits::Binary` — `{x:b}` dispatch.
    Binary,
    /// `core:prelude/traits::BinaryAlt` — `{x:#b}` dispatch.
    BinaryAlt,
    /// `core:prelude/traits::Octal` — `{x:o}` dispatch.
    Octal,
    /// `core:prelude/traits::OctalAlt` — `{x:#o}` dispatch.
    OctalAlt,
    /// `core:prelude/traits::LowerHex` — `{x:x}` dispatch.
    LowerHex,
    /// `core:prelude/traits::LowerHexAlt` — `{x:#x}` dispatch.
    LowerHexAlt,
    /// `core:prelude/traits::UpperHex` — `{x:X}` dispatch.
    UpperHex,
    /// `core:prelude/traits::UpperHexAlt` — `{x:#X}` dispatch.
    UpperHexAlt,
    /// `core:prelude/traits::LowerExp` — `{x:e}` dispatch.
    LowerExp,
    /// `core:prelude/traits::UpperExp` — `{x:E}` dispatch.
    UpperExp,

    // ── Format types (structs / enums) ─────────────────────────────────
    /// `core:prelude/format::Formatter` — the format target struct.
    Formatter,
    /// `core:prelude/format::Alignment` — alignment enum used by the
    /// formatter's pad path.
    Alignment,
    /// `core:serde::SerializeError` — error struct returned from every
    /// synthesised `Serializer` method.
    SerializeError,
    /// `core:serde::SerializeErrorKind` — error kind enum used by the
    /// synthesised error-literal helpers.
    SerializeErrorKind,
    /// `core:serde::DeserializeError` — error struct returned from every
    /// synthesised `Deserializer` method.
    DeserializeError,
    /// `core:serde::DeserializeErrorKind` — error kind enum used by the
    /// synthesised error-literal helpers.
    DeserializeErrorKind,

    // ── Variant cases ─────────────────────────────────────────────────
    /// `Option::Some` — anchors the case-name / case-index pair used by
    /// every site that constructs or matches `Some(_)`.
    OptionSome,
    /// `Option::None` — anchors the case-name / case-index pair used by
    /// every site that constructs or matches `None`.
    OptionNone,
    /// `Result::Ok` — anchors the case-name / case-index pair used by
    /// every site that constructs or matches `Ok(_)`.
    ResultOk,
    /// `Result::Err` — anchors the case-name / case-index pair used by
    /// every site that constructs or matches `Err(_)`.
    ResultErr,
    /// `Ordering::Less` — anchors the case-name / case-index pair used
    /// by operator dispatch lowering for `<` / `<=`.
    OrderingLess,
    /// `Ordering::Equal` — anchors the case-name / case-index pair used
    /// by operator dispatch lowering for `==` / `!=`.
    OrderingEqual,
    /// `Ordering::Greater` — anchors the case-name / case-index pair
    /// used by operator dispatch lowering for `>` / `>=`.
    OrderingGreater,
    /// `SerializeErrorKind::Custom` — anchors the case-name / case-index
    /// pair used by `serialize_error_literal`.
    SerializeErrorKindCustom,
    /// `DeserializeErrorKind::MissingField` — anchors the case-name /
    /// case-index pair used by `deserialize_error_literal`.
    DeserializeErrorKindMissingField,
    /// `DeserializeErrorKind::UnknownVariant` — anchors the case-name /
    /// case-index pair used by `deserialize_error_literal`.
    DeserializeErrorKindUnknownVariant,
    /// `DeserializeErrorKind::InvalidValue` — anchors the case-name /
    /// case-index pair used by `deserialize_error_literal`.
    DeserializeErrorKindInvalidValue,
    /// `Alignment::Left` — anchors the case-name / case-index pair used
    /// by template-string padding lowering.
    AlignmentLeft,
    /// `Alignment::Center` — anchors the case-name / case-index pair
    /// used by template-string padding lowering.
    AlignmentCenter,
    /// `Alignment::Right` — anchors the case-name / case-index pair used
    /// by template-string padding lowering.
    AlignmentRight,

    // ── Methods (impl-block functions) ────────────────────────────────
    /// `List<T>::push` — recognised by the WIR optimiser to collapse
    /// `List::new` + a sequence of `.push(...)` calls into
    /// `array.new_fixed`.
    ListPush,
    /// `String::push_str` — recognised by the WIR optimiser for
    /// string-building inlining.
    StringPushStr,
    /// `String::push_char` — recognised by the WIR optimiser for
    /// string-building inlining.
    StringPushChar,
    /// `String::get_byte_unchecked` — the unchecked byte read helper
    /// used by synthesised deserializers (`serde_synth`). Routed
    /// through this item so renames in the stdlib do not silently
    /// break code generation. See issue #1077.
    StringGetByteUnchecked,
    /// `ByteSlice::get_unchecked` (`ArraySlice<u8>`) — the unchecked byte
    /// read used by synthesised deserializers (`serde_synth`) when comparing
    /// a wire key against a struct's `FieldSchema`. Routed through this item
    /// for the same rename-safety reason as [`Self::StringGetByteUnchecked`].
    ByteSliceGetUnchecked,
    /// `ByteSlice::len` (`ArraySlice<u8>`) — the byte-length accessor the
    /// synthesised `FieldSchema::lookup` uses to size the wire key. Routed
    /// through a compiler item for the same rename-safety reason as
    /// [`Self::ByteSliceGetUnchecked`].
    ByteSliceLen,
    /// `i128::from_i64` — sign-extending constructor used by the
    /// wide-int literal lowering pass.
    I128FromI64,
    /// `i128::from_pair` — low/high pair constructor used by the
    /// wide-int literal lowering pass.
    I128FromPair,
    /// `u128::from_u64` — zero-extending constructor used by the
    /// wide-int literal lowering pass.
    U128FromU64,
    /// `u128::from_pair` — low/high pair constructor used by the
    /// wide-int literal lowering pass.
    U128FromPair,

    // ── Type families ─────────────────────────────────────────────────
    /// The tuple type family (`pub type [..T];`). The owning module is
    /// recorded so the compiler can synthesise tuple types under the
    /// correct module path.
    Tuple,
    /// `Array<T>` — the raw GC array, declared definition-less
    /// (`pub type Array<T>;`). The owning module and the user-facing
    /// name are recorded so the type resolver can bind the name to the
    /// `ResolvedType::BuiltinArray` builtin.
    Array,
}

impl CompilerItem {
    /// Every variant, in declaration order. Used by validation passes
    /// that need to check the full registry.
    pub const ALL: &'static [CompilerItem] = &[
        Self::List,
        Self::Box,
        Self::I128,
        Self::U128,
        Self::RangeExclusive,
        Self::RangeInclusive,
        Self::KilnRequest,
        Self::String,
        Self::Option,
        Self::Result,
        Self::Ordering,
        Self::Default,
        Self::Eq,
        Self::Ord,
        Self::From,
        Self::Serialize,
        Self::Deserialize,
        Self::Serializer,
        Self::Deserializer,
        Self::SerializeStruct,
        Self::SerializeSeq,
        Self::SerializeVariant,
        Self::DeserializeStruct,
        Self::DeserializeSeq,
        Self::DeserializeVariant,
        Self::FieldSchema,
        Self::Display,
        Self::DisplayAlt,
        Self::Inspect,
        Self::InspectAlt,
        Self::Binary,
        Self::BinaryAlt,
        Self::Octal,
        Self::OctalAlt,
        Self::LowerHex,
        Self::LowerHexAlt,
        Self::UpperHex,
        Self::UpperHexAlt,
        Self::LowerExp,
        Self::UpperExp,
        Self::Formatter,
        Self::Alignment,
        Self::SerializeError,
        Self::SerializeErrorKind,
        Self::DeserializeError,
        Self::DeserializeErrorKind,
        Self::OptionSome,
        Self::OptionNone,
        Self::ResultOk,
        Self::ResultErr,
        Self::OrderingLess,
        Self::OrderingEqual,
        Self::OrderingGreater,
        Self::SerializeErrorKindCustom,
        Self::DeserializeErrorKindMissingField,
        Self::DeserializeErrorKindUnknownVariant,
        Self::DeserializeErrorKindInvalidValue,
        Self::AlignmentLeft,
        Self::AlignmentCenter,
        Self::AlignmentRight,
        Self::ListPush,
        Self::StringPushStr,
        Self::StringPushChar,
        Self::StringGetByteUnchecked,
        Self::ByteSliceGetUnchecked,
        Self::ByteSliceLen,
        Self::I128FromI64,
        Self::I128FromPair,
        Self::U128FromU64,
        Self::U128FromPair,
        Self::Tuple,
        Self::Array,
    ];

    /// Total number of variants. `ALL.len()` at compile time.
    pub const COUNT: usize = Self::ALL.len();

    /// Snake-case name used as the argument of
    /// `#[compiler_item("...")]`.
    ///
    /// This is the canonical wire form; renaming this string is a
    /// breaking change because stdlib annotations and Rust-side
    /// references must agree.
    pub fn attr_name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Box => "box",
            Self::I128 => "i128",
            Self::U128 => "u128",
            Self::RangeExclusive => "range_exclusive",
            Self::RangeInclusive => "range_inclusive",
            Self::KilnRequest => "kiln_request",
            Self::String => "string",
            Self::Option => "option",
            Self::Result => "result",
            Self::Ordering => "ordering",
            Self::Default => "default",
            Self::Eq => "eq",
            Self::Ord => "ord",
            Self::From => "from",
            Self::Serialize => "serialize",
            Self::Deserialize => "deserialize",
            Self::Serializer => "serializer",
            Self::Deserializer => "deserializer",
            Self::SerializeStruct => "serialize_struct",
            Self::SerializeSeq => "serialize_seq",
            Self::SerializeVariant => "serialize_variant",
            Self::DeserializeStruct => "deserialize_struct",
            Self::DeserializeSeq => "deserialize_seq",
            Self::DeserializeVariant => "deserialize_variant",
            Self::FieldSchema => "field_schema",
            Self::Display => "display",
            Self::DisplayAlt => "display_alt",
            Self::Inspect => "inspect",
            Self::InspectAlt => "inspect_alt",
            Self::Binary => "binary",
            Self::BinaryAlt => "binary_alt",
            Self::Octal => "octal",
            Self::OctalAlt => "octal_alt",
            Self::LowerHex => "lower_hex",
            Self::LowerHexAlt => "lower_hex_alt",
            Self::UpperHex => "upper_hex",
            Self::UpperHexAlt => "upper_hex_alt",
            Self::LowerExp => "lower_exp",
            Self::UpperExp => "upper_exp",
            Self::Formatter => "formatter",
            Self::Alignment => "alignment",
            Self::SerializeError => "serialize_error",
            Self::SerializeErrorKind => "serialize_error_kind",
            Self::DeserializeError => "deserialize_error",
            Self::DeserializeErrorKind => "deserialize_error_kind",
            Self::OptionSome => "option_some",
            Self::OptionNone => "option_none",
            Self::ResultOk => "result_ok",
            Self::ResultErr => "result_err",
            Self::OrderingLess => "ordering_less",
            Self::OrderingEqual => "ordering_equal",
            Self::OrderingGreater => "ordering_greater",
            Self::SerializeErrorKindCustom => "serialize_error_kind_custom",
            Self::DeserializeErrorKindMissingField => "deserialize_error_kind_missing_field",
            Self::DeserializeErrorKindUnknownVariant => "deserialize_error_kind_unknown_variant",
            Self::DeserializeErrorKindInvalidValue => "deserialize_error_kind_invalid_value",
            Self::AlignmentLeft => "alignment_left",
            Self::AlignmentCenter => "alignment_center",
            Self::AlignmentRight => "alignment_right",
            Self::ListPush => "list_push",
            Self::StringPushStr => "string_push_str",
            Self::StringPushChar => "string_push_char",
            Self::StringGetByteUnchecked => "string_get_byte_unchecked",
            Self::ByteSliceGetUnchecked => "byte_slice_get_unchecked",
            Self::ByteSliceLen => "byte_slice_len",
            Self::I128FromI64 => "i128_from_i64",
            Self::I128FromPair => "i128_from_pair",
            Self::U128FromU64 => "u128_from_u64",
            Self::U128FromPair => "u128_from_pair",
            Self::Tuple => "tuple",
            Self::Array => "array",
        }
    }

    /// Resolve a `#[compiler_item("...")]` attribute argument back to
    /// the typed variant. Returns `None` for unknown names; the
    /// caller emits a diagnostic.
    pub fn from_attr_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|i| i.attr_name() == name)
    }

    /// Whether the registry must contain this item by the end of
    /// resolution for compilation of `world` to succeed.
    ///
    /// Items declared in `core::prelude::*` are always required — they
    /// underpin the core type system and the prelude is auto-loaded
    /// regardless of target world. Items declared in worlds the user
    /// did not opt into (e.g. `core:kiln/generator` for
    /// [`Self::KilnRequest`], `core:serde` for [`Self::Serialize`] /
    /// [`Self::Deserialize`]) are only required when the matching
    /// world / module is actually loaded; tracking that here keeps the
    /// validator from spuriously failing in CLI / HTTP builds that
    /// don't pull in kiln or serde.
    ///
    /// The validator scans this method for every [`Self::ALL`]
    /// variant after `annotate_modules` runs; a `true` return on an
    /// unregistered item is a hard error at compile time, surfacing
    /// the missing stdlib annotation immediately rather than at the
    /// first synthesis call that reaches for it.
    pub fn is_required(self, world: &str) -> bool {
        match self {
            // Always loaded — `core:prelude` is auto-imported.
            Self::List
            | Self::Box
            | Self::I128
            | Self::U128
            | Self::RangeExclusive
            | Self::RangeInclusive
            | Self::String
            | Self::Option
            | Self::Result
            | Self::Ordering
            | Self::Default
            | Self::Eq
            | Self::Ord
            | Self::From
            | Self::ListPush
            | Self::StringPushStr
            | Self::StringPushChar
            | Self::StringGetByteUnchecked
            | Self::ByteSliceGetUnchecked
            | Self::ByteSliceLen
            | Self::I128FromI64
            | Self::I128FromPair
            | Self::U128FromU64
            | Self::U128FromPair
            | Self::Tuple
            | Self::Array
            // Variant cases of always-loaded variants/enums travel with
            // their parents — Option, Result, and Ordering are all in
            // the auto-loaded prelude, so their cases are always
            // required too.
            | Self::OptionSome
            | Self::OptionNone
            | Self::ResultOk
            | Self::ResultErr
            | Self::OrderingLess
            | Self::OrderingEqual
            | Self::OrderingGreater
            // The format module is part of `core:prelude` so its
            // structs / enums / traits / cases are always required.
            | Self::Formatter
            | Self::Alignment
            | Self::AlignmentLeft
            | Self::AlignmentCenter
            | Self::AlignmentRight
            | Self::Display
            | Self::DisplayAlt
            | Self::Inspect
            | Self::InspectAlt
            | Self::Binary
            | Self::BinaryAlt
            | Self::Octal
            | Self::OctalAlt
            | Self::LowerHex
            | Self::LowerHexAlt
            | Self::UpperHex
            | Self::UpperHexAlt
            | Self::LowerExp
            | Self::UpperExp => true,
            // Kiln generator world only.
            Self::KilnRequest => world == "core:kiln/generator",
            // Loaded only when the user imports `core:serde` (which
            // happens implicitly for kiln-options decoding). The
            // validator skips the check; downstream synthesis ICEs
            // with a clear message if the items are reached for
            // without being registered.
            Self::Serialize
            | Self::Deserialize
            | Self::Serializer
            | Self::Deserializer
            | Self::SerializeStruct
            | Self::SerializeSeq
            | Self::SerializeVariant
            | Self::DeserializeStruct
            | Self::DeserializeSeq
            | Self::DeserializeVariant
            | Self::FieldSchema
            | Self::SerializeError
            | Self::SerializeErrorKind
            | Self::DeserializeError
            | Self::DeserializeErrorKind
            | Self::SerializeErrorKindCustom
            | Self::DeserializeErrorKindMissingField
            | Self::DeserializeErrorKindUnknownVariant
            | Self::DeserializeErrorKindInvalidValue => false,
        }
    }

    /// The kind of declaration this item must be attached to. Used by
    /// the elaborator to reject misuse like
    /// `#[compiler_item("option")]` on a trait.
    pub fn expected_kind(self) -> CompilerItemKind {
        match self {
            Self::List
            | Self::Box
            | Self::I128
            | Self::U128
            | Self::RangeExclusive
            | Self::RangeInclusive
            | Self::KilnRequest
            | Self::String => CompilerItemKind::Struct,
            Self::Option | Self::Result => CompilerItemKind::Variant,
            Self::Ordering | Self::Alignment => CompilerItemKind::Enum,
            Self::SerializeError | Self::DeserializeError => CompilerItemKind::Struct,
            Self::SerializeErrorKind | Self::DeserializeErrorKind => CompilerItemKind::Enum,
            Self::Formatter => CompilerItemKind::Struct,
            Self::Default
            | Self::Eq
            | Self::Ord
            | Self::From
            | Self::Serialize
            | Self::Deserialize
            | Self::Serializer
            | Self::Deserializer
            | Self::SerializeStruct
            | Self::SerializeSeq
            | Self::SerializeVariant
            | Self::DeserializeStruct
            | Self::DeserializeSeq
            | Self::DeserializeVariant
            | Self::FieldSchema
            | Self::Display
            | Self::DisplayAlt
            | Self::Inspect
            | Self::InspectAlt
            | Self::Binary
            | Self::BinaryAlt
            | Self::Octal
            | Self::OctalAlt
            | Self::LowerHex
            | Self::LowerHexAlt
            | Self::UpperHex
            | Self::UpperHexAlt
            | Self::LowerExp
            | Self::UpperExp => CompilerItemKind::Trait,
            Self::ListPush
            | Self::StringPushStr
            | Self::StringPushChar
            | Self::StringGetByteUnchecked
            | Self::ByteSliceGetUnchecked
            | Self::ByteSliceLen
            | Self::I128FromI64
            | Self::I128FromPair
            | Self::U128FromU64
            | Self::U128FromPair => CompilerItemKind::Method,
            Self::Tuple => CompilerItemKind::TupleFamily,
            Self::Array => CompilerItemKind::BuiltinType,
            Self::OptionSome | Self::OptionNone | Self::ResultOk | Self::ResultErr => {
                CompilerItemKind::VariantCase
            }
            Self::OrderingLess
            | Self::OrderingEqual
            | Self::OrderingGreater
            | Self::AlignmentLeft
            | Self::AlignmentCenter
            | Self::AlignmentRight
            | Self::SerializeErrorKindCustom
            | Self::DeserializeErrorKindMissingField
            | Self::DeserializeErrorKindUnknownVariant
            | Self::DeserializeErrorKindInvalidValue => CompilerItemKind::EnumCase,
        }
    }
}

impl fmt::Display for CompilerItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.attr_name())
    }
}

/// The category of declaration a [`CompilerItem`] is attached to.
///
/// Kept separate from [`CompilerItem`] so the elaborator can validate
/// the attribute placement (e.g. `Option` must be on a `variant`,
/// `Default` must be on a `trait`) and so the registry can offer
/// kind-specific accessors.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum CompilerItemKind {
    /// A `struct` declaration (`Box`, `Range*`, `KilnRequest`).
    Struct,
    /// A `variant` declaration (`Option`, `Result`).
    Variant,
    /// An `enum` declaration (`Ordering`).
    Enum,
    /// A `trait` declaration.
    Trait,
    /// A method inside an `impl` block.
    Method,
    /// The `pub type [..T];` declaration that owns the tuple family.
    TupleFamily,
    /// A named, definition-less builtin type (`pub type Array<T>;`).
    BuiltinType,
    /// One case of a `variant` declaration (e.g. `Option::Some`).
    VariantCase,
    /// One case of an `enum` declaration (e.g. `Ordering::Less`).
    EnumCase,
}

impl fmt::Display for CompilerItemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Struct => "struct",
            Self::Variant => "variant",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Method => "method",
            Self::TupleFamily => "tuple type family",
            Self::BuiltinType => "builtin type",
            Self::VariantCase => "variant case",
            Self::EnumCase => "enum case",
        })
    }
}

/// The resolved data for a registered [`CompilerItem`].
/// One associated type declaration captured from a trait registered
/// via `#[compiler_item("...")]`. Carries the source-side name plus
/// the source-side names of all trait bounds (e.g.
/// `type SeqSerializer: SerializeSeq;` →
/// `{ name: "SeqSerializer", bound_names: ["SerializeSeq"] }`).
///
/// The bound list is what makes
/// [`CompilerItems::trait_assoc_type_by_bound`] stable across stdlib
/// renames: the synthesiser asks "which assoc type is bound by
/// `SerializeSeq`?" instead of "is there an assoc type named
/// `SeqSerializer`?".
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraitAssocType {
    pub name: String,
    pub bound_names: Vec<String>,
}

///
/// One enum with a variant per [`CompilerItemKind`]. Each variant
/// carries enough information for downstream consumers to reconstruct
/// the lookup keys they need — most often the [`ModuleSource`] plus
/// the declared name (and, for methods, the owning type's name).
///
/// Consumers should generally not match on `Resolved` directly; use
/// the kind-checked accessors on [`CompilerItems`]
/// (e.g. [`CompilerItems::require_trait_module`]) instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Resolved {
    Struct {
        module_source: ModuleSource,
        name: String,
    },
    Variant {
        module_source: ModuleSource,
        name: String,
    },
    Enum {
        module_source: ModuleSource,
        name: String,
    },
    Trait {
        module_source: ModuleSource,
        name: String,
        /// Primary method name for **single-method** traits, captured
        /// automatically by the elaborator when registering the trait's
        /// `#[compiler_item("...")]` annotation. `None` for
        /// multi-method traits (`Serializer`, `Deserializer`, …).
        ///
        /// The format-family traits (`Display` / `DisplayAlt` /
        /// `Inspect` / `InspectAlt` / `Binary` / `BinaryAlt` / `Octal`
        /// / `OctalAlt` / `LowerHex` / `LowerHexAlt` / `UpperHex` /
        /// `UpperHexAlt` / `LowerExp` / `UpperExp`) all have exactly
        /// one method by design, so the synthesiser can look up the
        /// method name from the trait registration alone instead of
        /// hard-coding `"fmt"` / `"inspect"` at every call site.
        ///
        /// Reading: [`CompilerItems::trait_method_name`].
        method_name: Option<String>,
        /// Associated types declared on the trait, in source order.
        /// Each entry pairs the assoc type's source-side name with the
        /// source-side names of its trait bounds (e.g. for
        /// `type SeqSerializer: SerializeSeq;` the entry is
        /// `TraitAssocType { name: "SeqSerializer", bound_names: ["SerializeSeq"] }`).
        /// Auto-captured by the elaborator when registering the trait's
        /// `#[compiler_item("...")]` annotation; empty for traits
        /// without associated types.
        ///
        /// The serde synthesiser identifies each assoc type by its
        /// **bound trait** (itself a `#[compiler_item("...")]`-
        /// registered trait whose current spelling comes from the
        /// registry), not by its source-side spelling. That makes both
        /// ends rename-stable: renaming `SeqSerializer` to `SeqWriter`
        /// or renaming the bound `SerializeSeq` to `SeqWriterTrait`
        /// both keep flowing through the registry instead of panicking
        /// on a missing source-side name.
        ///
        /// Reading: [`CompilerItems::trait_assoc_type_by_bound`].
        assoc_types: Vec<TraitAssocType>,
    },
    Method {
        module_source: ModuleSource,
        /// The type the method is defined on (e.g. `"String"`).
        owner_type: String,
        name: String,
    },
    /// The module that owns the tuple type family. Tuples have no
    /// user-visible declared name on the Wado side, only an owning
    /// module; the [`ModuleSource`] is therefore the only payload.
    TupleFamily { module_source: ModuleSource },
    /// A named, definition-less builtin type (`Array<T>`). Carries the
    /// owning module and the user-facing name so the type resolver can
    /// bind the name to its builtin `ResolvedType`.
    BuiltinType {
        module_source: ModuleSource,
        name: String,
    },
    /// One case of a `variant` declaration. Carries the owning
    /// variant's name (`Option`, `Result`) so downstream consumers can
    /// reconstruct the `TirPattern::Variant.enum_type` / lookup keys,
    /// plus the case's own name and zero-based index.
    VariantCase {
        module_source: ModuleSource,
        /// The variant type the case belongs to (e.g. `"Option"`).
        parent_type: String,
        /// The case name (e.g. `"Some"`).
        name: String,
        /// Zero-based index of the case in declaration order.
        case_index: u32,
    },
    /// One case of an `enum` declaration. Same shape as
    /// [`Self::VariantCase`] but for payload-less `enum` cases
    /// (`Ordering::Less`, `Alignment::Right`, …).
    EnumCase {
        module_source: ModuleSource,
        /// The enum type the case belongs to (e.g. `"Ordering"`).
        parent_type: String,
        /// The case name (e.g. `"Less"`).
        name: String,
        /// Zero-based index of the case in declaration order.
        case_index: u32,
    },
}

impl Resolved {
    /// The kind of this resolution. Always matches
    /// [`CompilerItem::expected_kind`] for the registered item.
    pub fn kind(&self) -> CompilerItemKind {
        match self {
            Self::Struct { .. } => CompilerItemKind::Struct,
            Self::Variant { .. } => CompilerItemKind::Variant,
            Self::Enum { .. } => CompilerItemKind::Enum,
            Self::Trait { .. } => CompilerItemKind::Trait,
            Self::Method { .. } => CompilerItemKind::Method,
            Self::TupleFamily { .. } => CompilerItemKind::TupleFamily,
            Self::BuiltinType { .. } => CompilerItemKind::BuiltinType,
            Self::VariantCase { .. } => CompilerItemKind::VariantCase,
            Self::EnumCase { .. } => CompilerItemKind::EnumCase,
        }
    }

    /// The defining module of this item. Always set, including for
    /// the tuple family.
    pub fn module_source(&self) -> &ModuleSource {
        match self {
            Self::Struct { module_source, .. }
            | Self::Variant { module_source, .. }
            | Self::Enum { module_source, .. }
            | Self::Trait { module_source, .. }
            | Self::Method { module_source, .. }
            | Self::TupleFamily { module_source }
            | Self::BuiltinType { module_source, .. }
            | Self::VariantCase { module_source, .. }
            | Self::EnumCase { module_source, .. } => module_source,
        }
    }
}

/// Registry that binds each [`CompilerItem`] to a [`Resolved`].
///
/// Populated by the elaborator during the annotate phase as it walks
/// stdlib modules and discovers `#[compiler_item("...")]` attributes.
/// Downstream passes (lower, synthesis, optimize, codegen) read from
/// the registry instead of hard-coding stdlib paths.
///
/// Storage is a dense array indexed by [`CompilerItem`] (cast via
/// [`CompilerItem::ALL`]'s declaration order); every lookup is O(1).
#[derive(Clone, Debug)]
pub struct CompilerItems {
    items: Vec<Option<Resolved>>,
}

impl Default for CompilerItems {
    /// Must delegate to [`Self::new`]. Deriving `Default` would
    /// produce an empty `items` vector and every accessor would
    /// panic with an out-of-bounds index — `index_of` returns
    /// `0..CompilerItem::COUNT`.
    fn default() -> Self {
        Self::new()
    }
}

impl CompilerItems {
    pub fn new() -> Self {
        Self {
            items: vec![None; CompilerItem::COUNT],
        }
    }

    /// Internal: convert a [`CompilerItem`] to its dense index.
    /// `CompilerItem::ALL` is the source of truth for the ordering.
    fn index_of(item: CompilerItem) -> usize {
        CompilerItem::ALL
            .iter()
            .position(|i| *i == item)
            .expect("CompilerItem must appear in CompilerItem::ALL")
    }

    /// Get the resolved data for `item`, or `None` if the stdlib has
    /// not yet registered it. Most call sites should prefer
    /// [`Self::require`] or the kind-specific `require_*` helpers.
    pub fn get(&self, item: CompilerItem) -> Option<&Resolved> {
        self.items[Self::index_of(item)].as_ref()
    }

    /// Return the resolved data, panicking with a clear ICE message
    /// if the item was not registered. Use this when a downstream
    /// pass cannot proceed without the item — the registry is
    /// validated at the end of stdlib loading, so a missing item
    /// here indicates a bug in either the stdlib or the validator.
    pub fn require(&self, item: CompilerItem) -> &Resolved {
        self.get(item).unwrap_or_else(|| {
            panic!(
                "compiler item `{item}` is not registered; \
                 expected a `#[compiler_item(\"{name}\")]` annotation \
                 on a {kind} in `core::*`",
                name = item.attr_name(),
                kind = item.expected_kind(),
            )
        })
    }

    /// Register `item` with `resolved`. Returns an error if the kind
    /// of `resolved` does not match [`CompilerItem::expected_kind`]
    /// or if the slot is already filled with a different resolution.
    ///
    /// Re-registering the same `Resolved` value is a no-op — this
    /// matches existing elaborator behaviour where the same module may
    /// be visited multiple times during the annotate pass.
    pub fn register(
        &mut self,
        item: CompilerItem,
        resolved: Resolved,
    ) -> Result<(), RegisterError> {
        if resolved.kind() != item.expected_kind() {
            return Err(RegisterError::KindMismatch {
                item,
                expected: item.expected_kind(),
                actual: resolved.kind(),
            });
        }
        let idx = Self::index_of(item);
        match &self.items[idx] {
            Some(existing) if existing == &resolved => Ok(()),
            Some(existing) => Err(RegisterError::Duplicate {
                item,
                existing_module: existing.module_source().clone(),
                new_module: resolved.module_source().clone(),
            }),
            None => {
                self.items[idx] = Some(resolved);
                Ok(())
            }
        }
    }

    // ─── Kind-checked accessors ──────────────────────────────────────
    //
    // Each one panics on kind mismatch — that would mean
    // `CompilerItem::expected_kind` and the variant of `Resolved`
    // disagreed, which is impossible if `register` succeeded.

    /// Module + struct name of a [`CompilerItemKind::Struct`] item.
    pub fn require_struct(&self, item: CompilerItem) -> (&ModuleSource, &str) {
        match self.require(item) {
            Resolved::Struct {
                module_source,
                name,
            } => (module_source, name.as_str()),
            other => kind_mismatch_ice(item, "Struct", other),
        }
    }

    /// Module + variant name of a [`CompilerItemKind::Variant`] item
    /// (`Option`, `Result`).
    pub fn require_variant(&self, item: CompilerItem) -> (&ModuleSource, &str) {
        match self.require(item) {
            Resolved::Variant {
                module_source,
                name,
            } => (module_source, name.as_str()),
            other => kind_mismatch_ice(item, "Variant", other),
        }
    }

    /// Module-only accessor for a variant. Kept for migration of
    /// call sites that only need the module path (e.g.
    /// `TypeTable::make_option`).
    pub fn variant_module(&self, item: CompilerItem) -> Option<&ModuleSource> {
        match self.get(item)? {
            Resolved::Variant { module_source, .. } => Some(module_source),
            _ => None,
        }
    }

    /// Module + enum name of a [`CompilerItemKind::Enum`] item
    /// (`Ordering`).
    pub fn require_enum(&self, item: CompilerItem) -> (&ModuleSource, &str) {
        match self.require(item) {
            Resolved::Enum {
                module_source,
                name,
            } => (module_source, name.as_str()),
            other => kind_mismatch_ice(item, "Enum", other),
        }
    }

    /// Module + trait name of a [`CompilerItemKind::Trait`] item.
    pub fn require_trait(&self, item: CompilerItem) -> (&ModuleSource, &str) {
        match self.require(item) {
            Resolved::Trait {
                module_source,
                name,
                ..
            } => (module_source, name.as_str()),
            other => kind_mismatch_ice(item, "Trait", other),
        }
    }

    /// Name-only convenience for a [`CompilerItemKind::Trait`] item.
    /// Equivalent to `require_trait(item).1`; the dedicated helper
    /// keeps use sites readable when only the trait name is needed
    /// (e.g. when constructing a synthesised `LocalMethodName`).
    pub fn trait_name(&self, item: CompilerItem) -> &str {
        self.require_trait(item).1
    }

    /// Resolved method name of a **single-method** trait — `"fmt"` for
    /// `Display`, `"inspect"` for `Inspect`, and so on across the
    /// format-family traits. Panics for multi-method traits where
    /// `method_name` was not captured during registration; callers
    /// that need such method names must reach for a dedicated method
    /// `CompilerItem` instead.
    ///
    /// Routing the synthesiser's `Display::fmt` / `Inspect::inspect`
    /// call construction through this method removes the last hard
    /// dependency on the conventional source-side method spelling.
    pub fn trait_method_name(&self, item: CompilerItem) -> &str {
        match self.require(item) {
            Resolved::Trait {
                method_name: Some(name),
                ..
            } => name.as_str(),
            Resolved::Trait { name, .. } => panic!(
                "compiler item `{item}` (trait `{name}`) has no captured \
                 method name; `trait_method_name` is only valid for \
                 single-method traits"
            ),
            other => kind_mismatch_ice(item, "Trait", other),
        }
    }

    /// All associated types declared on a trait, in source order, with
    /// their names and trait bounds. Auto-captured when the trait's
    /// `#[compiler_item("...")]` is registered.
    pub fn trait_assoc_types(&self, item: CompilerItem) -> &[TraitAssocType] {
        match self.require(item) {
            Resolved::Trait { assoc_types, .. } => assoc_types.as_slice(),
            other => kind_mismatch_ice(item, "Trait", other),
        }
    }

    /// Single associated type name on a trait, identified by the
    /// source-side name of its trait bound. Both the bound trait's
    /// spelling (passed in by the caller) and the associated type's
    /// spelling (returned) come from the registry, so renaming either
    /// end in the stdlib flows through without hand-edits.
    ///
    /// Panics if no associated type with the given bound is found —
    /// the only legitimate way to hit that is to break the structural
    /// shape of the trait, which the synthesiser does not silently
    /// handle anywhere else either.
    pub fn trait_assoc_type_by_bound(&self, item: CompilerItem, bound_trait_name: &str) -> &str {
        let assoc = self
            .trait_assoc_types(item)
            .iter()
            .find(|a| a.bound_names.iter().any(|b| b == bound_trait_name));
        match assoc {
            Some(a) => a.name.as_str(),
            None => panic!(
                "compiler item `{item}` has no associated type bound by `{bound_trait_name}`; \
                 declared associated types: {:?}",
                self.trait_assoc_types(item)
            ),
        }
    }

    /// Name-only convenience for a [`CompilerItemKind::Struct`] item.
    pub fn struct_name(&self, item: CompilerItem) -> &str {
        self.require_struct(item).1
    }

    /// Name-only convenience for a [`CompilerItemKind::Variant`] item.
    pub fn variant_name(&self, item: CompilerItem) -> &str {
        self.require_variant(item).1
    }

    /// Name-only convenience for a [`CompilerItemKind::Enum`] item.
    pub fn enum_name(&self, item: CompilerItem) -> &str {
        self.require_enum(item).1
    }

    /// Name-only convenience for a [`CompilerItemKind::Method`] item.
    /// Returns the unmangled method name (e.g. `"push_str"`).
    pub fn method_name(&self, item: CompilerItem) -> &str {
        self.require_method(item).2
    }

    /// Module-only accessor for a trait.
    pub fn trait_module(&self, item: CompilerItem) -> Option<&ModuleSource> {
        match self.get(item)? {
            Resolved::Trait { module_source, .. } => Some(module_source),
            _ => None,
        }
    }

    /// Module-only accessor for a struct.
    pub fn struct_module(&self, item: CompilerItem) -> Option<&ModuleSource> {
        match self.get(item)? {
            Resolved::Struct { module_source, .. } => Some(module_source),
            _ => None,
        }
    }

    /// Module + owner-type name + method name of a
    /// [`CompilerItemKind::Method`] item.
    pub fn require_method(&self, item: CompilerItem) -> (&ModuleSource, &str, &str) {
        match self.require(item) {
            Resolved::Method {
                module_source,
                owner_type,
                name,
            } => (module_source, owner_type.as_str(), name.as_str()),
            other => kind_mismatch_ice(item, "Method", other),
        }
    }

    /// Module that owns the tuple type family.
    pub fn tuple_module(&self) -> Option<&ModuleSource> {
        match self.get(CompilerItem::Tuple)? {
            Resolved::TupleFamily { module_source } => Some(module_source),
            _ => None,
        }
    }

    /// Module + parent-type name + case name + case index of a
    /// [`CompilerItemKind::VariantCase`] item.
    pub fn require_variant_case(&self, item: CompilerItem) -> (&ModuleSource, &str, &str, u32) {
        match self.require(item) {
            Resolved::VariantCase {
                module_source,
                parent_type,
                name,
                case_index,
            } => (
                module_source,
                parent_type.as_str(),
                name.as_str(),
                *case_index,
            ),
            other => kind_mismatch_ice(item, "VariantCase", other),
        }
    }

    /// Module + parent-type name + case name + case index of a
    /// [`CompilerItemKind::EnumCase`] item.
    pub fn require_enum_case(&self, item: CompilerItem) -> (&ModuleSource, &str, &str, u32) {
        match self.require(item) {
            Resolved::EnumCase {
                module_source,
                parent_type,
                name,
                case_index,
            } => (
                module_source,
                parent_type.as_str(),
                name.as_str(),
                *case_index,
            ),
            other => kind_mismatch_ice(item, "EnumCase", other),
        }
    }

    /// Case name of a [`CompilerItemKind::VariantCase`] item.
    pub fn variant_case_name(&self, item: CompilerItem) -> &str {
        self.require_variant_case(item).2
    }

    /// Zero-based case index of a [`CompilerItemKind::VariantCase`] item.
    pub fn variant_case_index(&self, item: CompilerItem) -> u32 {
        self.require_variant_case(item).3
    }

    /// Case name of a [`CompilerItemKind::EnumCase`] item.
    pub fn enum_case_name(&self, item: CompilerItem) -> &str {
        self.require_enum_case(item).2
    }

    /// Zero-based case index of a [`CompilerItemKind::EnumCase`] item.
    pub fn enum_case_index(&self, item: CompilerItem) -> u32 {
        self.require_enum_case(item).3
    }

    /// List every required-but-missing [`CompilerItem`] for the given
    /// `world`. The elaborator calls this after `annotate_modules` so an
    /// unregistered prelude item (or world-specific item for the
    /// active world) fails the compile with a clear diagnostic
    /// instead of an ICE deep inside synthesis.
    pub fn missing_required(&self, world: &str) -> Vec<CompilerItem> {
        CompilerItem::ALL
            .iter()
            .copied()
            .filter(|item| item.is_required(world) && self.get(*item).is_none())
            .collect()
    }
}

#[cold]
#[inline(never)]
fn kind_mismatch_ice(item: CompilerItem, expected_variant: &str, got: &Resolved) -> ! {
    panic!(
        "compiler item `{item}` registered as {got_kind:?}, \
         but require_{expected_variant} was called",
        got_kind = got.kind(),
    )
}

/// Errors returned from [`CompilerItems::register`]. The elaborator
/// converts these into user-facing diagnostics (or compiler ICEs
/// when the violation is in stdlib code).
#[derive(Clone, Debug)]
pub enum RegisterError {
    /// The annotated declaration is the wrong kind for this item —
    /// e.g. `#[compiler_item("option")]` was placed on a struct
    /// instead of a variant.
    KindMismatch {
        item: CompilerItem,
        expected: CompilerItemKind,
        actual: CompilerItemKind,
    },
    /// Two different declarations both claim the same item. The
    /// stdlib must contain exactly one `#[compiler_item("x")]`
    /// annotation for each `x`.
    Duplicate {
        item: CompilerItem,
        existing_module: ModuleSource,
        new_module: ModuleSource,
    },
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KindMismatch {
                item,
                expected,
                actual,
            } => write!(
                f,
                "`#[compiler_item(\"{item}\")]` expects a {expected}, \
                 but it is attached to a {actual}"
            ),
            Self::Duplicate {
                item,
                existing_module,
                new_module,
            } => write!(
                f,
                "duplicate `#[compiler_item(\"{item}\")]`: already registered \
                 from `{existing_module}`, redeclared in `{new_module}`"
            ),
        }
    }
}

/// Parse all `#[compiler_item("...")]` arguments from a declaration's
/// attribute list.
///
/// Returns the matched [`CompilerItem`] values in encounter order,
/// plus a [`Vec`] of *unrecognised* argument strings so the elaborator
/// can emit a diagnostic without losing the original spelling.
///
/// In valid stdlib code, a single declaration carries at most one
/// `#[compiler_item("...")]` attribute. Multiple matches are not
/// rejected here; the elaborator decides whether to fold the bits
/// (legacy behaviour from the original `comp_feature` mechanism) or flag duplicates.
pub fn parse_compiler_item_attrs(
    attrs: &[crate::ast::Attribute],
) -> (Vec<CompilerItem>, Vec<String>) {
    let mut items = Vec::new();
    let mut unknown = Vec::new();
    for attr in attrs {
        if attr.name != "compiler_item" {
            continue;
        }
        for arg in &attr.args {
            let raw = arg.as_str();
            match CompilerItem::from_attr_name(raw) {
                Some(item) => items.push(item),
                None => unknown.push(raw.to_string()),
            }
        }
    }
    (items, unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_variants_have_unique_attr_names() {
        let mut names: Vec<&str> = CompilerItem::ALL.iter().map(|i| i.attr_name()).collect();
        names.sort_unstable();
        let len_before = names.len();
        names.dedup();
        assert_eq!(
            names.len(),
            len_before,
            "duplicate attr_name across CompilerItem variants"
        );
    }

    #[test]
    fn attr_name_roundtrips() {
        for &item in CompilerItem::ALL {
            assert_eq!(CompilerItem::from_attr_name(item.attr_name()), Some(item));
        }
    }

    #[test]
    fn count_matches_all_len() {
        assert_eq!(CompilerItem::COUNT, CompilerItem::ALL.len());
    }

    #[test]
    fn register_rejects_kind_mismatch() {
        let mut reg = CompilerItems::new();
        let err = reg
            .register(
                CompilerItem::Option,
                Resolved::Trait {
                    module_source: ModuleSource::types(),
                    name: "Option".into(),
                    method_name: None,
                    assoc_types: Vec::new(),
                },
            )
            .unwrap_err();
        assert!(matches!(err, RegisterError::KindMismatch { .. }));
    }

    #[test]
    fn register_rejects_duplicate() {
        let mut reg = CompilerItems::new();
        let first = Resolved::Variant {
            module_source: ModuleSource::types(),
            name: "Option".into(),
        };
        let second = Resolved::Variant {
            module_source: ModuleSource::prelude(),
            name: "Option".into(),
        };
        reg.register(CompilerItem::Option, first).unwrap();
        let err = reg.register(CompilerItem::Option, second).unwrap_err();
        assert!(matches!(err, RegisterError::Duplicate { .. }));
    }

    #[test]
    fn register_same_value_is_noop() {
        let mut reg = CompilerItems::new();
        let resolved = Resolved::Variant {
            module_source: ModuleSource::types(),
            name: "Option".into(),
        };
        reg.register(CompilerItem::Option, resolved.clone())
            .unwrap();
        reg.register(CompilerItem::Option, resolved).unwrap();
    }

    #[test]
    fn require_kind_checked_accessors_work() {
        let mut reg = CompilerItems::new();
        reg.register(
            CompilerItem::Default,
            Resolved::Trait {
                module_source: ModuleSource::traits(),
                name: "Default".into(),
                method_name: Some("default".into()),
                assoc_types: Vec::new(),
            },
        )
        .unwrap();
        let (module, name) = reg.require_trait(CompilerItem::Default);
        assert_eq!(name, "Default");
        assert_eq!(module, &ModuleSource::traits());
    }

    /// Regression: a `Default`-constructed registry must be usable
    /// like one from `new()`. A naive `#[derive(Default)]` would
    /// leave `items` empty and every accessor would index out of
    /// bounds.
    #[test]
    fn default_constructor_matches_new() {
        let reg = CompilerItems::default();
        for &item in CompilerItem::ALL {
            assert!(reg.get(item).is_none(), "{item} should start empty");
        }
    }

    /// Regression for issue #1077: the Rust side must not depend on
    /// the source-level method name. Register `StringPushStr` with a
    /// name that does *not* match the conventional `push_str` spelling
    /// and verify the registry hands that name back through
    /// [`CompilerItems::require_method`]. This locks in the contract:
    /// a Wado-side rename of a stdlib method is transparent to the
    /// Rust compiler as long as `#[compiler_item("...")]` stays put.
    #[test]
    fn method_name_round_trips_through_registry_even_when_renamed() {
        let mut reg = CompilerItems::new();
        reg.register(
            CompilerItem::StringPushStr,
            Resolved::Method {
                module_source: ModuleSource::string(),
                owner_type: "String".to_string(),
                name: "push_str_v2".to_string(),
            },
        )
        .unwrap();
        let (module, owner, name) = reg.require_method(CompilerItem::StringPushStr);
        assert_eq!(module, &ModuleSource::string());
        assert_eq!(owner, "String");
        assert_eq!(name, "push_str_v2");
    }

    /// Trait associated type names round-trip through the registry by
    /// **bound trait**: if the stdlib renamed
    /// `Serializer::SeqSerializer` to `Serializer::SeqWriter` (bound
    /// unchanged), or renamed the bound itself from `SerializeSeq` to
    /// `SeqWriterTrait`, the synthesiser still resolves the correct
    /// associated type by asking
    /// [`CompilerItems::trait_assoc_type_by_bound`] with the bound
    /// trait's current spelling (also looked up from the registry).
    #[test]
    fn assoc_types_round_trip_by_bound_even_when_renamed() {
        let mut reg = CompilerItems::new();
        reg.register(
            CompilerItem::Serializer,
            Resolved::Trait {
                module_source: ModuleSource::serde(),
                name: "Serializer".to_string(),
                method_name: None,
                assoc_types: vec![
                    TraitAssocType {
                        name: "SeqWriter".to_string(),
                        bound_names: vec!["SerializeSeq".to_string()],
                    },
                    TraitAssocType {
                        name: "StructWriter".to_string(),
                        bound_names: vec!["SerializeStruct".to_string()],
                    },
                    TraitAssocType {
                        name: "VariantWriter".to_string(),
                        bound_names: vec!["SerializeVariant".to_string()],
                    },
                ],
            },
        )
        .unwrap();
        // Bound-based lookup finds the renamed assoc type:
        assert_eq!(
            reg.trait_assoc_type_by_bound(CompilerItem::Serializer, "SerializeSeq"),
            "SeqWriter"
        );
        assert_eq!(
            reg.trait_assoc_type_by_bound(CompilerItem::Serializer, "SerializeStruct"),
            "StructWriter"
        );
        // The captured `assoc_types` are also inspectable.
        assert_eq!(reg.trait_assoc_types(CompilerItem::Serializer).len(), 3);
    }

    /// Counterpart to the rename-rewrite test: omitting the
    /// `#[compiler_item("...")]` annotation entirely leaves the
    /// registry empty for that item, so `get()` returns `None`. The
    /// documented ICE in `require_method` only fires when a downstream
    /// pass reaches for the item — the validator detects the missing
    /// registration before that point. This test pins both halves of
    /// the contract.
    #[test]
    fn missing_registration_returns_none() {
        let reg = CompilerItems::new();
        assert!(reg.get(CompilerItem::StringPushStr).is_none());
    }

    /// `require_method` is documented to panic with a clear ICE
    /// message when the item is unregistered. Lock that message
    /// shape so the validator's failure mode and the post-validator
    /// ICE remain consistent.
    #[test]
    #[should_panic(expected = "compiler item `string_push_str` is not registered")]
    fn require_panics_on_unregistered() {
        let reg = CompilerItems::new();
        let _ = reg.require_method(CompilerItem::StringPushStr);
    }

    /// Variant case round-trip: register a `VariantCase` item under a
    /// non-default case name and confirm the registry hands the same
    /// `(parent, name, index)` triple back. Pins the contract that
    /// stdlib renames of variant cases flow through the registry the
    /// same way method / trait renames already do (issues #1086 /
    /// #1090 / #1091).
    #[test]
    fn variant_case_round_trips_through_registry_even_when_renamed() {
        let mut reg = CompilerItems::new();
        reg.register(
            CompilerItem::OptionSome,
            Resolved::VariantCase {
                module_source: ModuleSource::types(),
                parent_type: "Option".to_string(),
                name: "SomeV2".to_string(),
                case_index: 0,
            },
        )
        .unwrap();
        let (module, parent, name, index) = reg.require_variant_case(CompilerItem::OptionSome);
        assert_eq!(module, &ModuleSource::types());
        assert_eq!(parent, "Option");
        assert_eq!(name, "SomeV2");
        assert_eq!(index, 0);
    }

    /// Mirror of [`variant_case_round_trips_through_registry_even_when_renamed`]
    /// for `EnumCase` items. Locks in that `Ordering::Less` /
    /// `Alignment::Left` and friends — payload-less enum cases — are
    /// just as renameable as their variant-with-payload counterparts.
    #[test]
    fn enum_case_round_trips_through_registry_even_when_renamed() {
        let mut reg = CompilerItems::new();
        reg.register(
            CompilerItem::OrderingLess,
            Resolved::EnumCase {
                module_source: ModuleSource::traits(),
                parent_type: "Ordering".to_string(),
                name: "LessV2".to_string(),
                case_index: 0,
            },
        )
        .unwrap();
        let (module, parent, name, index) = reg.require_enum_case(CompilerItem::OrderingLess);
        assert_eq!(module, &ModuleSource::traits());
        assert_eq!(parent, "Ordering");
        assert_eq!(name, "LessV2");
        assert_eq!(index, 0);
    }

    /// `VariantCase` items must reject being attached to an `EnumCase`
    /// resolved value — the kind mismatch surface that protected the
    /// earlier kinds (`Variant` vs `Trait`) needs to extend to the new
    /// case kinds too.
    #[test]
    fn register_rejects_variant_case_with_enum_case_resolved() {
        let mut reg = CompilerItems::new();
        let err = reg
            .register(
                CompilerItem::OptionSome,
                Resolved::EnumCase {
                    module_source: ModuleSource::types(),
                    parent_type: "Option".to_string(),
                    name: "Some".to_string(),
                    case_index: 0,
                },
            )
            .unwrap_err();
        assert!(matches!(err, RegisterError::KindMismatch { .. }));
    }

    /// Validator covers every required item in `ALL`. With an empty
    /// registry, every prelude-required item should appear in
    /// `missing_required`; world-specific items (`KilnRequest`,
    /// Serialize, Deserialize) should appear only for their world.
    #[test]
    fn missing_required_scopes_by_world() {
        let reg = CompilerItems::new();
        let cli_missing = reg.missing_required("core:cli/command");
        let kiln_missing = reg.missing_required("core:kiln/generator");
        assert!(
            !cli_missing.contains(&CompilerItem::KilnRequest),
            "KilnRequest is kiln-specific and must not be required in CLI builds",
        );
        assert!(
            kiln_missing.contains(&CompilerItem::KilnRequest),
            "KilnRequest must be required when targeting kiln/generator",
        );
        // Prelude items must show up regardless of world.
        for &core in &[
            CompilerItem::Option,
            CompilerItem::Result,
            CompilerItem::Eq,
            CompilerItem::From,
            CompilerItem::String,
            CompilerItem::List,
            CompilerItem::I128,
            CompilerItem::U128,
        ] {
            assert!(
                cli_missing.contains(&core),
                "{core} should be required in CLI world"
            );
            assert!(
                kiln_missing.contains(&core),
                "{core} should be required in kiln world"
            );
        }
    }
}
