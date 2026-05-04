//! CM Binding Synthesis phase.
//!
//! Generates TIR binding functions for Component Model boundary crossing.
//! Each binding handles lifting Wado values to CM flat ABI (lowering params)
//! and lifting CM flat ABI values back to Wado types (lifting results).
//!
//! Pipeline position: after `effect_check`, before monomorphize.
//! This ensures binding functions go through monomorphization, lowering,
//! and optimization.
//!
//! See `docs/wep-2026-02-15-cm-binding-synthesis.md` for design details.

mod import_adapter;
mod lift;
mod lower;
mod resource_rewrite;
mod task_return;
mod type_fixup;
mod types;

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::Type;
use crate::cm_abi;
use crate::component_model::WasiRegistry;
use crate::name::ModuleSource;
use crate::package::Package;
use crate::tir::{
    CallArg, FunctionRef, TirExpr, TirExprKind, TirFunction, TirLocal, TirParam, TirStmt, TypeId,
    TypeTable,
};

use super::common::{
    alloc_local, assign, binary, block, break_stmt, builtin_call, cast, cm_raw_call, expr_stmt,
    generic_method_call, i32_const, i64_const, if_stmt, internal_call, let_mut_stmt, let_stmt,
    local_ref, loop_stmt, option_none, option_some, param_local, return_stmt, synth_span,
};

pub use import_adapter::binding_func_name;
pub use lift::{synthesize_lift, synthesize_lift_with_context};
pub use lower::synthesize_lower;
pub use types::{
    LiftContext, cm_enum_byte_size, cm_flags_byte_align, cm_flags_byte_size, flatten_param_type,
    wasi_type_to_type_id,
};
use lower::synthesize_lower_wasi_type_to_memory;
use resource_rewrite::{rewrite_cm_resource_methods, synthesize_record_stream_reads};
use import_adapter::{make_binding_function, synthesize_adapter};
use task_return::{expand_task_returns_in_func, strip_task_returns_in_func};
use type_fixup::{collect_effect_calls_in_block, collect_local_type_updates, rewrite_calls_in_block};
use types::{
    binary_add, binary_ne, cm_val_type_to_type_id, cm_zero, compute_export_flat_param_types,
    compute_export_flat_return_types, export_needs_param_lifting, field_access, find_struct_decl,
    find_variant_decl, flat_types_from_type_id, flatten_export_type, type_id_to_ast_type,
    variant_payload, variant_tag, variant_test,
};


/// Build the export binding function name for a world export.
pub fn export_binding_func_name(export_name: &str) -> String {
    format!("__cm_export__{export_name}")
}


/// Build a `(module_source, name)` set for every effect/resource declared in
/// the loaded TIR modules. The CM binding synthesizer uses this to attach the
/// owning effect to each generated binding using the same `module_source` the
/// resolver assigns to user-written `with E` clauses.
///
/// Keying by `(module_source, name)` (rather than name alone) prevents
/// collisions when two modules declare an effect or resource with the same
/// name — `lookup_effect_owner` selects the canonical WASI module.
fn effect_owner_module_sources(
    modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
) -> IndexMap<(ModuleSource, String), ()> {
    let mut out: IndexMap<(ModuleSource, String), ()> = IndexMap::default();
    for (module_source, module) in modules {
        for effect in &module.effects {
            out.insert((module_source.clone(), effect.name.clone()), ());
        }
        for resource in &module.resources {
            out.insert((module_source.clone(), resource.name.clone()), ());
        }
    }
    out
}

/// Look up the canonical owning module for an effect/resource named `name`
/// whose binding targets WASI `package` (e.g. `"cli"`).
///
/// Preferred match: a `ModuleSource::Wasi { interface }` whose interface starts
/// with `"{package}/"` (e.g. `wasi:cli/stdio.wado` for package `"cli"`).
/// Falls back to any other owner with the same name if no WASI match exists.
fn lookup_effect_owner(
    owners: &IndexMap<(ModuleSource, String), ()>,
    name: &str,
    package: &str,
) -> Option<ModuleSource> {
    let wasi_prefix = format!("{package}/");
    let mut fallback: Option<ModuleSource> = None;
    for ((ms, n), ()) in owners {
        if n != name {
            continue;
        }
        if let ModuleSource::Wasi { interface } = ms
            && interface.starts_with(&wasi_prefix)
        {
            return Some(ms.clone());
        }
        if fallback.is_none() {
            fallback = Some(ms.clone());
        }
    }
    fallback
}

/// Synthesize TIR that lowers a Wado value to flat CM ABI values (on-stack).
///
/// Unlike `synthesize_lower` which stores to linear memory, this produces
/// TIR that yields individual flat values as locals. Used for export bindings
/// where results are passed to `task-return` as flat params.
///
/// Returns: list of local indices containing the flat values, and appends
/// statements to `stmts` for computing them.
pub(super) fn synthesize_lower_to_flat(
    value: TirExpr,
    type_id: TypeId,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
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
    resolved: &crate::tir::ResolvedType,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    ctx: LiftContext<'_>,
) -> Vec<FlatLocal> {
    use crate::tir::{PrimitiveType, ResolvedType};

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
        ResolvedType::Struct { name, .. } if name == "String" => {
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
                crate::tir::TirBinaryOp::Shr,
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
        } if name == "Array" && type_args.len() == 1 => {
            // Array<T> flat ABI: (ptr: i32, len: i32) pointing at
            // `len * cm_size(T)` bytes of linear memory with `cm_align(T)`
            // alignment, laid out per the Canonical ABI.
            let elem_type_id = type_args[0];
            let elem_ast_type = {
                let tt = ctx.type_table.borrow();
                type_id_to_ast_type(elem_type_id, &tt, ctx.wasi_registry)
            };
            let elem_size =
                crate::component_model::cm_size_with_registry(&elem_ast_type, ctx.wasi_registry);
            let elem_align =
                crate::component_model::cm_align_with_registry(&elem_ast_type, ctx.wasi_registry);

            // __arr = value
            let arr_local = alloc_local(next_local, locals, type_id);
            stmts.push(let_stmt("__arr_val", arr_local, type_id, value));

            // __len = Array::len(__arr)
            let len_local = alloc_local(next_local, locals, TypeTable::I32);
            stmts.push(let_stmt(
                "__arr_len",
                len_local,
                TypeTable::I32,
                generic_method_call(
                    local_ref(arr_local, "__arr_val", type_id),
                    "Array",
                    "len",
                    ModuleSource::prelude(),
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
                    crate::tir::TirBinaryOp::Mul,
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
                    crate::tir::TirBinaryOp::GtEq,
                    local_ref(i_local, "__arr_i", TypeTable::I32),
                    local_ref(len_local, "__arr_len", TypeTable::I32),
                    TypeTable::BOOL,
                ),
                block(vec![break_stmt()]),
                None,
            ));

            // __elem = (__arr[__i]) via the IndexValue<i32> trait method.
            let elem_local = alloc_local(next_local, locals, elem_type_id);
            let iv_info = crate::name::LocalMethodName::new(
                "Array".to_string(),
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
                            module_source: ModuleSource::array(),
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
                        crate::tir::TirBinaryOp::Mul,
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
                ctx.wasi_registry,
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
                        0,
                        "Some",
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
                let unwrapped =
                    variant_payload(local_ref(opt_local, "__opt_val", type_id), 0, inner_type_id);
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
                        crate::tir::TirBinaryOp::NotEq,
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
        ResolvedType::Struct { name, .. } if name != "String" => {
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
/// `lift_ctx` is consulted for Array / nested-struct lifts where the
/// WIR `struct_type_map` lookup needs the full CM resolution stack
/// (WASI + kiln registries, type-table cell, binding package hint).
/// When it is `None` the helper falls back to `Array<i32>` placeholders
/// and non-primitive composites resolve via `find_struct_decl` alone —
/// callers that exercise real structs (the three export-binding
/// synthesizers) always pass `Some(ctx)`.
fn synthesize_lift_from_flat_params(
    ty: &Type,
    flat_param_locals: &[u32],
    flat_types: &[cm_abi::CmValType],
    target_type_id: TypeId,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table_cell: &std::cell::RefCell<TypeTable>,
    lift_ctx: Option<LiftContext<'_>>,
) -> (TirExpr, usize) {
    match ty {
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
                let lifted = binary(
                    crate::tir::TirBinaryOp::NotEq,
                    raw,
                    i32_const(0),
                    TypeTable::BOOL,
                );
                (lifted, 1)
            }
            "char" => (local_ref(flat_param_locals[0], "__p", TypeTable::CHAR), 1),
            "String" => {
                // String flat ABI: (ptr: i32, len: i32) pointing to linear memory
                let ptr = local_ref(flat_param_locals[0], "__p", TypeTable::I32);
                let len = local_ref(flat_param_locals[1], "__p", TypeTable::I32);
                let lifted = internal_call("memory_to_gc_string", vec![ptr, len], target_type_id);
                (lifted, 2)
            }
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
                        let registry = lift_ctx
                            .as_ref()
                            .map(|c| c.wasi_registry)
                            .expect("lift_ctx required when reconstructing struct field AST types");
                        let field_tys: Vec<Type> = struct_decl
                            .fields
                            .iter()
                            .map(|f| type_id_to_ast_type(f.type_id, &tt, registry))
                            .collect();
                        // Prefer the already-registered TypeId so the WIR
                        // `struct_type_map` lookup hits — the
                        // `find_struct_by_name` index is populated when
                        // the resolver first processed the struct decl,
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
                        fields_out.push(crate::tir::TirStructField {
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
            "Array"
                if generic.args.len() == 1
                    && matches!(generic.args[0], Type::Named(ref n) if n.name == "u8") =>
            {
                // Array<u8> flat ABI: (ptr: i32, len: i32) pointing to linear memory
                let ptr = local_ref(flat_param_locals[0], "__p", TypeTable::I32);
                let len = local_ref(flat_param_locals[1], "__p", TypeTable::I32);
                let lifted = internal_call("memory_to_gc_array", vec![ptr, len], target_type_id);
                (lifted, 2)
            }
            "Array" => {
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
                // Use synthesize_lift to lift from linear memory. When a
                // `LiftContext` is available (real export binding calls),
                // route through `synthesize_lift_with_context` so the element
                // type and its registry (WASI or kiln) resolve correctly —
                // without it the list lift falls back to `Array<i32>` and
                // non-primitive element types blow up at monomorphization.
                let lifted = if let Some(ref ctx) = lift_ctx {
                    synthesize_lift_with_context(
                        ty,
                        local_ref(tmp_ptr_local, "__lift_tmp", TypeTable::I32),
                        next_local,
                        stmts,
                        locals,
                        ctx,
                    )
                } else {
                    synthesize_lift(
                        ty,
                        local_ref(tmp_ptr_local, "__lift_tmp", TypeTable::I32),
                        next_local,
                        stmts,
                        locals,
                    )
                };
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
                // Default: None
                stmts.push(let_mut_stmt(
                    "__opt_result",
                    result_local,
                    target_type_id,
                    option_none(target_type_id),
                ));

                // if disc != 0
                let mut then_stmts: Vec<TirStmt> = Vec::new();
                if inner_flat.is_empty() {
                    // Inner is unit — Some(())
                    let unit = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
                    then_stmts.push(expr_stmt(assign(
                        local_ref(result_local, "__opt_result", target_type_id),
                        option_some(unit, target_type_id),
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
                    then_stmts.push(expr_stmt(assign(
                        local_ref(result_local, "__opt_result", target_type_id),
                        option_some(inner_lifted, target_type_id),
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
fn synthesize_result_export_binding(
    export_name: &str,
    user_func: Rc<RefCell<TirFunction>>,
    entry_source: &ModuleSource,
    _return_type: &Type,
    flat_return_types: &[cm_abi::CmValType],
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    world_params: &[(String, Type)],
    wasi_registry: &WasiRegistry,
    cm_package: &str,
    interner: &RefCell<crate::name::ModuleSourceInterner>,
) -> Rc<RefCell<TirFunction>> {
    let binding_name = export_binding_func_name(export_name);
    let mut body_stmts: Vec<TirStmt> = Vec::new();
    let mut locals: Vec<TirLocal> = Vec::new();

    let user_func_ref = user_func.borrow();
    let user_return_type = user_func_ref.return_type;
    let needs_lifting = export_needs_param_lifting(&user_func_ref.params, type_table);
    let lift_ctx = LiftContext {
        wasi_registry,
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
                default_expr: None,
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
                Some(lift_ctx),
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
                default_expr: None,
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
        crate::tir::ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
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

    // === Ok case ===
    let mut ok_stmts: Vec<TirStmt> = Vec::new();

    // Set flat[0] = 0 (Ok discriminant)
    ok_stmts.push(expr_stmt(assign(
        local_ref(
            flat_locals[0].0,
            &flat_locals[0].1,
            cm_val_type_to_type_id(flat_return_types[0]),
        ),
        i32_const(0),
    )));

    // Extract Ok payload
    let ok_value = variant_payload(
        local_ref(result_local, "__result", user_return_type),
        0, // Ok case index
        ok_type_id,
    );

    // Lower Ok payload to flat values starting at flat[1]
    let tt = type_table.borrow();
    let ok_flat_types = flat_types_from_type_id(ok_type_id, tir_modules, &tt);
    drop(tt);

    if !ok_flat_types.is_empty() {
        // Store Ok payload in a local for reference
        let ok_local = alloc_local(&mut next_local, &mut locals, ok_type_id);
        ok_stmts.push(let_stmt("__ok_val", ok_local, ok_type_id, ok_value));

        let ok_lowered = synthesize_lower_to_flat(
            local_ref(ok_local, "__ok_val", ok_type_id),
            ok_type_id,
            &mut next_local,
            &mut ok_stmts,
            &mut locals,
            tir_modules,
            lift_ctx,
        );

        // Assign lowered values to flat locals [1..1+ok_flat_count]
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

    // Call task-return with flat values
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

    // === Err case ===
    let mut err_stmts: Vec<TirStmt> = Vec::new();

    // Set flat[0] = 1 (Err discriminant)
    err_stmts.push(expr_stmt(assign(
        local_ref(
            flat_locals[0].0,
            &flat_locals[0].1,
            cm_val_type_to_type_id(flat_return_types[0]),
        ),
        i32_const(1),
    )));

    // Extract Err payload
    let err_value = variant_payload(
        local_ref(result_local, "__result", user_return_type),
        1, // Err case index
        err_type_id,
    );
    let err_local = alloc_local(&mut next_local, &mut locals, err_type_id);
    err_stmts.push(let_stmt("__err_val", err_local, err_type_id, err_value));

    // Lower Err payload to flat values
    // For variant Err types (like ErrorCode), we need the discriminant and per-case payload
    let err_resolved = type_table.borrow().get(err_type_id).clone();

    // Check if Err type is a variant with payloads
    if let crate::tir::ResolvedType::Variant { name, .. } = &err_resolved {
        if let Some(variant_decl) = find_variant_decl(name, tir_modules) {
            // Variant lowering: discriminant + per-case payload extraction
            synthesize_variant_lower_to_flat(
                err_local,
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
        } else {
            // Unknown variant — lower as i32
            if flat_locals.len() > 1 {
                err_stmts.push(expr_stmt(assign(
                    local_ref(
                        flat_locals[1].0,
                        &flat_locals[1].1,
                        cm_val_type_to_type_id(flat_return_types[1]),
                    ),
                    local_ref(err_local, "__err_val", err_type_id),
                )));
            }
        }
    } else {
        // Non-variant Err type — lower directly
        let err_lowered = synthesize_lower_to_flat(
            local_ref(err_local, "__err_val", err_type_id),
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

    // Call task-return with flat values
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

    // === Combine Ok/Err into if-else ===
    body_stmts.push(if_stmt(
        variant_test(
            local_ref(result_local, "__result", user_return_type),
            0,
            "Ok",
        ),
        block(ok_stmts),
        Some(block(err_stmts)),
    ));

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
    variant_decl: &crate::tir::TirVariantDecl,
    flat_locals: &[(u32, String)], // flat locals for [disc, p2, p3, ...]
    flat_types: &[cm_abi::CmValType],
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
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

    // For each non-unit case, generate: if variant_test { extract payload, lower to flat }
    for case in &variant_decl.cases {
        let case_flat = {
            let tt = ctx.type_table.borrow();
            flat_types_from_type_id(case.payload, tir_modules, &tt)
        };
        if case_flat.is_empty() {
            continue; // Unit case — no payload to lower
        }

        let mut case_stmts: Vec<TirStmt> = Vec::new();

        // Extract payload
        let payload = variant_payload(
            local_ref(value_local, "__err_val", value_type_id),
            case.index,
            case.payload,
        );
        let payload_local = alloc_local(next_local, locals, case.payload);
        case_stmts.push(let_stmt(
            "__case_payload",
            payload_local,
            case.payload,
            payload,
        ));

        // Lower payload to flat values
        let lowered = synthesize_lower_to_flat(
            local_ref(payload_local, "__case_payload", case.payload),
            case.payload,
            next_local,
            &mut case_stmts,
            locals,
            tir_modules,
            ctx,
        );

        // Assign lowered values to flat locals [1..]
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

        stmts.push(if_stmt(
            variant_test(
                local_ref(value_local, "__err_val", value_type_id),
                case.index,
                &case.name,
            ),
            block(case_stmts),
            None,
        ));
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
fn synthesize_void_export_binding(
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
fn synthesize_general_export_binding(
    export_name: &str,
    user_func: Rc<RefCell<TirFunction>>,
    entry_source: &ModuleSource,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    world_params: &[(String, Type)],
    wasi_registry: &WasiRegistry,
    cm_package: &str,
    interner: &RefCell<crate::name::ModuleSourceInterner>,
) -> Rc<RefCell<TirFunction>> {
    let binding_name = export_binding_func_name(export_name);
    let mut body_stmts: Vec<TirStmt> = Vec::new();
    let mut locals: Vec<TirLocal> = Vec::new();

    let user_func_ref = user_func.borrow();
    let user_return_type = user_func_ref.return_type;
    let needs_lifting = export_needs_param_lifting(&user_func_ref.params, type_table);
    let lift_ctx = LiftContext {
        wasi_registry,
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
                default_expr: None,
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
                Some(lift_ctx),
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
                default_expr: None,
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
    let is_unit_return = matches!(tt.get(user_return_type), crate::tir::ResolvedType::Unit);
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

/// Synthesize a stub export binding that just calls `task-return(0)`.
///
/// Used when the world declares an export but the user didn't define the function
/// (e.g., test-only files that have `test` blocks but no `export fn run()`).
fn synthesize_void_stub_adapter(export_name: &str) -> Rc<RefCell<TirFunction>> {
    let binding_name = export_binding_func_name(export_name);

    // Just call task-return(0) — Ok discriminant for result<_, _>
    let body = block(vec![expr_stmt(cm_raw_call(
        "task-return",
        vec![i32_const(0)],
        TypeTable::UNIT,
    ))]);

    let binding = make_binding_function(binding_name, vec![], TypeTable::UNIT, body, 0, vec![]);
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
fn synthesize_async_export_binding(
    export_name: &str,
    user_func: Rc<RefCell<TirFunction>>,
    entry_source: &ModuleSource,
    tir_modules: &IndexMap<ModuleSource, crate::tir::TirModule>,
    type_table: &Rc<RefCell<TypeTable>>,
    world_params: &[(String, Type)],
    wasi_registry: &WasiRegistry,
    cm_package: &str,
    interner: &RefCell<crate::name::ModuleSourceInterner>,
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
                default_expr: None,
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
            wasi_registry,
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
                Some(lift_ctx),
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
                default_expr: None,
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

/// Phase entry point: generate CM binding functions and rewrite call sites.
///
/// For each WASI import function used in the program:
/// 1. Synthesizes a binding TIR function that handles CM boundary crossing
/// 2. Rewrites effect-like `Call` nodes to target the binding function
///
/// For each world export function:
/// 3. Synthesizes an export binding that wraps the user function with task-return
///
/// Adapter functions flow through monomorphize → lower → optimize → codegen
/// like any other function.
pub fn generate_adapters(mut project: Package) -> Result<Package, String> {
    let entry_source = project.entry_module_source.clone();

    // ---- Import adapters ----

    // Step 1: Collect all used WASI effect calls and resource method calls
    let mut seen_effects: IndexSet<String> = IndexSet::default();
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                collect_effect_calls_in_block(body, &mut seen_effects, project.wasi_registry);
            }
        }
    }

    if !seen_effects.is_empty() {
        // Step 2: Synthesize binding functions for each used WASI function
        let entry_type_table = project
            .tir_modules
            .get(&project.entry_module_source)
            .map(|m| m.type_table.clone())
            .unwrap_or_else(|| Rc::new(RefCell::new(TypeTable::new())));
        // Map effect/resource name → defining module source. Used to attach the
        // canonical owner as an effect on each generated binding so the
        // checker's `(module_source, name)` identity matches user-written
        // `with E` clauses (which the resolver also canonicalises to the
        // defining module).
        let owner_sources = effect_owner_module_sources(&project.tir_modules);
        let mut adapters: IndexMap<String, Rc<RefCell<TirFunction>>> = IndexMap::default();
        for qualified_name in &seen_effects {
            if let Some(func_info) = project.wasi_registry.get_function(qualified_name) {
                let func_info = func_info.clone();
                let binding_name =
                    binding_func_name(&func_info.interface_name, &func_info.method_name);
                let owner_module = lookup_effect_owner(
                    &owner_sources,
                    &func_info.interface_name,
                    &func_info.package,
                )
                .unwrap_or_else(|| project.interner.borrow_mut().wasi(&func_info.package));
                let adapter = synthesize_adapter(
                    &func_info,
                    project.wasi_registry,
                    &entry_type_table,
                    &project.interner,
                    &owner_module,
                );
                adapters.insert(qualified_name.clone(), adapter.clone());
                // Also index by binding function name for lookup
                adapters.insert(binding_name, adapter);
            }
        }

        // Step 3: Add binding functions to the entry module
        if let Some(entry_module) = project.tir_modules.get_mut(&entry_source) {
            for (key, adapter_rc) in &adapters {
                // Only add each adapter once (skip the duplicate keyed by binding_name)
                if key.contains("::") {
                    entry_module.functions.push(adapter_rc.clone());
                }
            }
        }

        // Step 4: Rewrite effect-like call nodes to target adapters
        let adapter_map: IndexMap<String, Rc<RefCell<TirFunction>>> = adapters
            .iter()
            .filter(|(k, _)| k.contains("::"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        for module in project.tir_modules.values() {
            for func_rc in &module.functions {
                let mut func = func_rc.borrow_mut();
                if let Some(body) = &mut func.body {
                    rewrite_calls_in_block(
                        body,
                        &adapter_map,
                        &entry_source,
                        project.wasi_registry,
                        &entry_type_table,
                    );
                }
                // Sync locals with any Let stmts that were updated by the rewrite
                // (e.g., streaming binding calls changing the let binding type to i32).
                if !func.locals.is_empty() {
                    let mut updates = Vec::new();
                    if let Some(body) = &func.body {
                        collect_local_type_updates(body, &func.locals, &mut updates);
                    }
                    for (idx, type_id) in updates {
                        func.locals[idx].type_id = type_id;
                    }
                }
            }
        }
    }

    // ---- Export adapters ----

    // Step 5: Synthesize export bindings for world exports (signature-driven)
    let world_info = project.world_registry.get(&project.target_world).cloned();
    if let Some(world_info) = world_info {
        let entry_type_table = project
            .tir_modules
            .get(&entry_source)
            .map(|m| m.type_table.clone())
            .unwrap_or_else(|| Rc::new(RefCell::new(TypeTable::new())));

        // Collect adapters in a read-only pass (synthesize_result_export_binding needs &tir_modules)
        let mut export_adapters: Vec<(String, String, Rc<RefCell<TirFunction>>)> = Vec::new();
        {
            let entry_module = project
                .tir_modules
                .get(&entry_source)
                .expect("entry module should exist");

            // Package hint for CM name resolution inside export adapters.
            // For `wasi:http/service` this is `"http"`; for
            // `core:kiln/generator` it is `"kiln"`. The hint biases bare-name
            // resolution towards the binding's owning package (e.g.
            // `ErrorCode` in `wasi:http` bindings) and feeds
            // `resolve_cm_source_for` as a fallback anchor. Derived from
            // the world's `fq_name` — the attribute-sourced identity is
            // the single source of truth.
            let binding_cm_package = world_info.package().to_string();

            for export in &world_info.exports {
                // Find the user's export function and check for missing `export` keyword
                let mut found_exported = None;
                let mut found_without_export = false;
                for f in &entry_module.functions {
                    let func = f.borrow();
                    if func.name == export.name {
                        if func.is_export {
                            found_exported = Some(f.clone());
                        } else {
                            found_without_export = true;
                        }
                    }
                }

                if found_exported.is_none() && found_without_export {
                    return Err(format!(
                        "function `{}` exists but is not marked with `export` keyword. \
                         Add `export` to make it a world entry point: `export fn {}(...)`",
                        export.name, export.name
                    ));
                }

                let binding_name = export_binding_func_name(&export.name);
                let adapter = if let Some(user_func_rc) = found_exported {
                    // Validate parameter count matches world declaration
                    {
                        let user_func = user_func_rc.borrow();
                        if user_func.params.len() != export.params.len() {
                            return Err(format!(
                                "export function `{}` has {} parameter(s), \
                                 but the world expects {} parameter(s)",
                                export.name,
                                user_func.params.len(),
                                export.params.len()
                            ));
                        }
                    }

                    // Check if user function is `export async fn`
                    let is_async_export = {
                        let user_func = user_func_rc.borrow();
                        user_func.is_async
                    };

                    if is_async_export {
                        // Async export: the user function calls task-return internally via
                        // `task return expr` stmts. Expand those stmts into CM task-return
                        // calls and synthesize a simple lifting adapter.
                        if let Some(return_type) = &export.return_type {
                            let tt = entry_type_table.borrow();
                            let flat_types = compute_export_flat_return_types(
                                return_type,
                                &project.tir_modules,
                                &tt,
                            );
                            drop(tt);
                            expand_task_returns_in_func(
                                &user_func_rc,
                                &flat_types,
                                &project.tir_modules,
                                &entry_type_table,
                                project.wasi_registry,
                                &binding_cm_package,
                                &project.interner,
                            );
                        }
                        synthesize_async_export_binding(
                            &export.name,
                            user_func_rc,
                            &entry_source,
                            &project.tir_modules,
                            &entry_type_table,
                            &export.params,
                            project.wasi_registry,
                            &binding_cm_package,
                            &project.interner,
                        )
                    } else {
                        // Check the user function's actual return type (signature-driven)
                        let user_returns_result = {
                            let user_func = user_func_rc.borrow();
                            let tt = entry_type_table.borrow();
                            matches!(
                                tt.get(user_func.return_type),
                                crate::tir::ResolvedType::GenericInstance { name, .. }
                                    if name == "Result"
                            )
                        };

                        if user_returns_result {
                            // Result<T, E> return: full lowering adapter (signature-driven)
                            let tt = entry_type_table.borrow();
                            let flat_types = compute_export_flat_return_types(
                                export.return_type.as_ref().unwrap(),
                                &project.tir_modules,
                                &tt,
                            );
                            drop(tt);
                            synthesize_result_export_binding(
                                &export.name,
                                user_func_rc,
                                &entry_source,
                                export.return_type.as_ref().unwrap(),
                                &flat_types,
                                &project.tir_modules,
                                &entry_type_table,
                                &export.params,
                                project.wasi_registry,
                                &binding_cm_package,
                                &project.interner,
                            )
                        } else {
                            // Non-Result return: check if we can use the simple void adapter
                            // (only when no params AND unit return type)
                            let is_void_no_params = export.params.is_empty() && {
                                let user_func = user_func_rc.borrow();
                                let tt = entry_type_table.borrow();
                                matches!(
                                    tt.get(user_func.return_type),
                                    crate::tir::ResolvedType::Unit
                                )
                            };

                            if is_void_no_params {
                                // Simple void adapter for () -> ()
                                synthesize_void_export_binding(
                                    &export.name,
                                    user_func_rc,
                                    &entry_source,
                                )
                            } else {
                                // General adapter: handles params (with lifting if needed)
                                // and non-void return types
                                synthesize_general_export_binding(
                                    &export.name,
                                    user_func_rc,
                                    &entry_source,
                                    &project.tir_modules,
                                    &entry_type_table,
                                    &export.params,
                                    project.wasi_registry,
                                    &binding_cm_package,
                                    &project.interner,
                                )
                            }
                        }
                    }
                } else {
                    // No user function: stub that just calls task-return(0)
                    synthesize_void_stub_adapter(&export.name)
                };
                export_adapters.push((export.name.clone(), binding_name, adapter));
            }
        }

        // Compute the correct task-return params from the export's flat return types.
        // The builtin registry defines task_return with a single i32 param, but for
        // Result-returning exports the task-return call passes the full flattened type.
        // Store on Package so optimize_dce can use it when creating the import.
        for export in &world_info.exports {
            if let Some(return_type) = &export.return_type {
                let tt = entry_type_table.borrow();
                let flat_types =
                    compute_export_flat_return_types(return_type, &project.tir_modules, &tt);
                project.task_return_flat_params = Some(
                    flat_types
                        .iter()
                        .map(|&vt| cm_val_type_to_type_id(vt))
                        .collect(),
                );
                break; // One export is enough — all share the same task-return
            }
        }

        // Push adapters with mutable access
        let entry_module = project
            .tir_modules
            .get_mut(&entry_source)
            .expect("entry module should exist");
        for (export_name, binding_name, adapter) in export_adapters {
            project
                .export_binding_names
                .insert(export_name, binding_name);
            entry_module.functions.push(adapter);
        }
    }

    // Step 6: Synthesize export bindings for test functions (__test_*)
    // Only when targeting the test world — in other worlds, tests are dead code.
    if project.is_test_world() {
        let entry_module = project
            .tir_modules
            .get_mut(&entry_source)
            .expect("entry module should exist");

        // Collect test functions first to avoid borrow conflict.
        // Test functions have is_export=false (they're not world exports),
        // but they need adapters for task-return when called via `wado test`.
        let test_funcs: Vec<(String, Rc<RefCell<TirFunction>>)> = entry_module
            .functions
            .iter()
            .filter(|f| f.borrow().name.starts_with("__test_"))
            .map(|f| (f.borrow().name.clone(), f.clone()))
            .collect();

        for (test_name, user_func_rc) in test_funcs {
            let binding_name = export_binding_func_name(&test_name);
            let adapter = synthesize_void_export_binding(&test_name, user_func_rc, &entry_source);
            project.export_binding_names.insert(test_name, binding_name);
            entry_module.functions.push(adapter);
        }
    }

    // Strip remaining TaskReturn from all modules.
    // `task return` is only valid inside `async fn` (checked by resolver).
    // Step 5 expands TaskReturn into CM calls for async exports that match the
    // target world. Any remaining async fn (unmatched exports, imported modules)
    // will be DCE'd — strip their TaskReturn stmts so they don't reach monomorphize.
    // This is idempotent: already-expanded functions have no TaskReturn stmts left.
    for module in project.tir_modules.values() {
        for f in &module.functions {
            let needs_strip = {
                let func = f.borrow();
                func.is_async
            };
            if needs_strip {
                strip_task_returns_in_func(f);
            }
        }
    }

    // ---- Record Stream Read Adapters ----
    // Generate binding functions for Stream<T>.read() where T is a WASI record type.
    // Must run before rewrite_cm_resource_methods so the generated functions are available.
    synthesize_record_stream_reads(&mut project);

    // ---- CM Resource Method Adapters ----
    // Rewrite #[cm("...")] resource method calls to target internal/builtin binding functions.
    // This replaces the inline WIR emission in wir_build/translate.rs with pre-monomorphization
    // synthesis, so the binding functions go through the normal compilation pipeline.
    rewrite_cm_resource_methods(&mut project);

    Ok(project)
}

#[cfg(test)]
mod tests {
    use super::types::param_needs_lifting;
    use super::*;
    use crate::ast::NamedType;
    use crate::component_model::WasiRegistry;
    use crate::tir::TirStmtKind;

    fn named_type(name: &str) -> Type {
        Type::Named(NamedType {
            id: crate::ast::AstId::fresh(),
            name: name.to_string(),
            span: synth_span(),
            source_interface: None,
        })
    }

    #[test]
    fn flatten_param_i32() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("i32"), &reg),
            vec![TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_i64() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("i64"), &reg),
            vec![TypeTable::I64]
        );
    }

    #[test]
    fn flatten_param_f64() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("f64"), &reg),
            vec![TypeTable::F64]
        );
    }

    #[test]
    fn flatten_param_string() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("String"), &reg),
            vec![TypeTable::I32, TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_bool() {
        let reg = WasiRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("bool"), &reg),
            vec![TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_unit() {
        let reg = WasiRegistry::new();
        assert!(flatten_param_type(&Type::Tuple(vec![]), &reg).is_empty());
    }

    #[test]
    fn flatten_param_newtype_u64() {
        let (reg, _) = WasiRegistry::build_from_stdlib();
        assert_eq!(
            flatten_param_type(&named_type("Duration"), reg),
            vec![TypeTable::I64]
        );
        assert_eq!(
            flatten_param_type(&named_type("Mark"), reg),
            vec![TypeTable::I64]
        );
    }

    #[test]
    fn binding_name() {
        assert_eq!(
            binding_func_name("Stdout", "write_via_stream"),
            "__cm_binding__Stdout_write_via_stream"
        );
    }

    #[test]
    fn lift_i32() {
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let expr = synthesize_lift(
            &named_type("i32"),
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut locals,
        );
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
        assert!(stmts.is_empty()); // primitives need no setup
    }

    #[test]
    fn lift_bool() {
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let expr = synthesize_lift(
            &named_type("bool"),
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut locals,
        );
        assert!(matches!(expr.kind, TirExprKind::Binary { .. }));
        assert_eq!(expr.type_id, TypeTable::BOOL);
    }

    #[test]
    fn lift_string() {
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let expr = synthesize_lift(
            &named_type("String"),
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut locals,
        );
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
    }

    #[test]
    fn lift_list_i32() {
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let list_ty = cm_abi::generic_type("Array", vec![named_type("i32")]);
        let expr = synthesize_lift(
            &list_ty,
            i32_const(100),
            &mut next_local,
            &mut stmts,
            &mut locals,
        );
        // Should produce setup stmts and return a local ref
        assert!(!stmts.is_empty());
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert!(next_local >= 5); // base, count, result, i, elem_addr
    }

    #[test]
    fn lift_option_i32() {
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let opt_ty = cm_abi::generic_type("Option", vec![named_type("i32")]);
        let expr = synthesize_lift(
            &opt_ty,
            i32_const(100),
            &mut next_local,
            &mut stmts,
            &mut locals,
        );
        assert!(!stmts.is_empty());
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert!(next_local >= 2); // disc, result
    }

    #[test]
    fn lift_result_unit_unit() {
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let result_ty =
            cm_abi::generic_type("Result", vec![Type::Tuple(vec![]), Type::Tuple(vec![])]);
        let expr = synthesize_lift(
            &result_ty,
            i32_const(100),
            &mut next_local,
            &mut stmts,
            &mut locals,
        );
        assert!(!stmts.is_empty());
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
    }

    #[test]
    fn lift_resource_handle() {
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let own_ty = cm_abi::generic_type("Own", vec![named_type("Fields")]);
        let expr = synthesize_lift(&own_ty, i32_const(100), &mut 0, &mut stmts, &mut locals);
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }

    #[test]
    fn lower_i32() {
        let stmts = synthesize_lower(
            &named_type("i32"),
            i32_const(42),
            i32_const(100),
            &mut 0,
            &mut vec![],
        );
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_bool() {
        let value = TirExpr::new(
            TirExprKind::BoolLiteral(true),
            TypeTable::BOOL,
            synth_span(),
        );
        let stmts = synthesize_lower(
            &named_type("bool"),
            value,
            i32_const(100),
            &mut 0,
            &mut vec![],
        );
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_unit() {
        let value = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
        let stmts = synthesize_lower(
            &Type::Tuple(vec![]),
            value,
            i32_const(100),
            &mut 0,
            &mut vec![],
        );
        assert!(stmts.is_empty());
    }

    #[test]
    fn lower_string() {
        let value = TirExpr::new(
            TirExprKind::StringLiteral("hello".to_string()),
            TypeTable::I32, // placeholder
            synth_span(),
        );
        let mut next_local = 10_u32;
        let stmts = synthesize_lower(
            &named_type("String"),
            value,
            i32_const(100),
            &mut next_local,
            &mut vec![],
        );
        // Should produce: let __packed = cm_lower_string(value); store ptr; store len
        assert_eq!(stmts.len(), 3);
        assert_eq!(next_local, 11); // one local allocated for __packed
    }

    #[test]
    fn helpers_i32_const() {
        let expr = i32_const(42);
        assert_eq!(expr.type_id, TypeTable::I32);
        match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => assert_eq!(*value, 42),
            other => panic!("expected IntLiteral, got {other:?}"),
        }
    }

    #[test]
    fn helpers_i64_const() {
        let expr = i64_const(123);
        assert_eq!(expr.type_id, TypeTable::I64);
        match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => assert_eq!(*value, 123),
            other => panic!("expected IntLiteral, got {other:?}"),
        }
    }

    #[test]
    fn helpers_builtin_call() {
        let call = builtin_call("i32_load", vec![i32_const(0)], TypeTable::I32);
        match &call.kind {
            TirExprKind::Call { func, args, .. } => {
                assert_eq!(func.name.clone(), "i32_load");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn helpers_internal_call() {
        let call = internal_call("cm_lower_string", vec![i32_const(0)], TypeTable::I64);
        match &call.kind {
            TirExprKind::Call { func, args, .. } => {
                assert_eq!(func.name.clone(), "cm_lower_string");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn helpers_cm_raw_call() {
        let call = cm_raw_call(
            "wasi:cli/Stdout::write_via_stream",
            vec![i32_const(0), i32_const(1), i32_const(2)],
            TypeTable::I32,
        );
        match &call.kind {
            TirExprKind::CmRawCall { local_name, args } => {
                assert_eq!(local_name, "wasi:cli/Stdout::write_via_stream");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected CmRawCall, got {other:?}"),
        }
    }

    #[test]
    fn helpers_let_stmt() {
        let stmt = let_stmt("x", 0, TypeTable::I32, i32_const(42));
        match &stmt.kind {
            TirStmtKind::Let {
                name,
                local_index,
                type_id,
                ..
            } => {
                assert_eq!(name, "x");
                assert_eq!(*local_index, 0);
                assert_eq!(*type_id, TypeTable::I32);
            }
            other => panic!("expected Let, got {other:?}"),
        }
    }

    // ---- Parameter lifting tests ----
    //
    // These exercise the TypeTable-driven classification in
    // `param_needs_lifting`. Each test constructs the minimum TIR shape
    // needed to reach a specific `ResolvedType` arm.

    fn mk_param(type_id: TypeId) -> crate::tir::TirParam {
        crate::tir::TirParam {
            name: String::new(),
            type_id,
            local_index: 0,
            is_mut: false,
            default_expr: None,
            span: crate::token::Span::new(0, 0, 0, 0),
        }
    }

    #[test]
    fn param_needs_lifting_primitives_passthrough() {
        let tt = TypeTable::new();
        // All Wasm-native primitive shapes flow through unchanged.
        assert!(!param_needs_lifting(TypeTable::I32, &tt));
        assert!(!param_needs_lifting(TypeTable::I64, &tt));
        assert!(!param_needs_lifting(TypeTable::F32, &tt));
        assert!(!param_needs_lifting(TypeTable::F64, &tt));
        assert!(!param_needs_lifting(TypeTable::U8, &tt));
        assert!(!param_needs_lifting(TypeTable::U16, &tt));
        assert!(!param_needs_lifting(TypeTable::CHAR, &tt));
    }

    #[test]
    fn param_needs_lifting_bool_lifts() {
        // `bool` needs a 0/!=0 widening at the CM boundary, so it
        // counts as needing a lift step.
        let tt = TypeTable::new();
        assert!(param_needs_lifting(TypeTable::BOOL, &tt));
    }

    #[test]
    fn param_needs_lifting_unit_lifts() {
        let tt = TypeTable::new();
        assert!(param_needs_lifting(TypeTable::UNIT, &tt));
    }

    #[test]
    fn param_needs_lifting_string() {
        let mut tt = TypeTable::new();
        let s = tt.make_struct("String".to_string(), ModuleSource::string());
        assert!(param_needs_lifting(s, &tt));
    }

    #[test]
    fn param_needs_lifting_resource() {
        // Resources are i32 handles — no lift.
        let mut tt = TypeTable::new();
        let r = tt.intern(crate::tir::ResolvedType::Resource {
            name: "Request".to_string(),
            module_source: ModuleSource::wasi_http(),
        });
        assert!(!param_needs_lifting(r, &tt));
    }

    #[test]
    fn param_needs_lifting_enum() {
        let mut tt = TypeTable::new();
        let mut interner = crate::name::ModuleSourceInterner::new();
        let e = tt.intern(crate::tir::ResolvedType::Enum {
            name: "Color".to_string(),
            module_source: interner.entry_point("<test>"),
        });
        assert!(!param_needs_lifting(e, &tt));
    }

    #[test]
    fn param_needs_lifting_option() {
        // Option<T> is a GenericInstance under the hood; build it directly
        // (avoids `make_option`'s dependency on comp-feature registration,
        // which isn't present in a bare `TypeTable::new()`).
        let mut tt = TypeTable::new();
        let opt = tt.intern(crate::tir::ResolvedType::GenericInstance {
            name: "Option".to_string(),
            module_source: ModuleSource::types(),
            type_args: vec![TypeTable::I32],
        });
        assert!(param_needs_lifting(opt, &tt));
    }

    #[test]
    fn param_needs_lifting_array() {
        let mut tt = TypeTable::new();
        let arr = tt.intern(crate::tir::ResolvedType::GenericInstance {
            name: "Array".to_string(),
            module_source: ModuleSource::prelude(),
            type_args: vec![TypeTable::I32],
        });
        assert!(param_needs_lifting(arr, &tt));
    }

    #[test]
    fn export_needs_lifting_empty() {
        let tt = std::cell::RefCell::new(TypeTable::new());
        assert!(!export_needs_param_lifting(&[], &tt));
    }

    #[test]
    fn export_needs_lifting_primitives_only() {
        let tt = std::cell::RefCell::new(TypeTable::new());
        let params = vec![mk_param(TypeTable::I32), mk_param(TypeTable::F64)];
        assert!(!export_needs_param_lifting(&params, &tt));
    }

    #[test]
    fn export_needs_lifting_with_string() {
        let tt_cell = std::cell::RefCell::new(TypeTable::new());
        let string_id = tt_cell
            .borrow_mut()
            .make_struct("String".to_string(), ModuleSource::string());
        let params = vec![mk_param(string_id)];
        assert!(export_needs_param_lifting(&params, &tt_cell));
    }

    #[test]
    fn compute_flat_params_empty() {
        let params: Vec<(String, Type)> = vec![];
        let type_table = TypeTable::new();
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert!(flat.is_empty());
    }

    #[test]
    fn compute_flat_params_primitives() {
        let params = vec![
            ("a".to_string(), named_type("i32")),
            ("b".to_string(), named_type("f64")),
        ];
        let type_table = TypeTable::new();
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert_eq!(flat, vec![cm_abi::CmValType::I32, cm_abi::CmValType::F64]);
    }

    #[test]
    fn compute_flat_params_string() {
        let params = vec![("name".to_string(), named_type("String"))];
        let type_table = TypeTable::new();
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert_eq!(flat, vec![cm_abi::CmValType::I32, cm_abi::CmValType::I32]);
    }

    #[test]
    fn compute_flat_params_mixed() {
        let params = vec![
            ("a".to_string(), named_type("i32")),
            ("name".to_string(), named_type("String")),
            ("b".to_string(), named_type("f32")),
        ];
        let type_table = TypeTable::new();
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert_eq!(
            flat,
            vec![
                cm_abi::CmValType::I32,
                cm_abi::CmValType::I32,
                cm_abi::CmValType::I32,
                cm_abi::CmValType::F32,
            ]
        );
    }

    // ---- Lift from flat params tests ----

    #[test]
    fn lift_from_flat_i32() {
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 1_u32;
        let type_table = std::cell::RefCell::new(TypeTable::new());
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("i32"),
            &[0],
            &[cm_abi::CmValType::I32],
            TypeTable::I32,
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            &type_table,
            None,
        );
        assert_eq!(consumed, 1);
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }

    #[test]
    fn lift_from_flat_string() {
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 2_u32;
        let type_table = std::cell::RefCell::new(TypeTable::new());
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("String"),
            &[0, 1],
            &[cm_abi::CmValType::I32, cm_abi::CmValType::I32],
            TypeTable::I32, // placeholder
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            &type_table,
            None,
        );
        assert_eq!(consumed, 2);
        // Should be a call to memory_to_gc_string
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
    }

    #[test]
    fn lift_from_flat_bool() {
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 1_u32;
        let type_table = std::cell::RefCell::new(TypeTable::new());
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("bool"),
            &[0],
            &[cm_abi::CmValType::I32],
            TypeTable::BOOL,
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            &type_table,
            None,
        );
        assert_eq!(consumed, 1);
        assert!(matches!(expr.kind, TirExprKind::Binary { .. }));
        assert_eq!(expr.type_id, TypeTable::BOOL);
    }

    #[test]
    fn lift_from_flat_unit() {
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let type_table = std::cell::RefCell::new(TypeTable::new());
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &Type::Tuple(vec![]),
            &[],
            &[],
            TypeTable::UNIT,
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            &type_table,
            None,
        );
        assert_eq!(consumed, 0);
        assert!(matches!(expr.kind, TirExprKind::Unit));
    }

    #[test]
    fn lift_from_flat_resource() {
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 1_u32;
        let type_table = std::cell::RefCell::new(TypeTable::new());
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("Request"),
            &[0],
            &[cm_abi::CmValType::I32],
            TypeTable::I32,
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            &type_table,
            None,
        );
        assert_eq!(consumed, 1);
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }
}
