//! Primitive-operation translation — string / bytes literals, binary and
//! unary operators, type casts, and array indexing.
//!
//! These methods are part of `FunctionTranslator`; see `translate.rs` for
//! the struct definition and the primary translation dispatch.

use crate::compiler_item::SeqField;
use crate::module_source::ModuleSource;
use crate::nir::{NirBinaryOp, NirExpr, NirExprKind, NirUnaryOp};
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::wir::{WirInstr, WirType};

use super::translate::FunctionTranslator;

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
    /// Anything else — reference types or primitives not handled by
    /// scalar binop/unop dispatch (e.g., `i128`, `u128`, `v128`).
    Other,
}

impl PrimitiveKind {
    /// Classify a `TypeId`'s underlying primitive.
    fn from_type_id(type_table: &TypeTable, type_id: TypeId) -> Self {
        match type_table.get(type_id) {
            ResolvedType::Primitive(p) => Self::from_primitive(*p),
            _ => Self::Other,
        }
    }

    /// Classify a `PrimitiveType` value.
    fn from_primitive(p: PrimitiveType) -> Self {
        match p {
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
            PrimitiveType::I128 | PrimitiveType::U128 | PrimitiveType::V128 => Self::Other,
        }
    }

    /// True for `I64Signed` / `I64Unsigned`.
    fn is_i64(self) -> bool {
        matches!(self, Self::I64Signed | Self::I64Unsigned)
    }

    /// True for unsigned integer families (`I32Unsigned`, `I64Unsigned`).
    fn is_unsigned(self) -> bool {
        matches!(self, Self::I32Unsigned | Self::I64Unsigned)
    }
}

impl FunctionTranslator<'_, '_> {
    /// Translate a string literal to WIR instructions.
    ///
    /// Short strings use a constant `array.new_fixed<u8>` repr; longer strings
    /// use a passive `array.new_data` data segment:
    ///   `array.new_data` $`u8_array`, $`data_idx` (offset=0, len=bytes)
    ///   struct.new $String (repr=array, used=len)
    pub(super) fn translate_string_literal(&self, s: &str) -> WirInstr {
        let byte_len = s.len();

        // Look up u8 array type
        let u8_array_type = self.ctx.array_type_by_name.get("u8").cloned();

        // Look up String struct type
        let string_struct_name =
            crate::name::StructName::new(ModuleSource::string(), "String".to_string());
        let string_type = self.ctx.struct_type_map.get(&string_struct_name).cloned();

        let (Some(array_type_id), Some(string_type_id)) = (u8_array_type, string_type) else {
            panic!("[WIR] String literal: u8 array or String struct type not registered");
        };

        let len_i32 = i32::try_from(byte_len).unwrap_or(0);

        if byte_len == 0 {
            // Empty string: array.new_default + struct.new
            self.struct_new(
                string_type_id,
                vec![
                    WirInstr::ArrayNewDefault {
                        type_id: array_type_id,
                        len: Box::new(WirInstr::I32Const(0)),
                    },
                    WirInstr::I32Const(0),
                ],
            )
        } else if byte_len <= self.ctx.package.string_inline_max_bytes {
            // Short string: materialize the bytes with `array.new_fixed<u8>`, a
            // valid Wasm *constant* instruction. This lets a constant string
            // global be promoted to an eager Wasm constant by
            // `wir_optimize::const_global` (the data-segment form below is not a
            // constant instruction, so longer strings stay lazy). `u8` elements
            // are carried as `I32Const`, like every other `u8` array literal.
            let elements = s
                .bytes()
                .map(|b| WirInstr::I32Const(i32::from(b)))
                .collect();
            self.struct_new(
                string_type_id,
                vec![
                    WirInstr::ArrayNewFixed {
                        type_id: array_type_id,
                        elements,
                    },
                    WirInstr::I32Const(len_i32),
                ],
            )
        } else {
            // Longer string: passive data segment (compact, but not constant).
            let data_index = self.ctx.string_literal_map.get(s).copied().unwrap_or(0);
            self.struct_new(
                string_type_id,
                vec![
                    WirInstr::ArrayNewData {
                        type_id: array_type_id,
                        data_index,
                        offset: Box::new(WirInstr::I32Const(0)),
                        len: Box::new(WirInstr::I32Const(len_i32)),
                    },
                    WirInstr::I32Const(len_i32),
                ],
            )
        }
    }

    /// Translate a bytes literal to WIR instructions.
    ///
    /// Creates an `List<u8>` struct from a passive data segment:
    ///   `array.new_data` $`u8_array`, $`data_idx` (offset=0, len=bytes)
    ///   struct.new $`List<u8>` (repr=array, used=len)
    pub(super) fn translate_bytes_literal(&self, b: &[u8]) -> WirInstr {
        let byte_len = b.len();

        // Look up u8 GC array type
        let u8_array_type = self.ctx.array_type_by_name.get("u8").cloned();

        // Look up List<u8> wrapper struct type
        let mangled =
            crate::name::mangle_generic_name("List", std::slice::from_ref(&"u8".to_string()));
        let list_struct_name = crate::name::StructName::new(ModuleSource::prelude(), mangled);
        let list_struct_type = self.ctx.struct_type_map.get(&list_struct_name).cloned();

        let (Some(gc_array_type_id), Some(struct_type_id)) = (u8_array_type, list_struct_type)
        else {
            panic!("[WIR] Bytes literal: u8 array or List<u8> struct type not registered");
        };

        if byte_len == 0 {
            self.struct_new(
                struct_type_id,
                vec![
                    WirInstr::ArrayNewDefault {
                        type_id: gc_array_type_id,
                        len: Box::new(WirInstr::I32Const(0)),
                    },
                    WirInstr::I32Const(0),
                ],
            )
        } else {
            let data_index = self.ctx.bytes_literal_map.get(b).copied().unwrap_or(0);
            let len_i32 = i32::try_from(byte_len).unwrap_or(0);

            self.struct_new(
                struct_type_id,
                vec![
                    WirInstr::ArrayNewData {
                        type_id: gc_array_type_id,
                        data_index,
                        offset: Box::new(WirInstr::I32Const(0)),
                        len: Box::new(WirInstr::I32Const(len_i32)),
                    },
                    WirInstr::I32Const(len_i32),
                ],
            )
        }
    }

    /// Translate a binary operation to WIR.
    pub(super) fn translate_binary_op(
        &self,
        op: &NirBinaryOp,
        left: Box<WirInstr>,
        right: Box<WirInstr>,
        left_type_id: TypeId,
    ) -> WirInstr {
        let kind = PrimitiveKind::from_type_id(self.type_table, left_type_id);

        match op {
            NirBinaryOp::Add => match kind {
                PrimitiveKind::F64 => WirInstr::F64Add(left, right),
                PrimitiveKind::F32 => WirInstr::F32Add(left, right),
                k if k.is_i64() => WirInstr::I64Add(left, right),
                _ => WirInstr::I32Add(left, right),
            },
            NirBinaryOp::Sub => match kind {
                PrimitiveKind::F64 => WirInstr::F64Sub(left, right),
                PrimitiveKind::F32 => WirInstr::F32Sub(left, right),
                k if k.is_i64() => WirInstr::I64Sub(left, right),
                _ => WirInstr::I32Sub(left, right),
            },
            NirBinaryOp::Mul => match kind {
                PrimitiveKind::F64 => WirInstr::F64Mul(left, right),
                PrimitiveKind::F32 => WirInstr::F32Mul(left, right),
                k if k.is_i64() => WirInstr::I64Mul(left, right),
                _ => WirInstr::I32Mul(left, right),
            },
            NirBinaryOp::Div => match kind {
                PrimitiveKind::F64 => WirInstr::F64Div(left, right),
                PrimitiveKind::F32 => WirInstr::F32Div(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64DivU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64DivS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32DivU(left, right),
                _ => WirInstr::I32DivS(left, right),
            },
            NirBinaryOp::Mod => match kind {
                PrimitiveKind::I64Unsigned => WirInstr::I64RemU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64RemS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32RemU(left, right),
                _ => WirInstr::I32RemS(left, right),
            },
            NirBinaryOp::Eq => match kind {
                PrimitiveKind::F64 => WirInstr::F64Eq(left, right),
                PrimitiveKind::F32 => WirInstr::F32Eq(left, right),
                k if k.is_i64() => WirInstr::I64Eq(left, right),
                _ => WirInstr::I32Eq(left, right),
            },
            NirBinaryOp::NotEq => match kind {
                PrimitiveKind::F64 => WirInstr::F64Ne(left, right),
                PrimitiveKind::F32 => WirInstr::F32Ne(left, right),
                k if k.is_i64() => WirInstr::I64Ne(left, right),
                _ => WirInstr::I32Ne(left, right),
            },
            NirBinaryOp::Lt => match kind {
                PrimitiveKind::F64 => WirInstr::F64Lt(left, right),
                PrimitiveKind::F32 => WirInstr::F32Lt(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64LtU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64LtS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32LtU(left, right),
                _ => WirInstr::I32LtS(left, right),
            },
            NirBinaryOp::LtEq => match kind {
                PrimitiveKind::F64 => WirInstr::F64Le(left, right),
                PrimitiveKind::F32 => WirInstr::F32Le(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64LeU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64LeS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32LeU(left, right),
                _ => WirInstr::I32LeS(left, right),
            },
            NirBinaryOp::Gt => match kind {
                PrimitiveKind::F64 => WirInstr::F64Gt(left, right),
                PrimitiveKind::F32 => WirInstr::F32Gt(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64GtU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64GtS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32GtU(left, right),
                _ => WirInstr::I32GtS(left, right),
            },
            NirBinaryOp::GtEq => match kind {
                PrimitiveKind::F64 => WirInstr::F64Ge(left, right),
                PrimitiveKind::F32 => WirInstr::F32Ge(left, right),
                PrimitiveKind::I64Unsigned => WirInstr::I64GeU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64GeS(left, right),
                PrimitiveKind::I32Unsigned => WirInstr::I32GeU(left, right),
                _ => WirInstr::I32GeS(left, right),
            },
            NirBinaryOp::And | NirBinaryOp::BitAnd => {
                if kind.is_i64() {
                    WirInstr::I64And(left, right)
                } else {
                    WirInstr::I32And(left, right)
                }
            }
            NirBinaryOp::Or | NirBinaryOp::BitOr => {
                if kind.is_i64() {
                    WirInstr::I64Or(left, right)
                } else {
                    WirInstr::I32Or(left, right)
                }
            }
            NirBinaryOp::BitXor => {
                if kind.is_i64() {
                    WirInstr::I64Xor(left, right)
                } else {
                    WirInstr::I32Xor(left, right)
                }
            }
            NirBinaryOp::Shl => {
                if kind.is_i64() {
                    WirInstr::I64Shl(left, right)
                } else {
                    WirInstr::I32Shl(left, right)
                }
            }
            NirBinaryOp::Shr => match kind {
                PrimitiveKind::I64Unsigned => WirInstr::I64ShrU(left, right),
                PrimitiveKind::I64Signed => WirInstr::I64ShrS(left, right),
                k if k.is_unsigned() => WirInstr::I32ShrU(left, right),
                _ => WirInstr::I32ShrS(left, right),
            },
            NirBinaryOp::RefEq => WirInstr::RefEq(left, right),
            NirBinaryOp::RefNotEq => WirInstr::I32Eqz(Box::new(WirInstr::RefEq(left, right))),
        }
    }

    /// Translate a unary operation to WIR.
    pub(super) fn translate_unary_op(
        &self,
        op: &NirUnaryOp,
        operand: Box<WirInstr>,
        operand_type_id: TypeId,
    ) -> WirInstr {
        let kind = PrimitiveKind::from_type_id(self.type_table, operand_type_id);

        match op {
            NirUnaryOp::Neg => match kind {
                PrimitiveKind::F64 => WirInstr::F64Neg(operand),
                PrimitiveKind::F32 => WirInstr::F32Neg(operand),
                k if k.is_i64() => WirInstr::I64Sub(Box::new(WirInstr::I64Const(0)), operand),
                _ => WirInstr::I32Sub(Box::new(WirInstr::I32Const(0)), operand),
            },
            NirUnaryOp::Not => WirInstr::I32Eqz(operand),
            NirUnaryOp::BitNot => {
                if kind.is_i64() {
                    WirInstr::I64Xor(operand, Box::new(WirInstr::I64Const(-1)))
                } else {
                    WirInstr::I32Xor(operand, Box::new(WirInstr::I32Const(-1)))
                }
            }
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
            _ => instr,
        }
    }

    /// Translate a type cast.
    pub(super) fn translate_cast(
        &mut self,
        inner: &NirExpr,
        from_type: TypeId,
        to_type: TypeId,
    ) -> WirInstr {
        // Optimize: IntLiteral cast to i64/u64 → emit I64Const directly to avoid i32 truncation
        if let NirExprKind::IntLiteral { value, .. } = &inner.kind
            && matches!(
                self.type_table.get(to_type),
                ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
            )
        {
            return WirInstr::I64Const(*value as i64);
        }

        let inner_instr = self.translate_expr(inner);
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
                // For other casts (struct casts, etc.), just pass through
                inner_instr
            }
        }
    }
    /// Translate array index read: `arr[i]`
    pub(super) fn translate_index(
        &mut self,
        array_expr: &NirExpr,
        index_expr: &NirExpr,
    ) -> WirInstr {
        let arr = self.translate_expr(array_expr);
        let idx = self.translate_expr(index_expr);

        // Unwrap reference types
        let base_type_id = match self.type_table.get(array_expr.type_id) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => array_expr.type_id,
        };

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
        array_expr: &NirExpr,
        index_expr: &NirExpr,
        val: WirInstr,
    ) -> WirInstr {
        let arr = self.translate_expr(array_expr);
        let idx = self.translate_expr(index_expr);

        let base_type_id = match self.type_table.get(array_expr.type_id) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => array_expr.type_id,
        };

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
