//! Call-site translation — function reference resolution, builtin intrinsic
//! lowering, indirect calls through canonical closures, and closure-to-
//! canonical conversion.
//!
//! These methods are part of `FunctionTranslator`; see `translate.rs` for
//! the struct definition and the primary translation dispatch.

use crate::name::ModuleSource;
use crate::tir::{CallArg, TirExpr, TirExprKind, TypeId, TypeTable};
use crate::wir::{WirInstr, WirType};

use super::context::WirContext;
use super::translate::FunctionTranslator;

/// Extract a compile-time constant i32 from a TIR expression (for SIMD lane indices).
fn extract_i32_const(expr: &TirExpr) -> u8 {
    match &expr.kind {
        TirExprKind::IntLiteral { value, .. } => *value as u8,
        _ => panic!("SIMD lane index must be a constant integer literal"),
    }
}

/// Helper macro for unary f64 builtins.
macro_rules! unary_f64 {
    ($self:expr, $args:expr, $variant:path) => {{
        let o = $self.translate_expr(&$args[0].expr);
        Some($variant(Box::new(o)))
    }};
}

/// Helper macro for binary f64 builtins.
macro_rules! binary_f64 {
    ($self:expr, $args:expr, $variant:path) => {{
        let l = $self.translate_expr(&$args[0].expr);
        let r = $self.translate_expr(&$args[1].expr);
        Some($variant(Box::new(l), Box::new(r)))
    }};
}

/// Helper macro for unary f32 builtins.
macro_rules! unary_f32 {
    ($self:expr, $args:expr, $variant:path) => {{
        let o = $self.translate_expr(&$args[0].expr);
        Some($variant(Box::new(o)))
    }};
}

/// Helper macro for binary f32 builtins.
macro_rules! binary_f32 {
    ($self:expr, $args:expr, $variant:path) => {{
        let l = $self.translate_expr(&$args[0].expr);
        let r = $self.translate_expr(&$args[1].expr);
        Some($variant(Box::new(l), Box::new(r)))
    }};
}

impl FunctionTranslator<'_, '_> {
    /// Resolve a TIR `FunctionRef` to a `WirFuncId`.
    pub(super) fn resolve_function_ref(
        &self,
        func_ref: &crate::tir::FunctionRef,
    ) -> Option<crate::wir::WirFuncId> {
        let module_source = &func_ref.module_source;
        let name = &func_ref.name;

        // Try direct name lookup
        let fq = format!("{module_source}/{name}");
        if let Some(id) = self.ctx.func_map.get(&fq) {
            return Some(id.clone());
        }
        // Try alias registered during import collection (builtin/{func_name})
        let alias = format!("builtin/{name}");
        if let Some(id) = self.ctx.func_map.get(&alias) {
            return Some(id.clone());
        }
        // Try with method info
        if let Some(method_info) = &func_ref.method_info {
            let mangled = method_info.to_mangled_name();
            let fq2 = format!("{module_source}/{mangled}");
            if let Some(id) = self.ctx.func_map.get(&fq2) {
                return Some(id.clone());
            }
            // Newtype fallback: if struct name is a newtype, try the base type name
            if let Some(id) = self.resolve_newtype_method(module_source, method_info) {
                return Some(id);
            }
        }
        None
    }

    /// Try to resolve a method call on a newtype by substituting the base type name.
    /// For example, `Location::sum` → `Point::sum` when `type Location = Point`.
    /// Follows the newtype chain for chained newtypes (C → B → A → Point).
    fn resolve_newtype_method(
        &self,
        module_source: &crate::name::ModuleSource,
        method_info: &crate::name::LocalMethodName,
    ) -> Option<crate::wir::WirFuncId> {
        let struct_name = &method_info.base_struct_name;
        // Find a Newtype in the type table with this name
        let base_name = self.resolve_newtype_to_base_struct_name(struct_name)?;
        // Build a new method name with the base type's struct name
        let mut resolved_info = method_info.clone();
        resolved_info.struct_name.clone_from(&base_name);
        resolved_info.base_struct_name = base_name;
        let mangled = resolved_info.to_mangled_name();
        let fq = format!("{module_source}/{mangled}");
        self.ctx.func_map.get(&fq).cloned()
    }

    /// Resolve a newtype name to the ultimate base struct/primitive name.
    /// Returns `None` if the name is not a newtype.
    fn resolve_newtype_to_base_struct_name(&self, name: &str) -> Option<String> {
        self.type_table
            .get_newtype_ultimate_base_name(name)
            .map(str::to_owned)
    }

    /// Translate a builtin intrinsic call to a WIR instruction.
    ///
    /// Returns `Some(instr)` for instruction-builtins (Wasm instructions),
    /// `None` for import-builtins (handled as regular function calls).
    pub(super) fn translate_builtin_call(
        &mut self,
        builtin_name: &str,
        args: &[CallArg],
        result_type_id: TypeId,
    ) -> Option<WirInstr> {
        match builtin_name {
            "builtin::i32_load" => {
                let addr = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Load {
                    offset: 0,
                    align: 2,
                    addr: Box::new(addr),
                })
            }
            "builtin::i32_load8_u" => {
                let addr = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Load8U {
                    offset: 0,
                    align: 0,
                    addr: Box::new(addr),
                })
            }
            "builtin::i32_load16_u" => {
                let addr = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Load16U {
                    offset: 0,
                    align: 1,
                    addr: Box::new(addr),
                })
            }

            "builtin::i64_load" => {
                let addr = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64Load {
                    offset: 0,
                    align: 3,
                    addr: Box::new(addr),
                })
            }

            "builtin::i32_store" => {
                let addr = self.translate_expr(&args[0].expr);
                let val = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32Store {
                    offset: 0,
                    align: 2,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "builtin::i32_store8" => {
                let addr = self.translate_expr(&args[0].expr);
                let val = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32Store8 {
                    offset: 0,
                    align: 0,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "builtin::i32_store16" => {
                let addr = self.translate_expr(&args[0].expr);
                let val = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32Store16 {
                    offset: 0,
                    align: 1,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "builtin::i64_store" => {
                let addr = self.translate_expr(&args[0].expr);
                let val = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64Store {
                    offset: 0,
                    align: 3,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }

            "builtin::array_new" => {
                // array.new_default: creates a new array of the given element type
                let len = self.translate_expr(&args[0].expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, result_type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    Some(WirInstr::ArrayNewDefault {
                        type_id,
                        len: Box::new(len),
                    })
                } else {
                    None
                }
            }
            "builtin::array_len" => {
                let arr = self.translate_expr(&args[0].expr);
                Some(WirInstr::ArrayLen(Box::new(arr)))
            }
            "builtin::array_get_u8" => {
                let arr = self.translate_expr(&args[0].expr);
                let idx = self.translate_expr(&args[1].expr);
                self.ctx
                    .array_type_by_name
                    .get("u8")
                    .map(|type_id| WirInstr::ArrayGetU {
                        type_id: type_id.clone(),
                        array: Box::new(arr),
                        index: Box::new(idx),
                        result_ty: WirType::I32,
                    })
            }
            "builtin::array_set_u8" => {
                let arr = self.translate_expr(&args[0].expr);
                let idx = self.translate_expr(&args[1].expr);
                let val = self.translate_expr(&args[2].expr);
                self.ctx
                    .array_type_by_name
                    .get("u8")
                    .map(|type_id| WirInstr::ArraySet {
                        type_id: type_id.clone(),
                        array: Box::new(arr),
                        index: Box::new(idx),
                        value: Box::new(val),
                    })
            }
            "builtin::array_get" => {
                let arr = self.translate_expr(&args[0].expr);
                let idx = self.translate_expr(&args[1].expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[0].expr.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    Some(WirInstr::ArrayGet {
                        type_id: type_id.clone(),
                        array: Box::new(arr),
                        index: Box::new(idx),
                        result_ty: self.array_element_wir_type(&type_id),
                    })
                } else {
                    None
                }
            }
            "builtin::array_set" => {
                let arr = self.translate_expr(&args[0].expr);
                let idx = self.translate_expr(&args[1].expr);
                let val = self.translate_expr(&args[2].expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[0].expr.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    Some(WirInstr::ArraySet {
                        type_id,
                        array: Box::new(arr),
                        index: Box::new(idx),
                        value: Box::new(val),
                    })
                } else {
                    None
                }
            }
            "builtin::array_copy" => {
                let dst = self.translate_expr(&args[0].expr);
                let dst_offset = self.translate_expr(&args[1].expr);
                let src = self.translate_expr(&args[2].expr);
                let src_offset = self.translate_expr(&args[3].expr);
                let len = self.translate_expr(&args[4].expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[0].expr.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    Some(WirInstr::ArrayCopy {
                        dest_type_id: type_id.clone(),
                        src_type_id: type_id,
                        dest: Box::new(dst),
                        dest_offset: Box::new(dst_offset),
                        src: Box::new(src),
                        src_offset: Box::new(src_offset),
                        len: Box::new(len),
                    })
                } else {
                    None
                }
            }
            "builtin::array_fill" => {
                let arr = self.translate_expr(&args[0].expr);
                let offset = self.translate_expr(&args[1].expr);
                let val = self.translate_expr(&args[2].expr);
                let len = self.translate_expr(&args[3].expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[0].expr.type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    Some(WirInstr::ArrayFill {
                        type_id,
                        array: Box::new(arr),
                        offset: Box::new(offset),
                        value: Box::new(val),
                        len: Box::new(len),
                    })
                } else {
                    None
                }
            }

            "builtin::f64_abs" => unary_f64!(self, args, WirInstr::F64Abs),
            "builtin::f64_ceil" => unary_f64!(self, args, WirInstr::F64Ceil),
            "builtin::f64_floor" => unary_f64!(self, args, WirInstr::F64Floor),
            "builtin::f64_trunc" => unary_f64!(self, args, WirInstr::F64Trunc),
            "builtin::f64_nearest" => unary_f64!(self, args, WirInstr::F64Nearest),
            "builtin::f64_sqrt" => unary_f64!(self, args, WirInstr::F64Sqrt),
            "builtin::f64_min" => binary_f64!(self, args, WirInstr::F64Min),
            "builtin::f64_max" => binary_f64!(self, args, WirInstr::F64Max),
            "builtin::f64_copysign" => binary_f64!(self, args, WirInstr::F64Copysign),
            "builtin::f32_abs" => unary_f32!(self, args, WirInstr::F32Abs),
            "builtin::f32_ceil" => unary_f32!(self, args, WirInstr::F32Ceil),
            "builtin::f32_floor" => unary_f32!(self, args, WirInstr::F32Floor),
            "builtin::f32_trunc" => unary_f32!(self, args, WirInstr::F32Trunc),
            "builtin::f32_nearest" => unary_f32!(self, args, WirInstr::F32Nearest),
            "builtin::f32_sqrt" => unary_f32!(self, args, WirInstr::F32Sqrt),
            "builtin::f32_min" => binary_f32!(self, args, WirInstr::F32Min),
            "builtin::f32_max" => binary_f32!(self, args, WirInstr::F32Max),
            "builtin::f32_copysign" => binary_f32!(self, args, WirInstr::F32Copysign),

            "builtin::ref_eq" => {
                let l = self.translate_expr(&args[0].expr);
                let r = self.translate_expr(&args[1].expr);
                Some(WirInstr::RefEq(Box::new(l), Box::new(r)))
            }

            "builtin::i32_and" => {
                let l = self.translate_expr(&args[0].expr);
                let r = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32And(Box::new(l), Box::new(r)))
            }
            "builtin::i32_eqz" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Eqz(Box::new(o)))
            }
            "builtin::i32_clz" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Clz(Box::new(o)))
            }
            "builtin::i64_clz" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64Clz(Box::new(o)))
            }
            "builtin::i32_ctz" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Ctz(Box::new(o)))
            }
            "builtin::i64_ctz" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64Ctz(Box::new(o)))
            }
            "builtin::i32_popcnt" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32Popcnt(Box::new(o)))
            }
            "builtin::i64_popcnt" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64Popcnt(Box::new(o)))
            }

            "builtin::i64_reinterpret_f64" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64ReinterpretF64(Box::new(o)))
            }
            "builtin::f64_reinterpret_i64" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64ReinterpretI64(Box::new(o)))
            }
            "builtin::i32_reinterpret_f32" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32ReinterpretF32(Box::new(o)))
            }
            "builtin::f32_reinterpret_i32" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32ReinterpretI32(Box::new(o)))
            }

            "builtin::v128_const" => {
                // The argument is an i128 literal interpreted as v128 bit pattern
                let o = self.translate_expr(&args[0].expr);
                // Extract the constant value - it should be an IntLiteral
                if let WirInstr::I32Const(v) = &o {
                    Some(WirInstr::V128Const(i128::from(*v)))
                } else if let WirInstr::I64Const(v) = &o {
                    Some(WirInstr::V128Const(i128::from(*v)))
                } else {
                    Some(WirInstr::V128Const(0))
                }
            }
            "builtin::v128_not" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::V128Not(Box::new(a)))
            }
            "builtin::v128_and" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::V128And(Box::new(a), Box::new(b)))
            }
            "builtin::v128_or" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::V128Or(Box::new(a), Box::new(b)))
            }
            "builtin::v128_xor" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::V128Xor(Box::new(a), Box::new(b)))
            }
            "builtin::v128_bitselect" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::V128Bitselect(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::i8x16_splat" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I8x16Splat(Box::new(a)))
            }
            "builtin::i8x16_extract_lane_s" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16ExtractLaneS(lane, Box::new(a)))
            }
            "builtin::i8x16_extract_lane_u" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16ExtractLaneU(lane, Box::new(a)))
            }
            "builtin::i8x16_replace_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let v = self.translate_expr(&args[2].expr);
                Some(WirInstr::I8x16ReplaceLane(lane, Box::new(a), Box::new(v)))
            }
            "builtin::i8x16_add" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16Add(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_sub" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16Sub(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_neg" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I8x16Neg(Box::new(a)))
            }
            "builtin::i8x16_eq" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16Eq(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_ne" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16Ne(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_lt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16LtS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_gt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16GtS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_le_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16LeS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_ge_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16GeS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_lt_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16LtU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_gt_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16GtU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_le_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16LeU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_ge_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16GeU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_shl" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16Shl(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_shr_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16ShrS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_shr_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16ShrU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_swizzle" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16Swizzle(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_splat" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8Splat(Box::new(a)))
            }
            "builtin::i16x8_extract_lane_s" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ExtractLaneS(lane, Box::new(a)))
            }
            "builtin::i16x8_extract_lane_u" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ExtractLaneU(lane, Box::new(a)))
            }
            "builtin::i16x8_replace_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let v = self.translate_expr(&args[2].expr);
                Some(WirInstr::I16x8ReplaceLane(lane, Box::new(a), Box::new(v)))
            }
            "builtin::i16x8_add" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Add(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_sub" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Sub(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_mul" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Mul(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_neg" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8Neg(Box::new(a)))
            }
            "builtin::i16x8_eq" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Eq(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_ne" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Ne(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_lt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8LtS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_gt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8GtS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_le_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8LeS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_ge_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8GeS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_lt_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8LtU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_gt_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8GtU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_le_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8LeU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_ge_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8GeU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_shl" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Shl(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_shr_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ShrS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_shr_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ShrU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_splat" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4Splat(Box::new(a)))
            }
            "builtin::i32x4_extract_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ExtractLane(lane, Box::new(a)))
            }
            "builtin::i32x4_replace_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let v = self.translate_expr(&args[2].expr);
                Some(WirInstr::I32x4ReplaceLane(lane, Box::new(a), Box::new(v)))
            }
            "builtin::i32x4_add" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4Add(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_sub" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4Sub(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_mul" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4Mul(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_neg" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4Neg(Box::new(a)))
            }
            "builtin::i32x4_eq" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4Eq(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_ne" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4Ne(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_lt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4LtS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_gt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4GtS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_le_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4LeS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_ge_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4GeS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_lt_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4LtU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_gt_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4GtU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_le_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4LeU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_ge_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4GeU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_shl" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4Shl(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_shr_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ShrS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_shr_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ShrU(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_splat" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2Splat(Box::new(a)))
            }
            "builtin::i64x2_extract_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ExtractLane(lane, Box::new(a)))
            }
            "builtin::i64x2_replace_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let v = self.translate_expr(&args[2].expr);
                Some(WirInstr::I64x2ReplaceLane(lane, Box::new(a), Box::new(v)))
            }
            "builtin::i64x2_add" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2Add(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_sub" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2Sub(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_mul" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2Mul(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_neg" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2Neg(Box::new(a)))
            }
            "builtin::i64x2_eq" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2Eq(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_ne" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2Ne(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_lt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2LtS(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_gt_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2GtS(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_le_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2LeS(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_ge_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2GeS(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_shl" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2Shl(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_shr_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ShrS(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_shr_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ShrU(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_splat" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Splat(Box::new(a)))
            }
            "builtin::f32x4_extract_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4ExtractLane(lane, Box::new(a)))
            }
            "builtin::f32x4_replace_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let v = self.translate_expr(&args[2].expr);
                Some(WirInstr::F32x4ReplaceLane(lane, Box::new(a), Box::new(v)))
            }
            "builtin::f32x4_add" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Add(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_sub" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Sub(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_mul" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Mul(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_div" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Div(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_neg" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Neg(Box::new(a)))
            }
            "builtin::f32x4_sqrt" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Sqrt(Box::new(a)))
            }
            "builtin::f32x4_abs" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Abs(Box::new(a)))
            }
            "builtin::f32x4_eq" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Eq(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_ne" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Ne(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_lt" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Lt(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_gt" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Gt(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_le" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Le(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_ge" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Ge(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_min" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Min(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_max" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4Max(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_splat" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Splat(Box::new(a)))
            }
            "builtin::f64x2_extract_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2ExtractLane(lane, Box::new(a)))
            }
            "builtin::f64x2_replace_lane" => {
                let lane = extract_i32_const(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let v = self.translate_expr(&args[2].expr);
                Some(WirInstr::F64x2ReplaceLane(lane, Box::new(a), Box::new(v)))
            }
            "builtin::f64x2_add" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Add(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_sub" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Sub(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_mul" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Mul(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_div" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Div(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_neg" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Neg(Box::new(a)))
            }
            "builtin::f64x2_sqrt" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Sqrt(Box::new(a)))
            }
            "builtin::f64x2_abs" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Abs(Box::new(a)))
            }
            "builtin::f64x2_eq" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Eq(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_ne" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Ne(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_lt" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Lt(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_gt" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Gt(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_le" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Le(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_ge" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Ge(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_min" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Min(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_max" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2Max(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_abs" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I8x16Abs(Box::new(a)))
            }
            "builtin::i8x16_add_sat_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16AddSatS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_add_sat_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16AddSatU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_sub_sat_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16SubSatS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_sub_sat_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16SubSatU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_min_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16MinS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_min_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16MinU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_max_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16MaxS(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_max_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16MaxU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_avgr_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16AvgrU(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_all_true" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I8x16AllTrue(Box::new(a)))
            }
            "builtin::i8x16_bitmask" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I8x16Bitmask(Box::new(a)))
            }
            "builtin::i8x16_narrow_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16NarrowI16x8S(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_narrow_i16x8_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16NarrowI16x8U(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_popcnt" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I8x16Popcnt(Box::new(a)))
            }
            "builtin::i16x8_abs" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8Abs(Box::new(a)))
            }
            "builtin::i16x8_add_sat_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8AddSatS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_add_sat_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8AddSatU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_sub_sat_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8SubSatS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_sub_sat_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8SubSatU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_min_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8MinS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_min_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8MinU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_max_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8MaxS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_max_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8MaxU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_avgr_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8AvgrU(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_all_true" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8AllTrue(Box::new(a)))
            }
            "builtin::i16x8_bitmask" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8Bitmask(Box::new(a)))
            }
            "builtin::i16x8_narrow_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8NarrowI32x4S(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_narrow_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8NarrowI32x4U(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_extend_low_i8x16_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8ExtendLowI8x16S(Box::new(a)))
            }
            "builtin::i16x8_extend_high_i8x16_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8ExtendHighI8x16S(Box::new(a)))
            }
            "builtin::i16x8_extend_low_i8x16_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8ExtendLowI8x16U(Box::new(a)))
            }
            "builtin::i16x8_extend_high_i8x16_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8ExtendHighI8x16U(Box::new(a)))
            }
            "builtin::i16x8_extmul_low_i8x16_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ExtMulLowI8x16S(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_extmul_high_i8x16_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ExtMulHighI8x16S(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_extmul_low_i8x16_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ExtMulLowI8x16U(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_extmul_high_i8x16_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8ExtMulHighI8x16U(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_extadd_pairwise_i8x16_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8ExtAddPairwiseI8x16S(Box::new(a)))
            }
            "builtin::i16x8_extadd_pairwise_i8x16_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I16x8ExtAddPairwiseI8x16U(Box::new(a)))
            }
            "builtin::i16x8_q15mulr_sat_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8Q15MulrSatS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_abs" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4Abs(Box::new(a)))
            }
            "builtin::i32x4_all_true" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4AllTrue(Box::new(a)))
            }
            "builtin::i32x4_bitmask" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4Bitmask(Box::new(a)))
            }
            "builtin::i32x4_min_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4MinS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_min_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4MinU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_max_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4MaxS(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_max_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4MaxU(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_dot_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4DotI16x8S(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_extend_low_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4ExtendLowI16x8S(Box::new(a)))
            }
            "builtin::i32x4_extend_high_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4ExtendHighI16x8S(Box::new(a)))
            }
            "builtin::i32x4_extend_low_i16x8_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4ExtendLowI16x8U(Box::new(a)))
            }
            "builtin::i32x4_extend_high_i16x8_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4ExtendHighI16x8U(Box::new(a)))
            }
            "builtin::i32x4_extmul_low_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ExtMulLowI16x8S(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_extmul_high_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ExtMulHighI16x8S(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_extmul_low_i16x8_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ExtMulLowI16x8U(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_extmul_high_i16x8_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I32x4ExtMulHighI16x8U(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_extadd_pairwise_i16x8_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4ExtAddPairwiseI16x8S(Box::new(a)))
            }
            "builtin::i32x4_extadd_pairwise_i16x8_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4ExtAddPairwiseI16x8U(Box::new(a)))
            }
            "builtin::i32x4_trunc_sat_f32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4TruncSatF32x4S(Box::new(a)))
            }
            "builtin::i32x4_trunc_sat_f32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4TruncSatF32x4U(Box::new(a)))
            }
            "builtin::i32x4_trunc_sat_f64x2_s_zero" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4TruncSatF64x2SZero(Box::new(a)))
            }
            "builtin::i32x4_trunc_sat_f64x2_u_zero" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4TruncSatF64x2UZero(Box::new(a)))
            }
            "builtin::i64x2_abs" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2Abs(Box::new(a)))
            }
            "builtin::i64x2_all_true" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2AllTrue(Box::new(a)))
            }
            "builtin::i64x2_bitmask" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2Bitmask(Box::new(a)))
            }
            "builtin::i64x2_extend_low_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2ExtendLowI32x4S(Box::new(a)))
            }
            "builtin::i64x2_extend_high_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2ExtendHighI32x4S(Box::new(a)))
            }
            "builtin::i64x2_extend_low_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2ExtendLowI32x4U(Box::new(a)))
            }
            "builtin::i64x2_extend_high_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I64x2ExtendHighI32x4U(Box::new(a)))
            }
            "builtin::i64x2_extmul_low_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ExtMulLowI32x4S(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_extmul_high_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ExtMulHighI32x4S(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_extmul_low_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ExtMulLowI32x4U(Box::new(a), Box::new(b)))
            }
            "builtin::i64x2_extmul_high_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I64x2ExtMulHighI32x4U(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_ceil" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Ceil(Box::new(a)))
            }
            "builtin::f32x4_floor" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Floor(Box::new(a)))
            }
            "builtin::f32x4_trunc" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Trunc(Box::new(a)))
            }
            "builtin::f32x4_nearest" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4Nearest(Box::new(a)))
            }
            "builtin::f32x4_pmin" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4PMin(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_pmax" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4PMax(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_convert_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4ConvertI32x4S(Box::new(a)))
            }
            "builtin::f32x4_convert_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4ConvertI32x4U(Box::new(a)))
            }
            "builtin::f32x4_demote_f64x2_zero" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F32x4DemoteF64x2Zero(Box::new(a)))
            }
            "builtin::f64x2_ceil" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Ceil(Box::new(a)))
            }
            "builtin::f64x2_floor" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Floor(Box::new(a)))
            }
            "builtin::f64x2_trunc" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Trunc(Box::new(a)))
            }
            "builtin::f64x2_nearest" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2Nearest(Box::new(a)))
            }
            "builtin::f64x2_pmin" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2PMin(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_pmax" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2PMax(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_convert_low_i32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2ConvertLowI32x4S(Box::new(a)))
            }
            "builtin::f64x2_convert_low_i32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2ConvertLowI32x4U(Box::new(a)))
            }
            "builtin::f64x2_promote_low_f32x4" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::F64x2PromoteLowF32x4(Box::new(a)))
            }
            "builtin::v128_andnot" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::V128AndNot(Box::new(a), Box::new(b)))
            }
            "builtin::v128_any_true" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::V128AnyTrue(Box::new(a)))
            }
            "builtin::i8x16_relaxed_swizzle" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I8x16RelaxedSwizzle(Box::new(a), Box::new(b)))
            }
            "builtin::i8x16_relaxed_laneselect" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::I8x16RelaxedLaneselect(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::i16x8_relaxed_laneselect" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::I16x8RelaxedLaneselect(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::i32x4_relaxed_laneselect" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::I32x4RelaxedLaneselect(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::i64x2_relaxed_laneselect" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::I64x2RelaxedLaneselect(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::f32x4_relaxed_madd" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::F32x4RelaxedMadd(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::f32x4_relaxed_nmadd" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::F32x4RelaxedNmadd(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::f64x2_relaxed_madd" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::F64x2RelaxedMadd(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::f64x2_relaxed_nmadd" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::F64x2RelaxedNmadd(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }
            "builtin::f32x4_relaxed_min" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4RelaxedMin(Box::new(a), Box::new(b)))
            }
            "builtin::f32x4_relaxed_max" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F32x4RelaxedMax(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_relaxed_min" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2RelaxedMin(Box::new(a), Box::new(b)))
            }
            "builtin::f64x2_relaxed_max" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::F64x2RelaxedMax(Box::new(a), Box::new(b)))
            }
            "builtin::i32x4_relaxed_trunc_f32x4_s" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4RelaxedTruncF32x4S(Box::new(a)))
            }
            "builtin::i32x4_relaxed_trunc_f32x4_u" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4RelaxedTruncF32x4U(Box::new(a)))
            }
            "builtin::i32x4_relaxed_trunc_f64x2_s_zero" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4RelaxedTruncF64x2SZero(Box::new(a)))
            }
            "builtin::i32x4_relaxed_trunc_f64x2_u_zero" => {
                let a = self.translate_expr(&args[0].expr);
                Some(WirInstr::I32x4RelaxedTruncF64x2UZero(Box::new(a)))
            }
            "builtin::i16x8_relaxed_q15mulr_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8RelaxedQ15mulrS(Box::new(a), Box::new(b)))
            }
            "builtin::i16x8_relaxed_dot_i8x16_i7x16_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                Some(WirInstr::I16x8RelaxedDotI8x16I7x16S(
                    Box::new(a),
                    Box::new(b),
                ))
            }
            "builtin::i32x4_relaxed_dot_i8x16_i7x16_add_s" => {
                let a = self.translate_expr(&args[0].expr);
                let b = self.translate_expr(&args[1].expr);
                let c = self.translate_expr(&args[2].expr);
                Some(WirInstr::I32x4RelaxedDotI8x16I7x16AddS(
                    Box::new(a),
                    Box::new(b),
                    Box::new(c),
                ))
            }

            "builtin::memory_grow" => {
                let o = self.translate_expr(&args[0].expr);
                Some(WirInstr::MemoryGrow(Box::new(o)))
            }
            "builtin::memory_size" => Some(WirInstr::MemorySize),
            "builtin::memory_fill" => {
                let dst = self.translate_expr(&args[0].expr);
                let value = self.translate_expr(&args[1].expr);
                let len = self.translate_expr(&args[2].expr);
                Some(WirInstr::MemoryFill {
                    dst: Box::new(dst),
                    value: Box::new(value),
                    len: Box::new(len),
                })
            }

            "builtin::unreachable" => Some(WirInstr::Unreachable),
            "builtin::likely" | "builtin::unlikely" => {
                let likely = builtin_name == "builtin::likely";
                let expr = self.translate_expr(&args[0].expr);
                Some(WirInstr::BranchHint {
                    likely,
                    expr: Box::new(expr),
                })
            }
            "builtin::select" => {
                let cond = self.translate_expr(&args[0].expr);
                let a = self.translate_expr(&args[1].expr);
                let b = self.translate_expr(&args[2].expr);
                let result_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, args[1].expr.type_id);
                Some(WirInstr::Select {
                    condition: Box::new(cond),
                    if_true: Box::new(a),
                    if_false: Box::new(b),
                    ty: Some(result_type),
                })
            }

            "builtin::i64_add128" => {
                let a_lo = Box::new(self.translate_expr(&args[0].expr));
                let a_hi = Box::new(self.translate_expr(&args[1].expr));
                let b_lo = Box::new(self.translate_expr(&args[2].expr));
                let b_hi = Box::new(self.translate_expr(&args[3].expr));
                Some(self.wrap_multivalue_i64(
                    WirInstr::I64Add128(a_lo, a_hi, b_lo, b_hi),
                    result_type_id,
                ))
            }
            "builtin::i64_sub128" => {
                let a_lo = Box::new(self.translate_expr(&args[0].expr));
                let a_hi = Box::new(self.translate_expr(&args[1].expr));
                let b_lo = Box::new(self.translate_expr(&args[2].expr));
                let b_hi = Box::new(self.translate_expr(&args[3].expr));
                Some(self.wrap_multivalue_i64(
                    WirInstr::I64Sub128(a_lo, a_hi, b_lo, b_hi),
                    result_type_id,
                ))
            }
            "builtin::i64_mul_wide_u" => {
                let a = Box::new(self.translate_expr(&args[0].expr));
                let b = Box::new(self.translate_expr(&args[1].expr));
                Some(self.wrap_multivalue_i64(WirInstr::I64MulWideU(a, b), result_type_id))
            }
            "builtin::i64_mul_wide_s" => {
                let a = Box::new(self.translate_expr(&args[0].expr));
                let b = Box::new(self.translate_expr(&args[1].expr));
                Some(self.wrap_multivalue_i64(WirInstr::I64MulWideS(a, b), result_type_id))
            }

            "builtin::i32_as_char" => Some(self.translate_expr(&args[0].expr)),

            "builtin::call_indirect_stdout_write_via_stream"
            | "builtin::call_indirect_stderr_write_via_stream" => {
                // Wado uses stackful async: canon lower without async flag,
                // so sync lower returns the result directly.
                let is_stderr = builtin_name.contains("stderr");
                let wasi_func_name = if is_stderr {
                    "wasi:cli/Stderr::write_via_stream"
                } else {
                    "wasi:cli/Stdout::write_via_stream"
                };
                let key = format!("wasi/{wasi_func_name}");
                if let Some(func_id) = self.ctx.func_map.get(&key).cloned() {
                    let call_args: Vec<WirInstr> =
                        args.iter().map(|a| self.translate_expr(&a.expr)).collect();
                    Some(WirInstr::Call {
                        func_id,
                        args: call_args,
                    })
                } else {
                    None
                }
            }

            // Not an instruction-builtin; fall through to function call resolution
            _ => None,
        }
    }
    /// Translate `IndirectCall { callee, args }` to `call_ref` through canonical closure.
    pub(super) fn translate_indirect_call(
        &mut self,
        callee: &TirExpr,
        args: &[TirExpr],
        result_type: TypeId,
    ) -> WirInstr {
        let callee_wir = self.translate_expr(callee);

        // Look up the Function type to get param/result info
        let fn_type = self.type_table.get(callee.type_id);
        let (param_types, return_type) = match fn_type {
            crate::tir::ResolvedType::Function {
                params,
                return_type,
                ..
            } => (params.clone(), *return_type),
            other => panic!(
                "[WIR] translate_indirect_call: expected Function type, got {other:?} (callee type_id={:?})",
                callee.type_id
            ),
        };

        // Compute canonical closure types directly by signature key
        let param_wirs: Vec<WirType> = param_types
            .iter()
            .map(|p| self.ctx.type_id_to_wir_type(self.type_table, *p))
            .collect();
        let result_wirs: Vec<WirType> =
            if return_type == TypeTable::UNIT || return_type == TypeTable::NEVER {
                vec![]
            } else {
                vec![self.ctx.type_id_to_wir_type(self.type_table, return_type)]
            };
        let key = WirContext::canonical_closure_key(&param_wirs, &result_wirs);
        let (fn_type_id, closure_struct_type_id) = if let Some((ftid, stid)) =
            self.ctx.canonical_closure_types.get(&key)
        {
            (ftid.clone(), stid.clone())
        } else {
            panic!(
                "[WIR] translate_indirect_call: canonical closure type not registered for signature {key:?}"
            );
        };

        // Generate a temp local for the callee as canonical closure struct ref
        let temp_name = format!("__indirect_call_{}", self.local_counter);
        self.local_counter += 1;
        let callee_ref_type = WirType::Ref {
            type_id: closure_struct_type_id.clone(),
            nullable: false,
        };

        // Build: declare temp, cast callee from abstract structref to canonical closure,
        // store, extract env + args + funcref, call_ref
        let mut stmts = vec![
            WirInstr::DeclareLocal {
                name: temp_name.clone(),
                ty: callee_ref_type.clone(),
            },
            WirInstr::LocalSet {
                name: temp_name.clone(),
                value: Box::new(WirInstr::RefCast {
                    type_id: closure_struct_type_id.clone(),
                    nullable: false,
                    expr: Box::new(callee_wir),
                }),
            },
        ];

        // Build args: env, then user args
        let env_result_ty = self.struct_field_wir_type(&closure_struct_type_id, "env");
        let env_arg = WirInstr::StructGet {
            type_id: closure_struct_type_id.clone(),
            field_name: "env".to_string(),
            expr: Box::new(WirInstr::LocalGet {
                name: temp_name.clone(),
                result_ty: callee_ref_type.clone(),
            }),
            result_ty: env_result_ty,
        };

        let mut call_args = vec![env_arg];
        for arg in args {
            let translated = self.translate_expr(arg);
            call_args.push(self.maybe_value_copy(arg, translated));
        }

        // func_ref = struct.get $closure "func"
        let func_result_ty = self.struct_field_wir_type(&closure_struct_type_id, "func");
        let func_ref = WirInstr::StructGet {
            type_id: closure_struct_type_id,
            field_name: "func".to_string(),
            expr: Box::new(WirInstr::LocalGet {
                name: temp_name,
                result_ty: callee_ref_type,
            }),
            result_ty: func_result_ty,
        };

        let call_ref = WirInstr::CallRef {
            type_id: fn_type_id,
            func_ref: Box::new(func_ref),
            args: call_args,
        };

        if result_type == TypeTable::UNIT || result_type == TypeTable::NEVER {
            stmts.push(call_ref);
            WirInstr::Seq(stmts)
        } else {
            // Need to return the call result as the block value
            stmts.push(call_ref);
            let result_wir = self.ctx.type_id_to_wir_type(self.type_table, result_type);
            WirInstr::Block {
                label: None,
                result: Some(result_wir),
                body: stmts,
            }
        }
    }

    /// Translate `ClosureToCanonical` — convert a functor struct to canonical closure.
    pub(super) fn translate_closure_to_canonical(
        &mut self,
        functor: &TirExpr,
        functor_id: u32,
        target_fn_type: TypeId,
        closure_module: &ModuleSource,
    ) -> WirInstr {
        let functor_instr = self.translate_expr(functor);

        // Look up the canonical closure struct type for the target function type
        let fn_resolved = self.type_table.get(target_fn_type);
        let (param_types, return_type) = match fn_resolved {
            crate::tir::ResolvedType::Function {
                params,
                return_type,
                ..
            } => (params.clone(), *return_type),
            other => panic!(
                "[WIR] translate_closure_to_canonical: expected Function type, got {other:?} (target_fn_type={target_fn_type:?})"
            ),
        };

        let param_wirs: Vec<WirType> = param_types
            .iter()
            .map(|p| self.ctx.type_id_to_wir_type(self.type_table, *p))
            .collect();
        let result_wirs: Vec<WirType> =
            if return_type == TypeTable::UNIT || return_type == TypeTable::NEVER {
                vec![]
            } else {
                vec![self.ctx.type_id_to_wir_type(self.type_table, return_type)]
            };

        // Get canonical closure type
        let key = WirContext::canonical_closure_key(&param_wirs, &result_wirs);
        let struct_type_id = if let Some((_, stid)) = self.ctx.canonical_closure_types.get(&key) {
            stid.clone()
        } else {
            panic!(
                "[WIR] translate_closure_to_canonical: canonical closure type not registered for signature {key:?}"
            );
        };

        // Look up the pre-registered wrapper function for this functor.
        // Use closure_module (the module where the closure was defined) for the lookup,
        // not self.module_source (which may differ after cross-module inlining).
        let functor_key = (closure_module.clone(), functor_id);
        let wrapper_func_id = if let Some(id) = self.ctx.closure_wrapper_funcs.get(&functor_key) {
            id.clone()
        } else {
            panic!(
                "[WIR] translate_closure_to_canonical: closure wrapper function not registered for {functor_key:?}"
            );
        };

        // Build: CanonicalClosure { env: functor_as_structref, func: ref.func $wrapper }
        self.struct_new(
            struct_type_id,
            vec![
                functor_instr,
                WirInstr::RefFunc {
                    func_id: wrapper_func_id,
                },
            ],
        )
    }
}
