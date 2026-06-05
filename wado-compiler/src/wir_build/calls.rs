//! Call-site translation — function reference resolution, builtin intrinsic
//! lowering, indirect calls through canonical closures, and closure-to-
//! canonical conversion.
//!
//! These methods are part of `FunctionTranslator`; see `translate.rs` for
//! the struct definition and the primary translation dispatch.

use crate::module_source::ModuleSource;
use crate::tir::{TypeId, TypeTable};
use crate::wir::{WirInstr, WirType};

use super::context::WirContext;
use super::translate::FunctionTranslator;
use crate::nir_arena::{ArenaCallArg, ExprId, ExprKind};

/// Extract a compile-time constant i32 from a TIR expression (for SIMD lane indices).
fn extract_i32_const(kind: &ExprKind) -> u8 {
    match kind {
        ExprKind::IntLiteral { value, .. } => *value as u8,
        _ => panic!("SIMD lane index must be a constant integer literal"),
    }
}

/// Mechanical builtin shapes — each evaluates its operand expressions
/// left-to-right (operand order matters: `translate_expr` can register
/// locals and literals) and wraps them in the given `WirInstr` variant.
macro_rules! unary {
    ($self:expr, $args:expr, $variant:path) => {{
        let a = $self.translate_expr($args[0].expr);
        $variant(Box::new(a))
    }};
}

macro_rules! binary {
    ($self:expr, $args:expr, $variant:path) => {{
        let a = $self.translate_expr($args[0].expr);
        let b = $self.translate_expr($args[1].expr);
        $variant(Box::new(a), Box::new(b))
    }};
}

macro_rules! ternary {
    ($self:expr, $args:expr, $variant:path) => {{
        let a = $self.translate_expr($args[0].expr);
        let b = $self.translate_expr($args[1].expr);
        let c = $self.translate_expr($args[2].expr);
        $variant(Box::new(a), Box::new(b), Box::new(c))
    }};
}

/// SIMD lane access — the lane index is a compile-time constant operand.
macro_rules! extract_lane {
    ($self:expr, $args:expr, $variant:path) => {{
        let lane = extract_i32_const(&$self.body.exprs[$args[0].expr].kind);
        let a = $self.translate_expr($args[1].expr);
        $variant(lane, Box::new(a))
    }};
}

macro_rules! replace_lane {
    ($self:expr, $args:expr, $variant:path) => {{
        let lane = extract_i32_const(&$self.body.exprs[$args[0].expr].kind);
        let a = $self.translate_expr($args[1].expr);
        let v = $self.translate_expr($args[2].expr);
        $variant(lane, Box::new(a), Box::new(v))
    }};
}

impl FunctionTranslator<'_, '_> {
    /// Resolve a TIR `FunctionRef` to a `WirFuncId`.
    pub(super) fn resolve_function_ref(
        &self,
        func_ref: &crate::nir::FunctionRef,
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
        module_source: &ModuleSource,
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
    /// For `builtin::array_clone::<T>(arr)` whose `arr` is typed as
    /// `BuiltinArray(elem)`, return the helper-name suffix to invoke on
    /// every element when `elem` is itself a value-typed struct (i.e.
    /// has a `$value_copy$T<id>` synthesized for it). Returns `None`
    /// for primitive elements where Wasm GC's plain `array.set` is
    /// already a deep copy.
    fn array_element_copy_func(&self, src_type_id: TypeId) -> Option<String> {
        use crate::tir::ResolvedType;
        let mut ty = src_type_id;
        while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = self.type_table.get(ty) {
            ty = *inner;
        }
        let elem = match self.type_table.get(ty) {
            ResolvedType::BuiltinArray(elem) => *elem,
            _ => return None,
        };
        if !crate::lower::plan::value_copy::needs_value_copy(elem, self.type_table) {
            return None;
        }
        Some(format!("$value_copy$T{}", elem.0))
    }

    ///
    /// Returns `Some(instr)` for instruction-builtins (Wasm instructions),
    /// `None` for import-builtins (handled as regular function calls).
    pub(super) fn translate_builtin_call(
        &mut self,
        builtin_name: &str,
        args: &[ArenaCallArg],
        result_type_id: TypeId,
    ) -> Option<WirInstr> {
        // Mechanical numeric / SIMD intrinsics map a builtin name onto a
        // fixed `WirInstr` shape with no surrounding context; they are
        // tabulated in `translate_mechanical_builtin`. Everything left here
        // needs type/context lookups, multi-value wrapping, or special
        // lowering.
        if let Some(instr) = self.translate_mechanical_builtin(builtin_name, args) {
            return Some(instr);
        }

        match builtin_name {
            "builtin::i32_load" => {
                let addr = self.translate_expr(args[0].expr);
                Some(WirInstr::I32Load {
                    offset: 0,
                    align: 2,
                    addr: Box::new(addr),
                })
            }
            "builtin::i32_load8_u" => {
                let addr = self.translate_expr(args[0].expr);
                Some(WirInstr::I32Load8U {
                    offset: 0,
                    align: 0,
                    addr: Box::new(addr),
                })
            }
            "builtin::i32_load8_s" => {
                let addr = self.translate_expr(args[0].expr);
                Some(WirInstr::I32Load8S {
                    offset: 0,
                    align: 0,
                    addr: Box::new(addr),
                })
            }
            "builtin::i32_load16_u" => {
                let addr = self.translate_expr(args[0].expr);
                Some(WirInstr::I32Load16U {
                    offset: 0,
                    align: 1,
                    addr: Box::new(addr),
                })
            }
            "builtin::i32_load16_s" => {
                let addr = self.translate_expr(args[0].expr);
                Some(WirInstr::I32Load16S {
                    offset: 0,
                    align: 1,
                    addr: Box::new(addr),
                })
            }

            "builtin::i64_load" => {
                let addr = self.translate_expr(args[0].expr);
                Some(WirInstr::I64Load {
                    offset: 0,
                    align: 3,
                    addr: Box::new(addr),
                })
            }

            "builtin::i32_store" => {
                let addr = self.translate_expr(args[0].expr);
                let val = self.translate_expr(args[1].expr);
                Some(WirInstr::I32Store {
                    offset: 0,
                    align: 2,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "builtin::i32_store8" => {
                let addr = self.translate_expr(args[0].expr);
                let val = self.translate_expr(args[1].expr);
                Some(WirInstr::I32Store8 {
                    offset: 0,
                    align: 0,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "builtin::i32_store16" => {
                let addr = self.translate_expr(args[0].expr);
                let val = self.translate_expr(args[1].expr);
                Some(WirInstr::I32Store16 {
                    offset: 0,
                    align: 1,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "builtin::i64_store" => {
                let addr = self.translate_expr(args[0].expr);
                let val = self.translate_expr(args[1].expr);
                Some(WirInstr::I64Store {
                    offset: 0,
                    align: 3,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "builtin::v128_load" => {
                let addr = self.translate_expr(args[0].expr);
                Some(WirInstr::V128Load {
                    offset: 0,
                    align: 4,
                    addr: Box::new(addr),
                })
            }
            "builtin::v128_store" => {
                let addr = self.translate_expr(args[0].expr);
                let val = self.translate_expr(args[1].expr);
                Some(WirInstr::V128Store {
                    offset: 0,
                    align: 4,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }

            "builtin::array_new" => {
                // array.new_default: creates a new array of the given element type
                let len = self.translate_expr(args[0].expr);
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
            "builtin::array_get_u8" => {
                let arr = self.translate_expr(args[0].expr);
                let idx = self.translate_expr(args[1].expr);
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
                let arr = self.translate_expr(args[0].expr);
                let idx = self.translate_expr(args[1].expr);
                let val = self.translate_expr(args[2].expr);
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
                let arr = self.translate_expr(args[0].expr);
                let idx = self.translate_expr(args[1].expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, self.body.exprs[args[0].expr].type_id);
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
                let arr = self.translate_expr(args[0].expr);
                let idx = self.translate_expr(args[1].expr);
                let val = self.translate_expr(args[2].expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, self.body.exprs[args[0].expr].type_id);
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
                let dst = self.translate_expr(args[0].expr);
                let dst_offset = self.translate_expr(args[1].expr);
                let src = self.translate_expr(args[2].expr);
                let src_offset = self.translate_expr(args[3].expr);
                let len = self.translate_expr(args[4].expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, self.body.exprs[args[0].expr].type_id);
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
            "builtin::array_clone" => {
                let src_expr = args[0].expr;
                let src = self.translate_expr(src_expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, self.body.exprs[src_expr].type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    let element_copy_func =
                        self.array_element_copy_func(self.body.exprs[src_expr].type_id);
                    Some(WirInstr::ArrayClone {
                        type_id,
                        src: Box::new(src),
                        element_copy_func,
                    })
                } else {
                    None
                }
            }
            // Spine-only array clone: copies the `repr` slots without the
            // per-element deep copy `array_clone` would emit for value-typed
            // elements. Emitted only by `value_copy_demote`, which proves the
            // shared elements are never mutated through either alias.
            "builtin::array_clone_shallow" => {
                let src_expr = args[0].expr;
                let src = self.translate_expr(src_expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, self.body.exprs[src_expr].type_id);
                if let WirType::Ref { type_id, .. } = wir_type {
                    Some(WirInstr::ArrayClone {
                        type_id,
                        src: Box::new(src),
                        element_copy_func: None,
                    })
                } else {
                    None
                }
            }
            "builtin::array_fill" => {
                let arr = self.translate_expr(args[0].expr);
                let offset = self.translate_expr(args[1].expr);
                let val = self.translate_expr(args[2].expr);
                let len = self.translate_expr(args[3].expr);
                let wir_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, self.body.exprs[args[0].expr].type_id);
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

            "builtin::v128_const" => {
                // The argument is an i128 literal interpreted as v128 bit pattern
                let o = self.translate_expr(args[0].expr);
                // Extract the constant value - it should be an IntLiteral
                if let WirInstr::I32Const(v) = &o {
                    Some(WirInstr::V128Const(i128::from(*v)))
                } else if let WirInstr::I64Const(v) = &o {
                    Some(WirInstr::V128Const(i128::from(*v)))
                } else {
                    Some(WirInstr::V128Const(0))
                }
            }
            "builtin::memory_size" => Some(WirInstr::MemorySize),
            "builtin::memory_fill" => {
                let dst = self.translate_expr(args[0].expr);
                let value = self.translate_expr(args[1].expr);
                let len = self.translate_expr(args[2].expr);
                Some(WirInstr::MemoryFill {
                    dst: Box::new(dst),
                    value: Box::new(value),
                    len: Box::new(len),
                })
            }

            "builtin::unreachable" => Some(WirInstr::Unreachable),
            "builtin::cold_path" => Some(WirInstr::ColdPath),
            "builtin::likely" | "builtin::unlikely" => {
                let likely = builtin_name == "builtin::likely";
                let expr = self.translate_expr(args[0].expr);
                Some(WirInstr::BranchHint {
                    likely,
                    expr: Box::new(expr),
                })
            }
            "builtin::select" => {
                let cond = self.translate_expr(args[0].expr);
                let a = self.translate_expr(args[1].expr);
                let b = self.translate_expr(args[2].expr);
                let result_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, self.body.exprs[args[1].expr].type_id);
                Some(WirInstr::Select {
                    condition: Box::new(cond),
                    if_true: Box::new(a),
                    if_false: Box::new(b),
                    ty: Some(result_type),
                })
            }

            "builtin::i64_add128" => {
                let a_lo = Box::new(self.translate_expr(args[0].expr));
                let a_hi = Box::new(self.translate_expr(args[1].expr));
                let b_lo = Box::new(self.translate_expr(args[2].expr));
                let b_hi = Box::new(self.translate_expr(args[3].expr));
                Some(self.wrap_multivalue_i64(
                    WirInstr::I64Add128(a_lo, a_hi, b_lo, b_hi),
                    result_type_id,
                ))
            }
            "builtin::i64_sub128" => {
                let a_lo = Box::new(self.translate_expr(args[0].expr));
                let a_hi = Box::new(self.translate_expr(args[1].expr));
                let b_lo = Box::new(self.translate_expr(args[2].expr));
                let b_hi = Box::new(self.translate_expr(args[3].expr));
                Some(self.wrap_multivalue_i64(
                    WirInstr::I64Sub128(a_lo, a_hi, b_lo, b_hi),
                    result_type_id,
                ))
            }
            "builtin::i64_mul_wide_u" => {
                let a = Box::new(self.translate_expr(args[0].expr));
                let b = Box::new(self.translate_expr(args[1].expr));
                Some(self.wrap_multivalue_i64(WirInstr::I64MulWideU(a, b), result_type_id))
            }
            "builtin::i64_mul_wide_s" => {
                let a = Box::new(self.translate_expr(args[0].expr));
                let b = Box::new(self.translate_expr(args[1].expr));
                Some(self.wrap_multivalue_i64(WirInstr::I64MulWideS(a, b), result_type_id))
            }

            "builtin::i32_as_char" => Some(self.translate_expr(args[0].expr)),

            "builtin::call_indirect_stdout_write_via_stream"
            | "builtin::call_indirect_stderr_write_via_stream" => {
                // The kiln generator world forbids every WASI interface
                // (WEP 2026-04-12 §"Design principles" #1 — determinism
                // by construction). The Wado stdlib's `panic` implementation
                // writes its message to stderr before trapping; in the kiln
                // world we short-circuit the stderr path to `unreachable` so
                // the generator component never imports `wasi:cli/stderr`.
                // The effect is equivalent: panic still traps, and the
                // generator surfaces as a `GeneratorRunnerError::Host` at
                // the kiln runtime boundary. Stdout goes the same way — a
                // generator that calls `println` shouldn't compile past the
                // import-refusal check anyway, but belt-and-suspenders is
                // cheap at the builtin boundary.
                if self.ctx.package.world_imports_interface("KilnHost") {
                    // `unreachable` is stack-polymorphic — it satisfies
                    // the declared i32 return of the stderr/stdout
                    // write-via-stream builtin without further work.
                    // The call-site arguments are intentionally dropped
                    // unevaluated: `log_stderr` only passes a freshly-
                    // constructed stream handle whose sole downstream
                    // use was to pipe data to stderr, which the kiln
                    // world forbids.
                    return Some(WirInstr::Unreachable);
                }

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
                        args.iter().map(|a| self.translate_expr(a.expr)).collect();
                    Some(WirInstr::Call {
                        func_id,
                        args: call_args,
                    })
                } else {
                    None
                }
            }

            // Not an instruction-builtin; fall through to function call resolution
            // Not an instruction-builtin; fall through to function call resolution
            _ => None,
        }
    }

    /// Lower a mechanical numeric or SIMD intrinsic: evaluate the operands
    /// and wrap them in a fixed `WirInstr` variant. Returns `None` for any
    /// name outside this set so `translate_builtin_call` can handle the
    /// context-dependent builtins.
    fn translate_mechanical_builtin(
        &mut self,
        builtin_name: &str,
        args: &[ArenaCallArg],
    ) -> Option<WirInstr> {
        Some(match builtin_name {
            "builtin::array_len" => unary!(self, args, WirInstr::ArrayLen),
            "builtin::f64_abs" => unary!(self, args, WirInstr::F64Abs),
            "builtin::f64_ceil" => unary!(self, args, WirInstr::F64Ceil),
            "builtin::f64_floor" => unary!(self, args, WirInstr::F64Floor),
            "builtin::f64_trunc" => unary!(self, args, WirInstr::F64Trunc),
            "builtin::f64_nearest" => unary!(self, args, WirInstr::F64Nearest),
            "builtin::f64_sqrt" => unary!(self, args, WirInstr::F64Sqrt),
            "builtin::f64_min" => binary!(self, args, WirInstr::F64Min),
            "builtin::f64_max" => binary!(self, args, WirInstr::F64Max),
            "builtin::f64_copysign" => binary!(self, args, WirInstr::F64Copysign),
            "builtin::f32_abs" => unary!(self, args, WirInstr::F32Abs),
            "builtin::f32_ceil" => unary!(self, args, WirInstr::F32Ceil),
            "builtin::f32_floor" => unary!(self, args, WirInstr::F32Floor),
            "builtin::f32_trunc" => unary!(self, args, WirInstr::F32Trunc),
            "builtin::f32_nearest" => unary!(self, args, WirInstr::F32Nearest),
            "builtin::f32_sqrt" => unary!(self, args, WirInstr::F32Sqrt),
            "builtin::f32_min" => binary!(self, args, WirInstr::F32Min),
            "builtin::f32_max" => binary!(self, args, WirInstr::F32Max),
            "builtin::f32_copysign" => binary!(self, args, WirInstr::F32Copysign),
            "builtin::ref_eq" => binary!(self, args, WirInstr::RefEq),
            "builtin::i32_and" => binary!(self, args, WirInstr::I32And),
            "builtin::i32_eqz" => unary!(self, args, WirInstr::I32Eqz),
            "builtin::i32_clz" => unary!(self, args, WirInstr::I32Clz),
            "builtin::i64_clz" => unary!(self, args, WirInstr::I64Clz),
            "builtin::i32_ctz" => unary!(self, args, WirInstr::I32Ctz),
            "builtin::i64_ctz" => unary!(self, args, WirInstr::I64Ctz),
            "builtin::i32_popcnt" => unary!(self, args, WirInstr::I32Popcnt),
            "builtin::i64_popcnt" => unary!(self, args, WirInstr::I64Popcnt),
            "builtin::i64_reinterpret_f64" => unary!(self, args, WirInstr::I64ReinterpretF64),
            "builtin::f64_reinterpret_i64" => unary!(self, args, WirInstr::F64ReinterpretI64),
            "builtin::i32_reinterpret_f32" => unary!(self, args, WirInstr::I32ReinterpretF32),
            "builtin::f32_reinterpret_i32" => unary!(self, args, WirInstr::F32ReinterpretI32),
            "builtin::v128_not" => unary!(self, args, WirInstr::V128Not),
            "builtin::v128_and" => binary!(self, args, WirInstr::V128And),
            "builtin::v128_or" => binary!(self, args, WirInstr::V128Or),
            "builtin::v128_xor" => binary!(self, args, WirInstr::V128Xor),
            "builtin::v128_bitselect" => ternary!(self, args, WirInstr::V128Bitselect),
            "builtin::i8x16_splat" => unary!(self, args, WirInstr::I8x16Splat),
            "builtin::i8x16_extract_lane_s" => {
                extract_lane!(self, args, WirInstr::I8x16ExtractLaneS)
            }
            "builtin::i8x16_extract_lane_u" => {
                extract_lane!(self, args, WirInstr::I8x16ExtractLaneU)
            }
            "builtin::i8x16_replace_lane" => replace_lane!(self, args, WirInstr::I8x16ReplaceLane),
            "builtin::i8x16_add" => binary!(self, args, WirInstr::I8x16Add),
            "builtin::i8x16_sub" => binary!(self, args, WirInstr::I8x16Sub),
            "builtin::i8x16_neg" => unary!(self, args, WirInstr::I8x16Neg),
            "builtin::i8x16_eq" => binary!(self, args, WirInstr::I8x16Eq),
            "builtin::i8x16_ne" => binary!(self, args, WirInstr::I8x16Ne),
            "builtin::i8x16_lt_s" => binary!(self, args, WirInstr::I8x16LtS),
            "builtin::i8x16_gt_s" => binary!(self, args, WirInstr::I8x16GtS),
            "builtin::i8x16_le_s" => binary!(self, args, WirInstr::I8x16LeS),
            "builtin::i8x16_ge_s" => binary!(self, args, WirInstr::I8x16GeS),
            "builtin::i8x16_lt_u" => binary!(self, args, WirInstr::I8x16LtU),
            "builtin::i8x16_gt_u" => binary!(self, args, WirInstr::I8x16GtU),
            "builtin::i8x16_le_u" => binary!(self, args, WirInstr::I8x16LeU),
            "builtin::i8x16_ge_u" => binary!(self, args, WirInstr::I8x16GeU),
            "builtin::i8x16_shl" => binary!(self, args, WirInstr::I8x16Shl),
            "builtin::i8x16_shr_s" => binary!(self, args, WirInstr::I8x16ShrS),
            "builtin::i8x16_shr_u" => binary!(self, args, WirInstr::I8x16ShrU),
            "builtin::i8x16_swizzle" => binary!(self, args, WirInstr::I8x16Swizzle),
            "builtin::i16x8_splat" => unary!(self, args, WirInstr::I16x8Splat),
            "builtin::i16x8_extract_lane_s" => {
                extract_lane!(self, args, WirInstr::I16x8ExtractLaneS)
            }
            "builtin::i16x8_extract_lane_u" => {
                extract_lane!(self, args, WirInstr::I16x8ExtractLaneU)
            }
            "builtin::i16x8_replace_lane" => replace_lane!(self, args, WirInstr::I16x8ReplaceLane),
            "builtin::i16x8_add" => binary!(self, args, WirInstr::I16x8Add),
            "builtin::i16x8_sub" => binary!(self, args, WirInstr::I16x8Sub),
            "builtin::i16x8_mul" => binary!(self, args, WirInstr::I16x8Mul),
            "builtin::i16x8_neg" => unary!(self, args, WirInstr::I16x8Neg),
            "builtin::i16x8_eq" => binary!(self, args, WirInstr::I16x8Eq),
            "builtin::i16x8_ne" => binary!(self, args, WirInstr::I16x8Ne),
            "builtin::i16x8_lt_s" => binary!(self, args, WirInstr::I16x8LtS),
            "builtin::i16x8_gt_s" => binary!(self, args, WirInstr::I16x8GtS),
            "builtin::i16x8_le_s" => binary!(self, args, WirInstr::I16x8LeS),
            "builtin::i16x8_ge_s" => binary!(self, args, WirInstr::I16x8GeS),
            "builtin::i16x8_lt_u" => binary!(self, args, WirInstr::I16x8LtU),
            "builtin::i16x8_gt_u" => binary!(self, args, WirInstr::I16x8GtU),
            "builtin::i16x8_le_u" => binary!(self, args, WirInstr::I16x8LeU),
            "builtin::i16x8_ge_u" => binary!(self, args, WirInstr::I16x8GeU),
            "builtin::i16x8_shl" => binary!(self, args, WirInstr::I16x8Shl),
            "builtin::i16x8_shr_s" => binary!(self, args, WirInstr::I16x8ShrS),
            "builtin::i16x8_shr_u" => binary!(self, args, WirInstr::I16x8ShrU),
            "builtin::i32x4_splat" => unary!(self, args, WirInstr::I32x4Splat),
            "builtin::i32x4_extract_lane" => extract_lane!(self, args, WirInstr::I32x4ExtractLane),
            "builtin::i32x4_replace_lane" => replace_lane!(self, args, WirInstr::I32x4ReplaceLane),
            "builtin::i32x4_add" => binary!(self, args, WirInstr::I32x4Add),
            "builtin::i32x4_sub" => binary!(self, args, WirInstr::I32x4Sub),
            "builtin::i32x4_mul" => binary!(self, args, WirInstr::I32x4Mul),
            "builtin::i32x4_neg" => unary!(self, args, WirInstr::I32x4Neg),
            "builtin::i32x4_eq" => binary!(self, args, WirInstr::I32x4Eq),
            "builtin::i32x4_ne" => binary!(self, args, WirInstr::I32x4Ne),
            "builtin::i32x4_lt_s" => binary!(self, args, WirInstr::I32x4LtS),
            "builtin::i32x4_gt_s" => binary!(self, args, WirInstr::I32x4GtS),
            "builtin::i32x4_le_s" => binary!(self, args, WirInstr::I32x4LeS),
            "builtin::i32x4_ge_s" => binary!(self, args, WirInstr::I32x4GeS),
            "builtin::i32x4_lt_u" => binary!(self, args, WirInstr::I32x4LtU),
            "builtin::i32x4_gt_u" => binary!(self, args, WirInstr::I32x4GtU),
            "builtin::i32x4_le_u" => binary!(self, args, WirInstr::I32x4LeU),
            "builtin::i32x4_ge_u" => binary!(self, args, WirInstr::I32x4GeU),
            "builtin::i32x4_shl" => binary!(self, args, WirInstr::I32x4Shl),
            "builtin::i32x4_shr_s" => binary!(self, args, WirInstr::I32x4ShrS),
            "builtin::i32x4_shr_u" => binary!(self, args, WirInstr::I32x4ShrU),
            "builtin::i64x2_splat" => unary!(self, args, WirInstr::I64x2Splat),
            "builtin::i64x2_extract_lane" => extract_lane!(self, args, WirInstr::I64x2ExtractLane),
            "builtin::i64x2_replace_lane" => replace_lane!(self, args, WirInstr::I64x2ReplaceLane),
            "builtin::i64x2_add" => binary!(self, args, WirInstr::I64x2Add),
            "builtin::i64x2_sub" => binary!(self, args, WirInstr::I64x2Sub),
            "builtin::i64x2_mul" => binary!(self, args, WirInstr::I64x2Mul),
            "builtin::i64x2_neg" => unary!(self, args, WirInstr::I64x2Neg),
            "builtin::i64x2_eq" => binary!(self, args, WirInstr::I64x2Eq),
            "builtin::i64x2_ne" => binary!(self, args, WirInstr::I64x2Ne),
            "builtin::i64x2_lt_s" => binary!(self, args, WirInstr::I64x2LtS),
            "builtin::i64x2_gt_s" => binary!(self, args, WirInstr::I64x2GtS),
            "builtin::i64x2_le_s" => binary!(self, args, WirInstr::I64x2LeS),
            "builtin::i64x2_ge_s" => binary!(self, args, WirInstr::I64x2GeS),
            "builtin::i64x2_shl" => binary!(self, args, WirInstr::I64x2Shl),
            "builtin::i64x2_shr_s" => binary!(self, args, WirInstr::I64x2ShrS),
            "builtin::i64x2_shr_u" => binary!(self, args, WirInstr::I64x2ShrU),
            "builtin::f32x4_splat" => unary!(self, args, WirInstr::F32x4Splat),
            "builtin::f32x4_extract_lane" => extract_lane!(self, args, WirInstr::F32x4ExtractLane),
            "builtin::f32x4_replace_lane" => replace_lane!(self, args, WirInstr::F32x4ReplaceLane),
            "builtin::f32x4_add" => binary!(self, args, WirInstr::F32x4Add),
            "builtin::f32x4_sub" => binary!(self, args, WirInstr::F32x4Sub),
            "builtin::f32x4_mul" => binary!(self, args, WirInstr::F32x4Mul),
            "builtin::f32x4_div" => binary!(self, args, WirInstr::F32x4Div),
            "builtin::f32x4_neg" => unary!(self, args, WirInstr::F32x4Neg),
            "builtin::f32x4_sqrt" => unary!(self, args, WirInstr::F32x4Sqrt),
            "builtin::f32x4_abs" => unary!(self, args, WirInstr::F32x4Abs),
            "builtin::f32x4_eq" => binary!(self, args, WirInstr::F32x4Eq),
            "builtin::f32x4_ne" => binary!(self, args, WirInstr::F32x4Ne),
            "builtin::f32x4_lt" => binary!(self, args, WirInstr::F32x4Lt),
            "builtin::f32x4_gt" => binary!(self, args, WirInstr::F32x4Gt),
            "builtin::f32x4_le" => binary!(self, args, WirInstr::F32x4Le),
            "builtin::f32x4_ge" => binary!(self, args, WirInstr::F32x4Ge),
            "builtin::f32x4_min" => binary!(self, args, WirInstr::F32x4Min),
            "builtin::f32x4_max" => binary!(self, args, WirInstr::F32x4Max),
            "builtin::f64x2_splat" => unary!(self, args, WirInstr::F64x2Splat),
            "builtin::f64x2_extract_lane" => extract_lane!(self, args, WirInstr::F64x2ExtractLane),
            "builtin::f64x2_replace_lane" => replace_lane!(self, args, WirInstr::F64x2ReplaceLane),
            "builtin::f64x2_add" => binary!(self, args, WirInstr::F64x2Add),
            "builtin::f64x2_sub" => binary!(self, args, WirInstr::F64x2Sub),
            "builtin::f64x2_mul" => binary!(self, args, WirInstr::F64x2Mul),
            "builtin::f64x2_div" => binary!(self, args, WirInstr::F64x2Div),
            "builtin::f64x2_neg" => unary!(self, args, WirInstr::F64x2Neg),
            "builtin::f64x2_sqrt" => unary!(self, args, WirInstr::F64x2Sqrt),
            "builtin::f64x2_abs" => unary!(self, args, WirInstr::F64x2Abs),
            "builtin::f64x2_eq" => binary!(self, args, WirInstr::F64x2Eq),
            "builtin::f64x2_ne" => binary!(self, args, WirInstr::F64x2Ne),
            "builtin::f64x2_lt" => binary!(self, args, WirInstr::F64x2Lt),
            "builtin::f64x2_gt" => binary!(self, args, WirInstr::F64x2Gt),
            "builtin::f64x2_le" => binary!(self, args, WirInstr::F64x2Le),
            "builtin::f64x2_ge" => binary!(self, args, WirInstr::F64x2Ge),
            "builtin::f64x2_min" => binary!(self, args, WirInstr::F64x2Min),
            "builtin::f64x2_max" => binary!(self, args, WirInstr::F64x2Max),
            "builtin::i8x16_abs" => unary!(self, args, WirInstr::I8x16Abs),
            "builtin::i8x16_add_sat_s" => binary!(self, args, WirInstr::I8x16AddSatS),
            "builtin::i8x16_add_sat_u" => binary!(self, args, WirInstr::I8x16AddSatU),
            "builtin::i8x16_sub_sat_s" => binary!(self, args, WirInstr::I8x16SubSatS),
            "builtin::i8x16_sub_sat_u" => binary!(self, args, WirInstr::I8x16SubSatU),
            "builtin::i8x16_min_s" => binary!(self, args, WirInstr::I8x16MinS),
            "builtin::i8x16_min_u" => binary!(self, args, WirInstr::I8x16MinU),
            "builtin::i8x16_max_s" => binary!(self, args, WirInstr::I8x16MaxS),
            "builtin::i8x16_max_u" => binary!(self, args, WirInstr::I8x16MaxU),
            "builtin::i8x16_avgr_u" => binary!(self, args, WirInstr::I8x16AvgrU),
            "builtin::i8x16_all_true" => unary!(self, args, WirInstr::I8x16AllTrue),
            "builtin::i8x16_bitmask" => unary!(self, args, WirInstr::I8x16Bitmask),
            "builtin::i8x16_narrow_i16x8_s" => binary!(self, args, WirInstr::I8x16NarrowI16x8S),
            "builtin::i8x16_narrow_i16x8_u" => binary!(self, args, WirInstr::I8x16NarrowI16x8U),
            "builtin::i8x16_popcnt" => unary!(self, args, WirInstr::I8x16Popcnt),
            "builtin::i16x8_abs" => unary!(self, args, WirInstr::I16x8Abs),
            "builtin::i16x8_add_sat_s" => binary!(self, args, WirInstr::I16x8AddSatS),
            "builtin::i16x8_add_sat_u" => binary!(self, args, WirInstr::I16x8AddSatU),
            "builtin::i16x8_sub_sat_s" => binary!(self, args, WirInstr::I16x8SubSatS),
            "builtin::i16x8_sub_sat_u" => binary!(self, args, WirInstr::I16x8SubSatU),
            "builtin::i16x8_min_s" => binary!(self, args, WirInstr::I16x8MinS),
            "builtin::i16x8_min_u" => binary!(self, args, WirInstr::I16x8MinU),
            "builtin::i16x8_max_s" => binary!(self, args, WirInstr::I16x8MaxS),
            "builtin::i16x8_max_u" => binary!(self, args, WirInstr::I16x8MaxU),
            "builtin::i16x8_avgr_u" => binary!(self, args, WirInstr::I16x8AvgrU),
            "builtin::i16x8_all_true" => unary!(self, args, WirInstr::I16x8AllTrue),
            "builtin::i16x8_bitmask" => unary!(self, args, WirInstr::I16x8Bitmask),
            "builtin::i16x8_narrow_i32x4_s" => binary!(self, args, WirInstr::I16x8NarrowI32x4S),
            "builtin::i16x8_narrow_i32x4_u" => binary!(self, args, WirInstr::I16x8NarrowI32x4U),
            "builtin::i16x8_extend_low_i8x16_s" => {
                unary!(self, args, WirInstr::I16x8ExtendLowI8x16S)
            }
            "builtin::i16x8_extend_high_i8x16_s" => {
                unary!(self, args, WirInstr::I16x8ExtendHighI8x16S)
            }
            "builtin::i16x8_extend_low_i8x16_u" => {
                unary!(self, args, WirInstr::I16x8ExtendLowI8x16U)
            }
            "builtin::i16x8_extend_high_i8x16_u" => {
                unary!(self, args, WirInstr::I16x8ExtendHighI8x16U)
            }
            "builtin::i16x8_extmul_low_i8x16_s" => {
                binary!(self, args, WirInstr::I16x8ExtMulLowI8x16S)
            }
            "builtin::i16x8_extmul_high_i8x16_s" => {
                binary!(self, args, WirInstr::I16x8ExtMulHighI8x16S)
            }
            "builtin::i16x8_extmul_low_i8x16_u" => {
                binary!(self, args, WirInstr::I16x8ExtMulLowI8x16U)
            }
            "builtin::i16x8_extmul_high_i8x16_u" => {
                binary!(self, args, WirInstr::I16x8ExtMulHighI8x16U)
            }
            "builtin::i16x8_extadd_pairwise_i8x16_s" => {
                unary!(self, args, WirInstr::I16x8ExtAddPairwiseI8x16S)
            }
            "builtin::i16x8_extadd_pairwise_i8x16_u" => {
                unary!(self, args, WirInstr::I16x8ExtAddPairwiseI8x16U)
            }
            "builtin::i16x8_q15mulr_sat_s" => binary!(self, args, WirInstr::I16x8Q15MulrSatS),
            "builtin::i32x4_abs" => unary!(self, args, WirInstr::I32x4Abs),
            "builtin::i32x4_all_true" => unary!(self, args, WirInstr::I32x4AllTrue),
            "builtin::i32x4_bitmask" => unary!(self, args, WirInstr::I32x4Bitmask),
            "builtin::i32x4_min_s" => binary!(self, args, WirInstr::I32x4MinS),
            "builtin::i32x4_min_u" => binary!(self, args, WirInstr::I32x4MinU),
            "builtin::i32x4_max_s" => binary!(self, args, WirInstr::I32x4MaxS),
            "builtin::i32x4_max_u" => binary!(self, args, WirInstr::I32x4MaxU),
            "builtin::i32x4_dot_i16x8_s" => binary!(self, args, WirInstr::I32x4DotI16x8S),
            "builtin::i32x4_extend_low_i16x8_s" => {
                unary!(self, args, WirInstr::I32x4ExtendLowI16x8S)
            }
            "builtin::i32x4_extend_high_i16x8_s" => {
                unary!(self, args, WirInstr::I32x4ExtendHighI16x8S)
            }
            "builtin::i32x4_extend_low_i16x8_u" => {
                unary!(self, args, WirInstr::I32x4ExtendLowI16x8U)
            }
            "builtin::i32x4_extend_high_i16x8_u" => {
                unary!(self, args, WirInstr::I32x4ExtendHighI16x8U)
            }
            "builtin::i32x4_extmul_low_i16x8_s" => {
                binary!(self, args, WirInstr::I32x4ExtMulLowI16x8S)
            }
            "builtin::i32x4_extmul_high_i16x8_s" => {
                binary!(self, args, WirInstr::I32x4ExtMulHighI16x8S)
            }
            "builtin::i32x4_extmul_low_i16x8_u" => {
                binary!(self, args, WirInstr::I32x4ExtMulLowI16x8U)
            }
            "builtin::i32x4_extmul_high_i16x8_u" => {
                binary!(self, args, WirInstr::I32x4ExtMulHighI16x8U)
            }
            "builtin::i32x4_extadd_pairwise_i16x8_s" => {
                unary!(self, args, WirInstr::I32x4ExtAddPairwiseI16x8S)
            }
            "builtin::i32x4_extadd_pairwise_i16x8_u" => {
                unary!(self, args, WirInstr::I32x4ExtAddPairwiseI16x8U)
            }
            "builtin::i32x4_trunc_sat_f32x4_s" => unary!(self, args, WirInstr::I32x4TruncSatF32x4S),
            "builtin::i32x4_trunc_sat_f32x4_u" => unary!(self, args, WirInstr::I32x4TruncSatF32x4U),
            "builtin::i32x4_trunc_sat_f64x2_s_zero" => {
                unary!(self, args, WirInstr::I32x4TruncSatF64x2SZero)
            }
            "builtin::i32x4_trunc_sat_f64x2_u_zero" => {
                unary!(self, args, WirInstr::I32x4TruncSatF64x2UZero)
            }
            "builtin::i64x2_abs" => unary!(self, args, WirInstr::I64x2Abs),
            "builtin::i64x2_all_true" => unary!(self, args, WirInstr::I64x2AllTrue),
            "builtin::i64x2_bitmask" => unary!(self, args, WirInstr::I64x2Bitmask),
            "builtin::i64x2_extend_low_i32x4_s" => {
                unary!(self, args, WirInstr::I64x2ExtendLowI32x4S)
            }
            "builtin::i64x2_extend_high_i32x4_s" => {
                unary!(self, args, WirInstr::I64x2ExtendHighI32x4S)
            }
            "builtin::i64x2_extend_low_i32x4_u" => {
                unary!(self, args, WirInstr::I64x2ExtendLowI32x4U)
            }
            "builtin::i64x2_extend_high_i32x4_u" => {
                unary!(self, args, WirInstr::I64x2ExtendHighI32x4U)
            }
            "builtin::i64x2_extmul_low_i32x4_s" => {
                binary!(self, args, WirInstr::I64x2ExtMulLowI32x4S)
            }
            "builtin::i64x2_extmul_high_i32x4_s" => {
                binary!(self, args, WirInstr::I64x2ExtMulHighI32x4S)
            }
            "builtin::i64x2_extmul_low_i32x4_u" => {
                binary!(self, args, WirInstr::I64x2ExtMulLowI32x4U)
            }
            "builtin::i64x2_extmul_high_i32x4_u" => {
                binary!(self, args, WirInstr::I64x2ExtMulHighI32x4U)
            }
            "builtin::f32x4_ceil" => unary!(self, args, WirInstr::F32x4Ceil),
            "builtin::f32x4_floor" => unary!(self, args, WirInstr::F32x4Floor),
            "builtin::f32x4_trunc" => unary!(self, args, WirInstr::F32x4Trunc),
            "builtin::f32x4_nearest" => unary!(self, args, WirInstr::F32x4Nearest),
            "builtin::f32x4_pmin" => binary!(self, args, WirInstr::F32x4PMin),
            "builtin::f32x4_pmax" => binary!(self, args, WirInstr::F32x4PMax),
            "builtin::f32x4_convert_i32x4_s" => unary!(self, args, WirInstr::F32x4ConvertI32x4S),
            "builtin::f32x4_convert_i32x4_u" => unary!(self, args, WirInstr::F32x4ConvertI32x4U),
            "builtin::f32x4_demote_f64x2_zero" => {
                unary!(self, args, WirInstr::F32x4DemoteF64x2Zero)
            }
            "builtin::f64x2_ceil" => unary!(self, args, WirInstr::F64x2Ceil),
            "builtin::f64x2_floor" => unary!(self, args, WirInstr::F64x2Floor),
            "builtin::f64x2_trunc" => unary!(self, args, WirInstr::F64x2Trunc),
            "builtin::f64x2_nearest" => unary!(self, args, WirInstr::F64x2Nearest),
            "builtin::f64x2_pmin" => binary!(self, args, WirInstr::F64x2PMin),
            "builtin::f64x2_pmax" => binary!(self, args, WirInstr::F64x2PMax),
            "builtin::f64x2_convert_low_i32x4_s" => {
                unary!(self, args, WirInstr::F64x2ConvertLowI32x4S)
            }
            "builtin::f64x2_convert_low_i32x4_u" => {
                unary!(self, args, WirInstr::F64x2ConvertLowI32x4U)
            }
            "builtin::f64x2_promote_low_f32x4" => {
                unary!(self, args, WirInstr::F64x2PromoteLowF32x4)
            }
            "builtin::v128_andnot" => binary!(self, args, WirInstr::V128AndNot),
            "builtin::v128_any_true" => unary!(self, args, WirInstr::V128AnyTrue),
            "builtin::i8x16_relaxed_swizzle" => binary!(self, args, WirInstr::I8x16RelaxedSwizzle),
            "builtin::i8x16_relaxed_laneselect" => {
                ternary!(self, args, WirInstr::I8x16RelaxedLaneselect)
            }
            "builtin::i16x8_relaxed_laneselect" => {
                ternary!(self, args, WirInstr::I16x8RelaxedLaneselect)
            }
            "builtin::i32x4_relaxed_laneselect" => {
                ternary!(self, args, WirInstr::I32x4RelaxedLaneselect)
            }
            "builtin::i64x2_relaxed_laneselect" => {
                ternary!(self, args, WirInstr::I64x2RelaxedLaneselect)
            }
            "builtin::f32x4_relaxed_madd" => ternary!(self, args, WirInstr::F32x4RelaxedMadd),
            "builtin::f32x4_relaxed_nmadd" => ternary!(self, args, WirInstr::F32x4RelaxedNmadd),
            "builtin::f64x2_relaxed_madd" => ternary!(self, args, WirInstr::F64x2RelaxedMadd),
            "builtin::f64x2_relaxed_nmadd" => ternary!(self, args, WirInstr::F64x2RelaxedNmadd),
            "builtin::f32x4_relaxed_min" => binary!(self, args, WirInstr::F32x4RelaxedMin),
            "builtin::f32x4_relaxed_max" => binary!(self, args, WirInstr::F32x4RelaxedMax),
            "builtin::f64x2_relaxed_min" => binary!(self, args, WirInstr::F64x2RelaxedMin),
            "builtin::f64x2_relaxed_max" => binary!(self, args, WirInstr::F64x2RelaxedMax),
            "builtin::i32x4_relaxed_trunc_f32x4_s" => {
                unary!(self, args, WirInstr::I32x4RelaxedTruncF32x4S)
            }
            "builtin::i32x4_relaxed_trunc_f32x4_u" => {
                unary!(self, args, WirInstr::I32x4RelaxedTruncF32x4U)
            }
            "builtin::i32x4_relaxed_trunc_f64x2_s_zero" => {
                unary!(self, args, WirInstr::I32x4RelaxedTruncF64x2SZero)
            }
            "builtin::i32x4_relaxed_trunc_f64x2_u_zero" => {
                unary!(self, args, WirInstr::I32x4RelaxedTruncF64x2UZero)
            }
            "builtin::i16x8_relaxed_q15mulr_s" => {
                binary!(self, args, WirInstr::I16x8RelaxedQ15mulrS)
            }
            "builtin::i16x8_relaxed_dot_i8x16_i7x16_s" => {
                binary!(self, args, WirInstr::I16x8RelaxedDotI8x16I7x16S)
            }
            "builtin::i32x4_relaxed_dot_i8x16_i7x16_add_s" => {
                ternary!(self, args, WirInstr::I32x4RelaxedDotI8x16I7x16AddS)
            }
            "builtin::memory_grow" => unary!(self, args, WirInstr::MemoryGrow),
            _ => return None,
        })
    }
    /// Translate `IndirectCall { callee, args }` to `call_ref` through canonical closure.
    pub(super) fn translate_indirect_call(
        &mut self,
        callee: ExprId,
        args: &[ExprId],
        result_type: TypeId,
    ) -> WirInstr {
        let callee_wir = self.translate_expr(callee);

        // Look up the Function type to get param/result info
        let fn_type = self.type_table.get(self.body.exprs[callee].type_id);
        let (param_types, return_type) = match fn_type {
            crate::tir::ResolvedType::Function {
                params,
                return_type,
                ..
            } => (params.clone(), *return_type),
            other => panic!(
                "[WIR] translate_indirect_call: expected Function type, got {other:?} (callee type_id={:?})",
                self.body.exprs[callee].type_id
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
        let (fn_type_id, closure_struct_type_id) = if let Some((ftid, stid, _)) =
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
            call_args.push(self.translate_expr(*arg));
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
        functor: ExprId,
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

        // Get canonical closure type and its inspectable schema flag.
        let key = WirContext::canonical_closure_key(&param_wirs, &result_wirs);
        let (struct_type_id, is_inspectable) = if let Some((_, stid, ins)) =
            self.ctx.canonical_closure_types.get(&key)
        {
            (stid.clone(), *ins)
        } else {
            panic!(
                "[WIR] translate_closure_to_canonical: canonical closure type not registered for signature {key:?}"
            );
        };

        // Look up the pre-registered wrapper triple for this functor.
        // Use closure_module (the module where the closure was defined) for the lookup,
        // not self.module_source (which may differ after cross-module inlining).
        let functor_key = (closure_module.clone(), functor_id);
        let wrappers = if let Some(w) = self.ctx.closure_wrapper_funcs.get(&functor_key) {
            w.clone()
        } else {
            panic!(
                "[WIR] translate_closure_to_canonical: closure wrappers not registered for {functor_key:?}"
            );
        };

        // Build `CanonicalClosure_K`. Field order must match the struct
        // declaration in `WirContext::get_or_create_canonical_closure_type`:
        //
        // - Inspectable layout: `{ env, inspect, inspect_alt, func }` —
        //   the env + vtable prefix is the layout of the shared
        //   `$canonical_inspectable_base` supertype, and the typed `func`
        //   slot comes last (per-signature).
        // - Slim layout: `{ env, func }` — no shared supertype, no
        //   inspect slots. Production builds that never inspect closures
        //   stay on this shape.
        let mut fields = vec![functor_instr];
        if is_inspectable {
            let inspect_id = wrappers
                .inspect
                .expect("inspectable canonical closure missing inspect wrapper");
            let inspect_alt_id = wrappers
                .inspect_alt
                .expect("inspectable canonical closure missing inspect_alt wrapper");
            fields.push(WirInstr::RefFunc {
                func_id: inspect_id,
            });
            fields.push(WirInstr::RefFunc {
                func_id: inspect_alt_id,
            });
        }
        fields.push(WirInstr::RefFunc {
            func_id: wrappers.call,
        });
        self.struct_new(struct_type_id, fields)
    }
}
