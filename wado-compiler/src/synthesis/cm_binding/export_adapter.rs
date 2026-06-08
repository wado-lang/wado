//! Export-side CM adapter synthesis.
//!
//! For each world export the user defines, the driver picks one of these
//! synthesizers based on the function shape:
//!
//! - [`synthesize_void_export_binding`] for `() -> ()` exports.
//! - [`synthesize_general_export_binding`] for non-Result returns.
//! - [`synthesize_result_export_binding`] for `Result<T, E>` returns.
//! - [`synthesize_async_export_binding`] for `export async fn` (whose body
//!   does its own `task return`).
//! - [`synthesize_void_stub_adapter`] for worlds that declare an export the
//!   user did not define (test-only files).
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
    TirStructField, TirVariantDecl, TypeId, TypeTable,
};

use crate::synthesis::common::{
    alloc_local, assign, binary, block, break_stmt, builtin_call, cast, cm_raw_call, expr_stmt,
    generic_method_call, i32_const, i64_const, if_stmt, internal_call, let_mut_stmt, let_stmt,
    local_ref, loop_stmt, option_none, option_some, param_local, return_stmt, synth_span,
};

use super::import_adapter::make_binding_function;
use super::lift::synthesize_lift;
use super::lower::synthesize_lower_wasi_type_to_memory;
use super::types::{
    LiftContext, binary_add, binary_ne, cm_val_type_to_type_id, cm_zero,
    compute_export_flat_param_types, export_needs_param_lifting, field_access, find_struct_decl,
    find_variant_decl, flat_types_from_type_id, flatten_export_type, type_id_to_ast_type,
    variant_payload, variant_tag, variant_test,
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
        ResolvedType::Resource { .. } | ResolvedType::Enum { .. } => {
            // Resource handles and enums are i32
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

            // ptr = packed as i32
            let ptr = cast(
                local_ref(packed_local, "__packed", TypeTable::I64),
                TypeTable::I32,
            );
            let ptr_local = alloc_local(next_local, locals, TypeTable::I32);
            stmts.push(let_stmt("__ptr", ptr_local, TypeTable::I32, ptr));

            // len = (packed >> 32) as i32
            let shifted = binary(
                TirBinaryOp::Shr,
                local_ref(packed_local, "__packed", TypeTable::I64),
                i64_const(32),
                TypeTable::I64,
            );
            let len = cast(shifted, TypeTable::I32);
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
            let elem_type_id = type_args[0];
            let elem_ast_type = {
                let tt = ctx.type_table.borrow();
                type_id_to_ast_type(elem_type_id, &tt, ctx.cm_interface_registry)
            };
            let elem_size = crate::component_model::cm_size_with_registry(
                &elem_ast_type,
                ctx.cm_interface_registry,
            );
            let elem_align = crate::component_model::cm_align_with_registry(
                &elem_ast_type,
                ctx.cm_interface_registry,
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
                ctx.cm_interface_registry,
                ctx.cm_package,
                ctx.type_table,
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
        } if name == "Option" && type_args.len() == 1 => {
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
                let inner_locals: Vec<(u32, cm_abi::CmValType, String)> = inner_flat_types
                    .iter()
                    .enumerate()
                    .map(|(i, &vt)| {
                        let tid = cm_val_type_to_type_id(vt);
                        let l = alloc_local(next_local, locals, tid);
                        let name = format!("__opt_inner_{i}");
                        stmts.push(let_mut_stmt(&name, l, tid, cm_zero(vt)));
                        (l, vt, name)
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
                for (i, flat_val) in inner_lowered.iter().enumerate() {
                    if i < inner_locals.len() {
                        let (target_local, target_vt, ref target_name) = inner_locals[i];
                        let target_type = cm_val_type_to_type_id(target_vt);
                        let source_type = cm_val_type_to_type_id(flat_val.cm_type);
                        let mut val = local_ref(flat_val.index, "__flat", source_type);
                        if flat_val.cm_type != target_vt {
                            val = cast(val, target_type);
                        }
                        then_stmts.push(expr_stmt(assign(
                            local_ref(target_local, target_name, target_type),
                            val,
                        )));
                    }
                }

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

                for (l, vt, _) in inner_locals {
                    result.push(FlatLocal {
                        index: l,
                        cm_type: vt,
                    });
                }
            }

            result
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
                let local = alloc_local(next_local, locals, TypeTable::I32);
                stmts.push(let_stmt("__flat", local, TypeTable::I32, value));
                vec![FlatLocal {
                    index: local,
                    cm_type: cm_abi::CmValType::I32,
                }]
            }
        }
        _ => {
            // For other types (including complex variants, newtypes, etc.), lower as i32
            let local = alloc_local(next_local, locals, TypeTable::I32);
            stmts.push(let_stmt("__flat", local, TypeTable::I32, value));
            vec![FlatLocal {
                index: local,
                cm_type: cm_abi::CmValType::I32,
            }]
        }
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
pub(super) fn synthesize_lift_from_flat_params(
    ty: &Type,
    flat_param_locals: &[u32],
    flat_types: &[cm_abi::CmValType],
    target_type_id: TypeId,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table_cell: &RefCell<TypeTable>,
    lift_ctx: LiftContext<'_>,
) -> (TirExpr, usize) {
    let names =
        super::types::CmStdlibNames::from_compiler_items(type_table_cell.borrow().compiler_items());
    match ty {
        Type::Named(named) if named.name == names.string => {
            // String flat ABI: (ptr: i32, len: i32) pointing to linear memory
            let ptr = local_ref(flat_param_locals[0], "__p", TypeTable::I32);
            let len = local_ref(flat_param_locals[1], "__p", TypeTable::I32);
            let lifted = internal_call("memory_to_gc_string", vec![ptr, len], target_type_id);
            (lifted, 2)
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
                            type_table_cell,
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
                // Resource handles, enums, unknown types → i32 passthrough
                (local_ref(flat_param_locals[0], "__p", TypeTable::I32), 1)
            }
        },
        Type::Generic(generic) => match generic.name.as_str() {
            n if n == names.array
                && generic.args.len() == 1
                && matches!(generic.args[0], Type::Named(ref n) if n.name == "u8") =>
            {
                // List<u8> flat ABI: (ptr: i32, len: i32) pointing to linear memory
                let ptr = local_ref(flat_param_locals[0], "__p", TypeTable::I32);
                let len = local_ref(flat_param_locals[1], "__p", TypeTable::I32);
                let lifted = internal_call("memory_to_gc_array", vec![ptr, len], target_type_id);
                (lifted, 2)
            }
            n if n == names.array => {
                // list<T> flat ABI: (ptr: i32, len: i32) — elements in linear memory
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
                let lifted = synthesize_lift(
                    ty,
                    local_ref(tmp_ptr_local, "__lift_tmp", TypeTable::I32),
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
            "Option" if generic.args.len() == 1 => {
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
                        type_table_cell,
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
                    type_table_cell,
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
        _ => (
            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
            0,
        ),
    }
}

/// Synthesize a CM export binding for an async export with a Result return type.
///
/// The binding:
/// 1. Lifts flat CM params to Wado-typed values (if needed)
/// 2. Calls the user's export function with lifted args
/// 3. Pattern-matches the Result<T, E> return value
/// 4. For Ok(T): lowers T to flat CM values, calls task-return
/// 5. For Err(E): lowers E to flat CM values, calls task-return
///
/// This is signature-driven: it examines the param/return types to generate
/// appropriate lifting/lowering code for any export signature.
pub(super) fn synthesize_result_export_binding(
    export_name: &str,
    user_func: Rc<RefCell<TirFunction>>,
    entry_source: &ModuleSource,
    _return_type: &Type,
    flat_return_types: &[cm_abi::CmValType],
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    world_params: &[(String, Type)],
    cm_interface_registry: &CmInterfaceRegistry,
    cm_package: &str,
    interner: &RefCell<ModuleSourceInterner>,
) -> Rc<RefCell<TirFunction>> {
    let binding_name = export_binding_func_name(export_name);
    let mut body_stmts: Vec<TirStmt> = Vec::new();
    let mut locals: Vec<TirLocal> = Vec::new();

    let user_func_ref = user_func.borrow();
    let user_return_type = user_func_ref.return_type;
    let needs_lifting = export_needs_param_lifting(&user_func_ref.params, type_table);
    let lift_ctx = LiftContext {
        cm_interface_registry,
        type_table,
        cm_package,
        interner,
    };

    // Build adapter params and call args
    let (adapter_params, call_args, param_count) = if needs_lifting {
        // Compute flat param types from world export signature
        let tt = type_table.borrow();
        let flat_param_types = compute_export_flat_param_types(world_params, tir_modules, &tt);
        drop(tt);

        // Create adapter params with flat types
        let flat_params: Vec<TirParam> = flat_param_types
            .iter()
            .enumerate()
            .map(|(i, &vt)| TirParam {
                name: format!("__p{i}"),
                type_id: cm_val_type_to_type_id(vt),
                local_index: i as u32,
                is_mut: false,
                span: synth_span(),
            })
            .collect();

        let flat_count = flat_params.len() as u32;
        for p in &flat_params {
            locals.push(param_local(&p.name, p.type_id, false));
        }

        let mut next_local_tmp = flat_count;
        let flat_param_locals: Vec<u32> = (0..flat_count).collect();

        // Lift flat params to Wado-typed call args
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
                &mut body_stmts,
                &mut locals,
                tir_modules,
                type_table,
                lift_ctx,
            );
            lifted_args.push(lifted);
            flat_offset += consumed;
        }

        (flat_params, lifted_args, next_local_tmp)
    } else {
        // No lifting needed — pass params through (resource handles, primitives)
        let params: Vec<TirParam> = user_func_ref
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| TirParam {
                name: p.name.clone(),
                type_id: p.type_id,
                local_index: i as u32,
                is_mut: false,
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
    };

    let mut next_local = param_count;

    // Call user function — derive param_is_mut from the actual function params.
    let call_user_param_is_mut: Vec<bool> =
        user_func.borrow().params.iter().map(|p| p.is_mut).collect();
    let call_user = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef::from_resolved(&user_func.borrow(), entry_source.clone()),
            type_args: vec![],
            args: call_args
                .into_iter()
                .zip(
                    call_user_param_is_mut
                        .into_iter()
                        .chain(std::iter::repeat(false)),
                )
                .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
                .collect(),
        },
        user_return_type,
        synth_span(),
    );

    // Store result in a local
    let result_local = alloc_local(&mut next_local, &mut locals, user_return_type);
    body_stmts.push(let_stmt(
        "__result",
        result_local,
        user_return_type,
        call_user,
    ));

    // Determine Ok and Err type IDs from the Result type
    let tt = type_table.borrow();
    let (ok_type_id, err_type_id) = match tt.get(user_return_type) {
        ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
            (type_args[0], type_args[1])
        }
        _ => {
            // Not a Result — shouldn't happen for result export bindings
            panic!(
                "Expected Result type for export binding return, got: {:?}",
                tt.get(user_return_type)
            );
        }
    };
    let (_, _, ok_case_name, ok_case_index) = tt
        .compiler_items()
        .require_variant_case(crate::compiler_item::CompilerItem::ResultOk);
    let ok_case_name = ok_case_name.to_string();
    let (_, _, err_case_name, err_case_index) = tt
        .compiler_items()
        .require_variant_case(crate::compiler_item::CompilerItem::ResultErr);
    let err_case_name = err_case_name.to_string();
    drop(tt);

    // Allocate mutable flat value locals (initialized to zero)
    // These hold the flattened task-return args
    let flat_locals: Vec<(u32, String)> = flat_return_types
        .iter()
        .enumerate()
        .map(|(i, &vt)| {
            let type_id = cm_val_type_to_type_id(vt);
            let local = alloc_local(&mut next_local, &mut locals, type_id);
            let name = format!("__tv_{i}");
            body_stmts.push(let_mut_stmt(&name, local, type_id, cm_zero(vt)));
            (local, name)
        })
        .collect();

    // Determine Ok flat-arg count up-front so the arm bodies know
    // whether to call `synthesize_lower_to_flat` on the payload local.
    let tt = type_table.borrow();
    let ok_flat_types = flat_types_from_type_id(ok_type_id, tir_modules, &tt);
    drop(tt);

    // Allocate the Ok payload binding local unconditionally so the
    // arm pattern can reference it even when the payload has no flat
    // slots (`Ok(())`-style); the binding is bound-but-unused in that
    // case, which wir_build handles cleanly.
    let ok_payload_local = alloc_local(&mut next_local, &mut locals, ok_type_id);
    let ok_payload_name = format!("__ok_val_{ok_payload_local}");

    // === Ok arm body ===
    let mut ok_stmts: Vec<TirStmt> = Vec::new();
    ok_stmts.push(expr_stmt(assign(
        local_ref(
            flat_locals[0].0,
            &flat_locals[0].1,
            cm_val_type_to_type_id(flat_return_types[0]),
        ),
        i32_const(0),
    )));
    if !ok_flat_types.is_empty() {
        let ok_lowered = synthesize_lower_to_flat(
            local_ref(ok_payload_local, &ok_payload_name, ok_type_id),
            ok_type_id,
            &mut next_local,
            &mut ok_stmts,
            &mut locals,
            tir_modules,
            lift_ctx,
        );
        for (i, flat_val) in ok_lowered.iter().enumerate() {
            if 1 + i < flat_locals.len() {
                let target_type = cm_val_type_to_type_id(flat_return_types[1 + i]);
                let source_type = cm_val_type_to_type_id(flat_val.cm_type);
                let mut val = local_ref(flat_val.index, "__flat", source_type);
                if flat_val.cm_type != flat_return_types[1 + i] {
                    val = cast(val, target_type);
                }
                ok_stmts.push(expr_stmt(assign(
                    local_ref(flat_locals[1 + i].0, &flat_locals[1 + i].1, target_type),
                    val,
                )));
            }
        }
    }
    let task_return_args: Vec<TirExpr> = flat_locals
        .iter()
        .zip(flat_return_types.iter())
        .map(|((local, name), &vt)| local_ref(*local, name, cm_val_type_to_type_id(vt)))
        .collect();
    ok_stmts.push(expr_stmt(cm_raw_call(
        "task-return",
        task_return_args,
        TypeTable::UNIT,
    )));
    ok_stmts.push(return_stmt(None));

    // === Err arm body ===
    let err_payload_local = alloc_local(&mut next_local, &mut locals, err_type_id);
    let err_payload_name = format!("__err_val_{err_payload_local}");
    let mut err_stmts: Vec<TirStmt> = Vec::new();
    err_stmts.push(expr_stmt(assign(
        local_ref(
            flat_locals[0].0,
            &flat_locals[0].1,
            cm_val_type_to_type_id(flat_return_types[0]),
        ),
        i32_const(1),
    )));
    let err_resolved = type_table.borrow().get(err_type_id).clone();
    if let ResolvedType::Variant { name, .. } = &err_resolved {
        if let Some(variant_decl) = find_variant_decl(name, tir_modules) {
            synthesize_variant_lower_to_flat(
                err_payload_local,
                err_type_id,
                &variant_decl,
                &flat_locals[1..],
                &flat_return_types[1..],
                &mut next_local,
                &mut err_stmts,
                &mut locals,
                tir_modules,
                lift_ctx,
            );
        } else if flat_locals.len() > 1 {
            err_stmts.push(expr_stmt(assign(
                local_ref(
                    flat_locals[1].0,
                    &flat_locals[1].1,
                    cm_val_type_to_type_id(flat_return_types[1]),
                ),
                local_ref(err_payload_local, &err_payload_name, err_type_id),
            )));
        }
    } else {
        let err_lowered = synthesize_lower_to_flat(
            local_ref(err_payload_local, &err_payload_name, err_type_id),
            err_type_id,
            &mut next_local,
            &mut err_stmts,
            &mut locals,
            tir_modules,
            lift_ctx,
        );
        for (i, flat_val) in err_lowered.iter().enumerate() {
            if 1 + i < flat_locals.len() {
                let target_type = cm_val_type_to_type_id(flat_return_types[1 + i]);
                let source_type = cm_val_type_to_type_id(flat_val.cm_type);
                let mut val = local_ref(flat_val.index, "__flat", source_type);
                if flat_val.cm_type != flat_return_types[1 + i] {
                    val = cast(val, target_type);
                }
                err_stmts.push(expr_stmt(assign(
                    local_ref(flat_locals[1 + i].0, &flat_locals[1 + i].1, target_type),
                    val,
                )));
            }
        }
    }
    let task_return_args: Vec<TirExpr> = flat_locals
        .iter()
        .zip(flat_return_types.iter())
        .map(|((local, name), &vt)| local_ref(*local, name, cm_val_type_to_type_id(vt)))
        .collect();
    err_stmts.push(expr_stmt(cm_raw_call(
        "task-return",
        task_return_args,
        TypeTable::UNIT,
    )));
    err_stmts.push(return_stmt(None));

    // === Combine Ok/Err arms in a single TIR `Match` ===
    let span = synth_span();
    let ok_arm = TirMatchArm {
        pattern: TirPattern::Variant {
            enum_type: user_return_type,
            variant_name: ok_case_name,
            bindings: vec![TirPattern::Binding {
                name: ok_payload_name,
                local_index: ok_payload_local,
                type_id: ok_type_id,
            }],
            payload_type: ok_type_id,
        },
        guard: None,
        body: TirExpr::new(
            TirExprKind::Block(TirBlock::new(ok_stmts, span)),
            TypeTable::UNIT,
            span,
        ),
        span,
    };
    let err_arm = TirMatchArm {
        pattern: TirPattern::Variant {
            enum_type: user_return_type,
            variant_name: err_case_name,
            bindings: vec![TirPattern::Binding {
                name: err_payload_name,
                local_index: err_payload_local,
                type_id: err_type_id,
            }],
            payload_type: err_type_id,
        },
        guard: None,
        body: TirExpr::new(
            TirExprKind::Block(TirBlock::new(err_stmts, span)),
            TypeTable::UNIT,
            span,
        ),
        span,
    };
    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(local_ref(result_local, "__result", user_return_type)),
            arms: vec![ok_arm, err_arm],
        },
        TypeTable::UNIT,
        span,
    );
    body_stmts.push(TirStmt::new(TirStmtKind::Expr(match_expr), span));
    // Suppress unused-variable warning for the case-index locals: the
    // canonical Match path encodes the case via the pattern itself.
    let _ = (ok_case_index, err_case_index);

    // Fallthrough (unreachable because both arms return, but emit task-return just in case)
    let fallthrough_args: Vec<TirExpr> = flat_return_types.iter().map(|&vt| cm_zero(vt)).collect();
    body_stmts.push(expr_stmt(cm_raw_call(
        "task-return",
        fallthrough_args,
        TypeTable::UNIT,
    )));

    let body = block(body_stmts);
    let local_count = next_local;

    let binding = make_binding_function(
        binding_name,
        adapter_params,
        TypeTable::UNIT,
        body,
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
            variant_tag(local_ref(value_local, "__err_val", value_type_id)),
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
            for (i, flat_val) in lowered.iter().enumerate() {
                if 1 + i < flat_locals.len() {
                    let target_type = cm_val_type_to_type_id(flat_types[1 + i]);
                    let source_type = cm_val_type_to_type_id(flat_val.cm_type);
                    let mut val = local_ref(flat_val.index, "__flat", source_type);
                    if flat_val.cm_type != flat_types[1 + i] {
                        val = cast(val, target_type);
                    }
                    case_stmts.push(expr_stmt(assign(
                        local_ref(flat_locals[1 + i].0, &flat_locals[1 + i].1, target_type),
                        val,
                    )));
                }
            }
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
                expr: Box::new(local_ref(value_local, "__err_val", value_type_id)),
                arms,
            },
            TypeTable::UNIT,
            span,
        );
        stmts.push(TirStmt::new(TirStmtKind::Expr(match_expr), span));
    }
}

/// Synthesize a CM export binding for a `() -> ()` async export.
///
/// The binding calls the user's export function and then calls `task-return(0)`
/// to signal successful completion. This replaces the task-return wrapping
/// that was previously done at the codegen level.
///
/// Generated TIR (for export name "run"):
/// ```text
/// fn __cm_export__run() {
///     run();
///     cm_raw_call task-return(0);
/// }
/// ```
pub(super) fn synthesize_void_export_binding(
    export_name: &str,
    user_func: Rc<RefCell<TirFunction>>,
    entry_source: &ModuleSource,
) -> Rc<RefCell<TirFunction>> {
    let binding_name = export_binding_func_name(export_name);
    let mut body_stmts: Vec<TirStmt> = Vec::new();

    // Call the user's export function
    let call_user = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef::from_resolved(&user_func.borrow(), entry_source.clone()),
            type_args: vec![],
            args: vec![],
        },
        TypeTable::UNIT,
        synth_span(),
    );
    body_stmts.push(expr_stmt(call_user));

    // Call task-return(0) — Ok discriminant for result<_, _>
    body_stmts.push(expr_stmt(cm_raw_call(
        "task-return",
        vec![i32_const(0)],
        TypeTable::UNIT,
    )));

    let body = block(body_stmts);

    let binding = make_binding_function(binding_name, vec![], TypeTable::UNIT, body, 0, vec![]);
    // Mark as export so DCE keeps it as a root
    {
        let mut b = binding.borrow_mut();
        b.is_export = true;
        b.is_cm_export = true;
    }
    binding
}

/// Synthesize a CM export binding for a non-Result return type.
///
/// For exports where the user function returns a non-Result type (e.g., `-> i32`,
/// `-> String`, `-> ()`), the CM export still wraps the return in `result<T, error-context>`.
/// The binding calls `task-return(0, ...flat_values)` — Ok with the lowered return value.
///
/// This handles parameter lifting from flat CM params when needed.
///
/// Generated TIR (for `export fn add(a: i32, b: i32) -> i32`):
/// ```text
/// fn __cm_export__add(__p0: i32, __p1: i32) {
///     let __result = add(__p0, __p1);
///     let __flat0 = __result as i32;
///     cm_raw_call task-return(0, __flat0);
/// }
/// ```
///
/// Known limitation: flat return types are computed from the user's return type,
/// not from the world's `result<T, error-context>` wrapper. If `error-context`
/// has more flat slots than the Ok payload, the binding may emit too few
/// task-return args. In practice this is safe because:
/// - Current worlds use `Result<(), ()>` (no error-context) or `Result<T, E>`
///   (handled by `synthesize_result_export_binding`)
/// - This function is only used when the user doesn't return Result
pub(super) fn synthesize_general_export_binding(
    export_name: &str,
    user_func: Rc<RefCell<TirFunction>>,
    entry_source: &ModuleSource,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    world_params: &[(String, Type)],
    cm_interface_registry: &CmInterfaceRegistry,
    cm_package: &str,
    interner: &RefCell<ModuleSourceInterner>,
) -> Rc<RefCell<TirFunction>> {
    let binding_name = export_binding_func_name(export_name);
    let mut body_stmts: Vec<TirStmt> = Vec::new();
    let mut locals: Vec<TirLocal> = Vec::new();

    let user_func_ref = user_func.borrow();
    let user_return_type = user_func_ref.return_type;
    let needs_lifting = export_needs_param_lifting(&user_func_ref.params, type_table);
    let lift_ctx = LiftContext {
        cm_interface_registry,
        type_table,
        cm_package,
        interner,
    };

    // Build adapter params and call args
    let (adapter_params, call_args, param_count) = if needs_lifting {
        let tt = type_table.borrow();
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
                &mut body_stmts,
                &mut locals,
                tir_modules,
                type_table,
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
    };

    let mut next_local = param_count;

    // Call user function — derive param_is_mut from the actual function params.
    let call_user_param_is_mut: Vec<bool> =
        user_func.borrow().params.iter().map(|p| p.is_mut).collect();
    let call_user = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef::from_resolved(&user_func.borrow(), entry_source.clone()),
            type_args: vec![],
            args: call_args
                .into_iter()
                .zip(
                    call_user_param_is_mut
                        .into_iter()
                        .chain(std::iter::repeat(false)),
                )
                .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
                .collect(),
        },
        user_return_type,
        synth_span(),
    );

    // Check if return type is unit
    let tt = type_table.borrow();
    let is_unit_return = matches!(tt.get(user_return_type), ResolvedType::Unit);
    let return_flat_types = flat_types_from_type_id(user_return_type, tir_modules, &tt);
    drop(tt);

    if is_unit_return || return_flat_types.is_empty() {
        // Unit return — just call user function and task-return(0)
        body_stmts.push(expr_stmt(call_user));
        body_stmts.push(expr_stmt(cm_raw_call(
            "task-return",
            vec![i32_const(0)],
            TypeTable::UNIT,
        )));
    } else {
        // Non-unit return — lower return value and call task-return(0, ...flat_values)
        let result_local = alloc_local(&mut next_local, &mut locals, user_return_type);
        body_stmts.push(let_stmt(
            "__result",
            result_local,
            user_return_type,
            call_user,
        ));

        let lowered = synthesize_lower_to_flat(
            local_ref(result_local, "__result", user_return_type),
            user_return_type,
            &mut next_local,
            &mut body_stmts,
            &mut locals,
            tir_modules,
            lift_ctx,
        );

        // Build task-return args: [0 (Ok disc), ...flat_return_values]
        let mut task_return_args = vec![i32_const(0)];
        for flat_val in &lowered {
            let val_type = cm_val_type_to_type_id(flat_val.cm_type);
            task_return_args.push(local_ref(flat_val.index, "__flat", val_type));
        }

        body_stmts.push(expr_stmt(cm_raw_call(
            "task-return",
            task_return_args,
            TypeTable::UNIT,
        )));
    }

    let body = block(body_stmts);
    let local_count = next_local;

    let binding = make_binding_function(
        binding_name,
        adapter_params,
        TypeTable::UNIT,
        body,
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

/// Synthesize an export binding for `export async fn` functions.
///
/// The user function calls `task-return` internally via `task return expr` stmts
/// (which are expanded by `expand_task_returns_in_func`). The binding only needs to
/// lift flat CM params to Wado types and call the user function.
///
/// Generated TIR (for `export async fn handle(request: Request)`):
/// ```text
/// fn __cm_export__handle(__p0: i32, ...) {
///     let __request = lift_request(__p0, ...);
///     handle(__request);  // user fn calls task-return internally
/// }
/// ```
pub(super) fn synthesize_async_export_binding(
    export_name: &str,
    user_func: Rc<RefCell<TirFunction>>,
    entry_source: &ModuleSource,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    world_params: &[(String, Type)],
    cm_interface_registry: &CmInterfaceRegistry,
    cm_package: &str,
    interner: &RefCell<ModuleSourceInterner>,
) -> Rc<RefCell<TirFunction>> {
    let binding_name = export_binding_func_name(export_name);
    let mut body_stmts: Vec<TirStmt> = Vec::new();
    let mut locals: Vec<TirLocal> = Vec::new();

    let user_func_ref = user_func.borrow();
    let needs_lifting = export_needs_param_lifting(&user_func_ref.params, type_table);

    let (adapter_params, call_args) = if needs_lifting {
        let tt = type_table.borrow();
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
        let lift_ctx = LiftContext {
            cm_interface_registry,
            type_table,
            cm_package,
            interner,
        };
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
                &mut body_stmts,
                &mut locals,
                tir_modules,
                type_table,
                lift_ctx,
            );
            lifted_args.push(lifted);
            flat_offset += consumed;
        }

        (flat_params, lifted_args)
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
                span: synth_span(),
            })
            .collect();

        for p in &params {
            locals.push(param_local(&p.name, p.type_id, false));
        }

        let args: Vec<TirExpr> = params
            .iter()
            .map(|p| local_ref(p.local_index, &p.name, p.type_id))
            .collect();

        (params, args)
    };

    // Call user function (it returns unit; task-return is called internally)
    let call_user_param_is_mut: Vec<bool> =
        user_func.borrow().params.iter().map(|p| p.is_mut).collect();
    let call_user = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef::from_resolved(&user_func.borrow(), entry_source.clone()),
            type_args: vec![],
            args: call_args
                .into_iter()
                .zip(
                    call_user_param_is_mut
                        .into_iter()
                        .chain(std::iter::repeat(false)),
                )
                .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
                .collect(),
        },
        TypeTable::UNIT,
        synth_span(),
    );
    body_stmts.push(expr_stmt(call_user));
    // No task-return here: user function handles it via task return stmts.

    let body = block(body_stmts);
    let local_count = locals.len() as u32;

    let binding = make_binding_function(
        binding_name,
        adapter_params,
        TypeTable::UNIT,
        body,
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
