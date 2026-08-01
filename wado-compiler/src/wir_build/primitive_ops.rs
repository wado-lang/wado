//! Primitive-operation translation — string / bytes literals, binary and
//! unary operators, type casts, and array indexing.
//!
//! These methods are part of `FunctionTranslator`; see `translate.rs` for
//! the struct definition and the primary translation dispatch.

use crate::compiler_item::SeqField;
use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::wir::{WirInstr, WirType};

use super::translate::FunctionTranslator;
use crate::nir_arena::Operand;

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
    /// The scalar kind a `TypeId` is represented by, or `None` when it has no
    /// scalar representation at all.
    ///
    /// Newtypes classify as their base — the wrapper is erased by then, so
    /// `type Meters = f64` must still reach the `f64` opcodes. An enum is its
    /// i32 discriminant and a flags set its i32 bitmask, so both are scalars.
    /// `None` covers reference types and the widths WIR carries no scalar for
    /// (`i128` / `u128` / `v128`).
    fn from_type_id(type_table: &TypeTable, type_id: TypeId) -> Option<Self> {
        match type_table.get(type_table.resolve_newtype_base(type_id)) {
            ResolvedType::Primitive(p) => Self::from_primitive(*p),
            // Discriminants and bitmasks are non-negative but lower through the
            // signed opcodes, which agree with the unsigned ones over their
            // range and keep the emitted code identical to the untyped
            // predecessor of this classification.
            ResolvedType::Enum { .. } | ResolvedType::Flags { .. } => Some(Self::I32Signed),
            // A `Never` operand diverges before the operation runs, so the
            // result is never observed and the surrounding lowering discards the
            // node. It still needs *an* opcode to keep its shape, and i32 is the
            // narrowest. (`Unit` is not here: it has no value to feed an opcode
            // at all, and `binop_operand_requires_trait` rejects it.)
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
            PrimitiveType::I128 | PrimitiveType::U128 | PrimitiveType::V128 => return None,
        })
    }
}

impl FunctionTranslator<'_, '_> {
    /// Translate a raw constant `Array<u8>` (`ExprKind::PackedArray`) — the
    /// `repr` of a `String` / `List<u8>` literal — to WIR.
    ///
    /// Short payloads use a constant `array.new_fixed<u8>` (a valid Wasm const
    /// instruction, so a const sequence global can be promoted eager by
    /// `wir_optimize::const_global`); longer ones use a passive `array.new_data`
    /// segment (compact, but not const). The `String` / `List<u8>` struct
    /// wrapping is emitted by the enclosing `StructLiteral`.
    pub(super) fn translate_packed_array(&self, b: &[u8]) -> WirInstr {
        let byte_len = b.len();
        let array_type_id = self
            .ctx
            .array_type_by_name
            .get("u8")
            .cloned()
            .expect("[WIR] PackedArray: u8 array type not registered");

        if byte_len == 0 {
            WirInstr::ArrayNewDefault {
                type_id: array_type_id,
                len: Box::new(WirInstr::I32Const(0)),
            }
        } else if crate::wir_build::packed_array_is_eager(
            byte_len,
            self.ctx.package.string_inline_max_bytes,
            self.force_fixed_string_repr,
        ) {
            let elements = b
                .iter()
                .map(|&x| WirInstr::I32Const(i32::from(x)))
                .collect();
            WirInstr::ArrayNewFixed {
                type_id: array_type_id,
                elements,
            }
        } else {
            // Every payload longer than `string_inline_max_bytes` is registered
            // by `register_literal_data` under the same threshold, so a miss here
            // means the two partitions disagreed — fail loudly instead of
            // silently emitting segment 0 (a different literal's bytes).
            let data_index = self.ctx.packed_data_map.get(b).copied().expect(
                "[WIR] PackedArray: long payload missing from packed_data_map (registration must cover every >threshold literal)",
            );
            let len_i32 = i32::try_from(byte_len)
                .unwrap_or_else(|_| panic!("[WIR] literal of {byte_len} bytes exceeds i32 length"));
            WirInstr::ArrayNewData {
                type_id: array_type_id,
                data_index,
                offset: Box::new(WirInstr::I32Const(0)),
                len: Box::new(WirInstr::I32Const(len_i32)),
            }
        }
    }

    /// The scalar kind `type_id` lowers to, for an operator that only has
    /// scalar opcodes. Type checking rejects the operator on anything else, so
    /// a non-scalar operand here means an earlier phase let one through — and
    /// the i32 opcodes this used to fall back to would silently misread an
    /// `f64` or a reference.
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
        &self,
        op: &NirBinaryOp,
        left: Box<WirInstr>,
        right: Box<WirInstr>,
        left_type_id: TypeId,
    ) -> WirInstr {
        // `RefEq` / `RefNotEq` are the reference-identity operators; they take
        // operands with no scalar kind, so the classification stays inside the
        // scalar arms below.
        if let NirBinaryOp::RefEq = op {
            return WirInstr::RefEq(left, right);
        }
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
                PrimitiveKind::I32Signed => WirInstr::I32DivS(left, right),
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
            NirBinaryOp::Eq => match kind {
                PrimitiveKind::F64 => WirInstr::F64Eq(left, right),
                PrimitiveKind::F32 => WirInstr::F32Eq(left, right),
                PrimitiveKind::I64Signed | PrimitiveKind::I64Unsigned => {
                    WirInstr::I64Eq(left, right)
                }
                PrimitiveKind::I32Signed | PrimitiveKind::I32Unsigned => {
                    WirInstr::I32Eq(left, right)
                }
            },
            NirBinaryOp::NotEq => match kind {
                PrimitiveKind::F64 => WirInstr::F64Ne(left, right),
                PrimitiveKind::F32 => WirInstr::F32Ne(left, right),
                PrimitiveKind::I64Signed | PrimitiveKind::I64Unsigned => {
                    WirInstr::I64Ne(left, right)
                }
                PrimitiveKind::I32Signed | PrimitiveKind::I32Unsigned => {
                    WirInstr::I32Ne(left, right)
                }
            },
            NirBinaryOp::Lt => match kind {
                PrimitiveKind::F64 => WirInstr::F64Lt(left, right),
                PrimitiveKind::F32 => WirInstr::F32Lt(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64LtU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64LtS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32LtU(left, right),
                PrimitiveKind::I32Signed => WirInstr::I32LtS(left, right),
            },
            NirBinaryOp::LtEq => match kind {
                PrimitiveKind::F64 => WirInstr::F64Le(left, right),
                PrimitiveKind::F32 => WirInstr::F32Le(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64LeU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64LeS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32LeU(left, right),
                PrimitiveKind::I32Signed => WirInstr::I32LeS(left, right),
            },
            NirBinaryOp::Gt => match kind {
                PrimitiveKind::F64 => WirInstr::F64Gt(left, right),
                PrimitiveKind::F32 => WirInstr::F32Gt(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64GtU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64GtS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32GtU(left, right),
                PrimitiveKind::I32Signed => WirInstr::I32GtS(left, right),
            },
            NirBinaryOp::GtEq => match kind {
                PrimitiveKind::F64 => WirInstr::F64Ge(left, right),
                PrimitiveKind::F32 => WirInstr::F32Ge(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64GeU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64GeS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32GeU(left, right),
                PrimitiveKind::I32Signed => WirInstr::I32GeS(left, right),
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
            }
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
            }
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
            }
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
            }
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
            NirBinaryOp::RefEq | NirBinaryOp::RefNotEq => unreachable!(),
        }
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
            PrimitiveType::U16 => {
                WirInstr::I32And(Box::new(instr), Box::new(WirInstr::I32Const(0xFFFF)))
            }
            // Already at or above i32 width: the value occupies the whole
            // register, so there is nothing to mask off.
            PrimitiveType::I32
            | PrimitiveType::U32
            | PrimitiveType::I64
            | PrimitiveType::U64
            | PrimitiveType::I128
            | PrimitiveType::U128
            | PrimitiveType::F32
            | PrimitiveType::F64
            | PrimitiveType::V128
            | PrimitiveType::Bool
            | PrimitiveType::Char => instr,
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
        let from = self.type_table.get(from_type);
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
            ) => {
                let truncated = WirInstr::I32TruncF64S(Box::new(inner_instr));
                Self::truncate_to_sub_i32(truncated, to_prim)
            }
            // f64 → unsigned i32-like
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::U32 | PrimitiveType::U16 | PrimitiveType::U8),
                ),
            ) => {
                let truncated = WirInstr::I32TruncF64U(Box::new(inner_instr));
                Self::truncate_to_sub_i32(truncated, to_prim)
            }
            // f64 → i64
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => WirInstr::I64TruncF64S(Box::new(inner_instr)),
            // f64 → u64
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::U64),
            ) => WirInstr::I64TruncF64U(Box::new(inner_instr)),
            // f32 → signed i32-like
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::I32 | PrimitiveType::I16 | PrimitiveType::I8),
                ),
            ) => {
                let truncated = WirInstr::I32TruncF32S(Box::new(inner_instr));
                Self::truncate_to_sub_i32(truncated, to_prim)
            }
            // f32 → unsigned i32-like
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(
                    to_prim @ (PrimitiveType::U32 | PrimitiveType::U16 | PrimitiveType::U8),
                ),
            ) => {
                let truncated = WirInstr::I32TruncF32U(Box::new(inner_instr));
                Self::truncate_to_sub_i32(truncated, to_prim)
            }
            // f32 → i64
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => WirInstr::I64TruncF32S(Box::new(inner_instr)),
            // f32 → u64
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::U64),
            ) => WirInstr::I64TruncF32U(Box::new(inner_instr)),
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
            _ => {
                // Other casts (newtype reinterprets, SIMD `v128` lane-type
                // reinterprets, enum→i32, struct→struct) are no-ops at the
                // Wasm level and pass through. But a pass-through only
                // produces valid Wasm when both sides share a representation
                // kind. A reference↔scalar cast reaching here means an
                // earlier phase failed to lower it — e.g. the struct-source
                // `i128/u128 as f64` of issue #1328, which used to silently
                // feed a boxed `(ref $type)` into an `f64` slot and only
                // tripped the validator deep in codegen. Fail loudly here,
                // at the layer that owns the lowering, instead.
                let from_wir = self.ctx.type_id_to_wir_type(self.type_table, from_type);
                let to_wir = self.ctx.type_id_to_wir_type(self.type_table, to_type);
                assert_eq!(
                    from_wir.is_reference(),
                    to_wir.is_reference(),
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
