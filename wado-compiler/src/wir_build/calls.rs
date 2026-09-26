//! Call-site translation — function reference resolution, builtin intrinsic
//! lowering, indirect calls through canonical closures, and closure-to-
//! canonical conversion.
//!
//! These methods are part of `FunctionTranslator`; see `translate.rs` for
//! the struct definition and the primary translation dispatch.

use std::array;

use crate::module_source::ModuleSource;
use crate::name::MangledName;
use crate::tir::{TypeId, TypeTable};
use crate::wir::{WirInstr, WirType, WirTypeId};

use super::context::WirContext;
use super::translate::{FunctionTranslator, declare_and_set_local};
use crate::lower::plan::value_copy;
use crate::nir;
use crate::nir::FuncId;
use crate::nir_arena::{ArenaCallArg, Body, Operand};
use crate::tir::ResolvedType;
use crate::wir::WirFuncId;
use crate::wir_build::translate::ref_binding_needs_boxing;

/// The compile-time constant of a SIMD lane operand, promoted into the
/// function's value pool. The elaborator has already rejected anything else.
fn operand_lane_const(body: &Body, op: Operand) -> u8 {
    let Some(lane) = body.operand_const_int(op) else {
        panic!("SIMD lane index survived elaboration unfolded")
    };
    lane as u8
}

/// Mechanical builtin shapes — each evaluates its operand expressions
/// left-to-right (operand order matters: `translate_operand` can register
/// locals and literals) and wraps them in the given `WirInstr` variant.
macro_rules! unary {
    ($self:expr, $args:expr, $variant:path) => {{
        let a = $self.translate_operand($args[0].expr);
        $variant(Box::new(a))
    }};
}

macro_rules! binary {
    ($self:expr, $args:expr, $variant:path) => {{
        let a = $self.translate_operand($args[0].expr);
        let b = $self.translate_operand($args[1].expr);
        $variant(Box::new(a), Box::new(b))
    }};
}

macro_rules! ternary {
    ($self:expr, $args:expr, $variant:path) => {{
        let a = $self.translate_operand($args[0].expr);
        let b = $self.translate_operand($args[1].expr);
        let c = $self.translate_operand($args[2].expr);
        $variant(Box::new(a), Box::new(b), Box::new(c))
    }};
}

/// SIMD lane access — the lane index is a compile-time constant operand.
macro_rules! extract_lane {
    ($self:expr, $args:expr, $variant:path) => {{
        let lane = operand_lane_const($self.body, $args[0].expr);
        let a = $self.translate_operand($args[1].expr);
        $variant(lane, Box::new(a))
    }};
}

macro_rules! replace_lane {
    ($self:expr, $args:expr, $variant:path) => {{
        let lane = operand_lane_const($self.body, $args[0].expr);
        let a = $self.translate_operand($args[1].expr);
        let v = $self.translate_operand($args[2].expr);
        $variant(lane, Box::new(a), Box::new(v))
    }};
}

/// The wide-integer builtins whose Wasm instruction pushes two i64s.
/// A `let [lo, hi] = …` over one of these binds the pair straight into the
/// binding locals (`pattern_match::try_bind_multivalue_builtin`); only a use in
/// expression position falls back to the tuple struct
/// [`FunctionTranslator::wrap_multivalue_i64`] builds.
pub(super) const MULTIVALUE_I64_BUILTINS: [&str; 4] = [
    "i64_add128",
    "i64_sub128",
    "i64_mul_wide_u",
    "i64_mul_wide_s",
];

/// How many Wasm results each of [`MULTIVALUE_I64_BUILTINS`] pushes.
pub(super) const MULTIVALUE_I64_RESULTS: usize = 2;

impl FunctionTranslator<'_, '_> {
    /// Translate one of [`MULTIVALUE_I64_BUILTINS`] to its bare two-result
    /// Wasm instruction, with no tuple struct around it.
    pub(super) fn translate_multivalue_i64_builtin(
        &mut self,
        builtin_name: &str,
        args: &[ArenaCallArg],
    ) -> WirInstr {
        let mut operand = |i: usize| Box::new(self.translate_operand(args[i].expr));
        match builtin_name {
            "i64_add128" => WirInstr::I64Add128(operand(0), operand(1), operand(2), operand(3)),
            "i64_sub128" => WirInstr::I64Sub128(operand(0), operand(1), operand(2), operand(3)),
            "i64_mul_wide_u" => WirInstr::I64MulWideU(operand(0), operand(1)),
            "i64_mul_wide_s" => WirInstr::I64MulWideS(operand(0), operand(1)),
            other => panic!("not a multi-value i64 builtin: {other}"),
        }
    }

    /// Wrap a multi-value [i64, i64] instruction in a tuple struct.
    /// The Wasm instruction pushes two i64s on the stack; we capture them
    /// into freshly declared locals via `MultiValueLocalBind`, then build
    /// a `StructNew` reading the locals back as the struct fields.
    fn wrap_multivalue_i64(&mut self, instr: WirInstr, result_type_id: TypeId) -> WirInstr {
        let type_id = self.ref_type_id(result_type_id);
        let lo = self.fresh_local("$mv_lo");
        let hi = self.fresh_local("$mv_hi");
        WirInstr::Seq(vec![
            WirInstr::DeclareLocal {
                name: lo.clone(),
                ty: WirType::I64,
            },
            WirInstr::DeclareLocal {
                name: hi.clone(),
                ty: WirType::I64,
            },
            WirInstr::MultiValueLocalBind {
                instr: Box::new(instr),
                locals: vec![Some(lo.clone()), Some(hi.clone())],
            },
            WirInstr::StructNew {
                type_id,
                fields: vec![
                    WirInstr::LocalGet {
                        name: lo,
                        result_ty: WirType::I64,
                    },
                    WirInstr::LocalGet {
                        name: hi,
                        result_ty: WirType::I64,
                    },
                ],
            },
        ])
    }

    /// Resolve a TIR `FunctionRef` to a `WirFuncId`.
    pub(super) fn resolve_function_ref(&self, func_ref: &nir::FunctionRef) -> Option<WirFuncId> {
        let module_source = &func_ref.module_source;
        let name = &func_ref.name;

        // Try direct name lookup
        let fq = MangledName::in_module(module_source, name);
        if let Some(id) = self.ctx.func_map.get(&fq) {
            return Some(id.clone());
        }
        // Try with method info
        if let Some(method_info) = &func_ref.method_info {
            let mangled = method_info.to_mangled_name();
            let fq2 = MangledName::in_module(module_source, &mangled);
            if let Some(id) = self.ctx.func_map.get(&fq2) {
                return Some(id.clone());
            }
        }
        None
    }

    /// Resolve a call target, preferring the stamped canonical `func_id` (a
    /// direct id→`WirFuncId` lookup, no name reconstruction) and falling back to
    /// [`Self::resolve_function_ref`] on the callee `descriptor` for an imported
    /// callee — imports are not entered into `funcid_map`, so the name path
    /// resolves them. `descriptor` is the record-derived [`Self::callee_descriptor`],
    /// not a node field (the call node carries no `FunctionRef`).
    pub(super) fn resolve_call(
        &self,
        descriptor: &nir::FunctionRef,
        func_id: FuncId,
    ) -> Option<WirFuncId> {
        if let Some(wid) = self.ctx.funcid_map.get(&func_id) {
            return Some(wid.clone());
        }
        self.resolve_function_ref(descriptor)
    }

    /// The callee's identity descriptor (`module_source`, `name`,
    /// `monomorph_info`, `method_info`) used for builtin / canonical dispatch and
    /// diagnostics. Reads it from the function record by `func_id` — the single
    /// source of truth (`FuncId == store position`, Phase 4), and the sole callee
    /// reference now that the call node carries no `FunctionRef`.
    pub(super) fn callee_descriptor(&self, func_id: FuncId) -> nir::FunctionRef {
        use cranelift_entity::EntityRef;
        let rec = self.ctx.package.functions[func_id.index()].borrow();
        nir::FunctionRef::from_resolved(&rec, rec.module_source.clone())
    }

    /// The 128-bit pattern a constant `i128` / `u128` operand denotes, or `None`
    /// for an operand that is not a literal. Two shapes reach here for one
    /// source literal: the constructor call `lower::wide_int_literal` emits, and
    /// the struct literal the inliner leaves once it has inlined that
    /// constructor — matching only one would answer differently per `-O` level.
    fn operand_const_wide_int(&self, op: Operand) -> Option<i128> {
        use crate::lower::wide_int_literal::{WideIntCtor, classify_ctor};
        use crate::nir_arena::ExprKind;

        match &self.body.exprs[op.as_expr()?].kind {
            ExprKind::Call { func_id, args, .. } => {
                let ctor = classify_ctor(self.type_table, &self.callee_descriptor(*func_id).name)?;
                let halves: Vec<u64> = args
                    .iter()
                    .map(|a| self.body.operand_const_int(a.expr))
                    .collect::<Option<_>>()?;
                Some(ctor.compose(&halves))
            }
            ExprKind::StructLiteral {
                struct_type,
                fields,
                ..
            } => {
                self.type_table.wide_int_item(*struct_type)?;
                let half = |index: u32| {
                    let field = fields.iter().find(|f| f.field_index == index)?;
                    self.body.operand_const_int(field.value)
                };
                Some(WideIntCtor::FromPair.compose(&[half(0)?, half(1)?]))
            }
            _ => None,
        }
    }

    /// For `builtin::array_clone::<T>(arr)` whose `arr` is typed as
    /// `BuiltinArray(elem)`, the `$value_copy$` helper to invoke on every
    /// element when `elem` is itself value-typed. `None` for a primitive
    /// element, where Wasm GC's plain `array.set` is already a deep copy.
    fn array_element_copy(&self, src_type_id: TypeId) -> Option<WirFuncId> {
        use crate::tir::ResolvedType;
        let ty = self.type_table.peel_refs(src_type_id);
        let elem = match self.type_table.get(ty) {
            ResolvedType::BuiltinArray(elem) => *elem,
            _ => return None,
        };
        if !value_copy::needs_value_copy(elem, self.type_table) {
            return None;
        }
        // Falling back to the bulk clone would answer a deep copy with a
        // shallow `array.copy`, aliasing every element.
        let helper = self
            .ctx
            .package
            .value_copy_helpers
            .get(elem, self.type_table)
            .unwrap_or_else(|| {
                panic!(
                    "no value-copy helper for array element {}",
                    self.type_table.mangle_type_arg_for_generic(elem)
                )
            });
        Some(self.ctx.funcid_map.get(helper).cloned().unwrap_or_else(|| {
            panic!(
                "the value-copy helper for array element {} reached WIR build with no function",
                self.type_table.mangle_type_arg_for_generic(elem)
            )
        }))
    }

    fn translate_array_ref_operand(
        &mut self,
        args: &[ArenaCallArg],
    ) -> (WirTypeId, WirInstr, TypeId) {
        let src_expr = args[0].expr;
        let src = self.translate_operand(src_expr);
        let src_type_id = self.operand_type_id(src_expr);
        (self.ref_type_id(src_type_id), src, src_type_id)
    }

    fn build_bulk_array_clone(
        &mut self,
        type_id: WirTypeId,
        src: WirInstr,
        len: Option<WirInstr>,
    ) -> WirInstr {
        // One set per clone: the `len` operand can hold a bulk clone of this
        // same type, and it runs between this one's `src` store and its copy.
        let src_name = self.fresh_local("$array_clone_src");
        let dst_name = self.fresh_local("$array_clone_dst");
        let len_name = self.fresh_local("$array_clone_len");
        let ref_ty = WirType::Ref {
            type_id: type_id.clone(),
            nullable: false,
        };

        let mut seq = Vec::new();
        seq.extend(declare_and_set_local(src_name.clone(), ref_ty.clone(), src));
        let len_instr = len.unwrap_or_else(|| {
            WirInstr::ArrayLen(Box::new(WirInstr::LocalGet {
                name: src_name.clone(),
                result_ty: ref_ty.clone(),
            }))
        });
        seq.extend(declare_and_set_local(
            len_name.clone(),
            WirType::I32,
            len_instr,
        ));
        seq.extend(declare_and_set_local(
            dst_name.clone(),
            ref_ty.clone(),
            WirInstr::ArrayNewDefault {
                type_id: type_id.clone(),
                len: Box::new(WirInstr::LocalGet {
                    name: len_name.clone(),
                    result_ty: WirType::I32,
                }),
            },
        ));
        seq.push(WirInstr::ArrayCopy {
            dest_type_id: type_id.clone(),
            src_type_id: type_id,
            dest: Box::new(WirInstr::LocalGet {
                name: dst_name.clone(),
                result_ty: ref_ty.clone(),
            }),
            dest_offset: Box::new(WirInstr::I32Const(0)),
            src: Box::new(WirInstr::LocalGet {
                name: src_name,
                result_ty: ref_ty.clone(),
            }),
            src_offset: Box::new(WirInstr::I32Const(0)),
            len: Box::new(WirInstr::LocalGet {
                name: len_name,
                result_ty: WirType::I32,
            }),
        });
        seq.push(WirInstr::LocalGet {
            name: dst_name,
            result_ty: ref_ty,
        });
        WirInstr::Seq(seq)
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
            "i32_load" => {
                let addr = self.translate_operand(args[0].expr);
                Some(WirInstr::I32Load {
                    offset: 0,
                    align: 2,
                    addr: Box::new(addr),
                })
            }
            "i32_load8_u" => {
                let addr = self.translate_operand(args[0].expr);
                Some(WirInstr::I32Load8U {
                    offset: 0,
                    align: 0,
                    addr: Box::new(addr),
                })
            }
            "i32_load8_s" => {
                let addr = self.translate_operand(args[0].expr);
                Some(WirInstr::I32Load8S {
                    offset: 0,
                    align: 0,
                    addr: Box::new(addr),
                })
            }
            "i32_load16_u" => {
                let addr = self.translate_operand(args[0].expr);
                Some(WirInstr::I32Load16U {
                    offset: 0,
                    align: 1,
                    addr: Box::new(addr),
                })
            }
            "i32_load16_s" => {
                let addr = self.translate_operand(args[0].expr);
                Some(WirInstr::I32Load16S {
                    offset: 0,
                    align: 1,
                    addr: Box::new(addr),
                })
            }

            "i64_load" => {
                let addr = self.translate_operand(args[0].expr);
                Some(WirInstr::I64Load {
                    offset: 0,
                    align: 3,
                    addr: Box::new(addr),
                })
            }

            "i32_store" => {
                let addr = self.translate_operand(args[0].expr);
                let val = self.translate_operand(args[1].expr);
                Some(WirInstr::I32Store {
                    offset: 0,
                    align: 2,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "i32_store8" => {
                let addr = self.translate_operand(args[0].expr);
                let val = self.translate_operand(args[1].expr);
                Some(WirInstr::I32Store8 {
                    offset: 0,
                    align: 0,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "i32_store16" => {
                let addr = self.translate_operand(args[0].expr);
                let val = self.translate_operand(args[1].expr);
                Some(WirInstr::I32Store16 {
                    offset: 0,
                    align: 1,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            "i64_store" => {
                let addr = self.translate_operand(args[0].expr);
                let val = self.translate_operand(args[1].expr);
                Some(WirInstr::I64Store {
                    offset: 0,
                    align: 3,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }
            // Floats have no dedicated WIR store/load; reinterpret to the
            // same-width integer and reuse the integer memory ops (byte-identical
            // to `fN.store` / `fN.load`).
            "f32_store" => {
                let addr = self.translate_operand(args[0].expr);
                let val = self.translate_operand(args[1].expr);
                Some(WirInstr::I32Store {
                    offset: 0,
                    align: 2,
                    addr: Box::new(addr),
                    value: Box::new(WirInstr::I32ReinterpretF32(Box::new(val))),
                })
            }
            "f64_store" => {
                let addr = self.translate_operand(args[0].expr);
                let val = self.translate_operand(args[1].expr);
                Some(WirInstr::I64Store {
                    offset: 0,
                    align: 3,
                    addr: Box::new(addr),
                    value: Box::new(WirInstr::I64ReinterpretF64(Box::new(val))),
                })
            }
            "f32_load" => {
                let addr = self.translate_operand(args[0].expr);
                Some(WirInstr::F32ReinterpretI32(Box::new(WirInstr::I32Load {
                    offset: 0,
                    align: 2,
                    addr: Box::new(addr),
                })))
            }
            "f64_load" => {
                let addr = self.translate_operand(args[0].expr);
                Some(WirInstr::F64ReinterpretI64(Box::new(WirInstr::I64Load {
                    offset: 0,
                    align: 3,
                    addr: Box::new(addr),
                })))
            }
            "v128_load" => {
                let addr = self.translate_operand(args[0].expr);
                Some(WirInstr::V128Load {
                    offset: 0,
                    align: 4,
                    addr: Box::new(addr),
                })
            }
            "v128_store" => {
                let addr = self.translate_operand(args[0].expr);
                let val = self.translate_operand(args[1].expr);
                Some(WirInstr::V128Store {
                    offset: 0,
                    align: 4,
                    addr: Box::new(addr),
                    value: Box::new(val),
                })
            }

            "array_new" => {
                // array.new_default: creates a new array of the given element type
                let len = self.translate_operand(args[0].expr);
                Some(WirInstr::ArrayNewDefault {
                    type_id: self.ref_type_id(result_type_id),
                    len: Box::new(len),
                })
            }
            "array_get_value_u8" => {
                let arr = self.translate_operand(args[0].expr);
                let idx = self.translate_operand(args[1].expr);
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
            "array_set_u8" => {
                let arr = self.translate_operand(args[0].expr);
                let idx = self.translate_operand(args[1].expr);
                let val = self.translate_operand(args[2].expr);
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
            "array_get_value" => {
                let arr = self.translate_operand(args[0].expr);
                let idx = self.translate_operand(args[1].expr);
                let type_id = self.ref_type_id(self.operand_type_id(args[0].expr));
                Some(WirInstr::ArrayGet {
                    result_ty: self.array_element_wir_type(&type_id),
                    type_id,
                    array: Box::new(arr),
                    index: Box::new(idx),
                })
            }
            "array_get_ref" | "array_get_ref_mut" => {
                let arr = self.translate_operand(args[0].expr);
                let idx = self.translate_operand(args[1].expr);
                let type_id = self.ref_type_id(self.operand_type_id(args[0].expr));
                let elem_ty = self.array_element_wir_type(&type_id);
                let get = WirInstr::ArrayGet {
                    type_id,
                    array: Box::new(arr),
                    index: Box::new(idx),
                    result_ty: elem_ty.clone(),
                };
                let result_wir = self.wir_type(result_type_id);
                if ref_binding_needs_boxing(&result_wir, &elem_ty)
                    && let WirType::Ref {
                        type_id: box_tid, ..
                    } = result_wir
                {
                    Some(self.struct_new(box_tid, vec![get]))
                } else {
                    Some(get)
                }
            }
            "array_set" => {
                let arr = self.translate_operand(args[0].expr);
                let idx = self.translate_operand(args[1].expr);
                let val = self.translate_operand(args[2].expr);
                Some(WirInstr::ArraySet {
                    type_id: self.ref_type_id(self.operand_type_id(args[0].expr)),
                    array: Box::new(arr),
                    index: Box::new(idx),
                    value: Box::new(val),
                })
            }
            "array_copy" => {
                let dst = self.translate_operand(args[0].expr);
                let dst_offset = self.translate_operand(args[1].expr);
                let src = self.translate_operand(args[2].expr);
                let src_offset = self.translate_operand(args[3].expr);
                let len = self.translate_operand(args[4].expr);
                let type_id = self.ref_type_id(self.operand_type_id(args[0].expr));
                Some(WirInstr::ArrayCopy {
                    dest_type_id: type_id.clone(),
                    src_type_id: type_id,
                    dest: Box::new(dst),
                    dest_offset: Box::new(dst_offset),
                    src: Box::new(src),
                    src_offset: Box::new(src_offset),
                    len: Box::new(len),
                })
            }
            "array_clone" => {
                let (type_id, src, src_type_id) = self.translate_array_ref_operand(args);
                match self.array_element_copy(src_type_id) {
                    Some(element_copy) => Some(WirInstr::ArrayClone {
                        type_id,
                        src: Box::new(src),
                        element_copy,
                        len: None,
                    }),
                    None => Some(self.build_bulk_array_clone(type_id, src, None)),
                }
            }
            "array_clone_prefix" => {
                let (type_id, src, src_type_id) = self.translate_array_ref_operand(args);
                let len = self.translate_operand(args[1].expr);
                match self.array_element_copy(src_type_id) {
                    Some(element_copy) => Some(WirInstr::ArrayClone {
                        type_id,
                        src: Box::new(src),
                        element_copy,
                        len: Some(Box::new(len)),
                    }),
                    None => Some(self.build_bulk_array_clone(type_id, src, Some(len))),
                }
            }
            "array_clone_shallow" => {
                // The demote pass retargets `array_clone` / `array_clone_prefix`
                // calls here by id, keeping the call-site args — so a second
                // arg, when present, is the prefix length to preserve.
                let (type_id, src, _) = self.translate_array_ref_operand(args);
                let len = args.get(1).map(|arg| self.translate_operand(arg.expr));
                Some(self.build_bulk_array_clone(type_id, src, len))
            }
            "array_fill" => {
                let arr = self.translate_operand(args[0].expr);
                let offset = self.translate_operand(args[1].expr);
                let val = self.translate_operand(args[2].expr);
                let len = self.translate_operand(args[3].expr);
                Some(WirInstr::ArrayFill {
                    type_id: self.ref_type_id(self.operand_type_id(args[0].expr)),
                    array: Box::new(arr),
                    offset: Box::new(offset),
                    value: Box::new(val),
                    len: Box::new(len),
                })
            }

            "v128_const" => Some(WirInstr::V128Const(
                self.operand_const_wide_int(args[0].expr)
                    .expect("a v128 bit pattern must be an i128 / u128 literal"),
            )),
            "memory_size" => Some(WirInstr::MemorySize),
            "memory_fill" => {
                let dst = self.translate_operand(args[0].expr);
                let value = self.translate_operand(args[1].expr);
                let len = self.translate_operand(args[2].expr);
                Some(WirInstr::MemoryFill {
                    dst: Box::new(dst),
                    value: Box::new(value),
                    len: Box::new(len),
                })
            }

            "unreachable" => Some(WirInstr::Unreachable),
            "cold_path" => {
                // Under `-f no-branch-hinting` the marker lowers to a plain
                // no-op: no `ColdPath` reaches WIR, so `apply_cold_path_hints`
                // synthesizes nothing. Dropping it here (not at NIR) keeps the
                // inliner's cold-path cost exclusion identical in both
                // configurations — see `CodegenFlags::branch_hinting`.
                if self.ctx.package.codegen_flags.branch_hinting {
                    Some(WirInstr::ColdPath)
                } else {
                    Some(WirInstr::Nop)
                }
            }
            "select" => {
                let cond = self.translate_operand(args[0].expr);
                let a = self.translate_operand(args[1].expr);
                let b = self.translate_operand(args[2].expr);
                let result_type = self
                    .ctx
                    .type_id_to_wir_type(self.type_table, self.operand_type_id(args[1].expr));
                Some(WirInstr::Select {
                    condition: Box::new(cond),
                    if_true: Box::new(a),
                    if_false: Box::new(b),
                    ty: Some(result_type),
                })
            }

            // A reference to a resource is its handle, not a GC reference
            // (divergence D6 in the reference-representation WEP).
            "ref_eq" => Some(
                match self
                    .ctx
                    .type_id_to_wir_type(self.type_table, self.operand_type_id(args[0].expr))
                {
                    WirType::I32 => binary!(self, args, WirInstr::I32Eq),
                    WirType::I64 => binary!(self, args, WirInstr::I64Eq),
                    _ => binary!(self, args, WirInstr::RefEq),
                },
            ),

            _ if MULTIVALUE_I64_BUILTINS.contains(&builtin_name) => {
                let instr = self.translate_multivalue_i64_builtin(builtin_name, args);
                Some(self.wrap_multivalue_i64(instr, result_type_id))
            }

            "i32_as_char" => Some(self.translate_operand(args[0].expr)),

            "call_indirect_stdout_write_via_stream" | "call_indirect_stderr_write_via_stream" => {
                // The ambient panic / assert-diagnostic path never forces its
                // own import. DCE registers `Std{out,err}::write_via_stream` in
                // `func_map` exactly when the world provides the sink (or a real
                // `eprintln` / `println` already uses it); a miss lowers to
                // `unreachable`, keeping a purely-computational component free of
                // `wasi:cli/stderr`.
                let is_stderr = builtin_name.contains("stderr");
                let wasi_func_name = if is_stderr {
                    "wasi:cli/Stderr::write_via_stream"
                } else {
                    "wasi:cli/Stdout::write_via_stream"
                };
                let key = MangledName::wasi_import(wasi_func_name);
                match self.ctx.func_map.get(&key).cloned() {
                    Some(func_id) => {
                        let call_args: Vec<WirInstr> = args
                            .iter()
                            .map(|a| self.translate_operand(a.expr))
                            .collect();
                        Some(WirInstr::Call {
                            func_id,
                            args: call_args,
                        })
                    }
                    None => Some(WirInstr::Unreachable),
                }
            }

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
            "array_len" => unary!(self, args, WirInstr::ArrayLen),
            "f64_abs" => unary!(self, args, WirInstr::F64Abs),
            "f64_ceil" => unary!(self, args, WirInstr::F64Ceil),
            "f64_floor" => unary!(self, args, WirInstr::F64Floor),
            "f64_trunc" => unary!(self, args, WirInstr::F64Trunc),
            "f64_nearest" => unary!(self, args, WirInstr::F64Nearest),
            "f64_sqrt" => unary!(self, args, WirInstr::F64Sqrt),
            "f64_min" => binary!(self, args, WirInstr::F64Min),
            "f64_max" => binary!(self, args, WirInstr::F64Max),
            "f64_copysign" => binary!(self, args, WirInstr::F64Copysign),
            "f32_abs" => unary!(self, args, WirInstr::F32Abs),
            "f32_ceil" => unary!(self, args, WirInstr::F32Ceil),
            "f32_floor" => unary!(self, args, WirInstr::F32Floor),
            "f32_trunc" => unary!(self, args, WirInstr::F32Trunc),
            "f32_nearest" => unary!(self, args, WirInstr::F32Nearest),
            "f32_sqrt" => unary!(self, args, WirInstr::F32Sqrt),
            "f32_min" => binary!(self, args, WirInstr::F32Min),
            "f32_max" => binary!(self, args, WirInstr::F32Max),
            "f32_copysign" => binary!(self, args, WirInstr::F32Copysign),
            "black_box" => unary!(self, args, WirInstr::BlackBox),
            "is_uninitialized" => {
                let mut a = self.translate_operand(args[0].expr);
                // The point of the read is to observe the `null` placeholder,
                // so it is typed nullable — otherwise codegen narrows it with
                // `ref.as_non_null` and traps before the test can run.
                if let WirInstr::GlobalGet { result_ty, .. } = &mut a {
                    result_ty.set_nullable();
                }
                WirInstr::RefIsNull(Box::new(a))
            }
            "i32_and" => binary!(self, args, WirInstr::I32And),
            "i32_eqz" => unary!(self, args, WirInstr::I32Eqz),
            "i32_clz" => unary!(self, args, WirInstr::I32Clz),
            "i64_clz" => unary!(self, args, WirInstr::I64Clz),
            "i32_ctz" => unary!(self, args, WirInstr::I32Ctz),
            "i64_ctz" => unary!(self, args, WirInstr::I64Ctz),
            "i32_popcnt" => unary!(self, args, WirInstr::I32Popcnt),
            "i64_popcnt" => unary!(self, args, WirInstr::I64Popcnt),
            "i64_reinterpret_f64" => unary!(self, args, WirInstr::I64ReinterpretF64),
            "f64_reinterpret_i64" => unary!(self, args, WirInstr::F64ReinterpretI64),
            "i32_reinterpret_f32" => unary!(self, args, WirInstr::I32ReinterpretF32),
            "f32_reinterpret_i32" => unary!(self, args, WirInstr::F32ReinterpretI32),
            // A half and its `u16` share a representation, so the cast is the
            // operand. Wasm has no half precision value type to convert to.
            "u16_reinterpret_f16"
            | "f16_reinterpret_u16"
            | "u16_reinterpret_bf16"
            | "bf16_reinterpret_u16" => self.translate_operand(args[0].expr),
            "v128_not" => unary!(self, args, WirInstr::V128Not),
            "v128_and" => binary!(self, args, WirInstr::V128And),
            "v128_or" => binary!(self, args, WirInstr::V128Or),
            "v128_xor" => binary!(self, args, WirInstr::V128Xor),
            "v128_bitselect" => ternary!(self, args, WirInstr::V128Bitselect),
            "i8x16_splat" => unary!(self, args, WirInstr::I8x16Splat),
            "i8x16_shuffle" => {
                let lanes = array::from_fn(|i| operand_lane_const(self.body, args[i].expr));
                let a = self.translate_operand(args[16].expr);
                let b = self.translate_operand(args[17].expr);
                WirInstr::I8x16Shuffle(lanes, Box::new(a), Box::new(b))
            }
            "i8x16_extract_lane_s" => {
                extract_lane!(self, args, WirInstr::I8x16ExtractLaneS)
            }
            "i8x16_extract_lane_u" => {
                extract_lane!(self, args, WirInstr::I8x16ExtractLaneU)
            }
            "i8x16_replace_lane" => replace_lane!(self, args, WirInstr::I8x16ReplaceLane),
            "i8x16_add" => binary!(self, args, WirInstr::I8x16Add),
            "i8x16_sub" => binary!(self, args, WirInstr::I8x16Sub),
            "i8x16_neg" => unary!(self, args, WirInstr::I8x16Neg),
            "i8x16_eq" => binary!(self, args, WirInstr::I8x16Eq),
            "i8x16_ne" => binary!(self, args, WirInstr::I8x16Ne),
            "i8x16_lt_s" => binary!(self, args, WirInstr::I8x16LtS),
            "i8x16_gt_s" => binary!(self, args, WirInstr::I8x16GtS),
            "i8x16_le_s" => binary!(self, args, WirInstr::I8x16LeS),
            "i8x16_ge_s" => binary!(self, args, WirInstr::I8x16GeS),
            "i8x16_lt_u" => binary!(self, args, WirInstr::I8x16LtU),
            "i8x16_gt_u" => binary!(self, args, WirInstr::I8x16GtU),
            "i8x16_le_u" => binary!(self, args, WirInstr::I8x16LeU),
            "i8x16_ge_u" => binary!(self, args, WirInstr::I8x16GeU),
            "i8x16_shl" => binary!(self, args, WirInstr::I8x16Shl),
            "i8x16_shr_s" => binary!(self, args, WirInstr::I8x16ShrS),
            "i8x16_shr_u" => binary!(self, args, WirInstr::I8x16ShrU),
            "i8x16_swizzle" => binary!(self, args, WirInstr::I8x16Swizzle),
            "i16x8_splat" => unary!(self, args, WirInstr::I16x8Splat),
            "i16x8_extract_lane_s" => {
                extract_lane!(self, args, WirInstr::I16x8ExtractLaneS)
            }
            "i16x8_extract_lane_u" => {
                extract_lane!(self, args, WirInstr::I16x8ExtractLaneU)
            }
            "i16x8_replace_lane" => replace_lane!(self, args, WirInstr::I16x8ReplaceLane),
            "i16x8_add" => binary!(self, args, WirInstr::I16x8Add),
            "i16x8_sub" => binary!(self, args, WirInstr::I16x8Sub),
            "i16x8_mul" => binary!(self, args, WirInstr::I16x8Mul),
            "i16x8_neg" => unary!(self, args, WirInstr::I16x8Neg),
            "i16x8_eq" => binary!(self, args, WirInstr::I16x8Eq),
            "i16x8_ne" => binary!(self, args, WirInstr::I16x8Ne),
            "i16x8_lt_s" => binary!(self, args, WirInstr::I16x8LtS),
            "i16x8_gt_s" => binary!(self, args, WirInstr::I16x8GtS),
            "i16x8_le_s" => binary!(self, args, WirInstr::I16x8LeS),
            "i16x8_ge_s" => binary!(self, args, WirInstr::I16x8GeS),
            "i16x8_lt_u" => binary!(self, args, WirInstr::I16x8LtU),
            "i16x8_gt_u" => binary!(self, args, WirInstr::I16x8GtU),
            "i16x8_le_u" => binary!(self, args, WirInstr::I16x8LeU),
            "i16x8_ge_u" => binary!(self, args, WirInstr::I16x8GeU),
            "i16x8_shl" => binary!(self, args, WirInstr::I16x8Shl),
            "i16x8_shr_s" => binary!(self, args, WirInstr::I16x8ShrS),
            "i16x8_shr_u" => binary!(self, args, WirInstr::I16x8ShrU),
            "i32x4_splat" => unary!(self, args, WirInstr::I32x4Splat),
            "i32x4_extract_lane" => extract_lane!(self, args, WirInstr::I32x4ExtractLane),
            "i32x4_replace_lane" => replace_lane!(self, args, WirInstr::I32x4ReplaceLane),
            "i32x4_add" => binary!(self, args, WirInstr::I32x4Add),
            "i32x4_sub" => binary!(self, args, WirInstr::I32x4Sub),
            "i32x4_mul" => binary!(self, args, WirInstr::I32x4Mul),
            "i32x4_neg" => unary!(self, args, WirInstr::I32x4Neg),
            "i32x4_eq" => binary!(self, args, WirInstr::I32x4Eq),
            "i32x4_ne" => binary!(self, args, WirInstr::I32x4Ne),
            "i32x4_lt_s" => binary!(self, args, WirInstr::I32x4LtS),
            "i32x4_gt_s" => binary!(self, args, WirInstr::I32x4GtS),
            "i32x4_le_s" => binary!(self, args, WirInstr::I32x4LeS),
            "i32x4_ge_s" => binary!(self, args, WirInstr::I32x4GeS),
            "i32x4_lt_u" => binary!(self, args, WirInstr::I32x4LtU),
            "i32x4_gt_u" => binary!(self, args, WirInstr::I32x4GtU),
            "i32x4_le_u" => binary!(self, args, WirInstr::I32x4LeU),
            "i32x4_ge_u" => binary!(self, args, WirInstr::I32x4GeU),
            "i32x4_shl" => binary!(self, args, WirInstr::I32x4Shl),
            "i32x4_shr_s" => binary!(self, args, WirInstr::I32x4ShrS),
            "i32x4_shr_u" => binary!(self, args, WirInstr::I32x4ShrU),
            "i64x2_splat" => unary!(self, args, WirInstr::I64x2Splat),
            "i64x2_extract_lane" => extract_lane!(self, args, WirInstr::I64x2ExtractLane),
            "i64x2_replace_lane" => replace_lane!(self, args, WirInstr::I64x2ReplaceLane),
            "i64x2_add" => binary!(self, args, WirInstr::I64x2Add),
            "i64x2_sub" => binary!(self, args, WirInstr::I64x2Sub),
            "i64x2_mul" => binary!(self, args, WirInstr::I64x2Mul),
            "i64x2_neg" => unary!(self, args, WirInstr::I64x2Neg),
            "i64x2_eq" => binary!(self, args, WirInstr::I64x2Eq),
            "i64x2_ne" => binary!(self, args, WirInstr::I64x2Ne),
            "i64x2_lt_s" => binary!(self, args, WirInstr::I64x2LtS),
            "i64x2_gt_s" => binary!(self, args, WirInstr::I64x2GtS),
            "i64x2_le_s" => binary!(self, args, WirInstr::I64x2LeS),
            "i64x2_ge_s" => binary!(self, args, WirInstr::I64x2GeS),
            "i64x2_shl" => binary!(self, args, WirInstr::I64x2Shl),
            "i64x2_shr_s" => binary!(self, args, WirInstr::I64x2ShrS),
            "i64x2_shr_u" => binary!(self, args, WirInstr::I64x2ShrU),
            "f32x4_splat" => unary!(self, args, WirInstr::F32x4Splat),
            "f32x4_extract_lane" => extract_lane!(self, args, WirInstr::F32x4ExtractLane),
            "f32x4_replace_lane" => replace_lane!(self, args, WirInstr::F32x4ReplaceLane),
            "f32x4_add" => binary!(self, args, WirInstr::F32x4Add),
            "f32x4_sub" => binary!(self, args, WirInstr::F32x4Sub),
            "f32x4_mul" => binary!(self, args, WirInstr::F32x4Mul),
            "f32x4_div" => binary!(self, args, WirInstr::F32x4Div),
            "f32x4_neg" => unary!(self, args, WirInstr::F32x4Neg),
            "f32x4_sqrt" => unary!(self, args, WirInstr::F32x4Sqrt),
            "f32x4_abs" => unary!(self, args, WirInstr::F32x4Abs),
            "f32x4_eq" => binary!(self, args, WirInstr::F32x4Eq),
            "f32x4_ne" => binary!(self, args, WirInstr::F32x4Ne),
            "f32x4_lt" => binary!(self, args, WirInstr::F32x4Lt),
            "f32x4_gt" => binary!(self, args, WirInstr::F32x4Gt),
            "f32x4_le" => binary!(self, args, WirInstr::F32x4Le),
            "f32x4_ge" => binary!(self, args, WirInstr::F32x4Ge),
            "f32x4_min" => binary!(self, args, WirInstr::F32x4Min),
            "f32x4_max" => binary!(self, args, WirInstr::F32x4Max),
            "f64x2_splat" => unary!(self, args, WirInstr::F64x2Splat),
            "f64x2_extract_lane" => extract_lane!(self, args, WirInstr::F64x2ExtractLane),
            "f64x2_replace_lane" => replace_lane!(self, args, WirInstr::F64x2ReplaceLane),
            "f64x2_add" => binary!(self, args, WirInstr::F64x2Add),
            "f64x2_sub" => binary!(self, args, WirInstr::F64x2Sub),
            "f64x2_mul" => binary!(self, args, WirInstr::F64x2Mul),
            "f64x2_div" => binary!(self, args, WirInstr::F64x2Div),
            "f64x2_neg" => unary!(self, args, WirInstr::F64x2Neg),
            "f64x2_sqrt" => unary!(self, args, WirInstr::F64x2Sqrt),
            "f64x2_abs" => unary!(self, args, WirInstr::F64x2Abs),
            "f64x2_eq" => binary!(self, args, WirInstr::F64x2Eq),
            "f64x2_ne" => binary!(self, args, WirInstr::F64x2Ne),
            "f64x2_lt" => binary!(self, args, WirInstr::F64x2Lt),
            "f64x2_gt" => binary!(self, args, WirInstr::F64x2Gt),
            "f64x2_le" => binary!(self, args, WirInstr::F64x2Le),
            "f64x2_ge" => binary!(self, args, WirInstr::F64x2Ge),
            "f64x2_min" => binary!(self, args, WirInstr::F64x2Min),
            "f64x2_max" => binary!(self, args, WirInstr::F64x2Max),
            "i8x16_abs" => unary!(self, args, WirInstr::I8x16Abs),
            "i8x16_add_sat_s" => binary!(self, args, WirInstr::I8x16AddSatS),
            "i8x16_add_sat_u" => binary!(self, args, WirInstr::I8x16AddSatU),
            "i8x16_sub_sat_s" => binary!(self, args, WirInstr::I8x16SubSatS),
            "i8x16_sub_sat_u" => binary!(self, args, WirInstr::I8x16SubSatU),
            "i8x16_min_s" => binary!(self, args, WirInstr::I8x16MinS),
            "i8x16_min_u" => binary!(self, args, WirInstr::I8x16MinU),
            "i8x16_max_s" => binary!(self, args, WirInstr::I8x16MaxS),
            "i8x16_max_u" => binary!(self, args, WirInstr::I8x16MaxU),
            "i8x16_avgr_u" => binary!(self, args, WirInstr::I8x16AvgrU),
            "i8x16_all_true" => unary!(self, args, WirInstr::I8x16AllTrue),
            "i8x16_bitmask" => unary!(self, args, WirInstr::I8x16Bitmask),
            "i8x16_narrow_i16x8_s" => binary!(self, args, WirInstr::I8x16NarrowI16x8S),
            "i8x16_narrow_i16x8_u" => binary!(self, args, WirInstr::I8x16NarrowI16x8U),
            "i8x16_popcnt" => unary!(self, args, WirInstr::I8x16Popcnt),
            "i16x8_abs" => unary!(self, args, WirInstr::I16x8Abs),
            "i16x8_add_sat_s" => binary!(self, args, WirInstr::I16x8AddSatS),
            "i16x8_add_sat_u" => binary!(self, args, WirInstr::I16x8AddSatU),
            "i16x8_sub_sat_s" => binary!(self, args, WirInstr::I16x8SubSatS),
            "i16x8_sub_sat_u" => binary!(self, args, WirInstr::I16x8SubSatU),
            "i16x8_min_s" => binary!(self, args, WirInstr::I16x8MinS),
            "i16x8_min_u" => binary!(self, args, WirInstr::I16x8MinU),
            "i16x8_max_s" => binary!(self, args, WirInstr::I16x8MaxS),
            "i16x8_max_u" => binary!(self, args, WirInstr::I16x8MaxU),
            "i16x8_avgr_u" => binary!(self, args, WirInstr::I16x8AvgrU),
            "i16x8_all_true" => unary!(self, args, WirInstr::I16x8AllTrue),
            "i16x8_bitmask" => unary!(self, args, WirInstr::I16x8Bitmask),
            "i16x8_narrow_i32x4_s" => binary!(self, args, WirInstr::I16x8NarrowI32x4S),
            "i16x8_narrow_i32x4_u" => binary!(self, args, WirInstr::I16x8NarrowI32x4U),
            "i16x8_extend_low_i8x16_s" => {
                unary!(self, args, WirInstr::I16x8ExtendLowI8x16S)
            }
            "i16x8_extend_high_i8x16_s" => {
                unary!(self, args, WirInstr::I16x8ExtendHighI8x16S)
            }
            "i16x8_extend_low_i8x16_u" => {
                unary!(self, args, WirInstr::I16x8ExtendLowI8x16U)
            }
            "i16x8_extend_high_i8x16_u" => {
                unary!(self, args, WirInstr::I16x8ExtendHighI8x16U)
            }
            "i16x8_extmul_low_i8x16_s" => {
                binary!(self, args, WirInstr::I16x8ExtMulLowI8x16S)
            }
            "i16x8_extmul_high_i8x16_s" => {
                binary!(self, args, WirInstr::I16x8ExtMulHighI8x16S)
            }
            "i16x8_extmul_low_i8x16_u" => {
                binary!(self, args, WirInstr::I16x8ExtMulLowI8x16U)
            }
            "i16x8_extmul_high_i8x16_u" => {
                binary!(self, args, WirInstr::I16x8ExtMulHighI8x16U)
            }
            "i16x8_extadd_pairwise_i8x16_s" => {
                unary!(self, args, WirInstr::I16x8ExtAddPairwiseI8x16S)
            }
            "i16x8_extadd_pairwise_i8x16_u" => {
                unary!(self, args, WirInstr::I16x8ExtAddPairwiseI8x16U)
            }
            "i16x8_q15mulr_sat_s" => binary!(self, args, WirInstr::I16x8Q15MulrSatS),
            "i32x4_abs" => unary!(self, args, WirInstr::I32x4Abs),
            "i32x4_all_true" => unary!(self, args, WirInstr::I32x4AllTrue),
            "i32x4_bitmask" => unary!(self, args, WirInstr::I32x4Bitmask),
            "i32x4_min_s" => binary!(self, args, WirInstr::I32x4MinS),
            "i32x4_min_u" => binary!(self, args, WirInstr::I32x4MinU),
            "i32x4_max_s" => binary!(self, args, WirInstr::I32x4MaxS),
            "i32x4_max_u" => binary!(self, args, WirInstr::I32x4MaxU),
            "i32x4_dot_i16x8_s" => binary!(self, args, WirInstr::I32x4DotI16x8S),
            "i32x4_extend_low_i16x8_s" => {
                unary!(self, args, WirInstr::I32x4ExtendLowI16x8S)
            }
            "i32x4_extend_high_i16x8_s" => {
                unary!(self, args, WirInstr::I32x4ExtendHighI16x8S)
            }
            "i32x4_extend_low_i16x8_u" => {
                unary!(self, args, WirInstr::I32x4ExtendLowI16x8U)
            }
            "i32x4_extend_high_i16x8_u" => {
                unary!(self, args, WirInstr::I32x4ExtendHighI16x8U)
            }
            "i32x4_extmul_low_i16x8_s" => {
                binary!(self, args, WirInstr::I32x4ExtMulLowI16x8S)
            }
            "i32x4_extmul_high_i16x8_s" => {
                binary!(self, args, WirInstr::I32x4ExtMulHighI16x8S)
            }
            "i32x4_extmul_low_i16x8_u" => {
                binary!(self, args, WirInstr::I32x4ExtMulLowI16x8U)
            }
            "i32x4_extmul_high_i16x8_u" => {
                binary!(self, args, WirInstr::I32x4ExtMulHighI16x8U)
            }
            "i32x4_extadd_pairwise_i16x8_s" => {
                unary!(self, args, WirInstr::I32x4ExtAddPairwiseI16x8S)
            }
            "i32x4_extadd_pairwise_i16x8_u" => {
                unary!(self, args, WirInstr::I32x4ExtAddPairwiseI16x8U)
            }
            "i32x4_trunc_sat_f32x4_s" => unary!(self, args, WirInstr::I32x4TruncSatF32x4S),
            "i32x4_trunc_sat_f32x4_u" => unary!(self, args, WirInstr::I32x4TruncSatF32x4U),
            "i32x4_trunc_sat_f64x2_s_zero" => {
                unary!(self, args, WirInstr::I32x4TruncSatF64x2SZero)
            }
            "i32x4_trunc_sat_f64x2_u_zero" => {
                unary!(self, args, WirInstr::I32x4TruncSatF64x2UZero)
            }
            "i64x2_abs" => unary!(self, args, WirInstr::I64x2Abs),
            "i64x2_all_true" => unary!(self, args, WirInstr::I64x2AllTrue),
            "i64x2_bitmask" => unary!(self, args, WirInstr::I64x2Bitmask),
            "i64x2_extend_low_i32x4_s" => {
                unary!(self, args, WirInstr::I64x2ExtendLowI32x4S)
            }
            "i64x2_extend_high_i32x4_s" => {
                unary!(self, args, WirInstr::I64x2ExtendHighI32x4S)
            }
            "i64x2_extend_low_i32x4_u" => {
                unary!(self, args, WirInstr::I64x2ExtendLowI32x4U)
            }
            "i64x2_extend_high_i32x4_u" => {
                unary!(self, args, WirInstr::I64x2ExtendHighI32x4U)
            }
            "i64x2_extmul_low_i32x4_s" => {
                binary!(self, args, WirInstr::I64x2ExtMulLowI32x4S)
            }
            "i64x2_extmul_high_i32x4_s" => {
                binary!(self, args, WirInstr::I64x2ExtMulHighI32x4S)
            }
            "i64x2_extmul_low_i32x4_u" => {
                binary!(self, args, WirInstr::I64x2ExtMulLowI32x4U)
            }
            "i64x2_extmul_high_i32x4_u" => {
                binary!(self, args, WirInstr::I64x2ExtMulHighI32x4U)
            }
            "f32x4_ceil" => unary!(self, args, WirInstr::F32x4Ceil),
            "f32x4_floor" => unary!(self, args, WirInstr::F32x4Floor),
            "f32x4_trunc" => unary!(self, args, WirInstr::F32x4Trunc),
            "f32x4_nearest" => unary!(self, args, WirInstr::F32x4Nearest),
            "f32x4_pmin" => binary!(self, args, WirInstr::F32x4PMin),
            "f32x4_pmax" => binary!(self, args, WirInstr::F32x4PMax),
            "f32x4_convert_i32x4_s" => unary!(self, args, WirInstr::F32x4ConvertI32x4S),
            "f32x4_convert_i32x4_u" => unary!(self, args, WirInstr::F32x4ConvertI32x4U),
            "f32x4_demote_f64x2_zero" => {
                unary!(self, args, WirInstr::F32x4DemoteF64x2Zero)
            }
            "f64x2_ceil" => unary!(self, args, WirInstr::F64x2Ceil),
            "f64x2_floor" => unary!(self, args, WirInstr::F64x2Floor),
            "f64x2_trunc" => unary!(self, args, WirInstr::F64x2Trunc),
            "f64x2_nearest" => unary!(self, args, WirInstr::F64x2Nearest),
            "f64x2_pmin" => binary!(self, args, WirInstr::F64x2PMin),
            "f64x2_pmax" => binary!(self, args, WirInstr::F64x2PMax),
            "f64x2_convert_low_i32x4_s" => {
                unary!(self, args, WirInstr::F64x2ConvertLowI32x4S)
            }
            "f64x2_convert_low_i32x4_u" => {
                unary!(self, args, WirInstr::F64x2ConvertLowI32x4U)
            }
            "f64x2_promote_low_f32x4" => {
                unary!(self, args, WirInstr::F64x2PromoteLowF32x4)
            }
            "v128_andnot" => binary!(self, args, WirInstr::V128AndNot),
            "v128_any_true" => unary!(self, args, WirInstr::V128AnyTrue),
            "i8x16_relaxed_swizzle" => binary!(self, args, WirInstr::I8x16RelaxedSwizzle),
            "i8x16_relaxed_laneselect" => {
                ternary!(self, args, WirInstr::I8x16RelaxedLaneselect)
            }
            "i16x8_relaxed_laneselect" => {
                ternary!(self, args, WirInstr::I16x8RelaxedLaneselect)
            }
            "i32x4_relaxed_laneselect" => {
                ternary!(self, args, WirInstr::I32x4RelaxedLaneselect)
            }
            "i64x2_relaxed_laneselect" => {
                ternary!(self, args, WirInstr::I64x2RelaxedLaneselect)
            }
            "f32x4_relaxed_madd" => ternary!(self, args, WirInstr::F32x4RelaxedMadd),
            "f32x4_relaxed_nmadd" => ternary!(self, args, WirInstr::F32x4RelaxedNmadd),
            "f64x2_relaxed_madd" => ternary!(self, args, WirInstr::F64x2RelaxedMadd),
            "f64x2_relaxed_nmadd" => ternary!(self, args, WirInstr::F64x2RelaxedNmadd),
            "f32x4_relaxed_min" => binary!(self, args, WirInstr::F32x4RelaxedMin),
            "f32x4_relaxed_max" => binary!(self, args, WirInstr::F32x4RelaxedMax),
            "f64x2_relaxed_min" => binary!(self, args, WirInstr::F64x2RelaxedMin),
            "f64x2_relaxed_max" => binary!(self, args, WirInstr::F64x2RelaxedMax),
            "i32x4_relaxed_trunc_f32x4_s" => {
                unary!(self, args, WirInstr::I32x4RelaxedTruncF32x4S)
            }
            "i32x4_relaxed_trunc_f32x4_u" => {
                unary!(self, args, WirInstr::I32x4RelaxedTruncF32x4U)
            }
            "i32x4_relaxed_trunc_f64x2_s_zero" => {
                unary!(self, args, WirInstr::I32x4RelaxedTruncF64x2SZero)
            }
            "i32x4_relaxed_trunc_f64x2_u_zero" => {
                unary!(self, args, WirInstr::I32x4RelaxedTruncF64x2UZero)
            }
            "i16x8_relaxed_q15mulr_s" => {
                binary!(self, args, WirInstr::I16x8RelaxedQ15mulrS)
            }
            "i16x8_relaxed_dot_i8x16_i7x16_s" => {
                binary!(self, args, WirInstr::I16x8RelaxedDotI8x16I7x16S)
            }
            "i32x4_relaxed_dot_i8x16_i7x16_add_s" => {
                ternary!(self, args, WirInstr::I32x4RelaxedDotI8x16I7x16AddS)
            }
            "memory_grow" => unary!(self, args, WirInstr::MemoryGrow),
            _ => return None,
        })
    }
    /// Translate `IndirectCall { callee, args }` to `call_ref` through canonical closure.
    pub(super) fn translate_indirect_call(
        &mut self,
        callee: Operand,
        args: &[Operand],
        result_type: TypeId,
    ) -> WirInstr {
        let callee_wir = self.translate_operand(callee);
        let callee_ty = self.operand_type_id(callee);

        // Look up the Function type to get param/result info
        let fn_type = self.type_table.get(callee_ty);
        let (param_types, return_type) = match fn_type {
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => (params.clone(), *return_type),
            other => panic!(
                "[WIR] translate_indirect_call: expected Function type, got {other:?} (callee type_id={callee_ty:?})"
            ),
        };

        // Compute canonical closure types directly by signature key. Unit
        // params are erased from the canonical signature (they have no Wasm
        // representation); every canonical-key site filters identically.
        let param_wirs: Vec<WirType> = param_types
            .iter()
            .map(|p| self.ctx.type_id_to_wir_type(self.type_table, *p))
            .filter(|t| !matches!(t, WirType::Unit))
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
        let temp_name = self.fresh_local("$indirect_call");
        let callee_ref_type = WirType::Ref {
            type_id: closure_struct_type_id.clone(),
            nullable: false,
        };

        // Build: declare temp, cast callee from abstract structref to canonical closure,
        // store, extract env + args + funcref, call_ref
        let mut stmts = declare_and_set_local(
            temp_name.clone(),
            callee_ref_type.clone(),
            WirInstr::RefCast {
                type_id: closure_struct_type_id.clone(),
                nullable: false,
                expr: Box::new(callee_wir),
            },
        )
        .to_vec();

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

        // Erase unit args from the call while preserving their evaluation
        // (mirrors the direct-call convention). The prelude runs after the
        // callee's own evaluation (the temp set above) and before the call,
        // keeping callee-then-args order; `env_arg` reads the immutable temp,
        // so leaving it inline cannot observe the prelude's effects.
        let (arg_prelude, user_args) = self.translate_args_erasing_unit(args);
        stmts.extend(arg_prelude);
        let mut call_args = vec![env_arg];
        call_args.extend(user_args);

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
        functor: Operand,
        functor_id: u32,
        target_fn_type: TypeId,
        closure_module: &ModuleSource,
    ) -> WirInstr {
        let functor_instr = self.translate_operand(functor);

        // Look up the canonical closure struct type for the target function type
        let fn_resolved = self.type_table.get(target_fn_type);
        let (param_types, return_type) = match fn_resolved {
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => (params.clone(), *return_type),
            other => panic!(
                "[WIR] translate_closure_to_canonical: expected Function type, got {other:?} (target_fn_type={target_fn_type:?})"
            ),
        };

        // Unit params are erased from the canonical signature; every
        // canonical-key site filters identically.
        let param_wirs: Vec<WirType> = param_types
            .iter()
            .map(|p| self.ctx.type_id_to_wir_type(self.type_table, *p))
            .filter(|t| !matches!(t, WirType::Unit))
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

        // Build `CanonicalClosure_K`, field order matching
        // `WirContext::get_or_create_canonical_closure_type`: the inspectable
        // layout `{ env, inspect, func }`, whose prefix is the shared
        // `$canonical_inspectable_base`, or the slim `{ env, func }` a build
        // that never inspects a closure stays on.
        let mut fields = vec![functor_instr];
        if is_inspectable {
            let inspect_id = wrappers
                .inspect
                .expect("inspectable canonical closure missing inspect wrapper");
            fields.push(WirInstr::RefFunc {
                func_id: inspect_id,
            });
        }
        fields.push(WirInstr::RefFunc {
            func_id: wrappers.call,
        });
        self.struct_new(struct_type_id, fields)
    }
}
