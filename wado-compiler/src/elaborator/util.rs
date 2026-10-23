//! Utility functions for the elaborator phase.

use crate::ast::{AstId, Literal, Pattern};
use crate::elaborator::float_literal::{FloatFormat, FloatLiteralError, float_literal_bits};
use crate::elaborator::stmt::{primitive_float_limit_owner, primitive_int_limit};
use crate::elaborator::trait_env::written_type_source;
use crate::elaborator::types::TypeError;
use crate::escape::{unescape_byte, unescape_char};
use crate::lexer::defaults_to_float;
use crate::primitive::PrimitiveType;
use crate::resolve::Resolutions;
use crate::tir::{
    FloatBound, FloatBoundKind, InstancePattern, PatternLiteral, RangeBound, ResolvedType,
    TirLiteralPattern, TypeId,
};
use crate::token::Span;
use crate::type_table::TypeTable;

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

/// What a number, byte, char or bool literal pattern names, or why it names
/// nothing: a value, or the one float a literal names that no integer is.
pub(super) fn literal_pattern(lit: &Literal) -> Result<InstancePattern, String> {
    match lit {
        Literal::Number(repr) => number_bound(repr).map(|bound| match bound {
            RangeBound::Discrete(value) => InstancePattern::Literal(value),
            RangeBound::Float(float) => InstancePattern::Float(float),
        }),
        _ => pattern_literal(lit).map(InstancePattern::Literal),
    }
}

/// The number literal `repr` as a pattern or range bound. An exponent literal
/// names an integer where its value is one, as it does where an integer is
/// expected, and a float otherwise.
fn number_bound(repr: &str) -> Result<RangeBound, String> {
    let (negated, digits) = repr
        .strip_prefix('-')
        .map_or((false, repr), |digits| (true, digits));
    match parse_u128_literal(digits) {
        Ok(magnitude) => Ok(RangeBound::Discrete(PatternLiteral::Int {
            magnitude,
            negated,
            owner: None,
            shown: repr.to_string(),
        })),
        Err(_) if defaults_to_float(repr) => Ok(RangeBound::Float(FloatBound {
            kind: FloatBoundKind::Literal {
                digits: digits.to_string(),
                negated,
            },
            shown: repr.to_string(),
        })),
        Err(error) => Err(error),
    }
}

/// The value a byte, char or bool literal pattern names, or why it names none.
fn pattern_literal(lit: &Literal) -> Result<PatternLiteral, String> {
    match lit {
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
        Pattern::Literal(Literal::Number(repr)) => Some(number_bound(repr)),
        Pattern::Literal(lit @ (Literal::Byte(_) | Literal::Char(_))) => {
            Some(pattern_literal(lit).map(RangeBound::Discrete))
        }
        _ => primitive_limit(pattern, resolutions).map(Ok),
    }
}

/// The float type's limit `pattern` names (`f64::MAX`, `f32::NAN`), which a
/// pattern matches as the one value it is.
pub(super) fn float_limit_pattern(
    pattern: &Pattern,
    resolutions: &Resolutions,
) -> Option<InstancePattern> {
    match primitive_limit(pattern, resolutions)? {
        RangeBound::Float(float) => Some(InstancePattern::Float(float)),
        RangeBound::Discrete(_) => None,
    }
}

/// The primitive's limit `pattern` names: `i32::MAX`, `f64::INFINITY`.
fn primitive_limit(pattern: &Pattern, resolutions: &Resolutions) -> Option<RangeBound> {
    let Pattern::Variant {
        variant_name,
        variant_qualifier: Some(qualifier),
        bindings,
        ..
    } = pattern
    else {
        return None;
    };
    if !bindings.is_empty() {
        return None;
    }
    let shown = format!("{}::{variant_name}", written_type_source(qualifier));
    if let Some((owner, value)) = primitive_int_limit(Some(qualifier), variant_name, resolutions) {
        return Some(RangeBound::Discrete(PatternLiteral::Int {
            magnitude: value.unsigned_abs(),
            negated: value < 0,
            owner: Some(owner),
            shown,
        }));
    }
    let owner = primitive_float_limit_owner(Some(qualifier), variant_name, resolutions)?;
    Some(RangeBound::Float(FloatBound {
        kind: FloatBoundKind::Limit {
            owner,
            name: variant_name.clone(),
        },
        shown,
    }))
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
    /// A float range, or the one float a literal or a limit names.
    Float(FloatRange),
}

impl SettledPattern {
    /// The keys of the values the pattern takes, inclusive, ordered as the
    /// scrutinee orders them.
    pub(super) fn keys(&self) -> (i128, i128) {
        match self {
            SettledPattern::Range {
                start,
                end,
                inclusive,
                ..
            } => (*start, if *inclusive { *end } else { end - 1 }),
            SettledPattern::Float(range) => range.keys(),
            SettledPattern::Literal(value) => {
                let key = match value {
                    TirLiteralPattern::I128(v) => *v,
                    TirLiteralPattern::U128(v) => v.cast_signed(),
                    TirLiteralPattern::Char(c) => i128::from(u32::from(*c)),
                    TirLiteralPattern::Bool(_)
                    | TirLiteralPattern::String(_)
                    | TirLiteralPattern::Null => {
                        unreachable!("only an integer or `char` literal keys a point")
                    }
                };
                (key, key)
            }
        }
    }

    /// Whether the pattern names one float that a rounding step can miss: any
    /// but a zero, an infinity or a NaN.
    pub(super) fn names_inexact_float(&self) -> bool {
        matches!(self, SettledPattern::Float(range) if range.names_inexact_float())
    }
}

/// What `pattern` names on the settled `scrutinee`, or why it names nothing.
pub(super) fn settle_instance_pattern(
    pattern: &InstancePattern,
    scrutinee: TypeId,
    type_table: &mut TypeTable,
) -> Result<SettledPattern, Vec<PatternLiteralError>> {
    let is_unsigned = type_table.is_unsigned_int(type_table.peel_refs(scrutinee));
    let format = float_format(scrutinee, type_table);
    match pattern {
        InstancePattern::Literal(value) => {
            if let Some(format) = format
                && let Some(bits) = int_literal_float_bits(value, format)
            {
                return bits
                    .map(|bits| SettledPattern::Float(FloatRange::point(format, bits)))
                    .map_err(|error| vec![error]);
            }
            match pattern_literal_error(value, scrutinee, type_table) {
                Some(error) => Err(vec![error]),
                None => Ok(SettledPattern::Literal(value.to_tir(is_unsigned))),
            }
        }
        InstancePattern::Float(bound) => {
            let Some(format) = format else {
                return Err(vec![PatternLiteralError::Mismatch(bound.demands())]);
            };
            float_bound_bits(bound, format)
                .map(|bits| SettledPattern::Float(FloatRange::point(format, bits)))
                .map_err(|error| vec![error])
        }
        InstancePattern::Range {
            start,
            end,
            inclusive,
        } if format.is_some() => {
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
/// with a NaN bound or with bounds out of order as `f64` reads them. Every
/// float type rounds alike, so what is out of order in `f64` is out of order,
/// or equal, in each.
pub(super) fn unsettled_pattern_errors(pattern: &InstancePattern) -> Vec<PatternLiteralError> {
    match pattern {
        InstancePattern::Literal(_) | InstancePattern::Float(_) => Vec::new(),
        InstancePattern::Range {
            start: start @ RangeBound::Discrete(_),
            end: end @ RangeBound::Discrete(_),
            inclusive,
        } => discrete_order_errors(start, end, *inclusive),
        InstancePattern::Range { start, end, .. } => {
            let errors: Vec<_> = [start, end]
                .into_iter()
                .filter_map(|bound| match bound {
                    RangeBound::Float(float) => {
                        nan_bound_error(float, FloatFormat::F64, float.f64_bits()?)
                    }
                    RangeBound::Discrete(_) => None,
                })
                .collect();
            if !errors.is_empty() {
                return errors;
            }
            f64_reading(pattern)
                .map(FloatRange::checked)
                .and_then(Result::err)
                .unwrap_or_default()
        }
    }
}

/// Whether `pattern` names one float a rounding step can miss on every float
/// type that reads it, before an instance settles the type. Only a pattern
/// that names a float whatever reads it is judged: a decimal literal, a float
/// limit, or a range with one of them as a bound.
pub(super) fn unsettled_names_inexact_float(pattern: &InstancePattern) -> bool {
    let names_float = match pattern {
        InstancePattern::Literal(_) => false,
        InstancePattern::Float(_) => true,
        InstancePattern::Range { start, end, .. } => [start, end]
            .into_iter()
            .any(|bound| matches!(bound, RangeBound::Float(_))),
    };
    names_float && f64_reading(pattern).is_some_and(|range| range.names_inexact_float())
}

/// `pattern` as `f64` reads it, where it names floats there.
fn f64_reading(pattern: &InstancePattern) -> Option<FloatRange> {
    let format = FloatFormat::F64;
    match pattern {
        InstancePattern::Literal(_) => None,
        InstancePattern::Float(float) => Some(FloatRange::point(format, float.f64_bits()?)),
        InstancePattern::Range {
            start,
            end,
            inclusive,
        } => Some(FloatRange {
            format,
            start: start.f64_bits()?,
            end: end.f64_bits()?,
            inclusive: *inclusive,
        }),
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
/// A pattern naming one float is the range from it to itself, a NaN included:
/// no range bound is a NaN, so a NaN range is that point.
pub(super) struct FloatRange {
    pub(super) format: FloatFormat,
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) inclusive: bool,
}

impl FloatRange {
    /// The pattern naming the one float `bits` holds in `format`.
    fn point(format: FloatFormat, bits: u64) -> Self {
        Self {
            format,
            start: bits,
            end: bits,
            inclusive: true,
        }
    }

    /// The keys of the values the range takes, inclusive: [`float_order_key`]
    /// of its bounds, the end stepped back below an exclusive bound.
    pub(super) fn keys(&self) -> (i128, i128) {
        let [start, end] = [self.start, self.end].map(|bits| float_order_key(self.format, bits));
        (start, if self.inclusive { end } else { end - 1 })
    }

    /// Whether this is a NaN, which matches every NaN.
    pub(super) fn is_nan(&self) -> bool {
        self.format.value(self.start).is_nan()
    }

    /// Whether the range takes one value, the float order's one `0` counted
    /// once.
    pub(super) fn is_point(&self) -> bool {
        let (lo, hi) = self.keys();
        lo == hi
    }

    /// Whether the range takes one float a rounding step can miss: any but a
    /// zero, an infinity or a NaN.
    fn names_inexact_float(&self) -> bool {
        let value = self.format.value(self.start);
        self.is_point() && value != 0.0 && value.is_finite()
    }

    /// This range, or why it names no values: its bounds out of order.
    fn checked(self) -> Result<Self, Vec<PatternLiteralError>> {
        let (lo, hi) = self.keys();
        let message = if lo > hi + i128::from(!self.inclusive) {
            "reversed range pattern"
        } else if lo > hi {
            "empty range pattern"
        } else {
            return Ok(self);
        };
        Err(vec![PatternLiteralError::Invalid(message.to_string())])
    }
}

/// A key ordering the floats of `format` as the float order does, so a range of
/// them is a range of keys: `-0.0` keys as `0.0`, adjacent floats key
/// adjacently, and every NaN keys as one value past `+Inf`.
pub(super) fn float_order_key(format: FloatFormat, bits: u64) -> i128 {
    let magnitude = i128::from(bits & !format.sign_bit());
    let infinity = float_infinity_key(format);
    if magnitude > infinity {
        infinity + 1
    } else if bits & format.sign_bit() == 0 {
        magnitude
    } else {
        -magnitude
    }
}

/// The [`float_order_key`] of `+Inf` in `format`: its magnitude. `-Inf` keys
/// as its negation, and a NaN as one past it.
pub(super) fn float_infinity_key(format: FloatFormat) -> i128 {
    i128::from(format.infinity_bits())
}

/// The bits of a float of `format` that [`float_order_key`] keys as `key`,
/// `-0.0` as `0.0` and a NaN as the format's own.
pub(super) fn float_of_order_key(format: FloatFormat, key: i128) -> u64 {
    let infinity = format.infinity_bits();
    let magnitude = u64::try_from(key.unsigned_abs()).expect("a float key fits its format");
    if magnitude > infinity {
        format.limit("NAN").expect("every float type has a NaN")
    } else if key < 0 {
        magnitude | format.sign_bit()
    } else {
        magnitude
    }
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
            let bits = float_bound_bits(bound, format)?;
            match nan_bound_error(bound, format, bits) {
                Some(error) => Err(error),
                None => Ok(BoundValue::Float(format, bits)),
            }
        }
        (RangeBound::Float(bound), None) => Err(PatternLiteralError::Mismatch(bound.demands())),
    }
}

/// The bits `bound` names in `format`, or why it names none.
fn float_bound_bits(bound: &FloatBound, format: FloatFormat) -> Result<u64, PatternLiteralError> {
    match &bound.kind {
        FloatBoundKind::Literal { digits, negated } => {
            signed_literal_bits(digits, *negated, format)
                .map_err(|error| PatternLiteralError::Invalid(error.message(&bound.shown)))
        }
        FloatBoundKind::Limit { owner, name } => {
            let (owner_format, bits) = limit_bits(*owner, name);
            if owner_format != format {
                return Err(PatternLiteralError::Mismatch(owner.as_str().to_string()));
            }
            Ok(bits)
        }
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
                .filter(|&scale| mantissa.is_multiple_of(scale))
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
/// (WEP 2026-03-14), and `type_param_bounds` keys both the same way; so does
/// an element of the pack.
pub(super) fn bound_param_name(table: &TypeTable, id: TypeId) -> Option<&String> {
    match table.get(id) {
        ResolvedType::TypeParam { name, .. } | ResolvedType::TypePack { name, .. } => Some(name),
        ResolvedType::PackElement { pack } => bound_param_name(table, *pack),
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
        let key = |value: f64| float_order_key(FloatFormat::F64, value.to_bits());
        assert_eq!(key(-tiny) + 1, key(0.0));
        assert_eq!(key(-0.0), key(0.0));
        assert_eq!(key(0.0) + 1, key(tiny));
        assert_eq!(key(f64::MAX) + 1, key(f64::INFINITY));
        assert_eq!(key(f64::NEG_INFINITY) + 1, key(f64::MIN));
        assert_eq!(key(f64::INFINITY) + 1, key(f64::NAN));
        assert_eq!(key(-f64::NAN), key(f64::NAN));
        assert!(key(-1.0) < key(-0.5));
    }

    #[test]
    fn order_keys_invert() {
        for format in [FloatFormat::F64, FloatFormat::F32, FloatFormat::F16] {
            let nan = float_order_key(format, format.limit("NAN").unwrap());
            for key in [-nan + 1, -1, 0, 1, nan - 1, nan] {
                assert_eq!(
                    float_order_key(format, float_of_order_key(format, key)),
                    key
                );
            }
        }
    }
}
