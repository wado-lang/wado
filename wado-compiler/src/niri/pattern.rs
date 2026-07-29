//! Deciding a pattern against a constant.
//!
//! A pattern answers one of three things: it matches, it does not, or the
//! engine cannot tell. The third is what keeps a match alive — an undecided arm
//! binds nothing knowable, and the implicit no-match trap has to survive.

use crate::const_eval::Value;
use crate::nir::NirLiteralPattern;
use crate::nir_arena::{Body, PatId, PatKind};

use super::{
    Interpreter, PatBindings, PatternMatch, bool_to_match, int_value_matches_i128,
    int_value_matches_u128, range_matches_int,
};

impl Interpreter<'_> {
    /// Whether `value` matches `pat`, recording into `binds` the locals the
    /// pattern binds and the sub-values they take. `binds` is only meaningful
    /// on [`PatternMatch::Yes`]; a rejected alternative may have left entries
    /// behind.
    pub(super) fn pattern_matches_a(
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
                    match self.pattern_matches_a(body, value, *alt, &mut alt_binds) {
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
                match self.operand_to_lattice_a(body, *expr).as_const() {
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

    /// Conjunction of the sub-pattern results over an aggregate's fields:
    /// definitely-no as soon as one field rules the pattern out, definitely-yes
    /// only when every listed field matches. A field the value does not carry —
    /// or a sub-pattern the engine does not model — makes the whole pattern
    /// `Unknown`. A value that is not an aggregate is `Unknown` rather than
    /// vacuously matching a field-less pattern.
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
            match self.pattern_matches_a(body, field_value, pat, binds) {
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
