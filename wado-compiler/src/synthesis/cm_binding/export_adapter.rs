//! Export-side CM adapter synthesis.
//!
//! Every world export gets one binding function, built by
//! [`synthesize_export_binding`]: a shared prelude lifts the flat CM params
//! into Wado values ([`build_export_adapter_params`]), the user function is
//! called once, and an [`ExportReturnStrategy`] — picked by the driver from
//! the export's shape — selects the epilogue:
//!
//! - [`ExportReturnStrategy::VoidTaskReturn`] for async `() -> ()` exports:
//!   `task-return(0)` after the call.
//! - [`ExportReturnStrategy::SyncReturn`] for sync `--lib` exports: return
//!   the flattened value directly (out-pointer for multi-value results).
//! - [`ExportReturnStrategy::AsyncTaskReturn`] for `export async fn`, whose
//!   body does its own `task return`.
//! - [`ExportReturnStrategy::ResultTaskReturn`] for async exports returning
//!   `Result<T, E>`: match Ok/Err, lower the payload, `task-return`.
//!
//! Each binding name is built by [`export_binding_func_name`]. The
//! `synthesize_lower_to_flat` / `synthesize_variant_lower_to_flat` /
//! `FlatLocal` triple is shared with `task_return.rs` (which expands inline
//! `task return` into the same flat-args sequence), hence their
//! `pub(super)` visibility.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::Type;
use crate::cm_abi;
use crate::component_model::CmInterfaceRegistry;
use crate::hashmap::IndexMap;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name::LocalMethodName;
use crate::tir::{
    CallArg, FunctionRef, PrimitiveType, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind,
    TirFunction, TirLocal, TirMatchArm, TirModule, TirParam, TirPattern, TirStmt, TirStmtKind,
    TirStructField, TirVariantCase, TirVariantDecl, TypeId, TypeTable,
};

use crate::synthesis::common::{
    alloc_local, assign, binary, block, break_stmt, builtin_call, cast, cm_raw_call, expr_stmt,
    generic_method_call, i32_const, if_stmt, internal_call, let_mut_stmt, let_stmt, local_ref,
    loop_stmt, null_expr, option_none, option_some, param_local, return_stmt, split_packed_ptr_len,
    synth_span,
};

use super::cm_free::{CmShapeContext, cm_shape, synthesize_free_cm_value};
use super::import_adapter::make_binding_function;
use super::lift::synthesize_lift_list;
use super::lower::synthesize_lower_wasi_type_to_memory;
use super::types::{
    LiftContext, LowerContext, binary_add, binary_ne, cm_val_type_to_type_id, cm_zero,
    coerce_flat_lift, coerce_flat_lower, compute_export_flat_param_types,
    compute_export_flat_return_types, export_needs_param_lifting, field_access, find_struct_decl,
    find_variant_decl, flat_types_from_type_id, flatten_export_type, is_unit_type,
    type_id_to_ast_type, variant_payload, variant_tag, variant_test,
};

/// Build the export binding function name for a world export.
pub fn export_binding_func_name(export_name: &str) -> String {
    format!("__cm_export__{export_name}")
}

/// Lower a Wado-typed value to flat CM ABI args (i32 / i64 / f32 / f64).
///
/// Used by export bindings (which feed `task-return`) and by the inline
/// `task return` expansion in `task_return.rs`.
pub(super) fn synthesize_lower_to_flat(
    value: TirExpr,
    type_id: TypeId,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    ctx: LiftContext<'_>,
) -> Vec<FlatLocal> {
    let resolved = ctx.type_table.borrow().get(type_id).clone();
    lower_to_flat_inner(
        value,
        type_id,
        &resolved,
        next_local,
        stmts,
        locals,
        tir_modules,
        ctx,
    )
}

/// A flat local: holds a lowered CM value with its CM type.
pub(super) struct FlatLocal {
    pub(super) index: u32,
    pub(super) cm_type: cm_abi::CmValType,
}

/// Inner recursive implementation of `synthesize_lower_to_flat`.
fn lower_to_flat_inner(
    value: TirExpr,
    type_id: TypeId,
    resolved: &ResolvedType,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    ctx: LiftContext<'_>,
) -> Vec<FlatLocal> {
    let names =
        super::types::CmStdlibNames::from_compiler_items(ctx.type_table.borrow().compiler_items());
    match resolved {
        ResolvedType::Primitive(p) => {
            let (flat_type_id, cm_type) = match p {
                PrimitiveType::I8
                | PrimitiveType::U8
                | PrimitiveType::I16
                | PrimitiveType::U16
                | PrimitiveType::I32
                | PrimitiveType::U32
                | PrimitiveType::Bool
                | PrimitiveType::Char => (TypeTable::I32, cm_abi::CmValType::I32),
                PrimitiveType::I64 | PrimitiveType::U64 => (TypeTable::I64, cm_abi::CmValType::I64),
                PrimitiveType::F32 => (TypeTable::F32, cm_abi::CmValType::F32),
                PrimitiveType::F64 => (TypeTable::F64, cm_abi::CmValType::F64),
                PrimitiveType::I128 | PrimitiveType::U128 => {
                    panic!("i128/u128 cannot appear at CM boundary")
                }
                PrimitiveType::V128 => {
                    panic!("v128 cannot appear at CM boundary")
                }
            };
            let cast_value = if flat_type_id == type_id {
                value
            } else {
                cast(value, flat_type_id)
            };
            let local = alloc_local(next_local, locals, flat_type_id);
            stmts.push(let_stmt("__flat", local, flat_type_id, cast_value));
            vec![FlatLocal {
                index: local,
                cm_type,
            }]
        }
        ResolvedType::Resource { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::GenericResource { .. }
        | ResolvedType::Flags { .. } => {
            // Resource handles (incl. Future/Stream/Own/Borrow), enums, and
            // flags bitmasks are i32
            let local = alloc_local(next_local, locals, TypeTable::I32);
            stmts.push(let_stmt("__flat", local, TypeTable::I32, value));
            vec![FlatLocal {
                index: local,
                cm_type: cm_abi::CmValType::I32,
            }]
        }
        ResolvedType::Struct { name, .. } if name == &names.string => {
            // String → cm_lower_string → packed i64, split to ptr(i32) and len(i32)
            let packed = internal_call("cm_lower_string", vec![value], TypeTable::I64);
            let packed_local = alloc_local(next_local, locals, TypeTable::I64);
            stmts.push(let_stmt("__packed", packed_local, TypeTable::I64, packed));

            let (ptr, len) =
                split_packed_ptr_len(local_ref(packed_local, "__packed", TypeTable::I64));
            let ptr_local = alloc_local(next_local, locals, TypeTable::I32);
            stmts.push(let_stmt("__ptr", ptr_local, TypeTable::I32, ptr));
            let len_local = alloc_local(next_local, locals, TypeTable::I32);
            stmts.push(let_stmt("__len", len_local, TypeTable::I32, len));

            vec![
                FlatLocal {
                    index: ptr_local,
                    cm_type: cm_abi::CmValType::I32,
                },
                FlatLocal {
                    index: len_local,
                    cm_type: cm_abi::CmValType::I32,
                },
            ]
        }
        ResolvedType::Unit => vec![],
        ResolvedType::GenericInstance {
            name, type_args, ..
        } if name == &names.array && type_args.len() == 1 => {
            // List<T> flat ABI: (ptr: i32, len: i32) pointing at
            // `len * cm_size(T)` bytes of linear memory with `cm_align(T)`
            // alignment, laid out per the Canonical ABI.
            let lower_ctx = LowerContext {
                cm_interface_registry: ctx.cm_interface_registry,
                type_table: ctx.type_table,
                wasi_package: ctx.cm_package,
                names: names.clone(),
            };
            let elem_type_id = type_args[0];
            let elem_ast_type = {
                let tt = ctx.type_table.borrow();
                type_id_to_ast_type(elem_type_id, &tt, ctx.cm_interface_registry)
            };
            let elem_size = crate::component_model::cm_size_with_registry_scoped(
                &elem_ast_type,
                ctx.cm_interface_registry,
                Some(ctx.cm_package),
            );
            let elem_align = crate::component_model::cm_align_with_registry_scoped(
                &elem_ast_type,
                ctx.cm_interface_registry,
                Some(ctx.cm_package),
            );

            // __arr = value
            let arr_local = alloc_local(next_local, locals, type_id);
            stmts.push(let_stmt("__arr_val", arr_local, type_id, value));

            // __len = List::len(__arr)
            let len_local = alloc_local(next_local, locals, TypeTable::I32);
            stmts.push(let_stmt(
                "__arr_len",
                len_local,
                TypeTable::I32,
                generic_method_call(
                    local_ref(arr_local, "__arr_val", type_id),
                    &names.array,
                    "len",
                    ModuleSource::list(),
                    vec![],
                    TypeTable::I32,
                ),
            ));

            // __bytes = __len * elem_size
            let bytes_local = alloc_local(next_local, locals, TypeTable::I32);
            stmts.push(let_stmt(
                "__arr_bytes",
                bytes_local,
                TypeTable::I32,
                binary(
                    TirBinaryOp::Mul,
                    local_ref(len_local, "__arr_len", TypeTable::I32),
                    i32_const(elem_size as i32),
                    TypeTable::I32,
                ),
            ));

            // __ptr = builtin::realloc(0, 0, elem_align, __bytes)  — i.e.
            // allocate `bytes` fresh bytes with `elem_align` alignment.
            let ptr_local = alloc_local(next_local, locals, TypeTable::I32);
            stmts.push(let_stmt(
                "__arr_ptr",
                ptr_local,
                TypeTable::I32,
                builtin_call(
                    "realloc",
                    vec![
                        i32_const(0),
                        i32_const(0),
                        i32_const(elem_align as i32),
                        local_ref(bytes_local, "__arr_bytes", TypeTable::I32),
                    ],
                    TypeTable::I32,
                ),
            ));

            // for let mut __i = 0; __i < __len; __i += 1 {
            //   let __elem = __arr[__i];
            //   synthesize_lower_wasi_type_to_memory(
            //     elem_ast_type, __elem,
            //     __ptr + __i * elem_size,
            //   );
            // }
            let i_local = alloc_local(next_local, locals, TypeTable::I32);
            stmts.push(let_mut_stmt(
                "__arr_i",
                i_local,
                TypeTable::I32,
                i32_const(0),
            ));

            let mut loop_stmts: Vec<TirStmt> = Vec::new();
            loop_stmts.push(if_stmt(
                binary(
                    TirBinaryOp::GtEq,
                    local_ref(i_local, "__arr_i", TypeTable::I32),
                    local_ref(len_local, "__arr_len", TypeTable::I32),
                    TypeTable::BOOL,
                ),
                block(vec![break_stmt()]),
                None,
            ));

            // __elem = (__arr[__i]) via the IndexValue<i32> trait method.
            let elem_local = alloc_local(next_local, locals, elem_type_id);
            let iv_info = LocalMethodName::new(
                names.array,
                Some("IndexValue<i32>".to_string()),
                "index_value".to_string(),
            );
            let iv_mangled = iv_info.to_mangled_name();
            loop_stmts.push(let_stmt(
                "__arr_elem",
                elem_local,
                elem_type_id,
                TirExpr::new(
                    TirExprKind::method_call(
                        Box::new(local_ref(arr_local, "__arr_val", type_id)),
                        FunctionRef {
                            module_source: ModuleSource::list(),
                            name: iv_mangled,
                            monomorph_info: None,
                            method_info: Some(iv_info),
                        },
                        vec![],
                        vec![CallArg::new(
                            local_ref(i_local, "__arr_i", TypeTable::I32),
                            false,
                        )],
                    ),
                    elem_type_id,
                    synth_span(),
                ),
            ));

            // __elem_addr = __ptr + __i * elem_size
            let elem_addr_local = alloc_local(next_local, locals, TypeTable::I32);
            loop_stmts.push(let_stmt(
                "__arr_elem_addr",
                elem_addr_local,
                TypeTable::I32,
                binary_add(
                    local_ref(ptr_local, "__arr_ptr", TypeTable::I32),
                    binary(
                        TirBinaryOp::Mul,
                        local_ref(i_local, "__arr_i", TypeTable::I32),
                        i32_const(elem_size as i32),
                        TypeTable::I32,
                    ),
                ),
            ));

            // Lower the element into the allocated slot.
            loop_stmts.extend(synthesize_lower_wasi_type_to_memory(
                &elem_ast_type,
                local_ref(elem_local, "__arr_elem", elem_type_id),
                local_ref(elem_addr_local, "__arr_elem_addr", TypeTable::I32),
                next_local,
                locals,
                &lower_ctx,
            ));

            // __i += 1
            loop_stmts.push(expr_stmt(assign(
                local_ref(i_local, "__arr_i", TypeTable::I32),
                binary_add(local_ref(i_local, "__arr_i", TypeTable::I32), i32_const(1)),
            )));

            stmts.push(loop_stmt(block(loop_stmts)));

            vec![
                FlatLocal {
                    index: ptr_local,
                    cm_type: cm_abi::CmValType::I32,
                },
                FlatLocal {
                    index: len_local,
                    cm_type: cm_abi::CmValType::I32,
                },
            ]
        }
        ResolvedType::GenericInstance {
            name, type_args, ..
        } if name == &names.option && type_args.len() == 1 => {
            // Option<T> → disc(i32) + flat(T)
            let inner_type_id = type_args[0];
            let mut result = Vec::new();

            // Save value to a local for reuse
            let opt_local = alloc_local(next_local, locals, type_id);
            stmts.push(let_stmt("__opt_val", opt_local, type_id, value));

            // Discriminant: VariantTest(Some) → 1 = Some, 0 = None
            let disc_expr = TirExpr::new(
                TirExprKind::Cast {
                    expr: Box::new(variant_test(
                        local_ref(opt_local, "__opt_val", type_id),
                        names.some_index,
                        &names.some_name,
                    )),
                    target_type: TypeTable::I32,
                },
                TypeTable::I32,
                synth_span(),
            );
            let disc_local = alloc_local(next_local, locals, TypeTable::I32);
            stmts.push(let_stmt(
                "__opt_disc",
                disc_local,
                TypeTable::I32,
                disc_expr,
            ));
            result.push(FlatLocal {
                index: disc_local,
                cm_type: cm_abi::CmValType::I32,
            });

            // If Some, lower the inner value
            let inner_flat_types = {
                let tt = ctx.type_table.borrow();
                flat_types_from_type_id(inner_type_id, tir_modules, &tt)
            };
            if !inner_flat_types.is_empty() {
                // Allocate locals for inner flat values (initialized to zero)
                let inner_locals: Vec<(u32, String)> = inner_flat_types
                    .iter()
                    .enumerate()
                    .map(|(i, &vt)| {
                        let tid = cm_val_type_to_type_id(vt);
                        let l = alloc_local(next_local, locals, tid);
                        let name = format!("__opt_inner_{i}");
                        stmts.push(let_mut_stmt(&name, l, tid, cm_zero(vt)));
                        (l, name)
                    })
                    .collect();

                // if disc != 0 { lower(variant_payload(value)) → inner_locals }
                let mut then_stmts: Vec<TirStmt> = Vec::new();
                let unwrapped = variant_payload(
                    local_ref(opt_local, "__opt_val", type_id),
                    names.some_index,
                    inner_type_id,
                );
                let inner_lowered = synthesize_lower_to_flat(
                    unwrapped,
                    inner_type_id,
                    next_local,
                    &mut then_stmts,
                    locals,
                    tir_modules,
                    ctx,
                );
                assign_flat_values_into_slots(
                    &inner_lowered,
                    &inner_locals,
                    &inner_flat_types,
                    &mut then_stmts,
                );

                stmts.push(if_stmt(
                    binary(
                        TirBinaryOp::NotEq,
                        local_ref(disc_local, "__opt_disc", TypeTable::I32),
                        i32_const(0),
                        TypeTable::BOOL,
                    ),
                    block(then_stmts),
                    None,
                ));

                for (&(l, _), &vt) in inner_locals.iter().zip(inner_flat_types.iter()) {
                    result.push(FlatLocal {
                        index: l,
                        cm_type: vt,
                    });
                }
            }

            result
        }
        ResolvedType::GenericInstance {
            name, type_args, ..
        } if name == &names.result && type_args.len() == 2 => {
            // Result<T, E> → disc(i32) + join(flat(T), flat(E)). disc 0 = Ok,
            // 1 = Err. The active arm's payload lowers into the shared joined
            // slots (the other arm leaves them zero), per the Canonical ABI's
            // variant join.
            let ok_type_id = type_args[0];
            let err_type_id = type_args[1];
            let mut result = Vec::new();

            let res_local = alloc_local(next_local, locals, type_id);
            stmts.push(let_stmt("__res_val", res_local, type_id, value));

            let (ok_index, err_name, err_index) = {
                let tt = ctx.type_table.borrow();
                let items = tt.compiler_items();
                let (_, _, _, ok_i) =
                    items.require_variant_case(crate::compiler_item::CompilerItem::ResultOk);
                let (_, _, err_n, err_i) =
                    items.require_variant_case(crate::compiler_item::CompilerItem::ResultErr);
                (ok_i, err_n.to_string(), err_i)
            };

            // disc: variant_test(Err) → 1 when Err, 0 when Ok.
            let disc_expr = cast(
                variant_test(
                    local_ref(res_local, "__res_val", type_id),
                    err_index,
                    &err_name,
                ),
                TypeTable::I32,
            );
            let disc_local = alloc_local(next_local, locals, TypeTable::I32);
            stmts.push(let_stmt(
                "__res_disc",
                disc_local,
                TypeTable::I32,
                disc_expr,
            ));
            result.push(FlatLocal {
                index: disc_local,
                cm_type: cm_abi::CmValType::I32,
            });

            let payload_flats: Vec<cm_abi::CmValType> = {
                let tt = ctx.type_table.borrow();
                flat_types_from_type_id(type_id, tir_modules, &tt)
                    .get(1..)
                    .map(<[cm_abi::CmValType]>::to_vec)
                    .unwrap_or_default()
            };
            if !payload_flats.is_empty() {
                let slot_locals: Vec<(u32, String)> = payload_flats
                    .iter()
                    .enumerate()
                    .map(|(i, &vt)| {
                        let tid = cm_val_type_to_type_id(vt);
                        let l = alloc_local(next_local, locals, tid);
                        let name = format!("__res_slot_{i}");
                        stmts.push(let_mut_stmt(&name, l, tid, cm_zero(vt)));
                        (l, name)
                    })
                    .collect();

                let lower_arm = |case_index: u32,
                                 payload_tid: TypeId,
                                 next_local: &mut u32,
                                 locals: &mut Vec<TirLocal>|
                 -> Vec<TirStmt> {
                    let mut arm_stmts: Vec<TirStmt> = Vec::new();
                    let payload = variant_payload(
                        local_ref(res_local, "__res_val", type_id),
                        case_index,
                        payload_tid,
                    );
                    let lowered = synthesize_lower_to_flat(
                        payload,
                        payload_tid,
                        next_local,
                        &mut arm_stmts,
                        locals,
                        tir_modules,
                        ctx,
                    );
                    assign_flat_values_into_slots(
                        &lowered,
                        &slot_locals,
                        &payload_flats,
                        &mut arm_stmts,
                    );
                    arm_stmts
                };

                let ok_stmts = lower_arm(ok_index, ok_type_id, next_local, locals);
                let err_stmts = lower_arm(err_index, err_type_id, next_local, locals);

                stmts.push(if_stmt(
                    binary(
                        TirBinaryOp::Eq,
                        local_ref(disc_local, "__res_disc", TypeTable::I32),
                        i32_const(ok_index as i32),
                        TypeTable::BOOL,
                    ),
                    block(ok_stmts),
                    Some(block(err_stmts)),
                ));

                for (&(l, _), &vt) in slot_locals.iter().zip(payload_flats.iter()) {
                    result.push(FlatLocal {
                        index: l,
                        cm_type: vt,
                    });
                }
            }

            result
        }
        ResolvedType::Variant { name, .. } => {
            // Named variant → disc(i32) + join of the case payload flats, per
            // the Canonical ABI. Reuses the same discriminant-plus-Match
            // lowering the memory path uses; a payload-less variant flattens
            // to the bare discriminant.
            let Some(variant_decl) = find_variant_decl(name, tir_modules) else {
                panic!("variant `{name}` has no TIR declaration; cannot lower it to flat CM values")
            };
            let flat_types: Vec<cm_abi::CmValType> = {
                let tt = ctx.type_table.borrow();
                flat_types_from_type_id(type_id, tir_modules, &tt)
            };

            let value_local = alloc_local(next_local, locals, type_id);
            stmts.push(let_stmt("__variant_val", value_local, type_id, value));

            let slot_locals: Vec<(u32, String)> = flat_types
                .iter()
                .enumerate()
                .map(|(i, &vt)| {
                    let tid = cm_val_type_to_type_id(vt);
                    let l = alloc_local(next_local, locals, tid);
                    let name = format!("__variant_slot_{i}");
                    stmts.push(let_mut_stmt(&name, l, tid, cm_zero(vt)));
                    (l, name)
                })
                .collect();

            synthesize_variant_lower_to_flat(
                value_local,
                type_id,
                &variant_decl,
                &slot_locals,
                &flat_types,
                next_local,
                stmts,
                locals,
                tir_modules,
                ctx,
            );

            slot_locals
                .iter()
                .zip(flat_types.iter())
                .map(|(&(l, _), &vt)| FlatLocal {
                    index: l,
                    cm_type: vt,
                })
                .collect()
        }
        ResolvedType::Struct { name, .. } if name != &names.string => {
            // Struct: concatenation of field flat types
            if let Some(struct_decl) = find_struct_decl(name, tir_modules) {
                let mut result = Vec::new();

                // Save value to a local
                let struct_local = alloc_local(next_local, locals, type_id);
                stmts.push(let_stmt("__struct_val", struct_local, type_id, value));

                for field in &struct_decl.fields {
                    let field_value = field_access(
                        local_ref(struct_local, "__struct_val", type_id),
                        &field.name,
                        field.index,
                        field.type_id,
                    );
                    let field_lowered = synthesize_lower_to_flat(
                        field_value,
                        field.type_id,
                        next_local,
                        stmts,
                        locals,
                        tir_modules,
                        ctx,
                    );
                    result.extend(field_lowered);
                }
                result
            } else {
                panic!("struct `{name}` has no TIR declaration; cannot lower it to flat CM values")
            }
        }
        ResolvedType::Newtype { base_type, .. } => {
            // Newtypes share their base's representation; lower through the base.
            let base = *base_type;
            let base_resolved = ctx.type_table.borrow().get(base).clone();
            lower_to_flat_inner(
                cast(value, base),
                base,
                &base_resolved,
                next_local,
                stmts,
                locals,
                tir_modules,
                ctx,
            )
        }
        other => {
            panic!("cannot lower `{other:?}` to flat CM values at the export boundary")
        }
    }
}

/// Assign lowered flat values into pre-allocated slot locals, coercing each
/// value to its slot's CM type ([`coerce_flat_lower`]). Trailing slots may
/// stay zero when the value flattens narrower than the join (variant arms),
/// but a lowered value count exceeding the slot count is an ABI flattening
/// bug.
fn assign_flat_values_into_slots(
    lowered: &[FlatLocal],
    slot_locals: &[(u32, String)],
    slot_types: &[cm_abi::CmValType],
    stmts: &mut Vec<TirStmt>,
) {
    assert!(
        lowered.len() <= slot_locals.len(),
        "lowered {} flat values but only {} flat slots: CM ABI flattening mismatch",
        lowered.len(),
        slot_locals.len(),
    );
    for (flat_val, (&(slot_local, ref slot_name), &slot_vt)) in lowered
        .iter()
        .zip(slot_locals.iter().zip(slot_types.iter()))
    {
        let slot_type_id = cm_val_type_to_type_id(slot_vt);
        let source_type_id = cm_val_type_to_type_id(flat_val.cm_type);
        let val = coerce_flat_lower(
            local_ref(flat_val.index, "__flat", source_type_id),
            flat_val.cm_type,
            slot_vt,
        );
        stmts.push(expr_stmt(assign(
            local_ref(slot_local, slot_name, slot_type_id),
            val,
        )));
    }
}

/// Lift a single export parameter from flat CM params to a Wado-typed value.
///
/// Flat parameters are the Wasm function parameters corresponding to a single
/// CM parameter. For example, a `String` parameter becomes two flat params
/// `(ptr: i32, len: i32)` pointing to data in linear memory.
///
/// Returns the lifted TIR expression and the number of flat params consumed.
///
/// `lift_ctx` carries the full CM resolution stack (WASI + kiln registries,
/// type-table cell, binding package hint) used for List / nested-struct
/// lifts and for the `struct_type_map` lookup in WIR.
#[allow(clippy::too_many_arguments)]
pub(super) fn synthesize_lift_from_flat_params(
    ty: &Type,
    flat_param_locals: &[u32],
    flat_types: &[cm_abi::CmValType],
    target_type_id: TypeId,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    lift_ctx: LiftContext<'_>,
) -> (TirExpr, usize) {
    let type_table_cell = lift_ctx.type_table;
    let names =
        super::types::CmStdlibNames::from_compiler_items(type_table_cell.borrow().compiler_items());
    match ty {
        Type::Named(named) if named.name == names.string => {
            // String flat ABI: (ptr: i32, len: i32) pointing to linear memory.
            // The caller lowered it there through *our* `realloc`, so the buffer
            // is the guest's to release once it has been copied onto the GC
            // heap. Nested strings are released by the lift site that reads
            // them; this arm is the only top-level one.
            let ptr = local_ref(flat_param_locals[0], "__p", TypeTable::I32);
            let len = local_ref(flat_param_locals[1], "__p", TypeTable::I32);
            let lifted_local = alloc_local(next_local, locals, target_type_id);
            stmts.push(let_stmt(
                "__lifted_string",
                lifted_local,
                target_type_id,
                internal_call(
                    "memory_to_gc_string",
                    vec![ptr.clone(), len.clone()],
                    target_type_id,
                ),
            ));
            stmts.push(if_stmt(
                binary(TirBinaryOp::Gt, len.clone(), i32_const(0), TypeTable::BOOL),
                block(vec![expr_stmt(builtin_call(
                    "realloc",
                    vec![ptr, len, i32_const(1), i32_const(0)],
                    TypeTable::I32,
                ))]),
                None,
            ));
            (
                local_ref(lifted_local, "__lifted_string", target_type_id),
                2,
            )
        }
        Type::Named(_)
            if matches!(
                type_table_cell.borrow().get(target_type_id),
                ResolvedType::Newtype { .. }
            ) =>
        {
            // A newtype shares its base's flat representation, so lift through
            // the base (the lower path peels symmetrically). Otherwise it hits
            // the i32 default below and reads its slot at the wrong core type
            // (`expected f64, found i32` for `Option<Meters>`).
            let (base_type_id, base_ast) = {
                let tt = type_table_cell.borrow();
                let ResolvedType::Newtype { base_type, .. } = tt.get(target_type_id) else {
                    unreachable!("guarded by the match arm above")
                };
                let base_type = *base_type;
                (
                    base_type,
                    type_id_to_ast_type(base_type, &tt, lift_ctx.cm_interface_registry),
                )
            };
            synthesize_lift_from_flat_params(
                &base_ast,
                flat_param_locals,
                flat_types,
                base_type_id,
                next_local,
                stmts,
                locals,
                tir_modules,
                lift_ctx,
            )
        }
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" => (local_ref(flat_param_locals[0], "__p", TypeTable::I32), 1),
            "i64" | "u64" => (local_ref(flat_param_locals[0], "__p", TypeTable::I64), 1),
            "f32" => (local_ref(flat_param_locals[0], "__p", TypeTable::F32), 1),
            "f64" => (local_ref(flat_param_locals[0], "__p", TypeTable::F64), 1),
            "i8" | "u8" | "i16" | "u16" => {
                (local_ref(flat_param_locals[0], "__p", TypeTable::I32), 1)
            }
            "bool" => {
                let raw = local_ref(flat_param_locals[0], "__p", TypeTable::I32);
                let lifted = binary(TirBinaryOp::NotEq, raw, i32_const(0), TypeTable::BOOL);
                (lifted, 1)
            }
            "char" => (local_ref(flat_param_locals[0], "__p", TypeTable::CHAR), 1),
            "()" => {
                let unit = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
                (unit, 0)
            }
            _ => {
                // Struct parameter: flatten to concatenation of field flat
                // values per the canonical ABI. Iterate the TIR struct decl
                // and recursively lift each field, then construct a
                // `StructLiteral`. Resource handles, enums, flags, and
                // unknown types fall through to i32 passthrough.
                if let Some(struct_decl) = find_struct_decl(&named.name, tir_modules) {
                    let mut offset = 0;
                    let mut fields_out = Vec::with_capacity(struct_decl.fields.len());
                    // Precompute each field's AST surface while the
                    // `TypeTable` borrow is live; drop the borrow before
                    // recursion so the inner list lift may take
                    // `borrow_mut()` via the `LiftContext`. Also
                    // resolve the struct's own type_id — `target_type_id`
                    // may arrive as a reference wrapper when the user
                    // function took the struct by value, so we consult
                    // the TIR struct decl's module source for the
                    // concrete `ResolvedType::Struct` id the
                    // `StructLiteral` WIR pass expects.
                    let (field_ast_tys, struct_type_id) = {
                        let tt = type_table_cell.borrow();
                        let field_tys: Vec<Type> = struct_decl
                            .fields
                            .iter()
                            .map(|f| {
                                type_id_to_ast_type(f.type_id, &tt, lift_ctx.cm_interface_registry)
                            })
                            .collect();
                        // Prefer the already-registered TypeId so the WIR
                        // `struct_type_map` lookup hits — the
                        // `find_struct_by_name` index is populated when
                        // the elaborator first processed the struct decl,
                        // and `target_type_id` may arrive as a reference
                        // wrapper or an unregistered intern.
                        let stid = tt
                            .find_struct_by_name(&struct_decl.name, &struct_decl.module_source)
                            .unwrap_or(target_type_id);
                        (field_tys, stid)
                    };
                    for (field, field_ast_ty) in struct_decl.fields.iter().zip(field_ast_tys.iter())
                    {
                        let (lifted, consumed) = synthesize_lift_from_flat_params(
                            field_ast_ty,
                            &flat_param_locals[offset..],
                            &flat_types[offset..],
                            field.type_id,
                            next_local,
                            stmts,
                            locals,
                            tir_modules,
                            lift_ctx,
                        );
                        fields_out.push(TirStructField {
                            name: field.name.clone(),
                            value: lifted,
                            field_index: field.index,
                        });
                        offset += consumed;
                    }
                    let struct_expr = TirExpr::new(
                        TirExprKind::StructLiteral {
                            struct_type: struct_type_id,
                            struct_name: named.name.clone(),
                            fields: fields_out,
                        },
                        struct_type_id,
                        synth_span(),
                    );
                    // Materialise into a local so it can be passed by
                    // value to the user function without re-evaluation.
                    let result_local = alloc_local(next_local, locals, struct_type_id);
                    stmts.push(let_stmt(
                        "__struct_lift",
                        result_local,
                        struct_type_id,
                        struct_expr,
                    ));
                    return (
                        local_ref(result_local, "__struct_lift", struct_type_id),
                        offset,
                    );
                }
                // Named `variant` (sum type with payloads): reconstruct the GC
                // value from flat params. The CM `variant` flat ABI is
                // `(disc: i32, ...joined-payload-flats)`; each case's payload
                // shares the joined slots positionally, so lift the active
                // case's payload recursively from `flat[1..]`.
                if let Some(variant_decl) = find_variant_decl(&named.name, tir_modules) {
                    return lift_variant_from_flat_params(
                        &variant_decl,
                        flat_param_locals,
                        flat_types,
                        target_type_id,
                        next_local,
                        stmts,
                        locals,
                        tir_modules,
                        lift_ctx,
                    );
                }
                // Resource handles, enums, unknown types → i32 passthrough
                (local_ref(flat_param_locals[0], "__p", TypeTable::I32), 1)
            }
        },
        Type::Generic(generic) => match generic.name.as_str() {
            n if n == names.array => {
                // list<T> flat ABI: (ptr: i32, len: i32) — elements in linear memory.
                // (A former `List<u8>` fast path via `memory_to_gc_array` produced a
                // bare `Array<u8>` mislabeled as `List<u8>`; the general path below
                // builds the real `List<T>` struct, so all element types share it.)
                // Write ptr/len to a temp memory block so we can reuse synthesize_lift
                let ptr = local_ref(flat_param_locals[0], "__p", TypeTable::I32);
                let len = local_ref(flat_param_locals[1], "__p", TypeTable::I32);
                // Allocate 8 bytes for ptr+len
                let tmp_ptr_local = alloc_local(next_local, locals, TypeTable::I32);
                stmts.push(let_stmt(
                    "__lift_tmp",
                    tmp_ptr_local,
                    TypeTable::I32,
                    builtin_call(
                        "realloc",
                        vec![i32_const(0), i32_const(0), i32_const(4), i32_const(8)],
                        TypeTable::I32,
                    ),
                ));
                // Write ptr at offset 0
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![local_ref(tmp_ptr_local, "__lift_tmp", TypeTable::I32), ptr],
                    TypeTable::UNIT,
                )));
                // Write len at offset 4
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![
                        binary_add(
                            local_ref(tmp_ptr_local, "__lift_tmp", TypeTable::I32),
                            i32_const(4),
                        ),
                        len,
                    ],
                    TypeTable::UNIT,
                )));
                // Lift into the user function's exact `List<T>` type
                // (`target_type_id`), not a rebuilt one, so a shared stdlib
                // element type (e.g. `InputFile`) does not resolve to a second
                // GC `TypeId` and mismatch the parameter.
                let lifted = synthesize_lift_list(
                    &generic.args[0],
                    local_ref(tmp_ptr_local, "__lift_tmp", TypeTable::I32),
                    Some(target_type_id),
                    next_local,
                    stmts,
                    locals,
                    &lift_ctx,
                );
                // Free temp memory
                stmts.push(expr_stmt(builtin_call(
                    "realloc",
                    vec![
                        local_ref(tmp_ptr_local, "__lift_tmp", TypeTable::I32),
                        i32_const(8),
                        i32_const(4),
                        i32_const(0),
                    ],
                    TypeTable::I32,
                )));
                (lifted, 2)
            }
            n if n == names.option && generic.args.len() == 1 => {
                // option<T> flat ABI: (disc: i32, ...T_flat)
                let (inner_flat, inner_type_id) = {
                    let tt = type_table_cell.borrow();
                    let mut out = Vec::new();
                    flatten_export_type(&generic.args[0], &mut out, tir_modules, &tt);
                    let inner_tid = tt.as_option(target_type_id).unwrap_or(target_type_id);
                    (out, inner_tid)
                };
                let total_flat = 1 + inner_flat.len();

                let disc = local_ref(flat_param_locals[0], "__p", TypeTable::I32);

                // if disc == 0 { None } else { Some(lift(inner_flat)) }
                let result_local = alloc_local(next_local, locals, target_type_id);
                let opt_none_expr = {
                    let tt = type_table_cell.borrow();
                    option_none(target_type_id, tt.compiler_items())
                };
                // Default: None
                stmts.push(let_mut_stmt(
                    "__opt_result",
                    result_local,
                    target_type_id,
                    opt_none_expr,
                ));

                // if disc != 0
                let mut then_stmts: Vec<TirStmt> = Vec::new();
                if inner_flat.is_empty() {
                    // Inner is unit — Some(())
                    let unit = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
                    let opt_some_unit = {
                        let tt = type_table_cell.borrow();
                        option_some(unit, target_type_id, tt.compiler_items())
                    };
                    then_stmts.push(expr_stmt(assign(
                        local_ref(result_local, "__opt_result", target_type_id),
                        opt_some_unit,
                    )));
                } else {
                    let (inner_lifted, _) = synthesize_lift_from_flat_params(
                        &generic.args[0],
                        &flat_param_locals[1..],
                        &flat_types[1..],
                        inner_type_id,
                        next_local,
                        &mut then_stmts,
                        locals,
                        tir_modules,
                        lift_ctx,
                    );
                    let opt_some_inner = {
                        let tt = type_table_cell.borrow();
                        option_some(inner_lifted, target_type_id, tt.compiler_items())
                    };
                    then_stmts.push(expr_stmt(assign(
                        local_ref(result_local, "__opt_result", target_type_id),
                        opt_some_inner,
                    )));
                }

                stmts.push(if_stmt(
                    binary_ne(disc, i32_const(0)),
                    block(then_stmts),
                    None,
                ));

                (
                    local_ref(result_local, "__opt_result", target_type_id),
                    total_flat,
                )
            }
            n if n == names.result && generic.args.len() == 2 => {
                // `Result<T, E>` is a 2-case variant (Ok=0, Err=1) — lift it
                // through the shared variant path with a synthetic decl carrying
                // the concrete arm TypeIds.
                let decl = synthetic_result_variant_decl(&type_table_cell.borrow(), target_type_id);
                lift_variant_from_flat_params(
                    &decl,
                    flat_param_locals,
                    flat_types,
                    target_type_id,
                    next_local,
                    stmts,
                    locals,
                    tir_modules,
                    lift_ctx,
                )
            }
            // Stream<T>, Future<T>, Own<T>, Borrow<T> — i32 handles
            _ => (local_ref(flat_param_locals[0], "__p", TypeTable::I32), 1),
        },
        Type::Tuple(elems) if elems.is_empty() => {
            let unit = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
            (unit, 0)
        }
        Type::Tuple(elems) => {
            // Tuple: lift each element from consecutive flat params
            let mut total_consumed = 0;
            let mut elem_exprs = Vec::new();
            let elem_type_ids = {
                let tt = type_table_cell.borrow();
                tt.as_tuple(target_type_id)
                    .unwrap_or_else(|| vec![target_type_id; elems.len()])
            };

            for (i, elem_ty) in elems.iter().enumerate() {
                let elem_tid = elem_type_ids.get(i).copied().unwrap_or(TypeTable::I32);
                let (lifted, consumed) = synthesize_lift_from_flat_params(
                    elem_ty,
                    &flat_param_locals[total_consumed..],
                    &flat_types[total_consumed..],
                    elem_tid,
                    next_local,
                    stmts,
                    locals,
                    tir_modules,
                    lift_ctx,
                );
                elem_exprs.push(lifted);
                total_consumed += consumed;
            }

            let tuple_expr = TirExpr::new(
                TirExprKind::TupleLiteral {
                    elements: elem_exprs,
                },
                target_type_id,
                synth_span(),
            );
            (tuple_expr, total_consumed)
        }
        Type::Reference(_) | Type::MutReference(_) => {
            (local_ref(flat_param_locals[0], "__p", TypeTable::I32), 1)
        }
        Type::NamespacedGeneric(_)
        | Type::Function(_)
        | Type::TypePackSpread(..)
        | Type::Infer(_)
        | Type::Error(_) => {
            panic!("cannot lift export parameter of type `{ty:?}` from flat CM params")
        }
    }
}

/// Build a synthetic `TirVariantDecl` for a concrete `Result<T, E>` so it can be
/// lifted/lowered through the shared named-variant path. `Result` is a 2-case
/// variant (Ok=0, Err=1); its arm payload types are the instance's type args.
fn synthetic_result_variant_decl(type_table: &TypeTable, result_type_id: TypeId) -> TirVariantDecl {
    let (ok_tid, err_tid) = match type_table.get(result_type_id) {
        ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
            (type_args[0], type_args[1])
        }
        _ => (TypeTable::UNIT, TypeTable::UNIT),
    };
    let items = type_table.compiler_items();
    let (_, _, ok_name, ok_index) =
        items.require_variant_case(crate::compiler_item::CompilerItem::ResultOk);
    let (_, _, err_name, err_index) =
        items.require_variant_case(crate::compiler_item::CompilerItem::ResultErr);
    let result_name = items
        .variant_name(crate::compiler_item::CompilerItem::Result)
        .to_string();
    let case = |name: &str, index: u32, payload: TypeId| TirVariantCase {
        name: name.to_string(),
        index,
        payload,
        span: synth_span(),
        wire_name_override: None,
    };
    TirVariantDecl {
        name: result_name,
        module_source: ModuleSource::default(),
        visibility: crate::ast::Visibility::Public,
        type_params: Vec::new(),
        cases: vec![
            case(ok_name, ok_index, ok_tid),
            case(err_name, err_index, err_tid),
        ],
        span: synth_span(),
        wire_name_policy: None,
    }
}

/// Re-map a variant/result case's payload flat slots from the declared (joined)
/// slot types to the case's natural flat types, inserting a bit-preserving
/// coercion ([`coerce_flat_lift`]) wherever the join widened a slot to a
/// different core class. Returns `(local_indices, natural_flat_types)` to feed
/// the recursive payload lift; coercion `let`s are appended to `stmts` so they
/// run only inside the active case's branch.
#[allow(clippy::too_many_arguments)]
fn coerce_payload_slots(
    payload_ty: &Type,
    slot_locals: &[u32],
    slot_types: &[cm_abi::CmValType],
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table_cell: &RefCell<TypeTable>,
) -> (Vec<u32>, Vec<cm_abi::CmValType>) {
    let natural: Vec<cm_abi::CmValType> = {
        let tt = type_table_cell.borrow();
        let mut out = Vec::new();
        flatten_export_type(payload_ty, &mut out, tir_modules, &tt);
        out
    };
    let mut out_locals = Vec::with_capacity(natural.len());
    for (j, &want) in natural.iter().enumerate() {
        let Some(slot_local) = slot_locals.get(j).copied() else {
            break;
        };
        let have = slot_types.get(j).copied().unwrap_or(want);
        if have == want {
            out_locals.push(slot_local);
        } else {
            let have_tid = cm_val_type_to_type_id(have);
            let want_tid = cm_val_type_to_type_id(want);
            let coerced = coerce_flat_lift(local_ref(slot_local, "__p", have_tid), have, want);
            let l = alloc_local(next_local, locals, want_tid);
            stmts.push(let_stmt("__coerce", l, want_tid, coerced));
            out_locals.push(l);
        }
    }
    (out_locals, natural)
}

/// Lift a named `variant` value from flat CM params.
///
/// The CM `variant` flat ABI is `(disc: i32, ...joined-payload-flats)` where the
/// payload slots are the positional join of every case's flattening. This reads
/// the discriminant, then for the active case lifts its payload recursively from
/// `flat[1..]` and constructs the GC value via `VariantConstruct`, dispatching
/// through an if/else chain (mirroring the memory-based `synthesize_lift_wasi_variant`).
///
/// Returns the lifted expression and the number of flat params consumed (the
/// discriminant plus the widest case's payload flattening).
#[allow(clippy::too_many_arguments)]
fn lift_variant_from_flat_params(
    variant_decl: &TirVariantDecl,
    flat_param_locals: &[u32],
    flat_types: &[cm_abi::CmValType],
    variant_type_id: TypeId,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    lift_ctx: LiftContext<'_>,
) -> (TirExpr, usize) {
    let type_table_cell = lift_ctx.type_table;
    // Widest case payload flattening sets the consumed slot count.
    let max_payload_flats = {
        let tt = type_table_cell.borrow();
        variant_decl
            .cases
            .iter()
            .map(|c| flat_types_from_type_id(c.payload, tir_modules, &tt).len())
            .max()
            .unwrap_or(0)
    };
    let total_flat = 1 + max_payload_flats;

    let result_local = alloc_local(next_local, locals, variant_type_id);
    stmts.push(let_mut_stmt(
        "__var_lift",
        result_local,
        variant_type_id,
        null_expr(variant_type_id),
    ));

    // Per-case payload AST surfaces, computed while the borrow is live.
    let case_payload_tys: Vec<Type> = {
        let tt = type_table_cell.borrow();
        variant_decl
            .cases
            .iter()
            .map(|c| type_id_to_ast_type(c.payload, &tt, lift_ctx.cm_interface_registry))
            .collect()
    };

    let disc = local_ref(flat_param_locals[0], "__p", TypeTable::I32);
    let disc_local = alloc_local(next_local, locals, TypeTable::I32);
    stmts.push(let_stmt("__var_disc", disc_local, TypeTable::I32, disc));

    // Build the if/else chain from the last case backwards: the final case is
    // the trailing `else`, earlier cases test `disc == i`.
    let case_count = variant_decl.cases.len();
    let mut current_else: Option<TirBlock> = None;
    for (i, case) in variant_decl.cases.iter().enumerate().rev() {
        let payload_ty = &case_payload_tys[i];
        let mut case_stmts: Vec<TirStmt> = Vec::new();
        let payload = if is_unit_type(payload_ty) {
            None
        } else {
            // Re-map the joined payload slots to this case's natural flat types,
            // bit-reinterpreting where the join widened the slot to a different
            // core class.
            let (payload_locals, payload_types) = coerce_payload_slots(
                payload_ty,
                &flat_param_locals[1..],
                &flat_types[1..],
                next_local,
                &mut case_stmts,
                locals,
                tir_modules,
                type_table_cell,
            );
            let (lifted, _) = synthesize_lift_from_flat_params(
                payload_ty,
                &payload_locals,
                &payload_types,
                case.payload,
                next_local,
                &mut case_stmts,
                locals,
                tir_modules,
                lift_ctx,
            );
            Some(Box::new(lifted))
        };
        case_stmts.push(expr_stmt(assign(
            local_ref(result_local, "__var_lift", variant_type_id),
            TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type: variant_type_id,
                    case_index: case.index,
                    case_name: case.name.clone(),
                    payload,
                },
                variant_type_id,
                synth_span(),
            ),
        )));

        if i == case_count - 1 {
            current_else = Some(block(case_stmts));
        } else {
            let cond = binary(
                TirBinaryOp::Eq,
                local_ref(disc_local, "__var_disc", TypeTable::I32),
                i32_const(case.index as i32),
                TypeTable::BOOL,
            );
            current_else = Some(block(vec![if_stmt(cond, block(case_stmts), current_else)]));
        }
    }
    if let Some(outer) = current_else {
        for stmt in outer.stmts {
            stmts.push(stmt);
        }
    }

    (
        local_ref(result_local, "__var_lift", variant_type_id),
        total_flat,
    )
}

/// Build the adapter parameters and call arguments for an export binding.
///
/// The shared prelude of [`synthesize_export_binding`]: lifts flat CM params
/// back to Wado-typed args (when `export_needs_param_lifting`) or passes the
/// user params through unchanged, regardless of which
/// [`ExportReturnStrategy`] follows. The lifting prelude is appended to
/// `body_stmts` and the param locals to `locals`.
///
/// Returns `(adapter_params, call_args, next_local)` where `next_local` is the
/// first free local index after the params and any lifting temporaries.
fn build_export_adapter_params(
    user_func_ref: &TirFunction,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    world_params: &[(String, Type)],
    lift_ctx: LiftContext<'_>,
    body_stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
) -> (Vec<TirParam>, Vec<TirExpr>, u32) {
    let needs_lifting = export_needs_param_lifting(&user_func_ref.params, lift_ctx.type_table);
    if needs_lifting {
        let tt = lift_ctx.type_table.borrow();
        let flat_param_types = compute_export_flat_param_types(world_params, tir_modules, &tt);
        drop(tt);

        let flat_params: Vec<TirParam> = flat_param_types
            .iter()
            .enumerate()
            .map(|(i, &vt)| TirParam {
                name: format!("__p{i}"),
                type_id: cm_val_type_to_type_id(vt),
                local_index: i as u32,
                is_mut: false,
                is_mut_ref: false,
                span: synth_span(),
            })
            .collect();

        let flat_count = flat_params.len() as u32;
        for p in &flat_params {
            locals.push(param_local(&p.name, p.type_id, false));
        }

        let mut next_local_tmp = flat_count;
        let flat_param_locals: Vec<u32> = (0..flat_count).collect();

        let mut lifted_args = Vec::new();
        let mut flat_offset = 0;
        for (i, (_name, param_ty)) in world_params.iter().enumerate() {
            let user_type_id = user_func_ref
                .params
                .get(i)
                .map(|p| p.type_id)
                .unwrap_or(TypeTable::I32);
            let (lifted, consumed) = synthesize_lift_from_flat_params(
                param_ty,
                &flat_param_locals[flat_offset..],
                &flat_param_types[flat_offset..],
                user_type_id,
                &mut next_local_tmp,
                body_stmts,
                locals,
                tir_modules,
                lift_ctx,
            );
            lifted_args.push(lifted);
            flat_offset += consumed;
        }

        (flat_params, lifted_args, next_local_tmp)
    } else {
        let params: Vec<TirParam> = user_func_ref
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| TirParam {
                name: p.name.clone(),
                type_id: p.type_id,
                local_index: i as u32,
                is_mut: false,
                is_mut_ref: false,
                span: synth_span(),
            })
            .collect();

        let count = params.len() as u32;
        for p in &params {
            locals.push(param_local(&p.name, p.type_id, false));
        }

        let args: Vec<TirExpr> = params
            .iter()
            .map(|p| local_ref(p.local_index, &p.name, p.type_id))
            .collect();

        (params, args, count)
    }
}

/// Selects the epilogue of [`synthesize_export_binding`]: what the binding
/// does with the user function's result. The prelude (lift flat CM params,
/// call the user function once) is shared by every strategy.
#[derive(Clone, Copy)]
pub(super) enum ExportReturnStrategy {
    /// Async `() -> ()` export: call, then `task-return(0)` (the Ok
    /// discriminant for `result<_, _>`).
    VoidTaskReturn,
    /// Sync `--lib` export: the synchronous canon lift returns the lowered
    /// value directly — a single-core-value result verbatim, a multi-value
    /// result (string, list, tuple, record, option, result) via an i32
    /// out-pointer into fresh linear memory.
    SyncReturn,
    /// `export async fn`: the user body performs its own `task return`
    /// (expanded by `expand_task_returns_in_func`), so the binding only
    /// lifts params and calls.
    AsyncTaskReturn,
    /// Async export returning `Result<T, E>`: match Ok/Err, lower the active
    /// payload into the joined flat slots, then `task-return`.
    ResultTaskReturn,
}

/// Compilation context an export binding is synthesized against.
pub(super) struct ExportBindingEnv<'a> {
    pub tir_modules: &'a IndexMap<ModuleSource, TirModule>,
    pub type_table: &'a Rc<RefCell<TypeTable>>,
    /// World-declared export params (CM surface types).
    pub world_params: &'a [(String, Type)],
    /// World-declared export return type (CM surface type).
    pub world_return: Option<&'a Type>,
    pub cm_interface_registry: &'a CmInterfaceRegistry,
    pub cm_package: &'a str,
    pub interner: &'a RefCell<ModuleSourceInterner>,
}

impl<'a> ExportBindingEnv<'a> {
    fn lift_ctx(&self) -> LiftContext<'a> {
        LiftContext {
            cm_interface_registry: self.cm_interface_registry,
            type_table: self.type_table,
            cm_package: self.cm_package,
            interner: self.interner,
        }
    }

    fn lower_ctx(&self) -> LowerContext<'a> {
        LowerContext {
            cm_interface_registry: self.cm_interface_registry,
            type_table: self.type_table,
            wasi_package: self.cm_package,
            names: super::types::CmStdlibNames::from_compiler_items(
                self.type_table.borrow().compiler_items(),
            ),
        }
    }
}

/// Synthesize the CM export binding for a world export: lift flat CM params
/// to Wado-typed values, call the user's export function once, then run the
/// `strategy` epilogue on the result.
pub(super) fn synthesize_export_binding(
    export_name: &str,
    user_func: &Rc<RefCell<TirFunction>>,
    callee_module: &ModuleSource,
    env: &ExportBindingEnv<'_>,
    strategy: ExportReturnStrategy,
) -> Rc<RefCell<TirFunction>> {
    let binding_name = export_binding_func_name(export_name);
    let mut body_stmts: Vec<TirStmt> = Vec::new();
    let mut locals: Vec<TirLocal> = Vec::new();
    let lift_ctx = env.lift_ctx();

    let user_func_ref = user_func.borrow();
    let user_return_type = user_func_ref.return_type;
    let (adapter_params, call_args, param_count) = build_export_adapter_params(
        &user_func_ref,
        env.tir_modules,
        env.world_params,
        lift_ctx,
        &mut body_stmts,
        &mut locals,
    );
    drop(user_func_ref);
    let mut next_local = param_count;

    let call_return_type = match strategy {
        ExportReturnStrategy::VoidTaskReturn | ExportReturnStrategy::AsyncTaskReturn => {
            TypeTable::UNIT
        }
        ExportReturnStrategy::SyncReturn | ExportReturnStrategy::ResultTaskReturn => {
            user_return_type
        }
    };
    let call_user = build_call_user(user_func, callee_module, call_args, call_return_type);

    let adapter_return = match strategy {
        ExportReturnStrategy::VoidTaskReturn => {
            body_stmts.push(expr_stmt(call_user));
            body_stmts.push(expr_stmt(cm_raw_call(
                "task-return",
                vec![i32_const(0)],
                TypeTable::UNIT,
            )));
            TypeTable::UNIT
        }
        ExportReturnStrategy::AsyncTaskReturn => {
            body_stmts.push(expr_stmt(call_user));
            TypeTable::UNIT
        }
        ExportReturnStrategy::SyncReturn => push_sync_return_epilogue(
            call_user,
            user_return_type,
            env,
            lift_ctx,
            &mut next_local,
            &mut body_stmts,
            &mut locals,
        ),
        ExportReturnStrategy::ResultTaskReturn => {
            push_result_task_return_epilogue(
                call_user,
                user_return_type,
                env,
                lift_ctx,
                &mut next_local,
                &mut body_stmts,
                &mut locals,
            );
            TypeTable::UNIT
        }
    };

    finalize_export_binding(
        binding_name,
        adapter_params,
        adapter_return,
        body_stmts,
        locals,
    )
}

/// Build the call to the user's export function, threading each user param's
/// `is_mut` into its `CallArg` (lifted args beyond the user param list are
/// immutable).
fn build_call_user(
    user_func: &Rc<RefCell<TirFunction>>,
    callee_module: &ModuleSource,
    call_args: Vec<TirExpr>,
    return_type: TypeId,
) -> TirExpr {
    let user_func_ref = user_func.borrow();
    let param_is_mut: Vec<bool> = user_func_ref.params.iter().map(|p| p.is_mut).collect();
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef::from_resolved(&user_func_ref, callee_module.clone()),
            type_args: vec![],
            args: call_args
                .into_iter()
                .zip(param_is_mut.into_iter().chain(std::iter::repeat(false)))
                .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
                .collect(),
        },
        return_type,
        synth_span(),
    )
}

/// Wrap the synthesized body into the binding `TirFunction`, marked as a CM
/// export so DCE keeps it as a root.
fn finalize_export_binding(
    binding_name: String,
    params: Vec<TirParam>,
    return_type: TypeId,
    body_stmts: Vec<TirStmt>,
    locals: Vec<TirLocal>,
) -> Rc<RefCell<TirFunction>> {
    let local_count = locals.len() as u32;
    let binding = make_binding_function(
        binding_name,
        params,
        return_type,
        block(body_stmts),
        local_count,
        locals,
    );
    {
        let mut b = binding.borrow_mut();
        b.is_export = true;
        b.is_cm_export = true;
    }
    binding
}

/// `SyncReturn` epilogue: the synchronous canon lift expects the core
/// function to return the flattened result — directly when it flattens to a
/// single core value, or via a single i32 out-pointer into linear memory for
/// multi-value results (strings, lists, tuples, records, options, results).
/// Returns the adapter's core return type.
fn push_sync_return_epilogue(
    call_user: TirExpr,
    user_return_type: TypeId,
    env: &ExportBindingEnv<'_>,
    lift_ctx: LiftContext<'_>,
    next_local: &mut u32,
    body_stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
) -> TypeId {
    let is_unit_return = matches!(
        env.type_table.borrow().get(user_return_type),
        ResolvedType::Unit
    );
    let flat_count = env
        .world_return
        .map(|ty| {
            let tt = env.type_table.borrow();
            compute_export_flat_return_types(ty, env.tir_modules, &tt).len()
        })
        .unwrap_or(0);

    if is_unit_return {
        body_stmts.push(expr_stmt(call_user));
        TypeTable::UNIT
    } else if flat_count > 1 {
        let ty = env
            .world_return
            .expect("multi-flat result implies a return type");
        let result_local = alloc_local(next_local, locals, user_return_type);
        body_stmts.push(let_stmt(
            "__result",
            result_local,
            user_return_type,
            call_user,
        ));

        let size = crate::component_model::cm_size_with_registry_scoped(
            ty,
            env.cm_interface_registry,
            Some(env.cm_package),
        );
        let align = crate::component_model::cm_align_with_registry_scoped(
            ty,
            env.cm_interface_registry,
            Some(env.cm_package),
        );
        let ptr_local = alloc_local(next_local, locals, TypeTable::I32);
        body_stmts.push(let_stmt(
            "__ret_ptr",
            ptr_local,
            TypeTable::I32,
            builtin_call(
                "realloc",
                vec![
                    i32_const(0),
                    i32_const(0),
                    i32_const(align as i32),
                    i32_const(size as i32),
                ],
                TypeTable::I32,
            ),
        ));

        body_stmts.extend(synthesize_lower_wasi_type_to_memory(
            ty,
            local_ref(result_local, "__result", user_return_type),
            local_ref(ptr_local, "__ret_ptr", TypeTable::I32),
            next_local,
            locals,
            &env.lower_ctx(),
        ));
        body_stmts.push(return_stmt(Some(local_ref(
            ptr_local,
            "__ret_ptr",
            TypeTable::I32,
        ))));
        TypeTable::I32
    } else {
        let result_local = alloc_local(next_local, locals, user_return_type);
        body_stmts.push(let_stmt(
            "__result",
            result_local,
            user_return_type,
            call_user,
        ));
        let flat = synthesize_lower_to_flat(
            local_ref(result_local, "__result", user_return_type),
            user_return_type,
            next_local,
            body_stmts,
            locals,
            env.tir_modules,
            lift_ctx,
        );
        match flat.first() {
            None => TypeTable::UNIT,
            Some(f) => {
                let tid = cm_val_type_to_type_id(f.cm_type);
                body_stmts.push(return_stmt(Some(local_ref(f.index, "__flat", tid))));
                tid
            }
        }
    }
}

/// The core function named by a sync lift's `post-return` canonical option.
/// Distinct from [`export_binding_func_name`] so the core module exports both.
pub(super) fn post_return_func_name(export_name: &str) -> String {
    format!("__cm_post_return__{export_name}")
}

/// Synthesize the `post-return` function for a sync-lifted export, or `None`
/// when nothing was allocated for it to reclaim.
///
/// The gate is the indirect return, not memory ownership: a result wider than
/// one core value comes back through a guest-allocated area that leaks without
/// this, even when nothing hangs off it — a record of two `u32` owns no memory
/// and still loses its eight bytes per call. Ownership only decides whether the
/// walk over nested buffers emits anything.
///
/// The canonical ABI calls it with the lifted core function's results, which in
/// the indirect case is that single out-pointer.
pub(super) fn synthesize_post_return(
    export_name: &str,
    env: &ExportBindingEnv<'_>,
) -> Option<Rc<RefCell<TirFunction>>> {
    let ty = env.world_return?;
    let flat_count = {
        let tt = env.type_table.borrow();
        compute_export_flat_return_types(ty, env.tir_modules, &tt).len()
    };
    // The condition `push_sync_return_epilogue` allocates the area under.
    if flat_count <= 1 {
        return None;
    }

    let names =
        super::types::CmStdlibNames::from_compiler_items(env.type_table.borrow().compiler_items());
    let shape_ctx = CmShapeContext {
        cm_interface_registry: env.cm_interface_registry,
        cm_package: env.cm_package,
        names: &names,
    };
    let shape = cm_shape(ty, &shape_ctx);

    let ptr = param_local("__ret_ptr", TypeTable::I32, false);
    let mut locals = vec![ptr];
    let mut next_local = 1;
    let addr = local_ref(0, "__ret_ptr", TypeTable::I32);

    let mut body = synthesize_free_cm_value(&shape, &addr, &mut next_local, &mut locals);
    let size = crate::component_model::cm_size_with_registry_scoped(
        ty,
        env.cm_interface_registry,
        Some(env.cm_package),
    );
    let align = crate::component_model::cm_align_with_registry_scoped(
        ty,
        env.cm_interface_registry,
        Some(env.cm_package),
    );
    body.push(expr_stmt(builtin_call(
        "realloc",
        vec![
            addr,
            i32_const(size as i32),
            i32_const(align as i32),
            i32_const(0),
        ],
        TypeTable::I32,
    )));

    Some(finalize_export_binding(
        post_return_func_name(export_name),
        vec![TirParam {
            name: "__ret_ptr".to_string(),
            type_id: TypeTable::I32,
            local_index: 0,
            is_mut: false,
            is_mut_ref: false,
            span: synth_span(),
        }],
        TypeTable::UNIT,
        body,
        locals,
    ))
}

/// `ResultTaskReturn` epilogue: store the `Result<T, E>` value, zero the flat
/// task-return slots, match Ok/Err to lower the active payload into the
/// joined slots ([`lower_result_arm`]), then call `task-return` once with the
/// filled slots.
fn push_result_task_return_epilogue(
    call_user: TirExpr,
    user_return_type: TypeId,
    env: &ExportBindingEnv<'_>,
    lift_ctx: LiftContext<'_>,
    next_local: &mut u32,
    body_stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
) {
    let return_ast = env
        .world_return
        .expect("Result export binding requires a world return type");
    let flat_return_types = {
        let tt = env.type_table.borrow();
        compute_export_flat_return_types(return_ast, env.tir_modules, &tt)
    };

    let result_local = alloc_local(next_local, locals, user_return_type);
    body_stmts.push(let_stmt(
        "__result",
        result_local,
        user_return_type,
        call_user,
    ));

    let tt = env.type_table.borrow();
    let (ok_type_id, err_type_id) = match tt.get(user_return_type) {
        ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
            (type_args[0], type_args[1])
        }
        other => panic!("expected Result type for export binding return, got: {other:?}"),
    };
    let names = super::types::CmStdlibNames::from_compiler_items(tt.compiler_items());
    drop(tt);

    // Mutable flat slots holding the flattened task-return args, zeroed.
    let flat_locals: Vec<(u32, String)> = flat_return_types
        .iter()
        .enumerate()
        .map(|(i, &vt)| {
            let type_id = cm_val_type_to_type_id(vt);
            let local = alloc_local(next_local, locals, type_id);
            let name = format!("__tv_{i}");
            body_stmts.push(let_mut_stmt(&name, local, type_id, cm_zero(vt)));
            (local, name)
        })
        .collect();

    // Payload binding locals are allocated unconditionally so the arm
    // patterns can reference them even when the payload has no flat slots
    // (`Ok(())`-style); a bound-but-unused binding is fine for wir_build.
    let ok_payload_local = alloc_local(next_local, locals, ok_type_id);
    let ok_payload_name = format!("__ok_val_{ok_payload_local}");
    let ok_stmts = lower_result_arm(
        0,
        ok_payload_local,
        &ok_payload_name,
        ok_type_id,
        &flat_locals,
        &flat_return_types,
        next_local,
        locals,
        env.tir_modules,
        lift_ctx,
    );

    let err_payload_local = alloc_local(next_local, locals, err_type_id);
    let err_payload_name = format!("__err_val_{err_payload_local}");
    let err_stmts = lower_result_arm(
        1,
        err_payload_local,
        &err_payload_name,
        err_type_id,
        &flat_locals,
        &flat_return_types,
        next_local,
        locals,
        env.tir_modules,
        lift_ctx,
    );

    let span = synth_span();
    let arm = |case_name: &str,
               payload_name: String,
               payload_local: u32,
               payload_type: TypeId,
               stmts: Vec<TirStmt>| TirMatchArm {
        pattern: TirPattern::Variant {
            enum_type: user_return_type,
            variant_name: case_name.to_string(),
            bindings: vec![TirPattern::Binding {
                name: payload_name,
                local_index: payload_local,
                type_id: payload_type,
            }],
            payload_type,
        },
        guard: None,
        body: TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, span)),
            TypeTable::UNIT,
            span,
        ),
        span,
    };
    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(local_ref(result_local, "__result", user_return_type)),
            arms: vec![
                arm(
                    &names.ok_name,
                    ok_payload_name,
                    ok_payload_local,
                    ok_type_id,
                    ok_stmts,
                ),
                arm(
                    &names.err_name,
                    err_payload_name,
                    err_payload_local,
                    err_type_id,
                    err_stmts,
                ),
            ],
        },
        TypeTable::UNIT,
        span,
    );
    body_stmts.push(TirStmt::new(TirStmtKind::Expr(match_expr), span));

    let task_return_args: Vec<TirExpr> = flat_locals
        .iter()
        .zip(flat_return_types.iter())
        .map(|((local, name), &vt)| local_ref(*local, name, cm_val_type_to_type_id(vt)))
        .collect();
    body_stmts.push(expr_stmt(cm_raw_call(
        "task-return",
        task_return_args,
        TypeTable::UNIT,
    )));
}

/// Build one arm body of the Result epilogue's Ok/Err match: set the
/// discriminant slot to `disc`, then lower the bound payload into the joined
/// payload slots. A named `variant` payload goes through
/// [`synthesize_variant_lower_to_flat`]; everything else through
/// [`synthesize_lower_to_flat`].
#[allow(clippy::too_many_arguments)]
fn lower_result_arm(
    disc: i32,
    payload_local: u32,
    payload_name: &str,
    payload_type_id: TypeId,
    flat_locals: &[(u32, String)],
    flat_return_types: &[cm_abi::CmValType],
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    lift_ctx: LiftContext<'_>,
) -> Vec<TirStmt> {
    let mut arm_stmts: Vec<TirStmt> = Vec::new();
    arm_stmts.push(expr_stmt(assign(
        local_ref(
            flat_locals[0].0,
            &flat_locals[0].1,
            cm_val_type_to_type_id(flat_return_types[0]),
        ),
        i32_const(disc),
    )));

    let payload_resolved = lift_ctx.type_table.borrow().get(payload_type_id).clone();
    if let ResolvedType::Variant { name, .. } = &payload_resolved {
        if let Some(variant_decl) = find_variant_decl(name, tir_modules) {
            synthesize_variant_lower_to_flat(
                payload_local,
                payload_type_id,
                &variant_decl,
                &flat_locals[1..],
                &flat_return_types[1..],
                next_local,
                &mut arm_stmts,
                locals,
                tir_modules,
                lift_ctx,
            );
        } else if flat_locals.len() > 1 {
            arm_stmts.push(expr_stmt(assign(
                local_ref(
                    flat_locals[1].0,
                    &flat_locals[1].1,
                    cm_val_type_to_type_id(flat_return_types[1]),
                ),
                local_ref(payload_local, payload_name, payload_type_id),
            )));
        }
    } else {
        let payload_flat_types = {
            let tt = lift_ctx.type_table.borrow();
            flat_types_from_type_id(payload_type_id, tir_modules, &tt)
        };
        if !payload_flat_types.is_empty() {
            let lowered = synthesize_lower_to_flat(
                local_ref(payload_local, payload_name, payload_type_id),
                payload_type_id,
                next_local,
                &mut arm_stmts,
                locals,
                tir_modules,
                lift_ctx,
            );
            assign_flat_values_into_slots(
                &lowered,
                &flat_locals[1..],
                &flat_return_types[1..],
                &mut arm_stmts,
            );
        }
    }
    arm_stmts
}

/// Synthesize TIR for lowering a variant value to flat CM locals.
///
/// Generates:
/// ```text
/// flat[0] = variant_tag(value)  // discriminant
/// if variant_test(value, case_0) { flat[1..] = lower(payload_0) }
/// if variant_test(value, case_1) { flat[1..] = lower(payload_1) }
/// ...
/// ```
pub(super) fn synthesize_variant_lower_to_flat(
    value_local: u32,
    value_type_id: TypeId,
    variant_decl: &TirVariantDecl,
    flat_locals: &[(u32, String)], // flat locals for [disc, p2, p3, ...]
    flat_types: &[cm_abi::CmValType],
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    ctx: LiftContext<'_>,
) {
    // Set flat[0] = discriminant
    if !flat_locals.is_empty() {
        stmts.push(expr_stmt(assign(
            local_ref(
                flat_locals[0].0,
                &flat_locals[0].1,
                cm_val_type_to_type_id(flat_types[0]),
            ),
            variant_tag(local_ref(value_local, "__variant_val", value_type_id)),
        )));
    }

    // Per-case payload flattening as a single TIR `Match` over the
    // materialised variant local. Payload arms bind the payload to
    // a fresh local and write the flattened values into the shared
    // `flat_locals[1..]`. Unit-payload arms carry an empty body. The
    // Match is exhaustive on the variant so no wildcard arm is added.
    let span = synth_span();
    let mut arms: Vec<TirMatchArm> = Vec::with_capacity(variant_decl.cases.len());
    for case in &variant_decl.cases {
        let case_flat = {
            let tt = ctx.type_table.borrow();
            flat_types_from_type_id(case.payload, tir_modules, &tt)
        };

        let mut case_stmts: Vec<TirStmt> = Vec::new();
        let bindings: Vec<TirPattern> = if case_flat.is_empty() {
            Vec::new()
        } else {
            let payload_local = alloc_local(next_local, locals, case.payload);
            let payload_name = format!("__case_payload_{payload_local}");
            let lowered = synthesize_lower_to_flat(
                local_ref(payload_local, &payload_name, case.payload),
                case.payload,
                next_local,
                &mut case_stmts,
                locals,
                tir_modules,
                ctx,
            );
            assign_flat_values_into_slots(
                &lowered,
                &flat_locals[1..],
                &flat_types[1..],
                &mut case_stmts,
            );
            vec![TirPattern::Binding {
                name: payload_name,
                local_index: payload_local,
                type_id: case.payload,
            }]
        };

        arms.push(TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: value_type_id,
                variant_name: case.name.clone(),
                bindings,
                payload_type: case.payload,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(TirBlock::new(case_stmts, span)),
                TypeTable::UNIT,
                span,
            ),
            span,
        });
    }
    if !arms.is_empty() {
        let match_expr = TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(local_ref(value_local, "__variant_val", value_type_id)),
                arms,
            },
            TypeTable::UNIT,
            span,
        );
        stmts.push(TirStmt::new(TirStmtKind::Expr(match_expr), span));
    }
}
