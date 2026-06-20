//! Downlevel lowering of the Wasm wide-arithmetic ops for `-f no-wide-arithmetic`.
//!
//! Wado emits the wide-arithmetic proposal (`i64.mul_wide_u/s`, `i64.add128`,
//! `i64.sub128`); wasmtime runs it natively, but V8 (Node, browsers) does not
//! implement the proposal, so jco-transpiled components carrying those opcodes
//! fail to compile under V8. The helpers here open-code each op with plain
//! 32-bit-limb i64 arithmetic.
//!
//! This module is deliberately self-contained: it adds only `WirEmitter`
//! methods that `emit.rs` calls from the wide-op match arms (gated on
//! `codegen_flags.wide_arithmetic`). Once V8 ships wide-arithmetic, delete this
//! file, its `mod` line, and the `else` branches in `emit.rs` — nothing else
//! depends on it.

use wasm_encoder::{Function, Instruction, ValType};

use crate::wir::WirInstr;

use super::WirEmitter;

/// Number of scratch i64 locals reserved per function for the software
/// lowering. Sized for the widest expansion, the 64×64→128 unsigned multiply.
const WIDE_ARITH_SCRATCH_COUNT: usize = 10;

impl WirEmitter<'_> {
    /// Pool of scratch i64 locals for the software lowering of the 128-bit ops.
    /// A single pool is shared across every wide-arith site in a function: each
    /// expansion evaluates its operands into these slots and consumes them
    /// before control leaves the op, so the slots are never simultaneously live
    /// across sites (the ops never nest — a multi-value 128-bit result is never
    /// an operand of another). Declared once per function, deduped on the first
    /// slot name.
    pub(super) fn declare_wide_arith_scratch(&mut self, locals: &mut Vec<(String, ValType)>) {
        if self.scratch_local_names.insert(Self::wide_scratch_name(0)) {
            locals.push((Self::wide_scratch_name(0), ValType::I64));
            for i in 1..WIDE_ARITH_SCRATCH_COUNT {
                locals.push((Self::wide_scratch_name(i), ValType::I64));
            }
        }
    }

    fn wide_scratch_name(i: usize) -> String {
        format!("__wide_arith_{i}")
    }

    /// Software 64×64→128 multiply. Pushes `[low, high]` (high on top), matching
    /// native `i64.mul_wide_u/s`. Splits each operand into 32-bit limbs and sums
    /// the partial products; for the signed form the unsigned product's high
    /// word is corrected by subtracting `b` when `a < 0` and `a` when `b < 0`.
    pub(super) fn emit_mul_wide_soft(
        &mut self,
        f: &mut Function,
        a: &WirInstr,
        b: &WirInstr,
        signed: bool,
    ) {
        const LOW32: i64 = 0xffff_ffff;
        let s: Vec<u32> = (0..WIDE_ARITH_SCRATCH_COUNT)
            .map(|i| self.resolve_local(&Self::wide_scratch_name(i)))
            .collect();
        let (wa, wb, al, ah, bl, bh, ll, m1, m2, carry) =
            (s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7], s[8], s[9]);

        // Evaluate each operand once into a scratch local.
        self.emit_instr(f, a);
        f.instruction(&Instruction::LocalSet(wa));
        self.emit_instr(f, b);
        f.instruction(&Instruction::LocalSet(wb));

        // 32-bit limbs: {al,ah} = {lo,hi} halves of a; {bl,bh} of b.
        for (src, lo, hi) in [(wa, al, ah), (wb, bl, bh)] {
            f.instruction(&Instruction::LocalGet(src));
            f.instruction(&Instruction::I64Const(LOW32));
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::LocalSet(lo));
            f.instruction(&Instruction::LocalGet(src));
            f.instruction(&Instruction::I64Const(32));
            f.instruction(&Instruction::I64ShrU);
            f.instruction(&Instruction::LocalSet(hi));
        }

        // Partial products: ll = al*bl, m1 = ah*bl, m2 = al*bh.
        for (x, y, dst) in [(al, bl, ll), (ah, bl, m1), (al, bh, m2)] {
            f.instruction(&Instruction::LocalGet(x));
            f.instruction(&Instruction::LocalGet(y));
            f.instruction(&Instruction::I64Mul);
            f.instruction(&Instruction::LocalSet(dst));
        }

        // carry = (ll >>> 32) + (m1 & LOW32) + (m2 & LOW32).
        f.instruction(&Instruction::LocalGet(ll));
        f.instruction(&Instruction::I64Const(32));
        f.instruction(&Instruction::I64ShrU);
        f.instruction(&Instruction::LocalGet(m1));
        f.instruction(&Instruction::I64Const(LOW32));
        f.instruction(&Instruction::I64And);
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::LocalGet(m2));
        f.instruction(&Instruction::I64Const(LOW32));
        f.instruction(&Instruction::I64And);
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::LocalSet(carry));

        // Push low = (carry << 32) | (ll & LOW32).
        f.instruction(&Instruction::LocalGet(carry));
        f.instruction(&Instruction::I64Const(32));
        f.instruction(&Instruction::I64Shl);
        f.instruction(&Instruction::LocalGet(ll));
        f.instruction(&Instruction::I64Const(LOW32));
        f.instruction(&Instruction::I64And);
        f.instruction(&Instruction::I64Or);

        // high = ah*bh + (m1 >>> 32) + (m2 >>> 32) + (carry >>> 32), pushed
        // above the low word that already sits on the stack.
        f.instruction(&Instruction::LocalGet(ah));
        f.instruction(&Instruction::LocalGet(bh));
        f.instruction(&Instruction::I64Mul);
        f.instruction(&Instruction::LocalGet(m1));
        f.instruction(&Instruction::I64Const(32));
        f.instruction(&Instruction::I64ShrU);
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::LocalGet(m2));
        f.instruction(&Instruction::I64Const(32));
        f.instruction(&Instruction::I64ShrU);
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::LocalGet(carry));
        f.instruction(&Instruction::I64Const(32));
        f.instruction(&Instruction::I64ShrU);
        f.instruction(&Instruction::I64Add);

        if signed {
            // Correct the high word: subtract b when a<0, a when b<0. The
            // arithmetic shift yields all-ones (−1) for a negative operand,
            // so `wb & (wa >>s 63)` is `wb` iff a<0, else 0 (and symmetrically).
            f.instruction(&Instruction::LocalSet(ll)); // stash unsigned high
            f.instruction(&Instruction::LocalGet(ll));
            f.instruction(&Instruction::LocalGet(wb));
            f.instruction(&Instruction::LocalGet(wa));
            f.instruction(&Instruction::I64Const(63));
            f.instruction(&Instruction::I64ShrS);
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::I64Sub);
            f.instruction(&Instruction::LocalGet(wa));
            f.instruction(&Instruction::LocalGet(wb));
            f.instruction(&Instruction::I64Const(63));
            f.instruction(&Instruction::I64ShrS);
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::I64Sub);
        }
    }

    /// Software 128-bit add/sub. Pushes `[low, high]`, matching native
    /// `i64.add128` / `i64.sub128`. The carry (add) or borrow (sub) out of the
    /// low word is recovered with an unsigned compare.
    pub(super) fn emit_add_sub_128_soft(
        &mut self,
        f: &mut Function,
        a_lo: &WirInstr,
        a_hi: &WirInstr,
        b_lo: &WirInstr,
        b_hi: &WirInstr,
        sub: bool,
    ) {
        let s: Vec<u32> = (0..6)
            .map(|i| self.resolve_local(&Self::wide_scratch_name(i)))
            .collect();
        let (al, ah, bl, bh, lo, c) = (s[0], s[1], s[2], s[3], s[4], s[5]);
        let combine = if sub {
            Instruction::I64Sub
        } else {
            Instruction::I64Add
        };

        // Evaluate operands once, in source order, into scratch locals.
        for (operand, slot) in [(a_lo, al), (a_hi, ah), (b_lo, bl), (b_hi, bh)] {
            self.emit_instr(f, operand);
            f.instruction(&Instruction::LocalSet(slot));
        }

        // lo = al ± bl.
        f.instruction(&Instruction::LocalGet(al));
        f.instruction(&Instruction::LocalGet(bl));
        f.instruction(&combine);
        f.instruction(&Instruction::LocalSet(lo));

        // c = carry/borrow (0 or 1): for sub, al < bl; for add, lo < al.
        if sub {
            f.instruction(&Instruction::LocalGet(al));
            f.instruction(&Instruction::LocalGet(bl));
        } else {
            f.instruction(&Instruction::LocalGet(lo));
            f.instruction(&Instruction::LocalGet(al));
        }
        f.instruction(&Instruction::I64LtU);
        f.instruction(&Instruction::I64ExtendI32U);
        f.instruction(&Instruction::LocalSet(c));

        // Push low, then high = ah ± bh ± c.
        f.instruction(&Instruction::LocalGet(lo));
        f.instruction(&Instruction::LocalGet(ah));
        f.instruction(&Instruction::LocalGet(bh));
        f.instruction(&combine);
        f.instruction(&Instruction::LocalGet(c));
        f.instruction(&combine);
    }
}
