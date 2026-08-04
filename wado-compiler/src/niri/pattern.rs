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
    ///
    /// An `Or` alternative preceded by an undecided one binds nothing: the
    /// earlier alternative is tried first at run time and would bind from its
    /// own positions.
    ///
    /// A tuple rest (`(a, ..)`) leaves its trailing sub-patterns without a
    /// fixed element index, so only the exact-arity form is decided.
    pub(super) fn pattern_matches(
        &self,
        body: &Body,
        value: &Value,
        pat: PatId,
        binds: &mut PatBindings,
    ) -> PatternMatch {
        match &body.pats[pat].kind {
            PatKind::Wildcard => PatternMatch::Yes,
            PatKind::Binding {
                local_index,
                type_id,
                ..
            } => {
                // A binding whose own type is a reference names the scrutinee's
                // storage rather than a copy of it, and the engine has no
                // reference values — handing it the referent's value would let
                // a later read through the handle see a copy.
                if self.type_table.is_reference_shaped(*type_id) {
                    return PatternMatch::Unknown;
                }
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
            PatKind::Tuple(pats, has_rest) if !*has_rest => self.all_fields_match(
                body,
                value,
                pats.iter()
                    .enumerate()
                    .map(|(i, p)| (u32::try_from(i).expect("tuple arity fits u32"), *p)),
                binds,
            ),
            // An enum value is its discriminant, so the case the pattern names
            // decides the arm outright.
            PatKind::Enum { case_index, .. } => match value {
                Value::Int { value: v, .. } => bool_to_match(*v == u64::from(*case_index)),
                _ => PatternMatch::Unknown,
            },
            PatKind::Variant {
                variant_name,
                bindings,
                ..
            } => self.variant_matches(body, value, variant_name, bindings, binds),
            PatKind::Tuple(_, _) => PatternMatch::Unknown,
        }
    }

    /// Whether a variant value holds the case `variant_name` spells, binding
    /// what its sub-patterns name.
    ///
    /// A payload the value does not carry where the pattern binds one is
    /// `Unknown`, not `No`: the case matched, and committing the arm without
    /// its bindings would drop what the body reads.
    fn variant_matches(
        &self,
        body: &Body,
        value: &Value,
        variant_name: &str,
        bindings: &[PatId],
        binds: &mut PatBindings,
    ) -> PatternMatch {
        let Value::Variant {
            case_name, payload, ..
        } = value
        else {
            return PatternMatch::Unknown;
        };
        if &**case_name != variant_name {
            return PatternMatch::No;
        }
        match bindings {
            [] => PatternMatch::Yes,
            [only] => match payload {
                Some(p) => self.pattern_matches(body, p, *only, binds),
                None => PatternMatch::Unknown,
            },
            many => {
                let Some(payload) = payload else {
                    return PatternMatch::Unknown;
                };
                self.all_fields_match(
                    body,
                    payload,
                    many.iter()
                        .enumerate()
                        .map(|(i, p)| (u32::try_from(i).expect("payload arity fits u32"), *p)),
                    binds,
                )
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

/// Outcome of testing a pattern against a constant scrutinee.
///
/// `Yes` makes every later arm an infeasible edge, `No` makes this arm one, and
/// `Unknown` leaves the arm in play — it joins with all later arms.
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

/// Whether a (raw bits, prim) integer equals a signed pattern literal, under
/// the prim's signedness.
pub(super) fn int_value_matches_i128(value: u64, prim: PrimitiveType, pat: i128) -> bool {
    let Some(v) = int_value_as_i128(value, prim) else {
        return false;
    };
    v == pat
}

/// Whether a (raw bits, prim) integer equals an unsigned pattern literal. A
/// negative signed value matches nothing.
pub(super) fn int_value_matches_u128(value: u64, prim: PrimitiveType, pat: u128) -> bool {
    if is_signed_int(prim) {
        let v = value as i64;
        if v < 0 {
            return false;
        }
        u128::from(v as u64) == pat
    } else {
        u128::from(value) == pat
    }
}

/// A (raw bits, prim) integer widened to i128, sign- or zero-extending per the
/// prim's signedness. `None` for a non-integer prim.
pub(super) fn int_value_as_i128(value: u64, prim: PrimitiveType) -> Option<i128> {
    if !is_int_prim(prim) {
        return None;
    }
    if is_signed_int(prim) {
        Some(i128::from(value as i64))
    } else {
        Some(i128::from(value))
    }
}

/// The i128 a range test compares in: zero-extended where the value is
/// unsigned, sign-extended otherwise. `None` for a negative signed value
/// against an unsigned-typed range, whose bounds start at zero or higher.
fn range_comparison_value(value: u64, prim: PrimitiveType, is_unsigned_pat: bool) -> Option<i128> {
    if !is_signed_int(prim) {
        return Some(i128::from(value));
    }
    if !is_unsigned_pat {
        return Some(i128::from(value as i64));
    }
    let signed = i128::from(value as i64);
    (signed >= 0).then_some(signed)
}

/// Whether a (raw bits, prim) integer falls inside a range pattern. `false` for
/// a non-integer prim.
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
    let Some(v) = range_comparison_value(value, prim, is_unsigned_pat) else {
        return false;
    };
    if inclusive {
        v >= start && v <= end
    } else {
        v >= start && v < end
    }
}
