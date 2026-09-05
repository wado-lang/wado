//! Match → bitset: a guardless `Match` over an integer, `char` or enum
//! scrutinee whose arms are literals or ranges and whose bodies are constant
//! booleans — the shape `x matches { A | B | … }` lowers to — becomes a mask
//! test: `(x - min) as u32 < range & (WORD >> (x - min)) & 1 != 0`, `WORD`
//! being the 64-bit word `x - min` falls in, picked by `select` when the set
//! spans more than one, and the range compare alone when the members are
//! contiguous. A handful of ALU instructions and no branch on the key, where
//! the cascade pays a compare per member and the `br_table` an indirect branch
//! keyed on the scrutinee. Ordered before `match_to_switch`, which would take
//! the dense ones; a set past [`BITSET_MAX_WORDS`] falls through to it.
//!
//! The scrutinee is evaluated once into a fresh local and the offset into a
//! second, so any scrutinee the match took is accepted and no read repeats
//! work: the test is a `Block` of the two `let`s and the expression.

use crate::nir::{FuncId, NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{ArmData, Body, ExprId, ExprKind, Operand, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::nir_value_graph::ValueKind;
use crate::tir::{TypeId, TypeTable};
use crate::token::Span;

use super::match_to_switch::{CaseKey, case_key, scrutinee_bits};
use super::select_lowering::select_call;

/// Below this many members a compare cascade is as cheap as the mask test.
const BITSET_MIN_MEMBERS: usize = 4;

/// Words the mask may span; each past the first costs a compare and a `select`.
const BITSET_MAX_WORDS: u32 = 4;

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
        let scrut_type = engine.body.operand_type(scrutinee);
        // The test offsets in `i32`, which every value of a narrower type
        // survives the cast to.
        if scrutinee_bits(self.type_table.get(scrut_type)).is_none_or(|bits| bits > 32) {
            return false;
        }
        let Some(set) = analyze(arms, engine.body) else {
            return false;
        };
        let span = engine.body.exprs[id].span;
        let kind = self.build(engine, scrutinee, scrut_type, &set, span);
        engine.replace_expr_kind(id, kind);
        true
    }
}

fn analyze(arms: &[ArmData], body: &Body) -> Option<Bitset> {
    // `match` is first-match-wins: a value keeps the first arm naming it, and
    // the arms past a wildcard are dead. Without a wildcard the match is
    // exhaustive, so every value has an arm and the default is never read.
    let default = arms
        .iter()
        .find(|arm| {
            matches!(
                case_key(&body.pats[arm.pattern].kind),
                Some(CaseKey::Wildcard)
            )
        })
        .map_or(Some(false), |arm| body.operand_const_bool(arm.body))?;
    let mut seen: Vec<i64> = Vec::new();
    let mut members: Vec<i64> = Vec::new();
    for arm in arms {
        if arm.guard.is_some() {
            return None;
        }
        let yields = body.operand_const_bool(arm.body)?;
        let (lo, hi) = match case_key(&body.pats[arm.pattern].kind)? {
            CaseKey::Value(v) => (v, v),
            CaseKey::Range { lo, hi } => (lo, hi),
            CaseKey::Wildcard => break,
        };
        // A range wider than the mask can hold is refused before it is walked.
        if hi.abs_diff(lo) >= u64::from(64 * BITSET_MAX_WORDS) {
            return None;
        }
        for v in lo..=hi {
            if seen.contains(&v) {
                continue;
            }
            seen.push(v);
            if yields != default {
                members.push(v);
            }
        }
    }
    if members.len() < BITSET_MIN_MEMBERS {
        return None;
    }
    let min = *members
        .iter()
        .min()
        .expect("at least BITSET_MIN_MEMBERS members");
    let max = *members
        .iter()
        .max()
        .expect("at least BITSET_MIN_MEMBERS members");
    let range = u32::try_from(max.abs_diff(min)).ok()?.checked_add(1)?;
    if range > 64 * BITSET_MAX_WORDS {
        return None;
    }
    let mut words = Vec::new();
    if members.len() < range as usize {
        words = vec![0u64; range.div_ceil(64) as usize];
        for v in members {
            let off = v.abs_diff(min);
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
        scrutinee: Operand,
        scrut_type: TypeId,
        set: &Bitset,
        span: Span,
    ) -> ExprKind {
        let alloc = |engine: &mut Engine, kind: ExprKind, ty: TypeId| {
            Operand::Expr(engine.alloc_expr(kind, ty, span))
        };
        let int = |engine: &mut Engine, v: u64, ty: TypeId| {
            engine.const_operand(ValueKind::Int(v, ty), ty)
        };
        let bind = |engine: &mut Engine, name: &str, ty: TypeId, value: Operand| {
            let name = format!("__bitset_{name}_{}", engine.locals().len());
            let local_index = engine.alloc_local(name.clone(), ty, false);
            let stmt = engine.alloc_stmt(
                StmtKind::Let {
                    name,
                    local_index,
                    is_mut: false,
                    is_reactive: false,
                    type_id: ty,
                    value,
                    skip_value_copy: true,
                },
                span,
            );
            (local_index, stmt)
        };
        let read = |engine: &mut Engine, local_index: u32, ty: TypeId| {
            let name = engine.locals()[local_index as usize].name.clone();
            alloc(
                engine,
                ExprKind::Local {
                    index: local_index,
                    name,
                },
                ty,
            )
        };
        let cast = |engine: &mut Engine, expr: Operand, ty: TypeId| {
            alloc(
                engine,
                ExprKind::Cast {
                    expr,
                    target_type: ty,
                },
                ty,
            )
        };
        let binary =
            |engine: &mut Engine, left: Operand, op: NirBinaryOp, right: Operand, ty: TypeId| {
                alloc(engine, ExprKind::Binary { left, op, right }, ty)
            };

        // `let x = scrutinee; let off = (x as i32 - min) as u32;`
        let (x, let_x) = bind(engine, "key", scrut_type, scrutinee);
        let x = read(engine, x, scrut_type);
        let x = if scrut_type == TypeTable::I32 {
            x
        } else {
            cast(engine, x, TypeTable::I32)
        };
        let min = int(engine, set.min as u64, TypeTable::I32);
        let off = binary(engine, x, NirBinaryOp::Sub, min, TypeTable::I32);
        let off = cast(engine, off, TypeTable::U32);
        let (off, let_off) = bind(engine, "offset", TypeTable::U32, off);

        let below = |engine: &mut Engine, bound: u32| {
            let off = read(engine, off, TypeTable::U32);
            let bound = int(engine, u64::from(bound), TypeTable::U32);
            binary(engine, off, NirBinaryOp::Lt, bound, TypeTable::BOOL)
        };
        let in_range = below(engine, set.range);
        let member = if let Some((&last_word, lower)) = set.words.split_last() {
            // The word the offset falls in, from the last one backwards so
            // each `select` guards the word below it.
            let mut word = int(engine, last_word, TypeTable::U64);
            for (i, &w) in lower.iter().enumerate().rev() {
                let guard = below(engine, 64 * (i as u32 + 1));
                let this = int(engine, w, TypeTable::U64);
                word = alloc(
                    engine,
                    select_call(self.select_id, TypeTable::U64, guard, this, word),
                    TypeTable::U64,
                );
            }
            // Wasm masks a shift count to the width, so the offset within the
            // word needs no `& 63`.
            let shift = read(engine, off, TypeTable::U32);
            let shift = cast(engine, shift, TypeTable::U64);
            let shifted = binary(engine, word, NirBinaryOp::Shr, shift, TypeTable::U64);
            let one = int(engine, 1, TypeTable::U64);
            let bit = binary(engine, shifted, NirBinaryOp::BitAnd, one, TypeTable::U64);
            let zero = int(engine, 0, TypeTable::U64);
            let hit = binary(engine, bit, NirBinaryOp::NotEq, zero, TypeTable::BOOL);
            // Both sides are pure and trap-free, so a bitwise `&` keeps the
            // test branch-free where `&&` would lower to a branch.
            binary(engine, in_range, NirBinaryOp::BitAnd, hit, TypeTable::BOOL)
        } else {
            in_range
        };
        let result = if set.default {
            alloc(
                engine,
                ExprKind::Unary {
                    op: NirUnaryOp::Not,
                    expr: member,
                },
                TypeTable::BOOL,
            )
        } else {
            member
        };
        let tail = engine.alloc_stmt(StmtKind::Expr(result), span);
        ExprKind::Block(engine.alloc_block(vec![let_x, let_off, tail], span))
    }
}
