//! Component Model canonical ABI translation — future / stream creation,
//! read / write lowering for canonical ABI payloads, result lifting, and
//! canonical method dispatch.
//!
//! These methods are part of `FunctionTranslator`; see `translate.rs` for
//! the struct definition and the primary translation dispatch.

use crate::module_source::ModuleSource;
use crate::nir::{CallArg, FunctionRef, NirExpr, NirExprKind};
use crate::tir::{PrimitiveType, ResolvedType, TypeId};
use crate::wir::{
    CanonicalIntrinsic, CmFuturePayload, CmScalarType, CmStreamPayload, WirFuncId, WirInstr,
    WirType,
};

use super::translate::FunctionTranslator;

impl FunctionTranslator<'_, '_> {
    /// Wrap a multi-value [i64, i64] instruction in a tuple struct.
    /// The Wasm instruction pushes two i64s on the stack; we capture them
    /// into freshly declared locals via `MultiValueLocalBind`, then build
    /// a `StructNew` reading the locals back as the struct fields.
    /// Subsequent destructure patterns at the call site are collapsed by
    /// `wir_optimize::elide_struct::elide_multi_field_struct_locals`,
    /// which recognises `LocalSet temp = StructNew { fields: [LocalGet, LocalGet] }`
    /// followed by `StructGet × N` and substitutes each field access
    /// directly — eliminating the heap allocation.
    pub(super) fn wrap_multivalue_i64(
        &mut self,
        instr: WirInstr,
        result_type_id: TypeId,
    ) -> WirInstr {
        let wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, result_type_id);
        let WirType::Ref { type_id, .. } = wir_type else {
            // Fallback: just emit the instruction (shouldn't happen).
            return instr;
        };
        self.local_counter += 1;
        let n = self.local_counter;
        let lo = format!("__mv_lo_{n}");
        let hi = format!("__mv_hi_{n}");
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

    // =========================================================================
    // Canonical resource method dispatch
    // =========================================================================

    /// Map a primitive type to its CM scalar type.
    ///
    /// Returns `Some(CmScalarType::S32)` for i32, `Some(CmScalarType::U8)` for u8, etc.
    /// Returns `None` for non-CM-scalar types (f32, f64 are included as float32/float64).
    fn primitive_to_cm_scalar(prim: &PrimitiveType) -> Option<CmScalarType> {
        match prim {
            PrimitiveType::I8 => Some(CmScalarType::S8),
            PrimitiveType::I16 => Some(CmScalarType::S16),
            PrimitiveType::I32 => Some(CmScalarType::S32),
            PrimitiveType::I64 => Some(CmScalarType::S64),
            PrimitiveType::U8 => Some(CmScalarType::U8),
            PrimitiveType::U16 => Some(CmScalarType::U16),
            PrimitiveType::U32 => Some(CmScalarType::U32),
            PrimitiveType::U64 => Some(CmScalarType::U64),
            PrimitiveType::F32 => Some(CmScalarType::F32),
            PrimitiveType::F64 => Some(CmScalarType::F64),
            PrimitiveType::Bool => Some(CmScalarType::Bool),
            PrimitiveType::Char => Some(CmScalarType::Char),
            _ => None,
        }
    }

    /// Get the CM future payload type from `MonomorphInfo` (for static methods like `Future::new`).
    fn cm_future_payload_from_monomorph(&self, func: &FunctionRef) -> CmFuturePayload {
        if let Some(ref info) = func.monomorph_info
            && !info.impl_type_args.is_empty()
        {
            return self.classify_future_payload(info.impl_type_args[0]);
        }
        CmFuturePayload::Trailers
    }

    /// Get the CM future payload type for a Future/FutureWritable receiver.
    fn cm_future_payload(&self, receiver_type_id: TypeId) -> CmFuturePayload {
        // Receiver is &Future<T> or &FutureWritable<T> — unwrap the reference
        let inner_type_id = match self.type_table.get(receiver_type_id) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => receiver_type_id,
        };
        if let ResolvedType::GenericResource { type_args, .. } = self.type_table.get(inner_type_id)
            && !type_args.is_empty()
        {
            return self.classify_future_payload(type_args[0]);
        }
        CmFuturePayload::Trailers
    }

    /// Classify a future's type argument into the CM future payload category.
    ///
    /// - Primitive scalar → `Scalar(s)`
    /// - `Result<(), E>` (unit Ok type) → `Transmission` (HTTP transmission result)
    /// - Anything else → `Trailers` (HTTP trailers pattern)
    fn classify_future_payload(&self, type_arg: TypeId) -> CmFuturePayload {
        match self.type_table.get(type_arg) {
            ResolvedType::Primitive(prim) => {
                if let Some(scalar) = Self::primitive_to_cm_scalar(prim) {
                    return CmFuturePayload::Scalar(scalar);
                }
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } if name == "Result" && type_args.len() >= 2 => {
                if matches!(self.type_table.get(type_args[0]), ResolvedType::Unit) {
                    // Determine ErrorCode source from the error type's package
                    let source = self.error_code_source(type_args[1]);
                    return CmFuturePayload::Transmission(source);
                }
            }
            _ => {}
        }
        CmFuturePayload::Trailers
    }

    /// Determine the WASI package source for an `ErrorCode` type.
    ///
    /// Extracts the package name from the type's `ModuleSource::Wasi { interface }`
    /// (e.g. `"http/types.wado"` → `"http"`). Falls back to `"cli"` if the error
    /// type is not a WASI-defined enum or variant.
    fn error_code_source(&self, error_type_id: TypeId) -> String {
        let module_source = match self.type_table.get(error_type_id) {
            ResolvedType::Enum { module_source, .. }
            | ResolvedType::Variant { module_source, .. } => module_source,
            _ => return "cli".to_string(),
        };
        if let ModuleSource::Wasi { interface } = module_source {
            // interface format is "{package}/..." (e.g., "http/types.wado")
            interface.split('/').next().unwrap_or("cli").to_string()
        } else {
            "cli".to_string()
        }
    }

    /// Dispatch canonical resource methods based on `#[cm("...")]` attribute.
    /// Returns `Some(WirInstr)` if the method has a canonical name and was handled.
    ///
    /// Most CM methods are now handled by synthesis (rewritten to `CmRawCall` or
    /// internal binding calls). Only future operations remain here because they
    /// require payload-parameterized canonical imports.
    pub(super) fn try_translate_canonical_method(
        &mut self,
        receiver: &NirExpr,
        func: &FunctionRef,
        args: &[CallArg],
        result_type_id: TypeId,
    ) -> Option<WirInstr> {
        let cm_name = func.method_info.clone()?.cm_name.as_ref()?.clone();
        let handle = self.translate_expr(receiver);
        match cm_name.as_str() {
            // Future operations: payload-parameterized canonical imports
            "future-read" => {
                let payload = self.cm_future_payload(receiver.type_id);
                Some(self.emit_future_read(handle, result_type_id, payload))
            }
            "future-write" => {
                let value_expr = &args[0].expr;
                let value_type_id = value_expr.type_id;
                let payload = self.cm_future_payload(receiver.type_id);
                // Variant-shaped futures (used for trailers): pattern-match on
                // TIR because general variant→CM lowering is not yet implemented.
                if let Some(resource_expr) = match_ok_some_resource(value_expr) {
                    let resource_handle = self.translate_expr(resource_expr);
                    return Some(self.emit_future_write_ok_some_resource(handle, resource_handle));
                }
                if match_ok_none(value_expr) {
                    return Some(self.emit_future_write_ok_none(handle));
                }
                // Scalar primitives are evaluated and lowered generically.
                let value_arg = self.translate_expr(value_expr);
                Some(self.emit_future_write(handle, value_arg, value_type_id, payload))
            }
            "future-cancel-read" => {
                let payload = self.cm_future_payload(receiver.type_id);
                Some(self.emit_drop_handle(CanonicalIntrinsic::FutureCancelRead(payload), handle))
            }
            "future-cancel-write" => {
                let payload = self.cm_future_payload(receiver.type_id);
                Some(self.emit_drop_handle(CanonicalIntrinsic::FutureCancelWrite(payload), handle))
            }
            "future-drop-readable" => {
                let payload = self.cm_future_payload(receiver.type_id);
                Some(self.emit_drop_handle(CanonicalIntrinsic::FutureDropReadable(payload), handle))
            }
            "future-drop-writable" => {
                let payload = self.cm_future_payload(receiver.type_id);
                Some(self.emit_drop_handle(CanonicalIntrinsic::FutureDropWritable(payload), handle))
            }

            other => panic!("[WIR] unhandled canonical method: {other}"),
        }
    }

    /// Dispatch canonical resource static methods (e.g., `Stream::new`, `Future::new`).
    /// Returns `Some(WirInstr)` if the canonical name was handled.
    ///
    /// Most static CM methods are now handled by synthesis. Only stream/future-new
    /// remain here because they require i64→tuple splitting with proper GC type casting.
    pub(super) fn try_translate_canonical_static_method(
        &mut self,
        canonical: &str,
        func: &FunctionRef,
        _args: &[CallArg],
        result_type_id: TypeId,
    ) -> Option<WirInstr> {
        match canonical {
            "stream-new" => Some(self.emit_stream_or_future_new(
                false,
                CmFuturePayload::Trailers,
                result_type_id,
            )),
            "future-new" => {
                let payload = self.cm_future_payload_from_monomorph(func);
                Some(self.emit_stream_or_future_new(true, payload, result_type_id))
            }
            _ => None,
        }
    }

    /// Emit a canonical drop call: `{canonical}(handle)`.
    ///
    /// Used for all CM resource drop operations:
    /// `stream-drop-readable`, `stream-drop-writable`,
    /// `future-drop-readable`, `future-drop-writable`,
    /// `waitable-set-drop`, `subtask-drop`, `error-context-drop`.
    fn emit_drop_handle(&mut self, intrinsic: CanonicalIntrinsic, handle: WirInstr) -> WirInstr {
        let func_id = self
            .ctx
            .ensure_canonical(intrinsic, vec![WirType::I32], vec![]);
        WirInstr::Call {
            func_id,
            args: vec![handle],
        }
    }

    /// Emit `stream-new()` or `future-new()` → split i64 into [`rx_i32`, `tx_i32`] tuple.
    ///
    /// Captures the two i32s into freshly declared locals, then builds a
    /// plain `StructNew` reading those locals. When the result is
    /// destructured at the call site (the typical case:
    /// `let [rx, tx] = Future::<T>::new();`),
    /// `wir_optimize::elide_struct::elide_multi_field_struct_locals`
    /// recognises `LocalSet __tmp = StructNew { fields: [LocalGet, LocalGet] }`
    /// followed by `StructGet × N` and substitutes each field access
    /// directly — eliminating the heap allocation. When only one of
    /// rx/tx is read, the same pass still fires and drops the unused
    /// `LocalGet`.
    fn emit_stream_or_future_new(
        &mut self,
        is_future: bool,
        payload: CmFuturePayload,
        result_type_id: TypeId,
    ) -> WirInstr {
        let intrinsic = if is_future {
            CanonicalIntrinsic::FutureNew(payload)
        } else {
            CanonicalIntrinsic::StreamNew(CmStreamPayload::U8)
        };
        let func_id = self
            .ctx
            .ensure_canonical(intrinsic, vec![], vec![WirType::I64]);

        // Resolve the result tuple type for StructNew.
        let wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, result_type_id);
        let type_id = match wir_type {
            WirType::Ref { type_id, .. } => type_id,
            _ => panic!(
                "[WIR] emit_stream_or_future_new expected Ref result type, got {wir_type:?} (result_type_id={result_type_id:?})"
            ),
        };

        // Declare a temp i64 local, call import → i64, then push low/high
        // i32 onto the stack as the multi-value producer for the
        // MultiValueLocalBind below.
        self.local_counter += 1;
        let n = self.local_counter;
        let temp = format!("__pair_temp_{n}");
        let lo = format!("__mv_lo_{n}");
        let hi = format!("__mv_hi_{n}");
        let declare_temp = WirInstr::DeclareLocal {
            name: temp.clone(),
            ty: WirType::I64,
        };
        let call = WirInstr::Call {
            func_id,
            args: vec![],
        };
        let set_temp = WirInstr::LocalSet {
            name: temp.clone(),
            value: Box::new(call),
        };
        let get_low = WirInstr::I32WrapI64(Box::new(WirInstr::LocalGet {
            name: temp.clone(),
            result_ty: WirType::I64,
        }));
        let get_high = WirInstr::I32WrapI64(Box::new(WirInstr::I64ShrU(
            Box::new(WirInstr::LocalGet {
                name: temp,
                result_ty: WirType::I64,
            }),
            Box::new(WirInstr::I64Const(32)),
        )));

        WirInstr::Seq(vec![
            declare_temp,
            set_temp,
            WirInstr::DeclareLocal {
                name: lo.clone(),
                ty: WirType::I32,
            },
            WirInstr::DeclareLocal {
                name: hi.clone(),
                ty: WirType::I32,
            },
            WirInstr::MultiValueLocalBind {
                instr: Box::new(WirInstr::Seq(vec![get_low, get_high])),
                locals: vec![Some(lo.clone()), Some(hi.clone())],
            },
            WirInstr::StructNew {
                type_id,
                fields: vec![
                    WirInstr::LocalGet {
                        name: lo,
                        result_ty: WirType::I32,
                    },
                    WirInstr::LocalGet {
                        name: hi,
                        result_ty: WirType::I32,
                    },
                ],
            },
        ])
    }

    /// Emit `future-read(handle)` → `Option<T>`.
    ///
    /// Currently only supports `T` = `Result<Option<Trailers>, ErrorCode>` (8-byte CM payload).
    /// The synthesis:
    /// 1. Allocate 8 bytes via realloc
    /// 2. Call `future-read(handle, ptr)` → `raw_result`
    /// 3. If BLOCKED (`0xFFFF_FFFF`), wait via waitable-set, re-read result
    /// 4. Extract status = result & 0xF
    /// 5. If COMPLETED (status 0): return `Option::Some(value)` — value read from ptr
    /// 6. If DROPPED (status 1): return `Option::None`
    /// 7. Free linear memory
    ///
    /// TODO: implement general CM lifting for arbitrary T values.
    /// Currently hardcoded for the `Ok(None)` pattern (all-zeros payload = 8 bytes).
    fn emit_future_read(
        &mut self,
        handle: WirInstr,
        result_type_id: TypeId,
        payload: CmFuturePayload,
    ) -> WirInstr {
        let Some(realloc_id) = self.ctx.func_map.get("builtin/realloc").cloned() else {
            panic!("[WIR] emit_future_read: builtin/realloc not registered");
        };
        // future-read: (handle: i32, ptr: i32) -> i32
        let future_read_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::FutureRead(payload),
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );
        let ws_new_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableSetNew,
            vec![],
            vec![WirType::I32],
        );
        let w_join_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableJoin,
            vec![WirType::I32, WirType::I32],
            vec![],
        );
        let ws_wait_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableSetWait,
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );

        self.local_counter += 1;
        let suffix = self.local_counter;
        let handle_name = format!("__fr_handle_{suffix}");
        let ptr_name = format!("__fr_ptr_{suffix}");
        let result_name = format!("__fr_result_{suffix}");
        let evt_ptr_name = format!("__fr_evtptr_{suffix}");

        let mut instrs = vec![];

        // Declare locals
        for (name, ty) in [
            (&handle_name, WirType::I32),
            (&ptr_name, WirType::I32),
            (&result_name, WirType::I32),
        ] {
            instrs.push(WirInstr::DeclareLocal {
                name: name.clone(),
                ty,
            });
        }

        // Save handle
        instrs.push(WirInstr::LocalSet {
            name: handle_name.clone(),
            value: Box::new(handle),
        });

        // Allocate buffer for the CM payload.
        // Use 40 bytes (same as future-write) to cover the worst-case CM layout.
        const BUF_SIZE: i32 = 40;
        const BUF_ALIGN: i32 = 8;

        instrs.push(WirInstr::LocalSet {
            name: ptr_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: realloc_id.clone(),
                args: vec![
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(BUF_ALIGN),
                    WirInstr::I32Const(BUF_SIZE),
                ],
            }),
        });

        // result = future-read(handle, ptr)
        instrs.push(WirInstr::LocalSet {
            name: result_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: future_read_id,
                args: vec![
                    WirInstr::LocalGet {
                        name: handle_name.clone(),
                        result_ty: WirType::I32,
                    },
                    WirInstr::LocalGet {
                        name: ptr_name.clone(),
                        result_ty: WirType::I32,
                    },
                ],
            }),
        });

        // canon future.read returns pack_copy_result:
        //   0xFFFF_FFFF (BLOCKED): future not ready, wait using the FUTURE handle
        //   (count << 4) | status:
        //     status 0 = COMPLETED: payload written to buffer → Some(lifted_value)
        //     status 1 = DROPPED: writer dropped → None
        //     status 2 = CANCELLED
        //   For futures, count is always 0, so COMPLETED = 0, DROPPED = 1.
        let ws_drop_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableSetDrop,
            vec![WirType::I32],
            vec![],
        );
        // Case 1: BLOCKED (0xFFFF_FFFF) → wait on future handle
        instrs.push(WirInstr::If {
            condition: Box::new(WirInstr::I32Eq(
                Box::new(WirInstr::LocalGet {
                    name: result_name.clone(),
                    result_ty: WirType::I32,
                }),
                Box::new(WirInstr::I32Const(-1)),
            )),
            result: None,
            then_body: {
                let evt = evt_ptr_name;
                let ws_name = format!("__fr_ws_{suffix}");
                vec![
                    WirInstr::DeclareLocal {
                        name: ws_name.clone(),
                        ty: WirType::I32,
                    },
                    WirInstr::DeclareLocal {
                        name: evt.clone(),
                        ty: WirType::I32,
                    },
                    // ws = waitable_set_new()
                    WirInstr::LocalSet {
                        name: ws_name.clone(),
                        value: Box::new(WirInstr::Call {
                            func_id: ws_new_id,
                            args: vec![],
                        }),
                    },
                    // waitable_join(FUTURE_HANDLE, ws) — wait on the future itself
                    WirInstr::Call {
                        func_id: w_join_id.clone(),
                        args: vec![
                            WirInstr::LocalGet {
                                name: handle_name.clone(),
                                result_ty: WirType::I32,
                            },
                            WirInstr::LocalGet {
                                name: ws_name.clone(),
                                result_ty: WirType::I32,
                            },
                        ],
                    },
                    // evt_ptr = realloc(0, 0, 4, 8)
                    WirInstr::LocalSet {
                        name: evt.clone(),
                        value: Box::new(WirInstr::Call {
                            func_id: realloc_id.clone(),
                            args: vec![
                                WirInstr::I32Const(0),
                                WirInstr::I32Const(0),
                                WirInstr::I32Const(4),
                                WirInstr::I32Const(8),
                            ],
                        }),
                    },
                    // waitable_set_wait(ws, evt_ptr)
                    WirInstr::Drop(Box::new(WirInstr::Call {
                        func_id: ws_wait_id,
                        args: vec![
                            WirInstr::LocalGet {
                                name: ws_name.clone(),
                                result_ty: WirType::I32,
                            },
                            WirInstr::LocalGet {
                                name: evt.clone(),
                                result_ty: WirType::I32,
                            },
                        ],
                    })),
                    // Free event buffer
                    WirInstr::Drop(Box::new(WirInstr::Call {
                        func_id: realloc_id.clone(),
                        args: vec![
                            WirInstr::LocalGet {
                                name: evt,
                                result_ty: WirType::I32,
                            },
                            WirInstr::I32Const(8),
                            WirInstr::I32Const(4),
                            WirInstr::I32Const(0),
                        ],
                    })),
                    // Unjoin future handle from waitable set before dropping it.
                    // waitable.join(handle, 0) removes the child relationship.
                    WirInstr::Call {
                        func_id: w_join_id,
                        args: vec![
                            WirInstr::LocalGet {
                                name: handle_name,
                                result_ty: WirType::I32,
                            },
                            WirInstr::I32Const(0),
                        ],
                    },
                    // Drop waitable set
                    WirInstr::Call {
                        func_id: ws_drop_id,
                        args: vec![WirInstr::LocalGet {
                            name: ws_name,
                            result_ty: WirType::I32,
                        }],
                    },
                    // After wait completes, the data transfer is done and the
                    // future handle is consumed.  Do NOT retry future-read;
                    // just mark the result as COMPLETED (0) so the payload
                    // lifter reads from the buffer.
                    WirInstr::LocalSet {
                        name: result_name.clone(),
                        value: Box::new(WirInstr::I32Const(0)),
                    },
                ]
            },
            else_body: None,
        });

        // pack_copy_result status:
        //   status 0 = COMPLETED → payload written to buffer → Some(lifted_value)
        //   status 1 = DROPPED → writer dropped → None
        //   After BLOCKED wait, result is set to 0 (COMPLETED).
        let option_wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, result_type_id);

        // Build the Some payload by lifting T from linear memory.
        let some_payload = self.lift_future_read_payload(result_type_id, &ptr_name);
        let some_variant =
            self.build_variant_case_wir(result_type_id, 0, "Some", Some(some_payload));
        let none_variant = self.build_variant_case_wir(result_type_id, 1, "None", None);

        self.local_counter += 1;
        let option_result_name = format!("__fr_opt_{}", self.local_counter);
        instrs.push(WirInstr::DeclareLocal {
            name: option_result_name.clone(),
            ty: option_wir_type.clone(),
        });

        // option_result = if (result & 0xF) == 0 { Some(lifted_value) } else { None }
        instrs.push(WirInstr::LocalSet {
            name: option_result_name.clone(),
            value: Box::new(WirInstr::If {
                condition: Box::new(WirInstr::I32Eq(
                    Box::new(WirInstr::I32And(
                        Box::new(WirInstr::LocalGet {
                            name: result_name,
                            result_ty: WirType::I32,
                        }),
                        Box::new(WirInstr::I32Const(0xF)),
                    )),
                    Box::new(WirInstr::I32Const(0)),
                )),
                result: Some(option_wir_type.clone()),
                then_body: vec![some_variant],
                else_body: Some(vec![none_variant]),
            }),
        });

        // Free payload buffer (after reading from it in the if-then branch)
        instrs.push(WirInstr::Drop(Box::new(WirInstr::Call {
            func_id: realloc_id,
            args: vec![
                WirInstr::LocalGet {
                    name: ptr_name.clone(),
                    result_ty: WirType::I32,
                },
                WirInstr::I32Const(BUF_SIZE),
                WirInstr::I32Const(BUF_ALIGN),
                WirInstr::I32Const(0),
            ],
        })));

        instrs.push(WirInstr::LocalGet {
            name: option_result_name,
            result_ty: option_wir_type,
        });

        WirInstr::Seq(instrs)
    }

    /// Lift the T value from the CM payload buffer for `Future::read`.
    ///
    /// `option_type_id` is `Option<T>`. We extract T and lift it from memory at `ptr_name`.
    ///
    /// Currently supports:
    /// - `T = Result<Option<own<resource>>, ErrorCode>` — the trailers pattern
    ///   CM layout: [`result_disc:i32`, `option_disc:i32`, handle:i32]
    ///   Ok(None) = [0, 0, 0], Ok(Some(h)) = [0, 1, h]
    ///   Err(...) → trap (not yet implemented)
    fn lift_future_read_payload(&mut self, option_type_id: TypeId, ptr_name: &str) -> WirInstr {
        // option_type_id = Option<T>; extract T
        let Some(inner_t) = self.type_table.as_option(option_type_id) else {
            panic!(
                "[WIR] lift_future_read_payload: expected Option<T>, got type_id={option_type_id:?}"
            );
        };

        // Check if T is a scalar numeric type (i32, i64, f32, f64, etc.)
        if let Some(load_instr) = self.lift_cm_scalar(inner_t, ptr_name) {
            return load_instr;
        }

        // Check if T is Result<Ok, Err>
        if let ResolvedType::GenericInstance {
            name, type_args, ..
        } = self.type_table.get(inner_t)
            && name == "Result"
            && type_args.len() == 2
        {
            let ok_type_id = type_args[0];

            // Check if Ok type is Option<R> (the trailers pattern)
            if let Some(inner_resource_type_id) = self.type_table.as_option(ok_type_id) {
                return self.lift_result_option_resource(
                    inner_t,
                    ok_type_id,
                    inner_resource_type_id,
                    ptr_name,
                );
            }

            // Check if Ok type is () (the transmission pattern: Result<(), E>)
            if matches!(self.type_table.get(ok_type_id), ResolvedType::Unit) {
                return self.lift_result_unit(inner_t, ptr_name);
            }
        }

        panic!("[WIR] lift_future_read_payload: unsupported payload type (inner_t={inner_t:?})");
    }

    /// Lift a scalar numeric value from CM linear memory at `ptr_name + 0`.
    ///
    /// Returns `Some(WirInstr)` for CM number types (i8–i64, u8–u64, f32, f64,
    /// bool, char), `None` otherwise.
    fn lift_cm_scalar(&self, type_id: TypeId, ptr_name: &str) -> Option<WirInstr> {
        let ResolvedType::Primitive(prim) = self.type_table.get(type_id) else {
            return None;
        };
        let ptr = || WirInstr::LocalGet {
            name: ptr_name.to_string(),
            result_ty: WirType::I32,
        };
        let instr = match prim {
            PrimitiveType::I8 | PrimitiveType::U8 | PrimitiveType::Bool => WirInstr::I32Load8U {
                offset: 0,
                align: 0,
                addr: Box::new(ptr()),
            },
            PrimitiveType::I16 | PrimitiveType::U16 => WirInstr::I32Load {
                offset: 0,
                align: 1,
                addr: Box::new(ptr()),
            },
            PrimitiveType::I32 | PrimitiveType::U32 | PrimitiveType::Char => WirInstr::I32Load {
                offset: 0,
                align: 2,
                addr: Box::new(ptr()),
            },
            PrimitiveType::I64 | PrimitiveType::U64 => WirInstr::I64Load {
                offset: 0,
                align: 3,
                addr: Box::new(ptr()),
            },
            _ => return None,
        };
        Some(instr)
    }

    /// Lift `Result<Option<R>, E>` from CM linear memory.
    ///
    /// CM layout at ptr:
    /// - offset 0: result discriminant (i32) — 0=Ok, 1=Err
    /// - offset 4: option discriminant (i32) — 0=None, 1=Some (when Ok)
    /// - offset 8: resource handle (i32) (when Ok+Some)
    ///
    /// Returns the constructed `Result` variant value.
    fn lift_result_option_resource(
        &mut self,
        result_type_id: TypeId,
        option_type_id: TypeId,
        _resource_type_id: TypeId,
        ptr_name: &str,
    ) -> WirInstr {
        let ptr = || WirInstr::LocalGet {
            name: ptr_name.to_string(),
            result_ty: WirType::I32,
        };

        // Read result discriminant at offset 0
        let result_disc = WirInstr::I32Load {
            offset: 0,
            align: 2,
            addr: Box::new(ptr()),
        };

        // Build Ok branch: lift Option<R> from offset 4
        let option_disc = WirInstr::I32Load {
            offset: 4,
            align: 2,
            addr: Box::new(ptr()),
        };

        // Build Option::None (for the resource)
        let inner_none = self.build_variant_case_wir(option_type_id, 1, "None", None);

        // Build Option::Some(handle) — read handle at offset 8
        let handle_val = WirInstr::I32Load {
            offset: 8,
            align: 2,
            addr: Box::new(ptr()),
        };
        let inner_some = self.build_variant_case_wir(option_type_id, 0, "Some", Some(handle_val));

        // Option<R> = if option_disc == 0 { None } else { Some(handle) }
        let option_wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, option_type_id);
        let lifted_option = WirInstr::If {
            condition: Box::new(WirInstr::I32Eqz(Box::new(option_disc))),
            result: Some(option_wir_type),
            then_body: vec![inner_none],
            else_body: Some(vec![inner_some]),
        };

        // Result::Ok(lifted_option)
        let result_ok = self.build_variant_case_wir(result_type_id, 0, "Ok", Some(lifted_option));

        // Result::Err — trap for now (ErrorCode lifting not yet implemented)
        let result_err = WirInstr::Unreachable;

        // Result = if result_disc == 0 { Ok(...) } else { Err (trap) }
        let result_wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, result_type_id);
        WirInstr::If {
            condition: Box::new(WirInstr::I32Eqz(Box::new(result_disc))),
            result: Some(result_wir_type),
            then_body: vec![result_ok],
            else_body: Some(vec![result_err]),
        }
    }

    /// Lift `Result<(), E>` from CM linear memory.
    ///
    /// CM layout at ptr:
    /// - offset 0: result discriminant (i32) — 0=Ok, 1=Err
    ///
    /// Returns `Result::Ok(())` when discriminant is 0, traps on Err.
    fn lift_result_unit(&mut self, result_type_id: TypeId, ptr_name: &str) -> WirInstr {
        let ptr = || WirInstr::LocalGet {
            name: ptr_name.to_string(),
            result_ty: WirType::I32,
        };

        let result_disc = WirInstr::I32Load {
            offset: 0,
            align: 2,
            addr: Box::new(ptr()),
        };

        let result_ok = self.build_variant_case_wir(result_type_id, 0, "Ok", None);

        // Err branch: trap (ErrorCode lifting not yet implemented for this pattern)
        let result_err = WirInstr::Unreachable;

        let result_wir_type = self
            .ctx
            .type_id_to_wir_type(self.type_table, result_type_id);
        WirInstr::If {
            condition: Box::new(WirInstr::I32Eqz(Box::new(result_disc))),
            result: Some(result_wir_type),
            then_body: vec![result_ok],
            else_body: Some(vec![result_err]),
        }
    }

    /// Emit WIR for `FutureWritable<T>::write(value)` for scalar value types.
    ///
    /// The variant-shaped futures (`Result<Option<R>, E>::Ok(None)` and
    /// `::Ok(Some(<resource>))`) are dispatched at the call site in
    /// `try_translate_canonical_method` because they require TIR-level
    /// pattern matching. Anything else panics — silently writing `Ok(None)`
    /// for an unrecognized shape produces wrong runtime behavior with no
    /// diagnostic.
    fn emit_future_write(
        &mut self,
        handle: WirInstr,
        value: WirInstr,
        value_type_id: TypeId,
        payload: CmFuturePayload,
    ) -> WirInstr {
        if let ResolvedType::Primitive(
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64
            | PrimitiveType::Bool
            | PrimitiveType::Char,
        ) = self.type_table.get(value_type_id)
        {
            return self.emit_future_write_scalar(handle, value, value_type_id, payload);
        }

        panic!(
            "[WIR] FutureWritable::write: unsupported value of type_id={value_type_id:?}. \
             Currently supported: scalar primitives (i8..i64, u8..u64, bool, char), \
             Result::Ok(Option::None), Result::Ok(Option::Some(<resource>)). \
             General variant→CM lowering is not yet implemented."
        );
    }

    /// Emit WIR to write a scalar numeric value into a `FutureWritable` handle.
    ///
    /// Allocates a CM buffer, stores the value, and calls `future-write` with
    /// `async` canonical option. If the operation returns BLOCKED, the buffer is
    /// intentionally kept alive — the reader will copy the data and complete
    /// the write on the same thread. The buffer is freed only on immediate
    /// completion (non-BLOCKED).
    fn emit_future_write_scalar(
        &mut self,
        handle: WirInstr,
        value: WirInstr,
        value_type_id: TypeId,
        payload: CmFuturePayload,
    ) -> WirInstr {
        let Some(realloc_id) = self.ctx.func_map.get("builtin/realloc").cloned() else {
            panic!("[WIR] emit_future_write_scalar: builtin/realloc not registered");
        };
        let future_write_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::FutureWrite(payload),
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );

        self.local_counter += 1;
        let suffix = self.local_counter;
        let ptr_name = format!("__fw_write_ptr_{suffix}");
        let result_name = format!("__fw_result_{suffix}");

        // Determine buffer size/alignment from the primitive type
        let ResolvedType::Primitive(prim) = self.type_table.get(value_type_id) else {
            panic!(
                "[WIR] emit_future_write_scalar: expected primitive type, got type_id={value_type_id:?}"
            );
        };
        let (buf_size, buf_align): (i32, i32) = match prim {
            PrimitiveType::I8 | PrimitiveType::U8 | PrimitiveType::Bool => (1, 1),
            PrimitiveType::I16 | PrimitiveType::U16 => (2, 2),
            PrimitiveType::I32 | PrimitiveType::U32 | PrimitiveType::Char => (4, 4),
            PrimitiveType::I64 | PrimitiveType::U64 => (8, 8),
            other => panic!("[WIR] emit_future_write_scalar: unsupported primitive {other:?}"),
        };

        let mut seq = vec![];

        // Declare locals
        for (name, ty) in [(&ptr_name, WirType::I32), (&result_name, WirType::I32)] {
            seq.push(WirInstr::DeclareLocal {
                name: name.clone(),
                ty,
            });
        }

        // Allocate buffer
        seq.push(WirInstr::LocalSet {
            name: ptr_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: realloc_id,
                args: vec![
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(buf_align),
                    WirInstr::I32Const(buf_size),
                ],
            }),
        });

        // Store value at ptr
        let ptr = || WirInstr::LocalGet {
            name: ptr_name.clone(),
            result_ty: WirType::I32,
        };
        let store_instr = match prim {
            PrimitiveType::I8 | PrimitiveType::U8 | PrimitiveType::Bool => WirInstr::I32Store8 {
                offset: 0,
                align: 0,
                addr: Box::new(ptr()),
                value: Box::new(value),
            },
            PrimitiveType::I16 | PrimitiveType::U16 => WirInstr::I32Store16 {
                offset: 0,
                align: 1,
                addr: Box::new(ptr()),
                value: Box::new(value),
            },
            PrimitiveType::I32 | PrimitiveType::U32 | PrimitiveType::Char => WirInstr::I32Store {
                offset: 0,
                align: 2,
                addr: Box::new(ptr()),
                value: Box::new(value),
            },
            PrimitiveType::I64 | PrimitiveType::U64 => WirInstr::I64Store {
                offset: 0,
                align: 3,
                addr: Box::new(ptr()),
                value: Box::new(value),
            },
            other => {
                panic!("[WIR] emit_future_write_scalar: unsupported store primitive {other:?}")
            }
        };
        seq.push(store_instr);

        // result = future_write(handle, ptr)
        // With `async` canonical option, BLOCKED means the data is pending and
        // the reader will pick it up. We drop the result — the buffer stays alive
        // until the reader copies the data on the same thread.
        seq.push(WirInstr::Drop(Box::new(WirInstr::Call {
            func_id: future_write_id,
            args: vec![
                handle,
                WirInstr::LocalGet {
                    name: ptr_name,
                    result_ty: WirType::I32,
                },
            ],
        })));

        // NOTE: We intentionally do NOT free the buffer here. When the canonical
        // `async` option returns BLOCKED, the CM runtime holds a reference to the
        // buffer. The reader will copy from it before this thread continues.
        // The buffer is leaked but this is acceptable for small scalar payloads.

        WirInstr::Seq(seq)
    }

    /// Emit the BLOCKED handling pattern for `future-write`.
    ///
    /// If `result == -1` (BLOCKED), creates a waitable-set, joins the handle,
    /// waits for the reader to consume the value, then frees the event buffer.
    fn emit_future_write_blocked_wait(
        &self,
        handle_name: &str,
        result_name: &str,
        evt_name: &str,
        realloc_id: &WirFuncId,
        ws_new_id: &WirFuncId,
        w_join_id: &WirFuncId,
        ws_wait_id: &WirFuncId,
    ) -> WirInstr {
        WirInstr::If {
            condition: Box::new(WirInstr::I32Eq(
                Box::new(WirInstr::LocalGet {
                    name: result_name.to_string(),
                    result_ty: WirType::I32,
                }),
                Box::new(WirInstr::I32Const(-1)),
            )),
            result: None,
            then_body: vec![
                WirInstr::DeclareLocal {
                    name: evt_name.to_string(),
                    ty: WirType::I32,
                },
                WirInstr::LocalSet {
                    name: result_name.to_string(),
                    value: Box::new(WirInstr::Call {
                        func_id: ws_new_id.clone(),
                        args: vec![],
                    }),
                },
                WirInstr::Call {
                    func_id: w_join_id.clone(),
                    args: vec![
                        WirInstr::LocalGet {
                            name: handle_name.to_string(),
                            result_ty: WirType::I32,
                        },
                        WirInstr::LocalGet {
                            name: result_name.to_string(),
                            result_ty: WirType::I32,
                        },
                    ],
                },
                WirInstr::LocalSet {
                    name: evt_name.to_string(),
                    value: Box::new(WirInstr::Call {
                        func_id: realloc_id.clone(),
                        args: vec![
                            WirInstr::I32Const(0),
                            WirInstr::I32Const(0),
                            WirInstr::I32Const(4),
                            WirInstr::I32Const(8),
                        ],
                    }),
                },
                WirInstr::Drop(Box::new(WirInstr::Call {
                    func_id: ws_wait_id.clone(),
                    args: vec![
                        WirInstr::LocalGet {
                            name: result_name.to_string(),
                            result_ty: WirType::I32,
                        },
                        WirInstr::LocalGet {
                            name: evt_name.to_string(),
                            result_ty: WirType::I32,
                        },
                    ],
                })),
                WirInstr::Drop(Box::new(WirInstr::Call {
                    func_id: realloc_id.clone(),
                    args: vec![
                        WirInstr::LocalGet {
                            name: evt_name.to_string(),
                            result_ty: WirType::I32,
                        },
                        WirInstr::I32Const(8),
                        WirInstr::I32Const(4),
                        WirInstr::I32Const(0),
                    ],
                })),
            ],
            else_body: None,
        }
    }

    /// Emit WIR to write `Ok(None)` (8 zero bytes) into a `FutureWritable` handle,
    /// then free the temporary buffer.
    ///
    /// Hardcoded encoding for `Result<Option<Trailers>, ErrorCode>::Ok(null)`:
    ///   - 4 bytes at offset 0: Ok discriminant (0)
    ///   - 4 bytes at offset 4: None discriminant (0)
    ///
    /// If `future-write` returns BLOCKED (`0xFFFF_FFFF`), waits via waitable-set
    /// for the reader to consume the value before continuing.
    fn emit_future_write_ok_none(&mut self, handle: WirInstr) -> WirInstr {
        let Some(realloc_id) = self.ctx.func_map.get("builtin/realloc").cloned() else {
            panic!("[WIR] emit_future_write_ok_none: builtin/realloc not registered");
        };
        let future_write_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::FutureWrite(CmFuturePayload::Trailers),
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );
        let ws_new_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableSetNew,
            vec![],
            vec![WirType::I32],
        );
        let w_join_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableJoin,
            vec![WirType::I32, WirType::I32],
            vec![],
        );
        let ws_wait_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableSetWait,
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );

        self.local_counter += 1;
        let suffix = self.local_counter;
        let ptr_name = format!("__fw_write_ptr_{suffix}");
        let handle_name = format!("__fw_handle_{suffix}");
        let result_name = format!("__fw_result_{suffix}");
        let evt_name = format!("__fw_evt_{suffix}");

        // The buffer must be large enough for the full CM layout of
        // result<option<own<trailers>>, error-code>. ErrorCode is a large variant
        // whose biggest cases contain option<string> (12 bytes) or dns-error-payload
        // (option<string> + option<u16> = 16 bytes). We allocate 40 bytes to cover
        // the worst case and zero-initialize the entire buffer, since Ok(None) is
        // represented as all-zeros.
        const BUF_SIZE: i32 = 40;
        const BUF_ALIGN: i32 = 8;

        let mut seq = vec![];

        // Declare locals
        for (name, ty) in [
            (&ptr_name, WirType::I32),
            (&handle_name, WirType::I32),
            (&result_name, WirType::I32),
        ] {
            seq.push(WirInstr::DeclareLocal {
                name: name.clone(),
                ty,
            });
        }

        // Save handle
        seq.push(WirInstr::LocalSet {
            name: handle_name.clone(),
            value: Box::new(handle),
        });

        // Allocate buffer
        seq.push(WirInstr::LocalSet {
            name: ptr_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: realloc_id.clone(),
                args: vec![
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(BUF_ALIGN),
                    WirInstr::I32Const(BUF_SIZE),
                ],
            }),
        });

        // Zero-initialize the entire buffer using i64 stores (8 bytes each).
        for i in 0..(BUF_SIZE / 8) {
            seq.push(WirInstr::I64Store {
                offset: u64::from((i * 8).cast_unsigned()),
                align: 3,
                addr: Box::new(WirInstr::LocalGet {
                    name: ptr_name.clone(),
                    result_ty: WirType::I32,
                }),
                value: Box::new(WirInstr::I64Const(0)),
            });
        }

        // result = future_write(handle, ptr)
        seq.push(WirInstr::LocalSet {
            name: result_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: future_write_id,
                args: vec![
                    WirInstr::LocalGet {
                        name: handle_name.clone(),
                        result_ty: WirType::I32,
                    },
                    WirInstr::LocalGet {
                        name: ptr_name.clone(),
                        result_ty: WirType::I32,
                    },
                ],
            }),
        });

        // If BLOCKED (0xFFFF_FFFF), wait via waitable-set for the reader to consume
        seq.push(self.emit_future_write_blocked_wait(
            &handle_name,
            &result_name,
            &evt_name,
            &realloc_id,
            &ws_new_id,
            &w_join_id,
            &ws_wait_id,
        ));

        // Free the payload buffer after future_write completes.
        seq.push(WirInstr::Drop(Box::new(WirInstr::Call {
            func_id: realloc_id,
            args: vec![
                WirInstr::LocalGet {
                    name: ptr_name,
                    result_ty: WirType::I32,
                },
                WirInstr::I32Const(BUF_SIZE),
                WirInstr::I32Const(BUF_ALIGN),
                WirInstr::I32Const(0),
            ],
        })));

        WirInstr::Seq(seq)
    }

    /// Emit WIR for `FutureWritable::write(Result::Ok(Option::Some(handle)))`,
    /// the trailer-shape future used to deliver a Fields resource.
    ///
    /// CM layout written to the buffer for `result<option<own<R>>, error-code>`:
    /// - offset 0:  result disc (u8) = 0 (Ok)
    /// - offsets 1-7: padding (payload align = 8 because error-code contains
    ///   cases with `option<u64>` payload — overall align is 8)
    /// - offset 8:  option disc (u8) = 1 (Some)
    /// - offsets 9-11: padding
    /// - offset 12: own<R> resource handle (u32)
    fn emit_future_write_ok_some_resource(
        &mut self,
        handle: WirInstr,
        resource_handle: WirInstr,
    ) -> WirInstr {
        let Some(realloc_id) = self.ctx.func_map.get("builtin/realloc").cloned() else {
            panic!("[WIR] emit_future_write_ok_some_resource: builtin/realloc not registered");
        };
        let future_write_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::FutureWrite(CmFuturePayload::Trailers),
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );
        let ws_new_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableSetNew,
            vec![],
            vec![WirType::I32],
        );
        let w_join_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableJoin,
            vec![WirType::I32, WirType::I32],
            vec![],
        );
        let ws_wait_id = self.ctx.ensure_canonical(
            CanonicalIntrinsic::WaitableSetWait,
            vec![WirType::I32, WirType::I32],
            vec![WirType::I32],
        );

        self.local_counter += 1;
        let suffix = self.local_counter;
        let ptr_name = format!("__fw_write_ptr_{suffix}");
        let handle_name = format!("__fw_handle_{suffix}");
        let result_name = format!("__fw_result_{suffix}");
        let evt_name = format!("__fw_evt_{suffix}");
        let resource_name = format!("__fw_resource_{suffix}");

        const BUF_SIZE: i32 = 40;
        const BUF_ALIGN: i32 = 8;

        let mut seq = vec![];

        for (name, ty) in [
            (&ptr_name, WirType::I32),
            (&handle_name, WirType::I32),
            (&result_name, WirType::I32),
            (&resource_name, WirType::I32),
        ] {
            seq.push(WirInstr::DeclareLocal {
                name: name.clone(),
                ty,
            });
        }

        seq.push(WirInstr::LocalSet {
            name: handle_name.clone(),
            value: Box::new(handle),
        });

        seq.push(WirInstr::LocalSet {
            name: resource_name.clone(),
            value: Box::new(resource_handle),
        });

        seq.push(WirInstr::LocalSet {
            name: ptr_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: realloc_id.clone(),
                args: vec![
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(0),
                    WirInstr::I32Const(BUF_ALIGN),
                    WirInstr::I32Const(BUF_SIZE),
                ],
            }),
        });

        for i in 0..(BUF_SIZE / 8) {
            seq.push(WirInstr::I64Store {
                offset: u64::from((i * 8).cast_unsigned()),
                align: 3,
                addr: Box::new(WirInstr::LocalGet {
                    name: ptr_name.clone(),
                    result_ty: WirType::I32,
                }),
                value: Box::new(WirInstr::I64Const(0)),
            });
        }

        seq.push(WirInstr::I32Store8 {
            offset: 8,
            align: 0,
            addr: Box::new(WirInstr::LocalGet {
                name: ptr_name.clone(),
                result_ty: WirType::I32,
            }),
            value: Box::new(WirInstr::I32Const(1)),
        });

        seq.push(WirInstr::I32Store {
            offset: 12,
            align: 2,
            addr: Box::new(WirInstr::LocalGet {
                name: ptr_name.clone(),
                result_ty: WirType::I32,
            }),
            value: Box::new(WirInstr::LocalGet {
                name: resource_name,
                result_ty: WirType::I32,
            }),
        });

        seq.push(WirInstr::LocalSet {
            name: result_name.clone(),
            value: Box::new(WirInstr::Call {
                func_id: future_write_id,
                args: vec![
                    WirInstr::LocalGet {
                        name: handle_name.clone(),
                        result_ty: WirType::I32,
                    },
                    WirInstr::LocalGet {
                        name: ptr_name.clone(),
                        result_ty: WirType::I32,
                    },
                ],
            }),
        });

        seq.push(self.emit_future_write_blocked_wait(
            &handle_name,
            &result_name,
            &evt_name,
            &realloc_id,
            &ws_new_id,
            &w_join_id,
            &ws_wait_id,
        ));

        seq.push(WirInstr::Drop(Box::new(WirInstr::Call {
            func_id: realloc_id,
            args: vec![
                WirInstr::LocalGet {
                    name: ptr_name,
                    result_ty: WirType::I32,
                },
                WirInstr::I32Const(BUF_SIZE),
                WirInstr::I32Const(BUF_ALIGN),
                WirInstr::I32Const(0),
            ],
        })));

        WirInstr::Seq(seq)
    }
}

/// Detect `Result::Ok(Option::Some(<inner>))` at TIR level and return `<inner>`.
///
/// Used to special-case future writes that deliver a single resource handle
/// (e.g. trailers) via the `Result<Option<own<R>>, _>` shape, since general
/// variant→CM-buffer lowering is not yet implemented.
fn match_ok_some_resource(expr: &NirExpr) -> Option<&NirExpr> {
    let NirExprKind::VariantConstruct {
        case_name: outer,
        payload: Some(outer_payload),
        ..
    } = &expr.kind
    else {
        return None;
    };
    if outer != "Ok" {
        return None;
    }
    let NirExprKind::VariantConstruct {
        case_name: inner,
        payload: Some(inner_payload),
        ..
    } = &outer_payload.kind
    else {
        return None;
    };
    if inner != "Some" {
        return None;
    }
    Some(inner_payload)
}

/// Detect `Result::Ok(Option::None)` or `Result::Ok(null)` at TIR level.
/// `null` stays as `NirExprKind::Null` in TIR (coerced to `Option::None` later).
fn match_ok_none(expr: &NirExpr) -> bool {
    let NirExprKind::VariantConstruct {
        case_name: outer,
        payload: Some(outer_payload),
        ..
    } = &expr.kind
    else {
        return false;
    };
    if outer != "Ok" {
        return false;
    }
    match &outer_payload.kind {
        NirExprKind::Null => true,
        NirExprKind::VariantConstruct {
            case_name: inner,
            payload,
            ..
        } => inner == "None" && payload.is_none(),
        _ => false,
    }
}
