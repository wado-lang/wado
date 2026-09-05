//! Match → bitset: a guardless `Match` over a small integer, `char` or enum
//! scrutinee whose arms are literals and whose bodies are constant booleans —
//! the shape `x matches { A | B | … }` lowers to — becomes a mask test:
//! `(x - min) as u32 < range && (WORD >> (x - min)) & 1 != 0`, `WORD` being
//! the 64-bit word `x - min` falls in, picked by `select` when the set spans
//! more than one. A handful of ALU instructions and no data-dependent branch,
//! where the cascade pays a compare per member and the `br_table` an indirect
//! branch keyed on the scrutinee. Ordered before `match_to_switch`, which
//! would take the dense ones.
//!
//! The scrutinee is read several times, so it has to be a pure leaf — a
//! promoted value or a local — and every read of it is a fresh node, since an
//! expression node has one parent. The rebuilt `x - min` chains hash-cons to
//! one value at extraction.

use crate::nir::{FuncId, NirBinaryOp, NirLiteralPattern, NirUnaryOp};
use crate::nir_arena::{ArenaCallArg, ArmData, Body, ExprId, ExprKind, Operand, PatKind};
use crate::nir_engine::{Engine, Rule};
use crate::nir_value_graph::ValueKind;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::token::Span;

/// Below this many members a compare cascade is as cheap as the mask test.
const BITSET_MIN_MEMBERS: usize = 4;

/// Words the mask may span; each past the first costs a compare and a `select`.
const BITSET_MAX_WORDS: i128 = 4;

pub(super) struct MatchToBitsetRule<'t> {
    type_table: &'t TypeTable,
    select_id: FuncId,
}

impl<'t> MatchToBitsetRule<'t> {
    pub(super) fn new(type_table: &'t TypeTable, select_id: FuncId) -> Self {
        Self {
            type_table,
            select_id,
        }
    }
}

struct Bitset {
    min: i64,
    /// Values from `min` the words cover, `1..=64 * BITSET_MAX_WORDS`.
    range: u32,
    /// Empty when every value in the range is a member: the range compare
    /// alone answers, and a mask of all ones would only repeat it.
    words: Vec<u64>,
    /// What a value outside the set yields; a member yields the opposite.
    default: bool,
}

/// A scrutinee this rule may read more than once.
enum Leaf {
    Value(Operand),
    Local { index: u32, name: String },
}

impl Leaf {
    fn of(body: &Body, op: Operand) -> Option<Self> {
        match op {
            Operand::Value(_) => Some(Self::Value(op)),
            Operand::Expr(e) => match &body.exprs[e].kind {
                ExprKind::Local { index, name } => Some(Self::Local {
                    index: *index,
                    name: name.clone(),
                }),
                _ => None,
            },
        }
    }

    fn read(&self, engine: &mut Engine, type_id: TypeId, span: Span) -> Operand {
        match self {
            Self::Value(op) => *op,
            Self::Local { index, name } => Operand::Expr(engine.alloc_expr(
                ExprKind::Local {
                    index: *index,
                    name: name.clone(),
                },
                type_id,
                span,
            )),
        }
    }
}

impl Rule for MatchToBitsetRule<'_> {
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let ExprKind::Match {
            expr: scrutinee,
            arms,
        } = &engine.body.exprs[id].kind
        else {
            return false;
        };
        if engine.body.exprs[id].type_id != TypeTable::BOOL {
            return false;
        }
        let scrutinee = *scrutinee;
        let Some(leaf) = Leaf::of(engine.body, scrutinee) else {
            return false;
        };
        let scrut_type = engine.body.operand_type(scrutinee);
        if !narrow_scrutinee(self.type_table.get(scrut_type)) {
            return false;
        }
        let arms = arms.clone();
        let Some(set) = analyze(&arms, engine.body) else {
            return false;
        };
        let span = engine.body.exprs[id].span;
        let kind = self.build(engine, &leaf, scrut_type, &set, span);
        engine.replace_expr_kind(id, kind);
        true
    }
}

/// The scrutinee types whose every value survives the `as i32` the test
/// offsets in.
fn narrow_scrutinee(scrutinee_type: &ResolvedType) -> bool {
    matches!(
        scrutinee_type,
        ResolvedType::Primitive(
            PrimitiveType::I32
                | PrimitiveType::U32
                | PrimitiveType::I16
                | PrimitiveType::U16
                | PrimitiveType::I8
                | PrimitiveType::U8
                | PrimitiveType::Char,
        ) | ResolvedType::Enum { .. }
    )
}

fn analyze(arms: &[ArmData], body: &Body) -> Option<Bitset> {
    // `match` is first-match-wins, so a value keeps the first arm naming it,
    // and the arms past a wildcard are dead.
    let mut cases: Vec<(i64, bool)> = Vec::new();
    let mut default = None;
    for arm in arms {
        if arm.guard.is_some() {
            return None;
        }
        let yields = body.operand_const_bool(arm.body)?;
        let value = match &body.pats[arm.pattern].kind {
            PatKind::Literal(NirLiteralPattern::I128(v)) => i64::try_from(*v).ok()?,
            PatKind::Literal(NirLiteralPattern::U128(v)) => i64::try_from(*v).ok()?,
            PatKind::Literal(NirLiteralPattern::Char(c)) => i64::from(u32::from(*c)),
            PatKind::Enum { case_index, .. } => i64::from(*case_index),
            PatKind::Wildcard => {
                default = Some(yields);
                break;
            }
            _ => return None,
        };
        if !cases.iter().any(|(v, _)| *v == value) {
            cases.push((value, yields));
        }
    }
    // Without a wildcard the match is exhaustive, so every value has an arm
    // and the default is never read.
    let default = default.unwrap_or(false);
    let members: Vec<i64> = cases
        .iter()
        .filter(|(_, yields)| *yields != default)
        .map(|(v, _)| *v)
        .collect();
    if members.len() < BITSET_MIN_MEMBERS {
        return None;
    }
    let min = *members.iter().min()?;
    let max = *members.iter().max()?;
    let range = i128::from(max) - i128::from(min) + 1;
    if range > 64 * BITSET_MAX_WORDS {
        return None;
    }
    let range = u32::try_from(range).expect("bounded above by 64 * BITSET_MAX_WORDS");
    let mut words = Vec::new();
    if members.len() < range as usize {
        words = vec![0u64; range.div_ceil(64) as usize];
        for v in members {
            let off = u64::try_from(v - min).expect("a member is at or past min");
            words[(off / 64) as usize] |= 1 << (off % 64);
        }
    }
    Some(Bitset {
        min,
        range,
        words,
        default,
    })
}

impl MatchToBitsetRule<'_> {
    fn build(
        &self,
        engine: &mut Engine,
        leaf: &Leaf,
        scrut_type: TypeId,
        set: &Bitset,
        span: Span,
    ) -> ExprKind {
        let alloc = |engine: &mut Engine, kind: ExprKind, ty: TypeId| {
            Operand::Expr(engine.alloc_expr(kind, ty, span))
        };
        let int = |engine: &mut Engine, v: u64, ty: TypeId| engine.const_operand(ValueKind::Int(v, ty), ty);
        // `(x - min) as u32`, rebuilt per read: the reads hash-cons to one value.
        let offset = |engine: &mut Engine| {
            let x = leaf.read(engine, scrut_type, span);
            let x = if scrut_type == TypeTable::I32 {
                x
            } else {
                alloc(
                    engine,
                    ExprKind::Cast {
                        expr: x,
                        target_type: TypeTable::I32,
                    },
                    TypeTable::I32,
                )
            };
            let min = int(engine, set.min as u64, TypeTable::I32);
            let off = alloc(
                engine,
                ExprKind::Binary {
                    left: x,
                    op: NirBinaryOp::Sub,
                    right: min,
                },
                TypeTable::I32,
            );
            alloc(
                engine,
                ExprKind::Cast {
                    expr: off,
                    target_type: TypeTable::U32,
                },
                TypeTable::U32,
            )
        };
        // `(x - min) as u32 < bound`, unallocated: the top of the test keeps
        // the match node's identity.
        let below = |engine: &mut Engine, bound: u32| {
            let off = offset(engine);
            let bound = int(engine, u64::from(bound), TypeTable::U32);
            ExprKind::Binary {
                left: off,
                op: NirBinaryOp::Lt,
                right: bound,
            }
        };
        let member = if let Some(&last_word) = set.words.last() {
            let in_range = below(engine, set.range);
            let in_range = alloc(engine, in_range, TypeTable::BOOL);
            // The word the offset falls in, from the last one backwards so
            // each `select` guards the word below it.
            let mut word = int(engine, last_word, TypeTable::U64);
            for (i, w) in set.words.iter().enumerate().rev().skip(1) {
                let guard = below(engine, 64 * (u32::try_from(i).expect("word count is bounded") + 1));
                let guard = alloc(engine, guard, TypeTable::BOOL);
                let this = int(engine, *w, TypeTable::U64);
                word = alloc(
                    engine,
                    ExprKind::Call {
                        func_id: self.select_id,
                        type_args: vec![TypeTable::U64],
                        args: vec![
                            ArenaCallArg {
                                expr: guard,
                                is_mut: false,
                            },
                            ArenaCallArg {
                                expr: this,
                                is_mut: false,
                            },
                            ArenaCallArg {
                                expr: word,
                                is_mut: false,
                            },
                        ],
                        has_receiver: false,
                    },
                    TypeTable::U64,
                );
            }
            // Wasm masks a shift count to the width, so the offset within the
            // word needs no `& 63`.
            let off = offset(engine);
            let shift = alloc(
                engine,
                ExprKind::Cast {
                    expr: off,
                    target_type: TypeTable::U64,
                },
                TypeTable::U64,
            );
            let shifted = alloc(
                engine,
                ExprKind::Binary {
                    left: word,
                    op: NirBinaryOp::Shr,
                    right: shift,
                },
                TypeTable::U64,
            );
            let one = int(engine, 1, TypeTable::U64);
            let bit = alloc(
                engine,
                ExprKind::Binary {
                    left: shifted,
                    op: NirBinaryOp::BitAnd,
                    right: one,
                },
                TypeTable::U64,
            );
            let zero = int(engine, 0, TypeTable::U64);
            let hit = alloc(
                engine,
                ExprKind::Binary {
                    left: bit,
                    op: NirBinaryOp::NotEq,
                    right: zero,
                },
                TypeTable::BOOL,
            );
            ExprKind::Binary {
                left: in_range,
                op: NirBinaryOp::And,
                right: hit,
            }
        } else {
            below(engine, set.range)
        };
        if !set.default {
            return member;
        }
        let member = alloc(engine, member, TypeTable::BOOL);
        ExprKind::Unary {
            op: NirUnaryOp::Not,
            expr: member,
        }
    }
}
