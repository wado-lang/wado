//! Primitive-operation translation — string / bytes literals, binary and
//! unary operators, type casts, and array indexing.
//!
//! These methods are part of `FunctionTranslator`; see `translate.rs` for
//! the struct definition and the primary translation dispatch.

use crate::compiler_item::SeqField;
use crate::const_eval::{int_bit_width, prim_of};
use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::primitive::PrimitiveType;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::wir::{WirInstr, WirType};

use super::translate::FunctionTranslator;
use crate::nir_arena::{Operand, PackedData};
use crate::wir_build::{packed_array_is_eager, packed_element_consts};

/// Classification of a TIR primitive type by the Wasm numeric type family
/// it is represented as, together with signedness for integer types.
///
/// Used by binary / unary op dispatch to pick the correct WIR instruction
/// (e.g., `I32Add` vs `I64Add`, `I32DivU` vs `I32DivS`) without repeatedly
/// matching on individual `PrimitiveType` variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrimitiveKind {
    /// `i8`, `i16`, `i32` — represented as Wasm `i32`, signed.
    I32Signed,
    /// `u8`, `u16`, `u32`, `bool`, `char` — represented as Wasm `i32`, unsigned.
    I32Unsigned,
    /// `i64` — represented as Wasm `i64`, signed.
    I64Signed,
    /// `u64` — represented as Wasm `i64`, unsigned.
    I64Unsigned,
    /// `f32`.
    F32,
    /// `f64`.
    F64,
}

impl PrimitiveKind {
    /// The scalar kind a `TypeId` is represented by, or `None` for one with no
    /// scalar representation: a reference type, or a width WIR carries no
    /// scalar for (`i128` / `u128` / `v128`).
    fn from_type_id(type_table: &TypeTable, type_id: TypeId) -> Option<Self> {
        match type_table.get(type_table.representation_head(type_id)) {
            ResolvedType::Primitive(p) => Self::from_primitive(*p),
            // A discriminant and a bitmask are both i32. Signed opcodes agree
            // with unsigned ones over their non-negative range.
            ResolvedType::Enum { .. } | ResolvedType::Flags { .. } => Some(Self::I32Signed),
            // A `Never` operand traps before the operation runs, so the node is
            // never observed — but it still needs an opcode to keep its shape.
            // (`Unit` has no value to feed one; the elaborator rejects it.)
            ResolvedType::Never => Some(Self::I32Signed),
            _ => None,
        }
    }

    /// Classify a `PrimitiveType` value.
    fn from_primitive(p: PrimitiveType) -> Option<Self> {
        Some(match p {
            PrimitiveType::I8 | PrimitiveType::I16 | PrimitiveType::I32 => Self::I32Signed,
            PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::Bool
            | PrimitiveType::Char => Self::I32Unsigned,
            PrimitiveType::I64 => Self::I64Signed,
            PrimitiveType::U64 => Self::I64Unsigned,
            PrimitiveType::F32 => Self::F32,
            PrimitiveType::F64 => Self::F64,
            // `v128` and the half types carry no arithmetic to classify.
            PrimitiveType::V128 | PrimitiveType::F16 | PrimitiveType::Bf16 => return None,
        })
    }
}

impl FunctionTranslator<'_, '_> {
    /// A constant array as WIR: a const `array.new_fixed`, which a global may run
    /// eagerly, where the elements encode smaller inline, else `array.new_data`.
    pub(super) fn translate_packed_array(&self, data: &PackedData, type_id: TypeId) -> WirInstr {
        let array_type_id = if data.as_bytes().is_some() {
            self.ctx
                .array_type_by_name
                .get("u8")
                .cloned()
                .expect("[WIR] PackedArray: u8 array type not registered")
        } else {
            self.ref_type_id(type_id)
        };

        if data.is_empty() {
            WirInstr::ArrayNewDefault {
                type_id: array_type_id,
                len: Box::new(WirInstr::I32Const(0)),
            }
        } else if packed_array_is_eager(
            data,
            self.ctx.package.string_inline_max_bytes,
            self.force_fixed_string_repr,
        ) {
            let elements = packed_element_consts(data).collect();
            WirInstr::ArrayNewFixed {
                type_id: array_type_id,
                elements,
            }
        } else {
            let data_index = self.ctx.packed_data_map.get(&data.bytes).copied().expect(
                "[WIR] PackedArray: `register_literal_data` registers every payload `packed_array_is_eager` refuses",
            );
            let len = data.len();
            let len_i32 = i32::try_from(len)
                .unwrap_or_else(|_| panic!("[WIR] literal of {len} elements exceeds i32 length"));
            WirInstr::ArrayNewData {
                type_id: array_type_id,
                data_index,
                offset: Box::new(WirInstr::I32Const(0)),
                len: Box::new(WirInstr::I32Const(len_i32)),
            }
        }
    }

    /// The scalar kind `type_id` lowers to, for an operator with only scalar
    /// opcodes. Type checking rejects the rest, and falling back to i32 would
    /// silently misread an `f64` or a reference.
    #[track_caller]
    fn scalar_kind(&self, type_id: TypeId, op: &impl std::fmt::Debug) -> PrimitiveKind {
        PrimitiveKind::from_type_id(self.type_table, type_id).unwrap_or_else(|| {
            panic!(
                "[WIR] `{op:?}` has no scalar lowering for {:?}",
                self.type_table.get(type_id)
            )
        })
    }

    /// Translate a binary operation to WIR.
    pub(super) fn translate_binary_op(
        &mut self,
        op: &NirBinaryOp,
        left: Box<WirInstr>,
        right: Box<WirInstr>,
        left_type_id: TypeId,
    ) -> WirInstr {
        // The reference-identity operator takes operands with no scalar kind.
        if let NirBinaryOp::RefNotEq = op {
            return WirInstr::I32Eqz(Box::new(WirInstr::RefEq(left, right)));
        }
        let kind = self.scalar_kind(left_type_id, op);

        match op {
            NirBinaryOp::Add => match kind {
                PrimitiveKind::F64 => WirInstr::F64Add(left, right),
                PrimitiveKind::F32 => WirInstr::F32Add(left, right),
                PrimitiveKind::I64Signed | PrimitiveKind::I64Unsigned => {
                    WirInstr::I64Add(left, right)
                }
                PrimitiveKind::I32Signed | PrimitiveKind::I32Unsigned => {
                    WirInstr::I32Add(left, right)
                }
            },
            NirBinaryOp::Sub => match kind {
                PrimitiveKind::F64 => WirInstr::F64Sub(left, right),
                PrimitiveKind::F32 => WirInstr::F32Sub(left, right),
                PrimitiveKind::I64Signed | PrimitiveKind::I64Unsigned => {
                    WirInstr::I64Sub(left, right)
                }
                PrimitiveKind::I32Signed | PrimitiveKind::I32Unsigned => {
                    WirInstr::I32Sub(left, right)
                }
            },
            NirBinaryOp::Mul => match kind {
                PrimitiveKind::F64 => WirInstr::F64Mul(left, right),
                PrimitiveKind::F32 => WirInstr::F32Mul(left, right),
                PrimitiveKind::I64Signed | PrimitiveKind::I64Unsigned => {
                    WirInstr::I64Mul(left, right)
                }
                PrimitiveKind::I32Signed | PrimitiveKind::I32Unsigned => {
                    WirInstr::I32Mul(left, right)
                }
            },
            NirBinaryOp::Div => match kind {
                PrimitiveKind::F64 => WirInstr::F64Div(left, right),
                PrimitiveKind::F32 => WirInstr::F32Div(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64DivU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64DivS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32DivU(left, right),
                PrimitiveKind::I32Signed => self.signed_div_i32(left, right, left_type_id),
            },
            NirBinaryOp::Mod => match kind {
                // Wasm has no float remainder; `%` on a float is a type error.
                PrimitiveKind::F32 | PrimitiveKind::F64 => {
                    panic!("[WIR] `%` has no float lowering")
                }
                PrimitiveKind::I64Unsigned => WirInstr::I64RemU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64RemS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32RemU(left, right),
                PrimitiveKind::I32Signed => WirInstr::I32RemS(left, right),
            },
            NirBinaryOp::Eq
            | NirBinaryOp::NotEq
            | NirBinaryOp::Lt
            | NirBinaryOp::LtEq
            | NirBinaryOp::Gt
            | NirBinaryOp::GtEq
                if let Some(width) = FloatWidth::of(kind) =>
            {
                self.float_order_comparison(*op, width, *left, *right)
            }
            NirBinaryOp::Eq => match kind {
                PrimitiveKind::I64Signed | PrimitiveKind::I64Unsigned => {
                    WirInstr::I64Eq(left, right)
                }
                PrimitiveKind::I32Signed | PrimitiveKind::I32Unsigned => {
                    WirInstr::I32Eq(left, right)
                }
                PrimitiveKind::F32 | PrimitiveKind::F64 => unreachable!(),
            },
            NirBinaryOp::NotEq => match kind {
                PrimitiveKind::I64Signed | PrimitiveKind::I64Unsigned => {
                    WirInstr::I64Ne(left, right)
                }
                PrimitiveKind::I32Signed | PrimitiveKind::I32Unsigned => {
                    WirInstr::I32Ne(left, right)
                }
                PrimitiveKind::F32 | PrimitiveKind::F64 => unreachable!(),
            },
            NirBinaryOp::Lt => match kind {
                PrimitiveKind::I64Unsigned => WirInstr::I64LtU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64LtS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32LtU(left, right),
                PrimitiveKind::I32Signed => WirInstr::I32LtS(left, right),
                PrimitiveKind::F32 | PrimitiveKind::F64 => unreachable!(),
            },
            NirBinaryOp::LtEq => match kind {
                PrimitiveKind::I64Unsigned => WirInstr::I64LeU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64LeS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32LeU(left, right),
                PrimitiveKind::I32Signed => WirInstr::I32LeS(left, right),
                PrimitiveKind::F32 | PrimitiveKind::F64 => unreachable!(),
            },
            NirBinaryOp::Gt => match kind {
                PrimitiveKind::I64Unsigned => WirInstr::I64GtU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64GtS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32GtU(left, right),
                PrimitiveKind::I32Signed => WirInstr::I32GtS(left, right),
                PrimitiveKind::F32 | PrimitiveKind::F64 => unreachable!(),
            },
            NirBinaryOp::GtEq => match kind {
                PrimitiveKind::I64Unsigned => WirInstr::I64GeU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64GeS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32GeU(left, right),
                PrimitiveKind::I32Signed => WirInstr::I32GeS(left, right),
                PrimitiveKind::F32 | PrimitiveKind::F64 => unreachable!(),
            },
            NirBinaryOp::And | NirBinaryOp::BitAnd => match kind {
                PrimitiveKind::F32 | PrimitiveKind::F64 => {
                    panic!("[WIR] `&` has no float lowering")
                }
                PrimitiveKind::I64Signed | PrimitiveKind::I64Unsigned => {
                    WirInstr::I64And(left, right)
                }
                PrimitiveKind::I32Signed | PrimitiveKind::I32Unsigned => {
                    WirInstr::I32And(left, right)
                }
            },
            NirBinaryOp::Or | NirBinaryOp::BitOr => match kind {
                PrimitiveKind::F32 | PrimitiveKind::F64 => {
                    panic!("[WIR] `|` has no float lowering")
                }
                PrimitiveKind::I64Signed | PrimitiveKind::I64Unsigned => {
                    WirInstr::I64Or(left, right)
                }
                PrimitiveKind::I32Signed | PrimitiveKind::I32Unsigned => {
                    WirInstr::I32Or(left, right)
                }
            },
            NirBinaryOp::BitXor => match kind {
                PrimitiveKind::F32 | PrimitiveKind::F64 => {
                    panic!("[WIR] `^` has no float lowering")
                }
                PrimitiveKind::I64Signed | PrimitiveKind::I64Unsigned => {
                    WirInstr::I64Xor(left, right)
                }
                PrimitiveKind::I32Signed | PrimitiveKind::I32Unsigned => {
                    WirInstr::I32Xor(left, right)
                }
            },
            NirBinaryOp::Shl => match kind {
                PrimitiveKind::F32 | PrimitiveKind::F64 => {
                    panic!("[WIR] `<<` has no float lowering")
                }
                PrimitiveKind::I64Signed | PrimitiveKind::I64Unsigned => {
                    WirInstr::I64Shl(left, right)
                }
                PrimitiveKind::I32Signed | PrimitiveKind::I32Unsigned => {
                    WirInstr::I32Shl(left, right)
                }
            },
            NirBinaryOp::Shr => match kind {
                PrimitiveKind::F32 | PrimitiveKind::F64 => {
                    panic!("[WIR] `>>` has no float lowering")
                }
                PrimitiveKind::I64Unsigned => WirInstr::I64ShrU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64ShrS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32ShrU(left, right),
                PrimitiveKind::I32Signed => WirInstr::I32ShrS(left, right),
            },
            // Returned above, before the operand kind is classified.
            NirBinaryOp::RefNotEq => unreachable!(),
        }
    }

    /// Signed `/` at the operand's width, which traps on `MIN / -1` as
    /// `i32.div_s` does. An `i8` or `i16` dividend is shifted into the top of
    /// the i32, where its `MIN` is i32's, so `div_s` overflows on exactly that
    /// case. Dividing again by the shift's power of two restores the quotient:
    /// truncating twice toward zero truncates once.
    fn signed_div_i32(
        &self,
        left: Box<WirInstr>,
        right: Box<WirInstr>,
        type_id: TypeId,
    ) -> WirInstr {
        let head = self.type_table.representation_head(type_id);
        let shift = prim_of(head, self.type_table).map_or(0, |p| 32 - int_bit_width(p));
        if shift == 0 {
            return WirInstr::I32DivS(left, right);
        }
        let widened = WirInstr::I32Shl(left, Box::new(WirInstr::I32Const(shift as i32)));
        WirInstr::I32DivS(
            Box::new(WirInstr::I32DivS(Box::new(widened), right)),
            Box::new(WirInstr::I32Const(1 << shift)),
        )
    }

    /// `left op right` under the float order, where every NaN is one value
    /// greater than `+Inf`: IEEE's instruction, joined with NaN tests where an
    /// operand may be a NaN.
    fn float_order_comparison(
        &mut self,
        op: NirBinaryOp,
        width: FloatWidth,
        left: WirInstr,
        right: WirInstr,
    ) -> WirInstr {
        // Against a constant that is not a NaN, IEEE's instruction answers
        // where it agrees with the order on a NaN operand, and the negation of
        // the opposite instruction answers the rest.
        let order_on_nan = if is_non_nan_const(&right) {
            Some(matches!(
                op,
                NirBinaryOp::NotEq | NirBinaryOp::Gt | NirBinaryOp::GtEq
            ))
        } else if is_non_nan_const(&left) {
            Some(matches!(
                op,
                NirBinaryOp::NotEq | NirBinaryOp::Lt | NirBinaryOp::LtEq
            ))
        } else {
            None
        };
        if let Some(order_on_nan) = order_on_nan {
            let ieee_on_nan = op == NirBinaryOp::NotEq;
            return if order_on_nan == ieee_on_nan {
                width.ieee(op, left, right)
            } else {
                eqz(width.ieee(opposite(op), left, right))
            };
        }

        let (prelude, operands) = self.bind_for_reuse(vec![left, right], width.wir_type());
        let [a, b] = <[WirInstr; 2]>::try_from(operands).expect("two operands bound");
        let is_nan = |x: &WirInstr| width.ieee(NirBinaryOp::NotEq, x.clone(), x.clone());
        let or = |x, y| WirInstr::I32Or(Box::new(x), Box::new(y));
        // `a <= b` is IEEE's `<=` or a NaN `b`, and `a >= b` is IEEE's `>=` or
        // a NaN `a`. The order is total, so `<` and `>` negate them.
        let le = |a: &WirInstr, b: &WirInstr| {
            or(
                width.ieee(NirBinaryOp::LtEq, a.clone(), b.clone()),
                is_nan(b),
            )
        };
        let ge = |a: &WirInstr, b: &WirInstr| {
            or(
                width.ieee(NirBinaryOp::GtEq, a.clone(), b.clone()),
                is_nan(a),
            )
        };
        let eq = |a: &WirInstr, b: &WirInstr| {
            or(
                width.ieee(NirBinaryOp::Eq, a.clone(), b.clone()),
                WirInstr::I32And(Box::new(is_nan(a)), Box::new(is_nan(b))),
            )
        };
        let comparison = match op {
            NirBinaryOp::Eq => eq(&a, &b),
            NirBinaryOp::NotEq => eqz(eq(&a, &b)),
            NirBinaryOp::LtEq => le(&a, &b),
            NirBinaryOp::GtEq => ge(&a, &b),
            NirBinaryOp::Lt => eqz(ge(&a, &b)),
            NirBinaryOp::Gt => eqz(le(&a, &b)),
            not_comparison => unreachable!("[WIR] `{not_comparison:?}` is not a comparison"),
        };
        with_prelude(prelude, comparison)
    }

    /// IEEE's NaN test, `x != x`, which the float order answers false.
    pub(super) fn float_is_nan(&mut self, width: FloatWidth, operand: WirInstr) -> WirInstr {
        let (prelude, mut operands) = self.bind_for_reuse(vec![operand], width.wir_type());
        let x = operands.pop().expect("one operand bound");
        with_prelude(prelude, width.ieee(NirBinaryOp::NotEq, x.clone(), x))
    }

    /// `operands` as instructions each safe to evaluate more than once, and
    /// the `local.set`s that evaluate them once, in order, ahead of the reads.
    /// A `local.get` stays in place only when every operand is a read: an
    /// operand evaluated ahead of it may write the local it reads.
    fn bind_for_reuse(
        &mut self,
        operands: Vec<WirInstr>,
        ty: WirType,
    ) -> (Vec<WirInstr>, Vec<WirInstr>) {
        let is_read = |x: &WirInstr| {
            matches!(
                x,
                WirInstr::LocalGet { .. } | WirInstr::F32Const(_) | WirInstr::F64Const(_)
            )
        };
        if operands.iter().all(is_read) {
            return (Vec::new(), operands);
        }
        let mut prelude = Vec::new();
        let reads = operands
            .into_iter()
            .map(|operand| {
                if matches!(operand, WirInstr::F32Const(_) | WirInstr::F64Const(_)) {
                    return operand;
                }
                let (set, read) = self.spill(operand, ty.clone(), "$float_cmp");
                prelude.extend(set);
                read
            })
            .collect();
        (prelude, reads)
    }

    /// Translate a unary operation to WIR.
    pub(super) fn translate_unary_op(
        &self,
        op: &NirUnaryOp,
        operand: Box<WirInstr>,
        operand_type_id: TypeId,
    ) -> WirInstr {
        match op {
            NirUnaryOp::Neg => match self.scalar_kind(operand_type_id, op) {
                PrimitiveKind::F64 => WirInstr::F64Neg(operand),
                PrimitiveKind::F32 => WirInstr::F32Neg(operand),
                PrimitiveKind::I64Signed | PrimitiveKind::I64Unsigned => {
                    WirInstr::I64Sub(Box::new(WirInstr::I64Const(0)), operand)
                }
                PrimitiveKind::I32Signed | PrimitiveKind::I32Unsigned => {
                    WirInstr::I32Sub(Box::new(WirInstr::I32Const(0)), operand)
                }
            },
            // `!` is logical negation on `bool`, which is already i32-shaped.
            NirUnaryOp::Not => WirInstr::I32Eqz(operand),
            // A `bool` holds one bit, so its complement is `i32.eqz`; the
            // integer `xor -1` would leave `-1` / `-2`, both reading back as
            // `true`.
            NirUnaryOp::BitNot
                if self.type_table.representation_head(operand_type_id) == TypeTable::BOOL =>
            {
                WirInstr::I32Eqz(operand)
            }
            NirUnaryOp::BitNot => match self.scalar_kind(operand_type_id, op) {
                PrimitiveKind::F32 | PrimitiveKind::F64 => {
                    panic!("[WIR] `~` has no float lowering")
                }
                PrimitiveKind::I64Signed | PrimitiveKind::I64Unsigned => {
                    WirInstr::I64Xor(operand, Box::new(WirInstr::I64Const(-1)))
                }
                PrimitiveKind::I32Signed | PrimitiveKind::I32Unsigned => {
                    WirInstr::I32Xor(operand, Box::new(WirInstr::I32Const(-1)))
                }
            },
            // Ref/MutRef/Deref handled above in translate_expr
            NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref => {
                WirInstr::Seq(vec![*operand])
            }
        }
    }

    /// Wrap an i32-producing instruction with sub-32-bit truncation if the
    /// target type is narrower than i32.
    pub(super) fn truncate_to_sub_i32(instr: WirInstr, target: &PrimitiveType) -> WirInstr {
        match target {
            PrimitiveType::I8 => WirInstr::I32Extend8S(Box::new(instr)),
            PrimitiveType::U8 => {
                WirInstr::I32And(Box::new(instr), Box::new(WirInstr::I32Const(0xFF)))
            }
            PrimitiveType::I16 => WirInstr::I32Extend16S(Box::new(instr)),
            PrimitiveType::U16 | PrimitiveType::F16 | PrimitiveType::Bf16 => {
                WirInstr::I32And(Box::new(instr), Box::new(WirInstr::I32Const(0xFFFF)))
            }
            // At or above i32 width: nothing to mask off.
            PrimitiveType::I32
            | PrimitiveType::U32
            | PrimitiveType::I64
            | PrimitiveType::U64
            | PrimitiveType::F32
            | PrimitiveType::F64
            | PrimitiveType::V128
            | PrimitiveType::Bool
            | PrimitiveType::Char => instr,
        }
    }

    /// `instr`, a float of type `from`, clamped to the range of the i32-like
    /// `target`, so the `trunc_sat` that follows saturates at `target`'s
    /// bounds rather than i32's, as Rust's `as` does. A NaN passes through the
    /// clamp, and `trunc_sat` makes it 0; so does a negative value on its way to
    /// an unsigned target, which therefore needs no lower bound.
    fn clamp_float_to(instr: WirInstr, from: PrimitiveType, target: &PrimitiveType) -> WirInstr {
        let (min, max): (Option<f64>, f64) = match target {
            PrimitiveType::I32 | PrimitiveType::U32 => return instr,
            PrimitiveType::I8 => (Some(f64::from(i8::MIN)), f64::from(i8::MAX)),
            PrimitiveType::U8 => (None, f64::from(u8::MAX)),
            PrimitiveType::I16 => (Some(f64::from(i16::MIN)), f64::from(i16::MAX)),
            PrimitiveType::U16 => (None, f64::from(u16::MAX)),
            PrimitiveType::I64
            | PrimitiveType::U64
            | PrimitiveType::F32
            | PrimitiveType::F64
            | PrimitiveType::F16
            | PrimitiveType::Bf16
            | PrimitiveType::V128
            | PrimitiveType::Bool
            | PrimitiveType::Char => {
                panic!("clamp_float_to: {target:?} is not an i32-like integer")
            }
        };
        let clamp = |instr: WirInstr, bound: f64, upper: bool| match (from, upper) {
            (PrimitiveType::F64, true) => {
                WirInstr::F64Min(Box::new(instr), Box::new(WirInstr::F64Const(bound)))
            }
            (PrimitiveType::F64, false) => {
                WirInstr::F64Max(Box::new(instr), Box::new(WirInstr::F64Const(bound)))
            }
            (PrimitiveType::F32, true) => {
                WirInstr::F32Min(Box::new(instr), Box::new(WirInstr::F32Const(bound as f32)))
            }
            (PrimitiveType::F32, false) => {
                WirInstr::F32Max(Box::new(instr), Box::new(WirInstr::F32Const(bound as f32)))
            }
            (other, _) => panic!("clamp_float_to: {other:?} is not f32 or f64"),
        };
        let below_max = clamp(instr, max, true);
        match min {
            Some(min) => clamp(below_max, min, false),
            None => below_max,
        }
    }

    /// Translate a type cast.
    pub(super) fn translate_cast(
        &mut self,
        inner: Operand,
        from_type: TypeId,
        to_type: TypeId,
    ) -> WirInstr {
        // Optimize: int-const cast to i64/u64 → emit I64Const directly to avoid
        // i32 truncation. A pure scalar constant lives in the value pool.
        let int_const = self.body.operand_const_int(inner);
        if let Some(value) = int_const
            && matches!(
                self.type_table.get(to_type),
                ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
            )
        {
            return WirInstr::I64Const(value as i64);
        }

        let inner_instr = self.translate_operand(inner);
        let from = match self.type_table.get(from_type) {
            // A discriminant and a bitmask are unsigned i32s, so they convert
            // as a `u32` does.
            ResolvedType::Enum { .. } | ResolvedType::Flags { .. } => {
                &ResolvedType::Primitive(PrimitiveType::U32)
            }
            other => other,
        };
        let to = self.type_table.get(to_type);

        // Numeric casts: extension/conversion mode is determined by the source
        // type's signedness. Signed sources sign-extend, unsigned sources zero-extend.
        match (from, to) {
            // i32-like signed → i64/u64: sign-extend
            (
                ResolvedType::Primitive(
                    PrimitiveType::I32 | PrimitiveType::I16 | PrimitiveType::I8,
                ),
                ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64),
            ) => WirInstr::I64ExtendI32S(Box::new(inner_instr)),
            // i32-like unsigned → i64/u64: zero-extend
            (
                ResolvedType::Primitive(
                    PrimitiveType::U32
                    | PrimitiveType::U16
                    | PrimitiveType::U8
                    | PrimitiveType::Bool
                    | PrimitiveType::Char,
                ),
                ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64),
            ) => WirInstr::I64ExtendI32U(Box::new(inner_instr)),
            // i64/u64 → i32-like: wrap (truncate lower 32 bits)
            (
                ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::I32
                    | PrimitiveType::U32
                    | PrimitiveType::I16
                    | PrimitiveType::U16
                    | PrimitiveType::I8
                    | PrimitiveType::U8
                    | PrimitiveType::Bool
                    | PrimitiveType::Char),
                ),
            ) => {
                let wrapped = WirInstr::I32WrapI64(Box::new(inner_instr));
                Self::truncate_to_sub_i32(wrapped, to_prim)
            }
            // i32-like signed → f64
            (
                ResolvedType::Primitive(
                    PrimitiveType::I32 | PrimitiveType::I16 | PrimitiveType::I8,
                ),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => WirInstr::F64ConvertI32S(Box::new(inner_instr)),
            // i32-like unsigned → f64
            (
                ResolvedType::Primitive(
                    PrimitiveType::U32
                    | PrimitiveType::U16
                    | PrimitiveType::U8
                    | PrimitiveType::Bool
                    | PrimitiveType::Char,
                ),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => WirInstr::F64ConvertI32U(Box::new(inner_instr)),
            // i32-like signed → f32
            (
                ResolvedType::Primitive(
                    PrimitiveType::I32 | PrimitiveType::I16 | PrimitiveType::I8,
                ),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => WirInstr::F32ConvertI32S(Box::new(inner_instr)),
            // i32-like unsigned → f32
            (
                ResolvedType::Primitive(
                    PrimitiveType::U32
                    | PrimitiveType::U16
                    | PrimitiveType::U8
                    | PrimitiveType::Bool
                    | PrimitiveType::Char,
                ),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => WirInstr::F32ConvertI32U(Box::new(inner_instr)),
            // i64 → f64 (signed)
            (
                ResolvedType::Primitive(PrimitiveType::I64),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => WirInstr::F64ConvertI64S(Box::new(inner_instr)),
            // u64 → f64 (unsigned)
            (
                ResolvedType::Primitive(PrimitiveType::U64),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => WirInstr::F64ConvertI64U(Box::new(inner_instr)),
            // i64 → f32 (signed)
            (
                ResolvedType::Primitive(PrimitiveType::I64),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => WirInstr::F32ConvertI64S(Box::new(inner_instr)),
            // u64 → f32 (unsigned)
            (
                ResolvedType::Primitive(PrimitiveType::U64),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => WirInstr::F32ConvertI64U(Box::new(inner_instr)),
            // f64 → signed i32-like
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::I32 | PrimitiveType::I16 | PrimitiveType::I8),
                ),
            ) => WirInstr::I32TruncSatF64S(Box::new(Self::clamp_float_to(
                inner_instr,
                PrimitiveType::F64,
                to_prim,
            ))),
            // f64 → unsigned i32-like
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::U32 | PrimitiveType::U16 | PrimitiveType::U8),
                ),
            ) => WirInstr::I32TruncSatF64U(Box::new(Self::clamp_float_to(
                inner_instr,
                PrimitiveType::F64,
                to_prim,
            ))),
            // f64 → i64
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => WirInstr::I64TruncSatF64S(Box::new(inner_instr)),
            // f64 → u64
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::U64),
            ) => WirInstr::I64TruncSatF64U(Box::new(inner_instr)),
            // f32 → signed i32-like
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::I32 | PrimitiveType::I16 | PrimitiveType::I8),
                ),
            ) => WirInstr::I32TruncSatF32S(Box::new(Self::clamp_float_to(
                inner_instr,
                PrimitiveType::F32,
                to_prim,
            ))),
            // f32 → unsigned i32-like
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::U32 | PrimitiveType::U16 | PrimitiveType::U8),
                ),
            ) => WirInstr::I32TruncSatF32U(Box::new(Self::clamp_float_to(
                inner_instr,
                PrimitiveType::F32,
                to_prim,
            ))),
            // f32 → i64
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => WirInstr::I64TruncSatF32S(Box::new(inner_instr)),
            // f32 → u64
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::U64),
            ) => WirInstr::I64TruncSatF32U(Box::new(inner_instr)),
            // f64 ↔ f32
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => WirInstr::F32DemoteF64(Box::new(inner_instr)),
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => WirInstr::F64PromoteF32(Box::new(inner_instr)),
            // Same-Wasm-size narrowing (e.g., i32 → u8, u32 → i16)
            (
                ResolvedType::Primitive(
                    PrimitiveType::I32
                    | PrimitiveType::U32
                    | PrimitiveType::I16
                    | PrimitiveType::U16
                    | PrimitiveType::I8
                    | PrimitiveType::U8
                    | PrimitiveType::Bool
                    | PrimitiveType::Char,
                ),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::I8
                    | PrimitiveType::U8
                    | PrimitiveType::I16
                    | PrimitiveType::U16),
                ),
            ) => Self::truncate_to_sub_i32(inner_instr, to_prim),
            // A diverging operand traps before the cast runs, so any target
            // representation holds: nothing is ever converted.
            (ResolvedType::Never, _) => inner_instr,
            _ => {
                // Other casts — newtype and SIMD reinterprets, i32↔u32,
                // struct→struct — are Wasm-level no-ops and pass through, which
                // is valid only when both sides share a representation kind. A
                // reference↔scalar cast here means an earlier phase failed to
                // lower it, so fail loudly at the layer that owns the lowering.
                let from_wir = self.ctx.type_id_to_wir_type(self.type_table, from_type);
                let to_wir = self.ctx.type_id_to_wir_type(self.type_table, to_type);
                assert_eq!(
                    from_wir.scalar_kind(),
                    to_wir.scalar_kind(),
                    "[WIR] cast crosses Wasm representations and was not lowered \
                     before WIR build: {from:?} ({from_wir:?}) as {to:?} ({to_wir:?})",
                    from = self.type_table.get(from_type),
                    to = self.type_table.get(to_type),
                );
                inner_instr
            }
        }
    }
    /// Translate array index read: `arr[i]`
    pub(super) fn translate_index(&mut self, array_op: Operand, index_op: Operand) -> WirInstr {
        let arr = self.translate_operand(array_op);
        let idx = self.translate_operand(index_op);

        let base_type_id = self.type_table.peel_refs(self.operand_type_id(array_op));

        if let Some(element_type_id) = self.type_table.as_list(base_type_id) {
            self.build_list_get(arr, idx, base_type_id, element_type_id)
        } else {
            panic!("[WIR] translate_index: expected array type, got type_id={base_type_id:?}");
        }
    }

    /// Build an array.get instruction sequence.
    /// Given an List<T> struct ref, extracts the repr field and does the appropriate get.
    fn build_list_get(
        &self,
        arr: WirInstr,
        idx: WirInstr,
        array_type_id: TypeId,
        element_type_id: TypeId,
    ) -> WirInstr {
        // Get the List<T> struct WirType
        let list_struct_wir = self.ctx.type_id_to_wir_type(self.type_table, array_type_id);
        let WirType::Ref {
            type_id: list_struct_type,
            ..
        } = list_struct_wir
        else {
            panic!(
                "[WIR] build_list_get: expected Ref List<T> struct, got {list_struct_wir:?} (array_type_id={array_type_id:?})"
            );
        };

        // Get the raw GC array type. Element name must match the
        // key `register_raw_array_type` uses
        // (`mangle_type_arg_for_generic`, qualifies Struct /
        // GenericInstance args by `ModuleSource`).
        let elem_name = self.type_table.mangle_type_arg_for_generic(element_type_id);
        let raw_array_type = self
            .ctx
            .array_type_by_name
            .get(&elem_name)
            .or_else(|| self.ctx.array_type_map.get(&element_type_id))
            .cloned();
        let Some(raw_type) = raw_array_type else {
            panic!(
                "[WIR] build_list_get: raw GC array type not registered (element_type_id={element_type_id:?}, elem_name={elem_name})"
            );
        };

        // StructGet field "repr" (field 0) to get raw array
        let repr_result_ty =
            self.struct_field_wir_type(&list_struct_type, SeqField::Backing.field_name());
        let raw_arr = WirInstr::StructGet {
            type_id: list_struct_type,
            field_name: SeqField::Backing.field_name().to_string(),
            expr: Box::new(arr),
            result_ty: repr_result_ty,
        };

        // Determine appropriate array get instruction based on element type
        let elem_resolved = self.type_table.get(element_type_id);
        let is_ref = matches!(
            elem_resolved,
            ResolvedType::GenericInstance { .. }
                | ResolvedType::Struct { .. }
                | ResolvedType::Function { .. }
                | ResolvedType::Ref(_)
                | ResolvedType::MutRef(_)
                | ResolvedType::Variant { .. }
        );

        let elem_result_ty = self.array_element_wir_type(&raw_type);
        let get_instr = if matches!(
            elem_resolved,
            ResolvedType::Primitive(PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::Bool)
        ) {
            WirInstr::ArrayGetU {
                type_id: raw_type,
                array: Box::new(raw_arr),
                index: Box::new(idx),
                result_ty: elem_result_ty,
            }
        } else if matches!(
            elem_resolved,
            ResolvedType::Primitive(PrimitiveType::I8 | PrimitiveType::I16)
        ) {
            WirInstr::ArrayGetS {
                type_id: raw_type,
                array: Box::new(raw_arr),
                index: Box::new(idx),
                result_ty: elem_result_ty,
            }
        } else {
            WirInstr::ArrayGet {
                type_id: raw_type,
                array: Box::new(raw_arr),
                index: Box::new(idx),
                result_ty: elem_result_ty,
            }
        };

        // For reference element types, convert nullable to non-null
        if is_ref {
            WirInstr::RefAsNonNull(Box::new(get_instr))
        } else {
            get_instr
        }
    }

    /// Translate array index assignment: `arr[i] = val`
    pub(super) fn translate_index_assign(
        &mut self,
        array_op: Operand,
        index_op: Operand,
        val: WirInstr,
    ) -> WirInstr {
        let arr = self.translate_operand(array_op);
        let idx = self.translate_operand(index_op);

        let base_type_id = self.type_table.peel_refs(self.operand_type_id(array_op));

        if let Some(element_type_id) = self.type_table.as_list(base_type_id) {
            let list_struct_wir = self.ctx.type_id_to_wir_type(self.type_table, base_type_id);
            let WirType::Ref {
                type_id: list_struct_type,
                ..
            } = list_struct_wir
            else {
                return WirInstr::Drop(Box::new(val));
            };

            // Same alignment as `build_list_get` above: lookup must
            // use the qualified mangle so the key matches what
            // `register_raw_array_type` registered.
            let elem_name = self.type_table.mangle_type_arg_for_generic(element_type_id);
            let raw_array_type = self
                .ctx
                .array_type_by_name
                .get(&elem_name)
                .or_else(|| self.ctx.array_type_map.get(&element_type_id))
                .cloned();
            let Some(raw_type) = raw_array_type else {
                return WirInstr::Drop(Box::new(val));
            };

            let repr_result_ty =
                self.struct_field_wir_type(&list_struct_type, SeqField::Backing.field_name());
            let raw_arr = WirInstr::StructGet {
                type_id: list_struct_type,
                field_name: SeqField::Backing.field_name().to_string(),
                expr: Box::new(arr),
                result_ty: repr_result_ty,
            };

            WirInstr::ArraySet {
                type_id: raw_type,
                array: Box::new(raw_arr),
                index: Box::new(idx),
                value: Box::new(val),
            }
        } else {
            WirInstr::Drop(Box::new(val))
        }
    }
}

/// The Wasm float type a float comparison runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FloatWidth {
    F32,
    F64,
}

impl FloatWidth {
    fn of(kind: PrimitiveKind) -> Option<Self> {
        match kind {
            PrimitiveKind::F32 => Some(Self::F32),
            PrimitiveKind::F64 => Some(Self::F64),
            PrimitiveKind::I32Signed
            | PrimitiveKind::I32Unsigned
            | PrimitiveKind::I64Signed
            | PrimitiveKind::I64Unsigned => None,
        }
    }

    fn wir_type(self) -> WirType {
        match self {
            Self::F32 => WirType::F32,
            Self::F64 => WirType::F64,
        }
    }

    /// The IEEE 754 comparison Wasm's instruction answers.
    fn ieee(self, op: NirBinaryOp, left: WirInstr, right: WirInstr) -> WirInstr {
        let (l, r) = (Box::new(left), Box::new(right));
        match (self, op) {
            (Self::F32, NirBinaryOp::Eq) => WirInstr::F32Eq(l, r),
            (Self::F32, NirBinaryOp::NotEq) => WirInstr::F32Ne(l, r),
            (Self::F32, NirBinaryOp::Lt) => WirInstr::F32Lt(l, r),
            (Self::F32, NirBinaryOp::LtEq) => WirInstr::F32Le(l, r),
            (Self::F32, NirBinaryOp::Gt) => WirInstr::F32Gt(l, r),
            (Self::F32, NirBinaryOp::GtEq) => WirInstr::F32Ge(l, r),
            (Self::F64, NirBinaryOp::Eq) => WirInstr::F64Eq(l, r),
            (Self::F64, NirBinaryOp::NotEq) => WirInstr::F64Ne(l, r),
            (Self::F64, NirBinaryOp::Lt) => WirInstr::F64Lt(l, r),
            (Self::F64, NirBinaryOp::LtEq) => WirInstr::F64Le(l, r),
            (Self::F64, NirBinaryOp::Gt) => WirInstr::F64Gt(l, r),
            (Self::F64, NirBinaryOp::GtEq) => WirInstr::F64Ge(l, r),
            (Self::F32 | Self::F64, not_comparison) => {
                unreachable!("[WIR] `{not_comparison:?}` is not a comparison")
            }
        }
    }
}

/// The comparison true exactly where `op` is false, for operands that are
/// not NaNs.
fn opposite(op: NirBinaryOp) -> NirBinaryOp {
    match op {
        NirBinaryOp::Eq => NirBinaryOp::NotEq,
        NirBinaryOp::NotEq => NirBinaryOp::Eq,
        NirBinaryOp::Lt => NirBinaryOp::GtEq,
        NirBinaryOp::LtEq => NirBinaryOp::Gt,
        NirBinaryOp::Gt => NirBinaryOp::LtEq,
        NirBinaryOp::GtEq => NirBinaryOp::Lt,
        not_comparison => unreachable!("[WIR] `{not_comparison:?}` is not a comparison"),
    }
}

fn is_non_nan_const(instr: &WirInstr) -> bool {
    matches!(instr, WirInstr::F32Const(v) if !v.is_nan())
        || matches!(instr, WirInstr::F64Const(v) if !v.is_nan())
}

fn eqz(instr: WirInstr) -> WirInstr {
    WirInstr::I32Eqz(Box::new(instr))
}

fn with_prelude(mut prelude: Vec<WirInstr>, value: WirInstr) -> WirInstr {
    if prelude.is_empty() {
        return value;
    }
    prelude.push(value);
    WirInstr::Seq(prelude)
}
