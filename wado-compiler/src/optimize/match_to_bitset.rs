//! Match → bitset: the set membership test `x matches { A | B | 'x'..='z' }`
//! becomes `(x - min) as u32 < range & (WORD >> (x - min)) & 1 != 0`, which
//! branches on nothing. Runs before `match_to_switch`, which takes what is
//! left.

use crate::nir::{FuncId, NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{ArmData, Body, ExprId, ExprKind, Operand, StmtId, StmtKind};
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

/// The values one arm names, and what it yields for them.
struct ArmSpan {
    lo: i64,
    hi: i64,
    yields: bool,
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
        // The offset is taken in `i32`, so a wider scrutinee would lose values
        // to the cast.
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
    // The arms past a wildcard are dead. A match without one is exhaustive, so
    // every value has an arm and nothing reads the default.
    let mut spans = Vec::new();
    let mut default = None;
    for arm in arms {
        if arm.guard.is_some() {
            return None;
        }
        let yields = body.operand_const_bool(arm.body)?;
        let (lo, hi) = match case_key(&body.pats[arm.pattern].kind)? {
            CaseKey::Value(v) => (v, v),
            CaseKey::Range { lo, hi } => (lo, hi),
            CaseKey::Wildcard => {
                default = Some(yields);
                break;
            }
        };
        spans.push(ArmSpan { lo, hi, yields });
    }
    let default = default.unwrap_or(false);

    // The window before a single value is walked: finding it by enumeration
    // would do the work of every arm the width goes on to refuse. Only a
    // member widens it, a value outside failing the range compare being the
    // answer the default gives anyway.
    let (mut min, mut max) = (i64::MAX, i64::MIN);
    for span in spans.iter().filter(|s| s.yields != default) {
        min = min.min(span.lo);
        max = max.max(span.hi);
        if max.abs_diff(min) >= u64::from(64 * BITSET_MAX_WORDS) {
            return None;
        }
    }
    if min > max {
        return None;
    }
    let range = u32::try_from(max.abs_diff(min)).ok()?.checked_add(1)?;

    // Bitmaps over the window, so a span costs its own width and no membership
    // scan. `seen` is what makes it first-match-wins.
    let words_len = range.div_ceil(64) as usize;
    let mut seen = vec![0u64; words_len];
    let mut words = vec![0u64; words_len];
    let mut members = 0usize;
    for span in &spans {
        for v in span.lo.max(min)..=span.hi.min(max) {
            let off = v.abs_diff(min);
            let (word, bit) = ((off / 64) as usize, 1u64 << (off % 64));
            if seen[word] & bit != 0 {
                continue;
            }
            seen[word] |= bit;
            if span.yields != default {
                words[word] |= bit;
                members += 1;
            }
        }
    }
    if members < BITSET_MIN_MEMBERS {
        return None;
    }
    if members == range as usize {
        words.clear();
    }
    Some(Bitset {
        min,
        range,
        words,
        default,
    })
}

/// Node construction at one span, so the test below reads as the expression it
/// builds rather than as a run of arena calls.
struct Build<'e, 'a> {
    engine: &'e mut Engine<'a>,
    span: Span,
}

impl Build<'_, '_> {
    fn expr(&mut self, kind: ExprKind, ty: TypeId) -> Operand {
        Operand::Expr(self.engine.alloc_expr(kind, ty, self.span))
    }

    fn int(&mut self, v: u64, ty: TypeId) -> Operand {
        self.engine.const_operand(ValueKind::Int(v, ty), ty)
    }

    fn cast(&mut self, expr: Operand, ty: TypeId) -> Operand {
        self.expr(
            ExprKind::Cast {
                expr,
                target_type: ty,
            },
            ty,
        )
    }

    fn binary(&mut self, left: Operand, op: NirBinaryOp, right: Operand, ty: TypeId) -> Operand {
        self.expr(ExprKind::Binary { left, op, right }, ty)
    }

    /// A fresh immutable local bound to `value`, as its index and its `let`.
    fn bind(&mut self, name: &str, ty: TypeId, value: Operand) -> (u32, StmtId) {
        let name = format!("__bitset_{name}_{}", self.engine.locals().len());
        let local_index = self.engine.alloc_local(name.clone(), ty, false);
        let stmt = self.engine.alloc_stmt(
            StmtKind::Let {
                name,
                local_index,
                is_mut: false,
                is_reactive: false,
                type_id: ty,
                value,
                skip_value_copy: true,
            },
            self.span,
        );
        (local_index, stmt)
    }

    fn read(&mut self, local_index: u32, ty: TypeId) -> Operand {
        let name = self.engine.locals()[local_index as usize].name.clone();
        self.expr(
            ExprKind::Local {
                index: local_index,
                name,
            },
            ty,
        )
    }
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
        let b = &mut Build { engine, span };

        let (key, let_key) = b.bind("key", scrut_type, scrutinee);
        let key = b.read(key, scrut_type);
        let key = if scrut_type == TypeTable::I32 {
            key
        } else {
            b.cast(key, TypeTable::I32)
        };
        let min = b.int(set.min as u64, TypeTable::I32);
        let off = b.binary(key, NirBinaryOp::Sub, min, TypeTable::I32);
        let off = b.cast(off, TypeTable::U32);
        let (off, let_off) = b.bind("offset", TypeTable::U32, off);

        let below = |b: &mut Build, bound: u32| {
            let off = b.read(off, TypeTable::U32);
            let bound = b.int(u64::from(bound), TypeTable::U32);
            b.binary(off, NirBinaryOp::Lt, bound, TypeTable::BOOL)
        };
        let in_range = below(b, set.range);
        let member = if let Some((&last_word, lower)) = set.words.split_last() {
            // The word the offset falls in, from the last one backwards so
            // each `select` guards the word below it.
            let mut word = b.int(last_word, TypeTable::U64);
            for (i, &w) in lower.iter().enumerate().rev() {
                let guard = below(b, 64 * (i as u32 + 1));
                let this = b.int(w, TypeTable::U64);
                let call = select_call(self.select_id, TypeTable::U64, guard, this, word);
                word = b.expr(call, TypeTable::U64);
            }
            // Wasm masks a shift count to the width, so the offset within the
            // word needs no `& 63`.
            let shift = b.read(off, TypeTable::U32);
            let shift = b.cast(shift, TypeTable::U64);
            let shifted = b.binary(word, NirBinaryOp::Shr, shift, TypeTable::U64);
            let one = b.int(1, TypeTable::U64);
            let bit = b.binary(shifted, NirBinaryOp::BitAnd, one, TypeTable::U64);
            let zero = b.int(0, TypeTable::U64);
            let hit = b.binary(bit, NirBinaryOp::NotEq, zero, TypeTable::BOOL);
            // Both operands are pure and trap-free, so a bitwise `&` keeps the
            // test branch-free where `&&` would lower to a branch.
            b.binary(in_range, NirBinaryOp::BitAnd, hit, TypeTable::BOOL)
        } else {
            in_range
        };
        let result = if set.default {
            b.expr(
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
        ExprKind::Block(engine.alloc_block(vec![let_key, let_off, tail], span))
    }
}
