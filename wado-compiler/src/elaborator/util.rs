//! Utility functions for the elaborator phase.

use crate::ast::{Literal, NumericSuffix, Pattern};
use crate::elaborator::float_literal::{FloatFormat, float_literal_bits};
use crate::elaborator::stmt::{primitive_assoc_const_to_i128, primitive_float_limit_owner};
use crate::elaborator::trait_env::written_type_source;
use crate::elaborator::types::TypeError;
use crate::escape::{unescape_byte, unescape_char};
use crate::primitive::PrimitiveType;
use crate::resolve::Resolutions;
use crate::tir::{InstancePattern, PatternLiteral, ResolvedType, TirPattern, TypeId, TypeTable};
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
        Literal::Number(repr, suffix) => {
            if denotes_float(repr, *suffix) {
                return Err("float literals cannot be used in match patterns".to_string());
            }
            let (negated, digits) = repr
                .strip_prefix('-')
                .map_or((false, repr.as_str()), |digits| (true, digits));
            Ok(PatternLiteral::Int {
                magnitude: parse_u128_literal(digits)?,
                negated,
                suffix: *suffix,
                shown: repr.clone(),
            })
        }
        Literal::Byte(raw) => Ok(PatternLiteral::Int {
            magnitude: unescape_byte(raw)?.into(),
            negated: false,
            suffix: None,
            shown: format!("b'{raw}'"),
        }),
        Literal::Char(raw) => unescape_char(raw).map(PatternLiteral::Char),
        Literal::Bool(b) => Ok(PatternLiteral::Bool(*b)),
        _ => unreachable!("the caller judges a {lit:?} pattern itself"),
    }
}

/// What a range-pattern bound names: a value of an integer or `char`
/// scrutinee, or a float, whose value turns on the float type that reads it.
pub(super) enum RangeBound {
    Discrete(PatternLiteral),
    Float(FloatBound),
}

/// A float range bound, before a float type rounds it.
pub(super) struct FloatBound {
    kind: FloatBoundKind,
    /// How a diagnostic writes it: `-1.5`, `f64::INFINITY`.
    shown: String,
}

enum FloatBoundKind {
    /// The unsigned literal `digits`, negated where `negated`.
    Literal {
        digits: String,
        negated: bool,
        suffix: Option<NumericSuffix>,
    },
    /// The limit `name` of the float type `owner`, as `f64::INFINITY`.
    Limit { owner: PrimitiveType, name: String },
}

/// What a range-pattern bound names: a number, byte or char literal, or a
/// primitive's limit (`i32::MAX`, `f64::INFINITY`). `None` for anything else,
/// a user constant included.
pub(super) fn range_bound(
    pattern: &Pattern,
    resolutions: &Resolutions,
) -> Option<Result<RangeBound, String>> {
    match pattern {
        Pattern::Literal(Literal::Number(repr, suffix)) if denotes_float(repr, *suffix) => {
            let (negated, digits) = repr
                .strip_prefix('-')
                .map_or((false, repr.as_str()), |digits| (true, digits));
            Some(Ok(RangeBound::Float(FloatBound {
                kind: FloatBoundKind::Literal {
                    digits: digits.to_string(),
                    negated,
                    suffix: *suffix,
                },
                shown: repr.clone(),
            })))
        }
        Pattern::Literal(lit @ (Literal::Number(..) | Literal::Byte(_) | Literal::Char(_))) => {
            Some(pattern_literal(lit).map(RangeBound::Discrete))
        }
        Pattern::Variant {
            variant_name,
            variant_qualifier: Some(qualifier),
            bindings,
            ..
        } if bindings.is_empty() => {
            let shown = format!("{}::{variant_name}", written_type_source(qualifier));
            if let Some(value) =
                primitive_assoc_const_to_i128(Some(qualifier), variant_name, resolutions)
            {
                return Some(Ok(RangeBound::Discrete(PatternLiteral::Int {
                    magnitude: value.unsigned_abs(),
                    negated: value < 0,
                    suffix: None,
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

/// A float range pattern, its bounds rounded into the float type that reads it.
pub(crate) struct FloatRange {
    pub(crate) format: FloatFormat,
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) inclusive: bool,
}

impl FloatRange {
    /// The keys of the values the range takes, inclusive: [`float_order_key`]
    /// of its bounds, the end stepped back below an exclusive bound.
    pub(super) fn keys(&self) -> (i128, i128) {
        let [start, end] =
            [self.start, self.end].map(|bits| float_order_key(self.format.value(bits)));
        (start, if self.inclusive { end } else { end - 1 })
    }
}

/// A key ordering the non-NaN floats as the float order does, so a range of
/// floats is a range of keys: `-0.0` keys as `0.0`, and adjacent `f64`s key
/// adjacently.
fn float_order_key(value: f64) -> i128 {
    assert!(!value.is_nan(), "a NaN is no range bound");
    // `-0.0 + 0.0` is `0.0`.
    let bits = (value + 0.0).to_bits().cast_signed();
    i128::from(if bits < 0 { bits ^ i64::MAX } else { bits })
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
    let scrutinee = type_table.peel_refs(scrutinee);
    let prim = type_table.primitive_head(scrutinee);
    let format = prim.and_then(FloatFormat::of);
    let mut errors = Vec::new();
    let mut bits = |bound: &RangeBound, errors: &mut Vec<PatternLiteralError>| {
        let found = match (bound, format) {
            (RangeBound::Discrete(lit), _) => {
                pattern_literal_error(lit, scrutinee, type_table).map(Err)
            }
            (RangeBound::Float(bound), Some(format)) => {
                Some(float_bound_bits(bound, format, scrutinee, type_table))
            }
            (RangeBound::Float(bound), None) => Some(Err(PatternLiteralError::Mismatch(
                bound.demands(type_table),
            ))),
        };
        match found {
            Some(Ok(bits)) => Some(bits),
            Some(Err(error)) => {
                errors.push(error);
                None
            }
            None => None,
        }
    };
    let (start, end) = (bits(start, &mut errors), bits(end, &mut errors));
    let (Some(format), Some(start), Some(end)) = (format, start, end) else {
        assert!(
            !errors.is_empty(),
            "a float range on a non-float names a mismatch"
        );
        return Err(errors);
    };
    let range = FloatRange {
        format,
        start,
        end,
        inclusive,
    };
    let (lo, hi) = range.keys();
    let message = if lo > hi + i128::from(!inclusive) {
        "reversed range pattern"
    } else if lo > hi {
        "empty range pattern"
    } else if lo == hi && inclusive {
        "a float range with equal bounds names one value, as a float literal pattern would"
    } else {
        return Ok(range);
    };
    Err(vec![PatternLiteralError::Invalid(message.to_string())])
}

/// The bits `bound` names in `format`, read by `scrutinee`, or why it names
/// none.
fn float_bound_bits(
    bound: &FloatBound,
    format: FloatFormat,
    scrutinee: TypeId,
    type_table: &mut TypeTable,
) -> Result<u64, PatternLiteralError> {
    let bits = match &bound.kind {
        FloatBoundKind::Literal {
            digits,
            negated,
            suffix,
        } => {
            if let Some(suffix) = suffix {
                let suffix_type = type_table.numeric_suffix_type(*suffix);
                if type_table.type_key(suffix_type) != type_table.type_key(scrutinee) {
                    return Err(PatternLiteralError::Mismatch(
                        type_table.type_name(suffix_type),
                    ));
                }
            }
            let bits = float_literal_bits(digits, format)
                .map_err(|error| PatternLiteralError::Invalid(error.message(&bound.shown)))?;
            if *negated {
                bits | format.sign_bit()
            } else {
                bits
            }
        }
        FloatBoundKind::Limit { owner, name } => {
            if FloatFormat::of(*owner) != Some(format) {
                return Err(PatternLiteralError::Mismatch(owner.as_str().to_string()));
            }
            format
                .limit(name)
                .expect("a limit bound names one of the format's limits")
        }
    };
    if format.value(bits).is_nan() {
        return Err(PatternLiteralError::Invalid(format!(
            "a NaN is no range bound: `{}` matches no value",
            bound.shown
        )));
    }
    Ok(bits)
}

impl FloatBound {
    /// The type the bound demands of a scrutinee that is no float.
    fn demands(&self, type_table: &mut TypeTable) -> String {
        match &self.kind {
            FloatBoundKind::Literal {
                suffix: Some(suffix),
                ..
            } => {
                let suffix_type = type_table.numeric_suffix_type(*suffix);
                type_table.type_name(suffix_type)
            }
            FloatBoundKind::Literal { suffix: None, .. } => "a float type".to_string(),
            FloatBoundKind::Limit { owner, .. } => owner.as_str().to_string(),
        }
    }
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

/// `pattern` on `scrutinee`, settled: an unsigned instance compares unsigned.
pub(crate) fn lower_literal_pattern(
    pattern: &InstancePattern,
    scrutinee: TypeId,
    type_table: &TypeTable,
) -> TirPattern {
    pattern.lower(type_table.is_unsigned_int(type_table.peel_refs(scrutinee)))
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
pub(crate) fn pattern_literal_error(
    lit: &PatternLiteral,
    scrutinee: TypeId,
    type_table: &mut TypeTable,
) -> Option<PatternLiteralError> {
    let scrutinee = type_table.peel_refs(scrutinee);
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
pub(super) fn pattern_literal_mismatch(
    lit: &PatternLiteral,
    scrutinee: TypeId,
    type_table: &mut TypeTable,
) -> Option<String> {
    let scrutinee = type_table.peel_refs(scrutinee);
    let head = type_table.representation_head(scrutinee);
    let expected = match lit {
        PatternLiteral::Int {
            suffix: Some(suffix),
            ..
        } => {
            let suffix_type = type_table.numeric_suffix_type(*suffix);
            return (type_table.type_key(suffix_type) != type_table.type_key(scrutinee))
                .then(|| type_table.type_name(suffix_type));
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

/// Why the bounds of a range pattern name no values of `scrutinee`.
pub(crate) fn range_bound_errors(
    start: &PatternLiteral,
    end: &PatternLiteral,
    scrutinee: TypeId,
    type_table: &mut TypeTable,
) -> Vec<PatternLiteralError> {
    [start, end]
        .into_iter()
        .filter_map(|bound| pattern_literal_error(bound, scrutinee, type_table))
        .collect()
}

/// Why the range `start..end` names no values whatever type reads it: its
/// bounds out of order.
pub(super) fn range_order_error(
    start: &PatternLiteral,
    end: &PatternLiteral,
    inclusive: bool,
) -> Option<String> {
    let order = range_order_key(start).cmp(&range_order_key(end));
    if order.is_gt() {
        Some("reversed range pattern".to_string())
    } else if !inclusive && order.is_ge() {
        Some("empty range pattern".to_string())
    } else {
        None
    }
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
    } else if clean.contains('e') {
        // Scientific notation: parse as f64 first, then convert
        let value: f64 = clean
            .parse()
            .map_err(|_| format!("invalid integer literal: {repr}"))?;
        if value.fract() != 0.0 {
            return Err(format!("integer literal has fractional part: {repr}"));
        }
        if value < 0.0 || value > u128::MAX as f64 {
            return Err(format!("integer literal out of range: {repr}"));
        }
        Ok(value as u128)
    } else {
        clean
            .parse()
            .map_err(|_| format!("invalid integer literal: {repr}"))
    }
}

/// Parse a signed integer literal into an i128 value.
/// Supports decimal, hex, binary, octal, and scientific notation.
/// For non-negative values, delegates to `parse_u128_literal` with an i128 range check.
pub(crate) fn parse_i128_literal(repr: &str) -> Result<i128, String> {
    let clean = normalize_numeric_literal(repr);

    if clean.starts_with('-') {
        // Scientific notation in negative numbers
        if clean.contains('e') {
            let value: f64 = clean
                .parse()
                .map_err(|_| format!("invalid integer literal: {repr}"))?;
            if value.fract() != 0.0 {
                return Err(format!("integer literal has fractional part: {repr}"));
            }
            return Ok(value as i128);
        }
        return clean
            .parse()
            .map_err(|_| format!("invalid integer literal: {repr}"));
    }

    // Non-negative: delegate to unsigned parser, then check i128 range
    let unsigned = parse_u128_literal(repr)?;
    i128::try_from(unsigned).map_err(|_| format!("integer literal out of range: {repr}"))
}

/// Whether a number literal denotes a float: it can only be one, or its
/// suffix names a float type.
pub(super) fn denotes_float(repr: &str, suffix: Option<NumericSuffix>) -> bool {
    is_float_only_literal(repr) || suffix.is_some_and(NumericSuffix::is_float)
}

/// The digits of a number literal that denotes an integer.
pub(super) fn integer_digits(repr: &str, suffix: Option<NumericSuffix>) -> Option<&str> {
    (!denotes_float(repr, suffix)).then_some(repr)
}

/// Check if a number literal can only be a float (has decimal point or negative exponent).
pub(crate) fn is_float_only_literal(repr: &str) -> bool {
    if repr.contains('.') {
        return true;
    }

    // Check for negative exponent (e.g., "1e-5")
    let lower = normalize_numeric_literal(repr);
    if let Some(e_pos) = lower.find('e') {
        let after_e = &lower[e_pos + 1..];
        if after_e.starts_with('-') {
            return true;
        }
    }

    false
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
