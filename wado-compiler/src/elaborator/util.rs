//! Utility functions for the elaborator phase.

use crate::ast::{AstId, Literal, Pattern};
use crate::elaborator::float_literal::{FloatFormat, FloatLiteralError, float_literal_bits};
use crate::elaborator::stmt::{primitive_float_limit_owner, primitive_int_limit};
use crate::elaborator::trait_env::written_type_source;
use crate::elaborator::types::TypeError;
use crate::escape::{unescape_byte, unescape_char};
use crate::lexer::{defaults_to_float, has_decimal_point};
use crate::primitive::PrimitiveType;
use crate::resolve::Resolutions;
use crate::tir::{
    FloatBound, FloatBoundKind, InstancePattern, PatternLiteral, RangeBound, ResolvedType,
    TirLiteralPattern, TypeId, TypeTable,
};
use crate::token::Span;

/// Why the integer literal `repr` of `magnitude`, negated where `negated`, is no
/// value of `target_type`. Every base checks the numeric range, annotated or cast.
pub(super) fn int_literal_range_error(
    magnitude: u128,
    negated: bool,
    repr: &str,
    target_type: TypeId,
    type_table: &TypeTable,
) -> Option<String> {
    let shown = if negated {
        format!("-{repr}")
    } else {
        repr.to_string()
    };
    int_value_range_error(
        saturated_value(magnitude, negated),
        &shown,
        target_type,
        type_table,
    )
}

/// `magnitude`, negated where `negated`. Past `i128` is out of range for every
/// type `int_range` answers for, so it saturates.
fn saturated_value(magnitude: u128, negated: bool) -> i128 {
    match i128::try_from(magnitude) {
        Ok(v) if negated => -v,
        Ok(v) => v,
        Err(_) if negated => i128::MIN,
        Err(_) => i128::MAX,
    }
}

/// The value a number, byte, char or bool literal pattern names, or why it
/// names none.
pub(super) fn pattern_literal(lit: &Literal) -> Result<PatternLiteral, String> {
    match lit {
        Literal::Number(repr) => {
            if has_decimal_point(repr) {
                return Err("float literals cannot be used in match patterns".to_string());
            }
            let (negated, digits) = repr
                .strip_prefix('-')
                .map_or((false, repr.as_str()), |digits| (true, digits));
            Ok(PatternLiteral::Int {
                magnitude: parse_u128_literal(digits)?,
                negated,
                owner: None,
                shown: repr.clone(),
            })
        }
        Literal::Byte(raw) => Ok(PatternLiteral::Int {
            magnitude: unescape_byte(raw)?.into(),
            negated: false,
            owner: None,
            shown: format!("b'{raw}'"),
        }),
        Literal::Char(raw) => unescape_char(raw).map(PatternLiteral::Char),
        Literal::Bool(b) => Ok(PatternLiteral::Bool(*b)),
        _ => unreachable!("the caller judges a {lit:?} pattern itself"),
    }
}

/// What a range-pattern bound names when the compiler knows its value: a
/// number, byte or char literal, or a primitive's limit (`i32::MAX`,
/// `f64::INFINITY`). `None` for anything else, any other constant included.
pub(super) fn range_bound(
    pattern: &Pattern,
    resolutions: &Resolutions,
) -> Option<Result<RangeBound, String>> {
    match pattern {
        // An exponent literal names an integer where its value is one, as it
        // does where an integer is expected, and a float otherwise.
        Pattern::Literal(lit @ Literal::Number(repr)) => match pattern_literal(lit) {
            Ok(value) => Some(Ok(RangeBound::Discrete(value))),
            Err(_) if defaults_to_float(repr) => {
                let (negated, digits) = repr
                    .strip_prefix('-')
                    .map_or((false, repr.as_str()), |digits| (true, digits));
                Some(Ok(RangeBound::Float(FloatBound {
                    kind: FloatBoundKind::Literal {
                        digits: digits.to_string(),
                        negated,
                    },
                    shown: repr.clone(),
                })))
            }
            Err(error) => Some(Err(error)),
        },
        Pattern::Literal(lit @ (Literal::Byte(_) | Literal::Char(_))) => {
            Some(pattern_literal(lit).map(RangeBound::Discrete))
        }
        Pattern::Variant {
            variant_name,
            variant_qualifier: Some(qualifier),
            bindings,
            ..
        } if bindings.is_empty() => {
            let shown = format!("{}::{variant_name}", written_type_source(qualifier));
            if let Some((owner, value)) =
                primitive_int_limit(Some(qualifier), variant_name, resolutions)
            {
                return Some(Ok(RangeBound::Discrete(PatternLiteral::Int {
                    magnitude: value.unsigned_abs(),
                    negated: value < 0,
                    owner: Some(owner),
                    shown,
                })));
            }
            let owner = primitive_float_limit_owner(Some(qualifier), variant_name, resolutions)?;
            Some(Ok(RangeBound::Float(FloatBound {
                kind: FloatBoundKind::Limit {
                    owner,
                    name: variant_name.clone(),
                },
                shown,
            })))
        }
        _ => None,
    }
}

/// The range pattern `start..end` (`..=` where `inclusive`), or why a bound
/// names no value. `None` where a bound is neither a literal nor a primitive's
/// limit.
pub(super) fn range_pattern(
    start: &Pattern,
    end: &Pattern,
    inclusive: bool,
    resolutions: &Resolutions,
) -> Option<Result<InstancePattern, Vec<String>>> {
    let (start, end) = (
        range_bound(start, resolutions)?,
        range_bound(end, resolutions)?,
    );
    Some(match (start, end) {
        (Ok(start), Ok(end)) => Ok(InstancePattern::Range {
            start,
            end,
            inclusive,
        }),
        (start, end) => Err([start.err(), end.err()].into_iter().flatten().collect()),
    })
}

/// A literal or range pattern on a settled scrutinee.
pub(super) enum SettledPattern {
    Literal(TirLiteralPattern),
    /// The bits of its bounds, read unsigned where `is_unsigned`.
    Range {
        start: i128,
        end: i128,
        inclusive: bool,
        is_unsigned: bool,
    },
    Float(FloatRange),
}

impl SettledPattern {
    /// The keys of the values a range takes, inclusive, ordered as the
    /// scrutinee orders them.
    pub(super) fn range_keys(&self) -> (i128, i128) {
        match self {
            SettledPattern::Range {
                start,
                end,
                inclusive,
                ..
            } => (*start, if *inclusive { *end } else { end - 1 }),
            SettledPattern::Float(range) => range.keys(),
            SettledPattern::Literal(_) => unreachable!("a literal pattern is no range"),
        }
    }
}

/// What `pattern` names on the settled `scrutinee`, or why it names nothing.
pub(super) fn settle_instance_pattern(
    pattern: &InstancePattern,
    scrutinee: TypeId,
    type_table: &mut TypeTable,
) -> Result<SettledPattern, Vec<PatternLiteralError>> {
    let is_unsigned = type_table.is_unsigned_int(type_table.peel_refs(scrutinee));
    match pattern {
        InstancePattern::Literal(value) => {
            match pattern_literal_error(value, scrutinee, type_table) {
                Some(error) => Err(vec![error]),
                None => Ok(SettledPattern::Literal(value.to_tir(is_unsigned))),
            }
        }
        InstancePattern::Range {
            start,
            end,
            inclusive,
        } if float_format(scrutinee, type_table).is_some() => {
            float_range(start, end, *inclusive, scrutinee, type_table).map(SettledPattern::Float)
        }
        InstancePattern::Range {
            start,
            end,
            inclusive,
        } => {
            let values = [start, end].map(|bound| bound_value(bound, scrutinee, type_table));
            let mut errors = discrete_order_errors(start, end, *inclusive);
            let [Ok(start), Ok(end)] = values else {
                errors.extend(values.into_iter().filter_map(Result::err));
                return Err(errors);
            };
            if !errors.is_empty() {
                return Err(errors);
            }
            let [start, end] = [start, end].map(|value| match value {
                BoundValue::Int(bits) => bits,
                BoundValue::Char(c) => i128::from(u32::from(c)),
                BoundValue::Float(..) => unreachable!("only a float reads a float bound"),
            });
            Ok(SettledPattern::Range {
                start,
                end,
                inclusive: *inclusive,
                is_unsigned,
            })
        }
    }
}

/// Why `pattern` names no value whatever type reads it, as far as that shows
/// before an instance settles the type: a range out of order, or a float range
/// with a NaN bound or with bounds out of order or equal as `f64` reads them.
/// Every float type rounds alike, so what is out of order in `f64` is out of
/// order, or equal, in each.
pub(super) fn unsettled_pattern_errors(pattern: &InstancePattern) -> Vec<PatternLiteralError> {
    match pattern {
        InstancePattern::Literal(_) => Vec::new(),
        InstancePattern::Range {
            start: start @ RangeBound::Discrete(_),
            end: end @ RangeBound::Discrete(_),
            inclusive,
        } => discrete_order_errors(start, end, *inclusive),
        InstancePattern::Range {
            start,
            end,
            inclusive,
        } => {
            let mut errors: Vec<_> = [start, end]
                .into_iter()
                .filter_map(|bound| match bound {
                    RangeBound::Float(float) => {
                        nan_bound_error(float, FloatFormat::F64, float.f64_bits()?)
                    }
                    RangeBound::Discrete(_) => None,
                })
                .collect();
            if errors.is_empty()
                && let (Some(start), Some(end)) = (start.f64_bits(), end.f64_bits())
            {
                let range = FloatRange {
                    format: FloatFormat::F64,
                    start,
                    end,
                    inclusive: *inclusive,
                };
                errors.extend(range.checked().err().into_iter().flatten());
            }
            errors
        }
    }
}

/// Why the range `start..end` of two integer or `char` literals names no
/// values whatever type reads it: its bounds out of order.
fn discrete_order_errors(
    start: &RangeBound,
    end: &RangeBound,
    inclusive: bool,
) -> Vec<PatternLiteralError> {
    let (RangeBound::Discrete(start), RangeBound::Discrete(end)) = (start, end) else {
        return Vec::new();
    };
    let order = range_order_key(start).cmp(&range_order_key(end));
    let message = if order.is_gt() {
        "reversed range pattern"
    } else if !inclusive && order.is_ge() {
        "empty range pattern"
    } else {
        return Vec::new();
    };
    vec![PatternLiteralError::Invalid(message.to_string())]
}

/// A float range pattern, its bounds rounded into the float type that reads it.
pub(super) struct FloatRange {
    pub(super) format: FloatFormat,
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) inclusive: bool,
}

impl FloatRange {
    /// The keys of the values the range takes, inclusive: [`float_order_key`]
    /// of its bounds, the end stepped back below an exclusive bound.
    pub(super) fn keys(&self) -> (i128, i128) {
        let [start, end] =
            [self.start, self.end].map(|bits| float_order_key(self.format.value(bits)));
        (start, if self.inclusive { end } else { end - 1 })
    }

    /// This range, or why it names no values: its bounds out of order, or equal.
    fn checked(self) -> Result<Self, Vec<PatternLiteralError>> {
        let (lo, hi) = self.keys();
        let message = if lo > hi + i128::from(!self.inclusive) {
            "reversed range pattern"
        } else if lo > hi {
            "empty range pattern"
        } else if lo == hi && self.inclusive {
            "a float range with equal bounds names one value, as a float literal pattern would"
        } else {
            return Ok(self);
        };
        Err(vec![PatternLiteralError::Invalid(message.to_string())])
    }
}

/// A key ordering the non-NaN floats as the float order does, so a range of
/// floats is a range of keys: `-0.0` keys as `0.0`, and adjacent `f64`s key
/// adjacently.
fn float_order_key(value: f64) -> i128 {
    assert!(!value.is_nan(), "a NaN is no range bound");
    // `-0.0 + 0.0` is `0.0`, so no negative key is spent on `-0.0`.
    let bits = (value + 0.0).to_bits().cast_signed();
    i128::from(if bits < 0 {
        (bits ^ i64::MAX) + 1
    } else {
        bits
    })
}

/// The float range `start..end` (`..=` where `inclusive`) over `scrutinee`, or
/// why it names no values of it.
pub(super) fn float_range(
    start: &RangeBound,
    end: &RangeBound,
    inclusive: bool,
    scrutinee: TypeId,
    type_table: &mut TypeTable,
) -> Result<FloatRange, Vec<PatternLiteralError>> {
    let values = [start, end].map(|bound| bound_value(bound, scrutinee, type_table));
    let [
        Ok(BoundValue::Float(format, start)),
        Ok(BoundValue::Float(_, end)),
    ] = values
    else {
        let errors: Vec<_> = values.into_iter().filter_map(Result::err).collect();
        assert!(
            !errors.is_empty(),
            "a float range on a non-float names a mismatch"
        );
        return Err(errors);
    };
    FloatRange {
        format,
        start,
        end,
        inclusive,
    }
    .checked()
}

/// Where a pattern naming a constant names it: the site annotate records the
/// comparison on, and reify reads it from. A range records on its first
/// constant bound.
pub(super) fn constant_pattern_site(pattern: &Pattern) -> (AstId, Span) {
    match pattern {
        Pattern::Ident { id, span, .. } | Pattern::MutIdent { id, span, .. } => (*id, *span),
        Pattern::Variant { name_id, span, .. } => (*name_id, *span),
        _ => unreachable!("only a name names a constant"),
    }
}

/// The value a range bound names on a settled scrutinee.
pub(super) enum BoundValue {
    /// The bits an integer compares by, read unsigned on an unsigned scrutinee.
    Int(i128),
    Char(char),
    /// The bits of a float in the scrutinee's format.
    Float(FloatFormat, u64),
}

/// The value `bound` names on `scrutinee`, or why it names none.
pub(super) fn bound_value(
    bound: &RangeBound,
    scrutinee: TypeId,
    type_table: &mut TypeTable,
) -> Result<BoundValue, PatternLiteralError> {
    let scrutinee = type_table.peel_refs(scrutinee);
    match (bound, float_format(scrutinee, type_table)) {
        (RangeBound::Discrete(lit), Some(format)) => match int_literal_float_bits(lit, format) {
            Some(bits) => bits.map(|bits| BoundValue::Float(format, bits)),
            None => Err(pattern_literal_error(lit, scrutinee, type_table)
                .expect("an integer limit or a char is no float")),
        },
        (RangeBound::Discrete(lit), None) => {
            if let Some(error) = pattern_literal_error(lit, scrutinee, type_table) {
                return Err(error);
            }
            Ok(match lit {
                PatternLiteral::Char(c) => BoundValue::Char(*c),
                PatternLiteral::Int { .. } => BoundValue::Int(lit.bits()),
                PatternLiteral::Bool(_) => unreachable!("a range bound is never a `bool`"),
            })
        }
        (RangeBound::Float(bound), Some(format)) => {
            float_bound_bits(bound, format).map(|bits| BoundValue::Float(format, bits))
        }
        (RangeBound::Float(bound), None) => Err(PatternLiteralError::Mismatch(bound.demands())),
    }
}

/// The bits `bound` names in `format`, or why it names none.
fn float_bound_bits(bound: &FloatBound, format: FloatFormat) -> Result<u64, PatternLiteralError> {
    let bits = match &bound.kind {
        FloatBoundKind::Literal { digits, negated } => {
            signed_literal_bits(digits, *negated, format)
                .map_err(|error| PatternLiteralError::Invalid(error.message(&bound.shown)))?
        }
        FloatBoundKind::Limit { owner, name } => {
            let (owner_format, bits) = limit_bits(*owner, name);
            if owner_format != format {
                return Err(PatternLiteralError::Mismatch(owner.as_str().to_string()));
            }
            bits
        }
    };
    match nan_bound_error(bound, format, bits) {
        Some(error) => Err(error),
        None => Ok(bits),
    }
}

/// The bits the literal `digits`, negated where `negated`, rounds to in
/// `format`.
fn signed_literal_bits(
    digits: &str,
    negated: bool,
    format: FloatFormat,
) -> Result<u64, FloatLiteralError> {
    let bits = float_literal_bits(digits, format)?;
    Ok(if negated {
        bits | format.sign_bit()
    } else {
        bits
    })
}

/// Whether a range pattern ranges over `scrutinee`, read through newtypes: an
/// integer, `char` or float.
pub(super) fn takes_range_patterns(type_table: &TypeTable, scrutinee: TypeId) -> bool {
    let head = type_table.representation_head(scrutinee);
    type_table.is_integer(head)
        || type_table.is_wide_int(head)
        || type_table.is_primitive(head, PrimitiveType::Char)
        || float_format(head, type_table).is_some()
}

/// The float format of `scrutinee`, read through references and newtypes.
fn float_format(scrutinee: TypeId, type_table: &TypeTable) -> Option<FloatFormat> {
    type_table
        .primitive_head(type_table.peel_refs(scrutinee))
        .and_then(FloatFormat::of)
}

/// The bits an integer literal names in `format`: its value, as an integer
/// literal converts wherever a float is expected. `None` for a limit, which
/// keeps its own type.
fn int_literal_float_bits(
    lit: &PatternLiteral,
    format: FloatFormat,
) -> Option<Result<u64, PatternLiteralError>> {
    let PatternLiteral::Int {
        magnitude,
        negated,
        owner: None,
        shown,
    } = lit
    else {
        return None;
    };
    Some(
        signed_literal_bits(&magnitude.to_string(), *negated, format)
            .map_err(|error| PatternLiteralError::Invalid(error.message(shown))),
    )
}

impl RangeBound {
    /// The bits of the `f64` the bound names on a float, whatever float type
    /// reads it. `None` where that shows only per instance.
    fn f64_bits(&self) -> Option<u64> {
        match self {
            RangeBound::Float(float) => float.f64_bits(),
            RangeBound::Discrete(lit) => int_literal_float_bits(lit, FloatFormat::F64)?.ok(),
        }
    }
}

/// Why `bound`, holding `bits` in `format`, is no bound: it is a NaN.
fn nan_bound_error(
    bound: &FloatBound,
    format: FloatFormat,
    bits: u64,
) -> Option<PatternLiteralError> {
    format.value(bits).is_nan().then(|| {
        PatternLiteralError::Invalid(format!(
            "a NaN is no range bound: `{}` matches no value",
            bound.shown
        ))
    })
}

impl FloatBound {
    /// The type the bound demands of a scrutinee it names no value of.
    fn demands(&self) -> String {
        match &self.kind {
            FloatBoundKind::Literal { .. } => "a float type".to_string(),
            FloatBoundKind::Limit { owner, .. } => owner.as_str().to_string(),
        }
    }

    /// The bits of the `f64` the bound names, whatever type reads it: a literal
    /// rounded into `f64`, a limit widened from its own type. `None` for a
    /// literal past `f64`'s range, which each instance reports.
    fn f64_bits(&self) -> Option<u64> {
        let format = FloatFormat::F64;
        match &self.kind {
            FloatBoundKind::Literal {
                digits, negated, ..
            } => signed_literal_bits(digits, *negated, format).ok(),
            FloatBoundKind::Limit { owner, name } => {
                let (owner, bits) = limit_bits(*owner, name);
                Some(owner.value(bits).to_bits())
            }
        }
    }
}

/// The format of the float type `owner` and the bits of its limit `name`, as
/// `range_bound` recorded the pair.
fn limit_bits(owner: PrimitiveType, name: &str) -> (FloatFormat, u64) {
    let format = FloatFormat::of(owner).expect("a limit's owner is a float type");
    let bits = format
        .limit(name)
        .expect("a limit bound names one of its type's limits");
    (format, bits)
}

/// Whether a literal pattern on `scrutinee` can be judged yet. An unsettled
/// head cannot: an unresolved type is reported where it is unresolved, and a
/// type parameter is judged per instance, as `TirPattern::PerInstance`.
pub(super) fn settles_literal_patterns(type_table: &TypeTable, scrutinee: TypeId) -> bool {
    let head = type_table.representation_head(type_table.peel_refs(scrutinee));
    match type_table.get(head) {
        ResolvedType::Primitive(_)
        | ResolvedType::Struct { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Variant { .. }
        | ResolvedType::Flags { .. }
        | ResolvedType::Unit => true,
        ResolvedType::GenericInstance { .. } => type_table.is_concrete(head),
        _ => false,
    }
}

/// Why a literal pattern or range bound names no value of the settled
/// `scrutinee`. A reference scrutinee is read through, as match ergonomics
/// reads it: a literal under `&i32` names an `i32`.
pub(crate) enum PatternLiteralError {
    /// Of another kind: the type it demands.
    Mismatch(String),
    /// Out of the scrutinee's range.
    Invalid(String),
}

impl PatternLiteralError {
    /// This error at `span`, on a scrutinee of `scrutinee` type.
    pub(crate) fn at(self, scrutinee: TypeId, span: Span, type_table: &TypeTable) -> TypeError {
        match self {
            PatternLiteralError::Mismatch(expected) => TypeError::PatternTypeMismatch {
                expected,
                found: type_table.type_name(type_table.peel_refs(scrutinee)),
                span,
            },
            PatternLiteralError::Invalid(message) => TypeError::InvalidPattern { message, span },
        }
    }
}

/// Why `lit` names no value of `scrutinee`.
pub(super) fn pattern_literal_error(
    lit: &PatternLiteral,
    scrutinee: TypeId,
    type_table: &mut TypeTable,
) -> Option<PatternLiteralError> {
    let scrutinee = type_table.peel_refs(scrutinee);
    if let PatternLiteral::Int {
        owner: None, shown, ..
    } = lit
        && float_format(scrutinee, type_table).is_some()
    {
        let float = type_table.type_name(scrutinee);
        return Some(PatternLiteralError::Invalid(format!(
            "`{shown}` is a float on `{float}`, and a float literal is no pattern; \
             compare it in a guard"
        )));
    }
    if let Some(expected) = pattern_literal_mismatch(lit, scrutinee, type_table) {
        return Some(PatternLiteralError::Mismatch(expected));
    }
    let PatternLiteral::Int {
        magnitude,
        negated,
        shown,
        ..
    } = lit
    else {
        return None;
    };
    int_value_range_error(
        saturated_value(*magnitude, *negated),
        shown,
        scrutinee,
        type_table,
    )
    .map(PatternLiteralError::Invalid)
}

/// The type `lit` demands of `scrutinee`, when `scrutinee` is not it.
fn pattern_literal_mismatch(
    lit: &PatternLiteral,
    scrutinee: TypeId,
    type_table: &mut TypeTable,
) -> Option<String> {
    let scrutinee = type_table.peel_refs(scrutinee);
    let head = type_table.representation_head(scrutinee);
    let expected = match lit {
        PatternLiteral::Int {
            owner: Some(owner), ..
        } => {
            let owner_type = TypeTable::primitive_type_id(*owner);
            return (type_table.type_key(owner_type) != type_table.type_key(scrutinee))
                .then(|| owner.as_str().to_string());
        }
        PatternLiteral::Int { .. }
            if !type_table.is_integer(head) && !type_table.is_wide_int(head) =>
        {
            "an integer type"
        }
        PatternLiteral::Char(_) if !type_table.is_primitive(head, PrimitiveType::Char) => "char",
        PatternLiteral::Bool(_) if !type_table.is_primitive(head, PrimitiveType::Bool) => "bool",
        _ => return None,
    };
    Some(expected.to_string())
}

/// A range bound's value as a key ordering it among the integers. Every type
/// with both bounds in range orders them alike, so the order needs no type.
fn range_order_key(bound: &PatternLiteral) -> (bool, u128) {
    match bound {
        PatternLiteral::Int {
            magnitude,
            negated: true,
            ..
        } if *magnitude != 0 => (false, u128::MAX - magnitude),
        PatternLiteral::Int { magnitude, .. } => (true, *magnitude),
        PatternLiteral::Char(c) => (true, u128::from(u32::from(*c))),
        PatternLiteral::Bool(_) => unreachable!("a range bound is never a `bool`"),
    }
}

/// Why `value`, written `shown`, is no value of the integer `target_type`.
fn int_value_range_error(
    value: i128,
    shown: &str,
    target_type: TypeId,
    type_table: &TypeTable,
) -> Option<String> {
    let prim = type_table.primitive_head(target_type)?;
    let (min, max) = prim.int_range()?;
    (value < min || value > max)
        .then(|| format!("literal out of range for `{}`: {shown}", prim.as_str()))
}

/// Normalize a numeric literal representation: remove underscores and lowercase.
/// This produces a canonical form for parsing (e.g., `"0x_FF"` → `"0xff"`, `"1E10"` → `"1e10"`).
pub(super) fn normalize_numeric_literal(repr: &str) -> String {
    repr.chars()
        .filter(|&c| c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Parse an unsigned integer literal into a u128 value.
/// Supports decimal, hex, binary, octal, and scientific notation (e.g., "1e10").
pub(crate) fn parse_u128_literal(repr: &str) -> Result<u128, String> {
    let clean = normalize_numeric_literal(repr);

    if let Some(hex) = clean.strip_prefix("0x") {
        u128::from_str_radix(hex, 16).map_err(|_| format!("invalid hex literal: {repr}"))
    } else if let Some(bin) = clean.strip_prefix("0b") {
        u128::from_str_radix(bin, 2).map_err(|_| format!("invalid binary literal: {repr}"))
    } else if let Some(oct) = clean.strip_prefix("0o") {
        u128::from_str_radix(oct, 8).map_err(|_| format!("invalid octal literal: {repr}"))
    } else if let Some((mantissa, exponent)) = clean.split_once('e') {
        let invalid = || format!("invalid integer literal: {repr}");
        let mantissa: u128 = mantissa.parse().map_err(|_| invalid())?;
        let exponent: i32 = exponent.parse().map_err(|_| invalid())?;
        if mantissa == 0 {
            return Ok(0);
        }
        let scale = 10_u128.checked_pow(exponent.unsigned_abs());
        if exponent >= 0 {
            scale
                .and_then(|scale| mantissa.checked_mul(scale))
                .ok_or_else(|| format!("integer literal out of range: {repr}"))
        } else {
            scale
                .filter(|scale| mantissa % scale == 0)
                .map(|scale| mantissa / scale)
                .ok_or_else(|| format!("`{repr}` is not a whole number"))
        }
    } else {
        clean
            .parse()
            .map_err(|_| format!("invalid integer literal: {repr}"))
    }
}

/// Parse a signed integer literal into an i128 value.
/// Supports decimal, hex, binary, octal, and scientific notation.
pub(crate) fn parse_i128_literal(repr: &str) -> Result<i128, String> {
    let out_of_range = || format!("integer literal out of range: {repr}");
    match repr.strip_prefix('-') {
        Some(magnitude) => 0_i128
            .checked_sub_unsigned(parse_u128_literal(magnitude)?)
            .ok_or_else(out_of_range),
        None => i128::try_from(parse_u128_literal(repr)?).map_err(|_| out_of_range()),
    }
}

/// The digits of a number literal that defaults to an integer.
pub(super) fn integer_digits(repr: &str) -> Option<&str> {
    (!defaults_to_float(repr)).then_some(repr)
}

/// The name a type carries its trait bounds under, where it carries any.
/// A pack answers with its own name: a pack's bound holds of each member
/// (WEP 2026-03-14), and `type_param_bounds` keys both the same way.
pub(super) fn bound_param_name(resolved: &ResolvedType) -> Option<&String> {
    match resolved {
        ResolvedType::TypeParam { name, .. } | ResolvedType::TypePack { name, .. } => Some(name),
        _ => None,
    }
}

/// Run `body` with `owner`'s `field` set to `value`, answering its result and
/// what the field then held. The enclosing value returns even on a panic.
pub(super) fn replaced<O, T, R>(
    owner: &mut O,
    field: for<'s> fn(&'s mut O) -> &'s mut T,
    value: T,
    body: impl FnOnce(&mut O) -> R,
) -> (R, T) {
    struct Restore<'r, O, T> {
        owner: &'r mut O,
        field: for<'s> fn(&'s mut O) -> &'s mut T,
        saved: Option<T>,
    }
    impl<O, T> Drop for Restore<'_, O, T> {
        fn drop(&mut self) {
            if let Some(saved) = self.saved.take() {
                *(self.field)(self.owner) = saved;
            }
        }
    }
    let saved = std::mem::replace(field(owner), value);
    let mut guard = Restore {
        owner,
        field,
        saved: Some(saved),
    };
    let result = body(guard.owner);
    let saved = guard.saved.take().expect("enclosing value present");
    let left = std::mem::replace(field(guard.owner), saved);
    (result, left)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_floats_key_adjacently() {
        let tiny = f64::from_bits(1);
        let key = float_order_key;
        assert_eq!(key(-tiny) + 1, key(0.0));
        assert_eq!(key(-0.0), key(0.0));
        assert_eq!(key(0.0) + 1, key(tiny));
        assert_eq!(key(f64::MAX) + 1, key(f64::INFINITY));
        assert_eq!(key(f64::NEG_INFINITY) + 1, key(f64::MIN));
        assert!(key(-1.0) < key(-0.5));
    }
}
