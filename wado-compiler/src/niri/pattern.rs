//! Deciding a pattern against a constant.
//!
//! A pattern answers one of three things: it matches, it does not, or the
//! engine cannot tell. The third is what keeps a match alive — an undecided arm
//! binds nothing knowable, and the implicit no-match trap has to survive.

use crate::const_eval::{Value, is_int_prim, is_signed_int};
use crate::nir::NirLiteralPattern;
use crate::nir_arena::{Body, PatId, PatKind};
use crate::tir::PrimitiveType;

use super::{Interpreter, PatBindings};

impl Interpreter<'_> {
    /// Whether `value` matches `pat`, recording into `binds` the locals the
    /// pattern binds and the sub-values they take. `binds` is only meaningful
    /// on [`PatternMatch::Yes`]; a rejected alternative may have left entries
    /// behind.
    pub(super) fn pattern_matches(
        &self,
        body: &Body,
        value: &Value,
        pat: PatId,
        binds: &mut PatBindings,
    ) -> PatternMatch {
        match &body.pats[pat].kind {
            PatKind::Wildcard => PatternMatch::Yes,
            PatKind::Binding { local_index, .. } => {
                binds.push((*local_index, value.clone()));
                PatternMatch::Yes
            }
            PatKind::Literal(lit) => match (lit, value) {
                (NirLiteralPattern::I128(p), Value::Int { value: v, prim }) => {
                    bool_to_match(int_value_matches_i128(*v, *prim, *p))
                }
                (NirLiteralPattern::U128(p), Value::Int { value: v, prim }) => {
                    bool_to_match(int_value_matches_u128(*v, *prim, *p))
                }
                (NirLiteralPattern::Bool(p), Value::Bool(v)) => bool_to_match(p == v),
                (NirLiteralPattern::Char(p), Value::Char(v)) => bool_to_match(p == v),
                (
                    NirLiteralPattern::I128(_)
                    | NirLiteralPattern::U128(_)
                    | NirLiteralPattern::Bool(_)
                    | NirLiteralPattern::Char(_),
                    _,
                ) => PatternMatch::No,
                (NirLiteralPattern::String(_) | NirLiteralPattern::Null, _) => {
                    PatternMatch::Unknown
                }
            },
            PatKind::Or(alts) => {
                let mut any_unknown = false;
                for alt in alts {
                    let mut alt_binds = PatBindings::new();
                    match self.pattern_matches(body, value, *alt, &mut alt_binds) {
                        PatternMatch::Yes => {
                            // Alternatives are tried in order at run time, so an
                            // undecided earlier one may be the one that matches
                            // — and it would bind from its own positions.
                            if any_unknown && !alt_binds.is_empty() {
                                return PatternMatch::Unknown;
                            }
                            binds.append(&mut alt_binds);
                            return PatternMatch::Yes;
                        }
                        PatternMatch::No => {}
                        PatternMatch::Unknown => any_unknown = true,
                    }
                }
                if any_unknown {
                    PatternMatch::Unknown
                } else {
                    PatternMatch::No
                }
            }
            PatKind::Range {
                start,
                end,
                inclusive,
                is_unsigned,
            } => match value {
                Value::Int { value: v, prim } => bool_to_match(range_matches_int(
                    *v,
                    *prim,
                    *start,
                    *end,
                    *inclusive,
                    *is_unsigned,
                )),
                Value::Char(c) => {
                    let cp = i128::from(u32::from(*c));
                    bool_to_match(if *inclusive {
                        cp >= *start && cp <= *end
                    } else {
                        cp >= *start && cp < *end
                    })
                }
                _ => PatternMatch::No,
            },
            PatKind::ConstantValue { expr } => {
                match self.operand_to_lattice(body, *expr).as_const() {
                    Some(v) if &v == value => PatternMatch::Yes,
                    Some(_) => PatternMatch::No,
                    None => PatternMatch::Unknown,
                }
            }
            PatKind::Struct { fields, .. } => self.all_fields_match(
                body,
                value,
                fields.iter().map(|f| (f.field_index, f.pattern)),
                binds,
            ),
            // A tuple rest (`(a, ..)`) leaves the trailing sub-patterns without
            // a fixed element index, so only the exact-arity form is modelled.
            PatKind::Tuple(pats, has_rest) if !*has_rest => self.all_fields_match(
                body,
                value,
                pats.iter()
                    .enumerate()
                    .map(|(i, p)| (u32::try_from(i).expect("tuple arity fits u32"), *p)),
                binds,
            ),
            PatKind::Tuple(_, _) | PatKind::Variant { .. } | PatKind::Enum { .. } => {
                PatternMatch::Unknown
            }
        }
    }

    /// The sub-pattern results conjoined over an aggregate's fields. A field
    /// the value does not carry makes the whole pattern `Unknown`, as does a
    /// value that is not an aggregate — vacuously matching a field-less pattern
    /// would commit an arm on no evidence.
    fn all_fields_match(
        &self,
        body: &Body,
        value: &Value,
        fields: impl Iterator<Item = (u32, PatId)>,
        binds: &mut PatBindings,
    ) -> PatternMatch {
        if !matches!(value, Value::Aggregate { .. }) {
            return PatternMatch::Unknown;
        }
        let mut any_unknown = false;
        for (field_index, pat) in fields {
            let Some(field_value) = value.field(field_index) else {
                return PatternMatch::Unknown;
            };
            match self.pattern_matches(body, field_value, pat, binds) {
                PatternMatch::No => return PatternMatch::No,
                PatternMatch::Unknown => any_unknown = true,
                PatternMatch::Yes => {}
            }
        }
        if any_unknown {
            PatternMatch::Unknown
        } else {
            PatternMatch::Yes
        }
    }
}

/// Outcome of testing a pattern against a constant scrutinee
/// [`Value`]. The three states mirror the pattern's contribution to
/// SCCP feasibility in [`Interpreter::match_lattice`]:
///
/// - `Yes` — the pattern provably matches; later arms are infeasible
///   edges.
/// - `No` — the pattern provably does not match; this arm is an
///   infeasible edge.
/// - `Unknown` — the engine cannot decide (an unmodelled pattern
///   shape, a guard the engine doesn't analyze, a `ConstantValue`
///   whose inner expression doesn't reduce). The arm stays in play
///   and contributes to the join with all later arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PatternMatch {
    Yes,
    No,
    Unknown,
}

pub(super) fn bool_to_match(b: bool) -> PatternMatch {
    if b {
        PatternMatch::Yes
    } else {
        PatternMatch::No
    }
}

/// Compare an integer value (raw bits + prim) against a signed i128
/// pattern literal. Returns `true` iff the values are equal under
/// the prim's signedness interpretation.
pub(super) fn int_value_matches_i128(value: u64, prim: PrimitiveType, pat: i128) -> bool {
    let Some(v) = int_value_as_i128(value, prim) else {
        return false;
    };
    v == pat
}

/// Compare an integer value (raw bits + prim) against an unsigned
/// u128 pattern literal.
pub(super) fn int_value_matches_u128(value: u64, prim: PrimitiveType, pat: u128) -> bool {
    if is_signed_int(prim) {
        // Signed value cannot represent values outside i64 range
        // anyway; reinterpret as unsigned for comparison.
        let v = value as i64;
        if v < 0 {
            return false;
        }
        u128::from(v as u64) == pat
    } else {
        u128::from(value) == pat
    }
}

/// Convert a (raw bits, prim) integer into an i128, sign- or
/// zero-extending per the prim's signedness. Returns `None` for
/// non-integer prims.
pub(super) fn int_value_as_i128(value: u64, prim: PrimitiveType) -> Option<i128> {
    if !is_int_prim(prim) {
        return None;
    }
    if is_signed_int(prim) {
        // Stored as sign-extended i64 → widen to i128.
        Some(i128::from(value as i64))
    } else {
        Some(i128::from(value))
    }
}

/// Decide whether a (raw bits, prim) integer falls inside a range
/// pattern. Returns `false` for non-integer prims and for negative
/// signed values against an unsigned-typed range (which by
/// construction starts at zero or higher); otherwise returns the
/// usual half-open / closed range membership test in i128 space.
pub(super) fn range_matches_int(
    value: u64,
    prim: PrimitiveType,
    start: i128,
    end: i128,
    inclusive: bool,
    is_unsigned_pat: bool,
) -> bool {
    if !is_int_prim(prim) {
        return false;
    }
    let v: i128 = if is_unsigned_pat || !is_signed_int(prim) {
        // Treat the value as unsigned. For a signed prim with negative
        // bits, the unsigned reinterpretation differs — fall back to
        // sign-extended comparison, then ensure it stays nonneg before
        // entering an unsigned range check.
        if is_signed_int(prim) {
            let signed = i128::from(value as i64);
            if signed < 0 {
                // The pattern is unsigned; a negative scrutinee can't
                // be in `[start, end]` when start ≥ 0.
                return false;
            }
            signed
        } else {
            i128::from(value)
        }
    } else {
        i128::from(value as i64)
    };
    if inclusive {
        v >= start && v <= end
    } else {
        v >= start && v < end
    }
}
