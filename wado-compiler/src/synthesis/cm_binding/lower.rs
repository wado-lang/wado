//! CM ABI lower: store a Wado value into linear memory or flatten it to flat args.
//!
//! Two output shapes:
//!
//! - [`synthesize_lower`] / [`synthesize_lower_tuple`] /
//!   [`synthesize_lower_wasi_type_to_memory`] /
//!   [`synthesize_lower_wasi_variant_to_memory`] /
//!   [`synthesize_lower_option_to_memory`] write to linear memory at a
//!   given address (used for outptr returns and indirect param buffers).
//! - [`synthesize_flatten_value_to_flat_args`] /
//!   [`synthesize_flatten_option_to_flat_args`] flatten a GC value into
//!   a `Vec<TirExpr>` of i32 / i64 / f32 / f64 args (used for direct
//!   imports that take their params on the operand stack).

use std::cell::RefCell;

use crate::ast::{NamedType, Type};
use crate::cm_abi;
use crate::component_model::WasiRegistry;
use crate::tir::{TirBinaryOp, TirExpr, TirExprKind, TirLocal, TirStmt, TypeId, TypeTable};

use crate::synthesis::common::{
    alloc_local, assign, binary, block, builtin_call, cast, expr_stmt, i32_const, i64_const,
    if_stmt, internal_call, let_mut_stmt, let_stmt, local_ref, synth_span,
};

use super::types::{
    binary_add, flatten_param_type, kebab_to_pascal, variant_payload, variant_tag, variant_test,
    wasi_type_to_type_id,
};

/// Synthesize TIR statements that store a Wado value into linear memory.
///
/// For primitives, this is a single `builtin::i32_store` (or similar).
/// For String, calls `cm_lower_string` and stores ptr/len.
///
/// `next_local` is a counter for allocating intermediate local variables.
pub fn synthesize_lower(
    ty: &Type,
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
) -> Vec<TirStmt> {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i32" | "u32" => vec![expr_stmt(builtin_call(
                "i32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "i64" | "u64" => vec![expr_stmt(builtin_call(
                "i64_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "f32" => vec![expr_stmt(builtin_call(
                "f32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "f64" => vec![expr_stmt(builtin_call(
                "f64_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "i8" | "u8" => vec![expr_stmt(builtin_call(
                "i32_store8",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "i16" | "u16" => vec![expr_stmt(builtin_call(
                "i32_store16",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
            "bool" => {
                let as_i32 = cast(value, TypeTable::I32);
                vec![expr_stmt(builtin_call(
                    "i32_store8",
                    vec![addr, as_i32],
                    TypeTable::UNIT,
                ))]
            }
            "char" => {
                let as_i32 = cast(value, TypeTable::I32);
                vec![expr_stmt(builtin_call(
                    "i32_store",
                    vec![addr, as_i32],
                    TypeTable::UNIT,
                ))]
            }
            "String" => {
                // cm_lower_string(value) returns packed i64: (ptr | (len << 32))
                let packed_local = *next_local;
                locals.push(TirLocal::synth(*next_local, TypeTable::I64, false));
                *next_local += 1;
                let packed = internal_call("cm_lower_string", vec![value], TypeTable::I64);
                let mut stmts = vec![let_stmt("__packed", packed_local, TypeTable::I64, packed)];

                // Store ptr (low 32 bits) at addr
                let ptr = cast(
                    local_ref(packed_local, "__packed", TypeTable::I64),
                    TypeTable::I32,
                );
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![addr.clone(), ptr],
                    TypeTable::UNIT,
                )));

                // Store len (high 32 bits) at addr + 4
                let shifted = binary(
                    TirBinaryOp::Shr,
                    local_ref(packed_local, "__packed", TypeTable::I64),
                    i64_const(32),
                    TypeTable::I64,
                );
                let len = cast(shifted, TypeTable::I32);
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![
                        binary(TirBinaryOp::Add, addr, i32_const(4), TypeTable::I32),
                        len,
                    ],
                    TypeTable::UNIT,
                )));
                stmts
            }
            // Unknown named types: treat as i32 handles (enums, resources)
            _ => vec![expr_stmt(builtin_call(
                "i32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
        },
        Type::Generic(g) => match g.name.as_str() {
            // list<T>: lowered as (ptr, len) pair stored at addr
            "Array"
                if g.args.len() == 1 && matches!(&g.args[0], Type::Named(n) if n.name == "u8") =>
            {
                // list<u8>: use cm_lower_array_u8 → packed i64, store (ptr, len)
                let packed_local = *next_local;
                locals.push(TirLocal::synth(*next_local, TypeTable::I64, false));
                *next_local += 1;
                let packed = internal_call("cm_lower_array_u8", vec![value], TypeTable::I64);
                let mut stmts = vec![let_stmt(
                    "__elem_packed",
                    packed_local,
                    TypeTable::I64,
                    packed,
                )];
                // Store ptr (low 32 bits) at addr
                let ptr = cast(
                    local_ref(packed_local, "__elem_packed", TypeTable::I64),
                    TypeTable::I32,
                );
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![addr.clone(), ptr],
                    TypeTable::UNIT,
                )));
                // Store len (high 32 bits) at addr + 4
                let shifted = binary(
                    TirBinaryOp::Shr,
                    local_ref(packed_local, "__elem_packed", TypeTable::I64),
                    i64_const(32),
                    TypeTable::I64,
                );
                let len = cast(shifted, TypeTable::I32);
                stmts.push(expr_stmt(builtin_call(
                    "i32_store",
                    vec![
                        binary(TirBinaryOp::Add, addr, i32_const(4), TypeTable::I32),
                        len,
                    ],
                    TypeTable::UNIT,
                )));
                stmts
            }
            "Array" => {
                // General list<T>: treat as opaque i32 for now
                vec![expr_stmt(builtin_call(
                    "i32_store",
                    vec![addr, value],
                    TypeTable::UNIT,
                ))]
            }
            // Own<T>, Borrow<T>, Stream<T>, Future<T>: i32 handles
            _ => vec![expr_stmt(builtin_call(
                "i32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
        },
        Type::Tuple(elems) if elems.is_empty() => vec![],
        Type::Tuple(_) => {
            // Tuple lowering requires type_table for correct TypeIds.
            // Callers with tuple elements should call synthesize_lower_tuple directly.
            vec![]
        }
        Type::Reference(_) | Type::MutReference(_) => vec![expr_stmt(builtin_call(
            "i32_store",
            vec![addr, value],
            TypeTable::UNIT,
        ))],
        _ => vec![],
    }
}

/// Lower a GC tuple to linear memory at `addr`.
///
/// Each tuple element is extracted via `FieldAccess` and recursively lowered
/// at the computed CM ABI offset.
pub(super) fn synthesize_lower_tuple(
    elems: &[Type],
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
    wasi_registry: &WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) -> Vec<TirStmt> {
    let layout = cm_abi::layout_tuple(elems);
    let mut stmts = Vec::new();

    // Materialize the tuple value into a local so we can access fields
    let tuple_local = *next_local;
    locals.push(TirLocal::synth(tuple_local, value.type_id, false));
    *next_local += 1;
    stmts.push(let_stmt("__tuple", tuple_local, value.type_id, value));

    for (i, elem_ty) in elems.iter().enumerate() {
        let offset = layout.offsets[i] as i32;
        let field_addr = if offset == 0 {
            addr.clone()
        } else {
            binary_add(addr.clone(), i32_const(offset))
        };

        // Determine the type_id for this field
        let field_type_id = {
            let mut tt = type_table.borrow_mut();
            wasi_type_to_type_id(elem_ty, &mut tt, wasi_registry, wasi_package)
        };

        // Extract the i-th field from the tuple using FieldAccess
        let field_expr = TirExpr::new(
            TirExprKind::FieldAccess {
                expr: Box::new(local_ref(
                    tuple_local,
                    "__tuple",
                    locals[tuple_local as usize].type_id,
                )),
                field_index: i as u32,
                field_name: format!("{i}"),
            },
            field_type_id,
            synth_span(),
        );

        // Recursively lower: for tuples use synthesize_lower_tuple, otherwise synthesize_lower
        let field_stmts = if let Type::Tuple(sub_elems) = elem_ty {
            synthesize_lower_tuple(
                sub_elems,
                field_expr,
                field_addr,
                next_local,
                locals,
                wasi_registry,
                wasi_package,
                type_table,
            )
        } else {
            synthesize_lower(elem_ty, field_expr, field_addr, next_local, locals)
        };
        stmts.extend(field_stmts);
    }

    stmts
}

/// Lower a WASI variant (GC struct) to a linear memory buffer at the given address.
///
/// Memory layout (Canonical ABI):
/// - discriminant byte at offset 0
/// - payload at `align_to(1, payload_align)`, lowered using `synthesize_lower`
///
/// For each variant case with a payload, generates:
///   if `variant_test(value`, `case_i`) { `store_payload(payload_addr`, `variant_payload(value`, i)) }
pub(super) fn synthesize_lower_wasi_variant_to_memory(
    named: &NamedType,
    source: &str,
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    wasi_registry: &WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) {
    let name = named.name.as_str();
    let cases = if let Some(c) = wasi_registry.get_variant_cases_by_source(source, name) {
        c.to_vec()
    } else {
        // Fallback: store as i32
        stmts.push(expr_stmt(builtin_call(
            "i32_store8",
            vec![addr, variant_tag(value)],
            TypeTable::UNIT,
        )));
        return;
    };

    let value_type_id = value.type_id;

    // Materialize value into a local so we can reference it multiple times
    let value_local = alloc_local(next_local, locals, value_type_id);
    stmts.push(let_stmt("__variant_val", value_local, value_type_id, value));

    // Store discriminant byte
    stmts.push(expr_stmt(builtin_call(
        "i32_store8",
        vec![
            addr.clone(),
            variant_tag(local_ref(value_local, "__variant_val", value_type_id)),
        ],
        TypeTable::UNIT,
    )));

    // Compute payload offset (aligned to max payload alignment)
    let mut max_payload_align = 1u32;
    for case in &cases {
        if let Some(payload_ty) = &case.payload {
            max_payload_align = max_payload_align.max(
                crate::component_model::cm_align_with_registry(payload_ty, wasi_registry),
            );
        }
    }
    let payload_offset = cm_abi::align_to(1, max_payload_align);

    let payload_addr = if payload_offset == 0 {
        addr
    } else {
        binary_add(addr, i32_const(payload_offset as i32))
    };

    // For each case with a payload, generate conditional store
    for (case_idx, case) in cases.iter().enumerate() {
        if let Some(payload_ty) = &case.payload {
            let payload_type_id = {
                let mut tt = type_table.borrow_mut();
                wasi_type_to_type_id(payload_ty, &mut tt, wasi_registry, wasi_package)
            };

            let payload_expr = variant_payload(
                local_ref(value_local, "__variant_val", value_type_id),
                case_idx as u32,
                payload_type_id,
            );

            let case_name = kebab_to_pascal(&case.cm_name);
            let case_stmts = synthesize_lower_wasi_type_to_memory(
                payload_ty,
                payload_expr,
                payload_addr.clone(),
                next_local,
                locals,
                wasi_registry,
                wasi_package,
                type_table,
            );

            stmts.push(if_stmt(
                variant_test(
                    local_ref(value_local, "__variant_val", value_type_id),
                    case_idx as u32,
                    &case_name,
                ),
                block(case_stmts),
                None,
            ));
        }
    }
}

/// Lower an `Option<T>` value to linear memory at the given address.
///
/// Memory layout (Canonical ABI for `option<T>`):
/// - discriminant byte at offset 0 (0=None, 1=Some)
/// - payload at `align_to(1, payload_align)`, lowered using `synthesize_lower_wasi_type_to_memory`
pub(super) fn synthesize_lower_option_to_memory(
    inner_type: &Type,
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    wasi_registry: &WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) {
    let value_type_id = value.type_id;

    // Materialize value into a local so we can reference it multiple times
    let value_local = alloc_local(next_local, locals, value_type_id);
    stmts.push(let_stmt("__opt_val", value_local, value_type_id, value));

    // Store discriminant byte: variant_test(Some) → 1 = Some, 0 = None.
    // Use variant_test (ref.test) rather than variant_tag (struct.get)
    // because variant_tag traps on null refs.
    stmts.push(expr_stmt(builtin_call(
        "i32_store8",
        vec![
            addr.clone(),
            variant_test(
                local_ref(value_local, "__opt_val", value_type_id),
                0,
                "Some",
            ),
        ],
        TypeTable::UNIT,
    )));

    // Compute payload offset (aligned to payload alignment)
    let payload_align = crate::component_model::cm_align_with_registry(inner_type, wasi_registry);
    let payload_offset = cm_abi::align_to(1, payload_align);

    let payload_addr = if payload_offset == 0 {
        addr
    } else {
        binary_add(addr, i32_const(payload_offset as i32))
    };

    // If Some: lower payload to memory
    let inner_type_id = {
        let mut tt = type_table.borrow_mut();
        wasi_type_to_type_id(inner_type, &mut tt, wasi_registry, wasi_package)
    };
    let payload_expr = variant_payload(
        local_ref(value_local, "__opt_val", value_type_id),
        0,
        inner_type_id,
    );

    let case_stmts = synthesize_lower_wasi_type_to_memory(
        inner_type,
        payload_expr,
        payload_addr,
        next_local,
        locals,
        wasi_registry,
        wasi_package,
        type_table,
    );

    stmts.push(if_stmt(
        variant_test(
            local_ref(value_local, "__opt_val", value_type_id),
            0,
            "Some",
        ),
        block(case_stmts),
        None,
    ));
}

/// Flatten a WASI type value (GC ref) to flat CM ABI args (i32/i64/f32/f64).
///
/// Used for sync function params where the binding receives a GC value
/// but needs to pass flat values to the WASI import.
/// Appends lowering statements to `stmts` and flat value expressions to `flat_args`.
pub(super) fn synthesize_flatten_value_to_flat_args(
    ty: &Type,
    value: TirExpr,
    prefix: &str,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    flat_args: &mut Vec<TirExpr>,
    wasi_registry: &WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) {
    let resolved = wasi_registry.resolve_type(ty);
    match &resolved {
        // String → cm_lower_string → packed i64 → (ptr, len)
        Type::Named(n) if n.name == "String" => {
            let packed_local = alloc_local(next_local, locals, TypeTable::I64);
            stmts.push(let_stmt(
                &format!("{prefix}_packed"),
                packed_local,
                TypeTable::I64,
                internal_call("cm_lower_string", vec![value], TypeTable::I64),
            ));
            // ptr = packed as i32 (low 32 bits)
            flat_args.push(cast(
                local_ref(packed_local, &format!("{prefix}_packed"), TypeTable::I64),
                TypeTable::I32,
            ));
            // len = (packed >> 32) as i32 (high 32 bits)
            flat_args.push(cast(
                binary(
                    TirBinaryOp::Shr,
                    local_ref(packed_local, &format!("{prefix}_packed"), TypeTable::I64),
                    i64_const(32),
                    TypeTable::I64,
                ),
                TypeTable::I32,
            ));
        }
        // Enum → variant_tag (single i32)
        Type::Named(n)
            if n.source_interface.as_deref().is_some_and(|s| {
                s.starts_with("wasi:")
                    && wasi_registry
                        .get_enum_variants_by_source(s, &n.name)
                        .is_some()
            }) =>
        {
            flat_args.push(variant_tag(value));
        }
        // Variant → disc + join of all case payload flats
        Type::Named(n)
            if n.source_interface.as_deref().is_some_and(|s| {
                s.starts_with("wasi:")
                    && wasi_registry
                        .get_variant_cases_by_source(s, &n.name)
                        .is_some()
            }) =>
        {
            let source = n
                .source_interface
                .as_deref()
                .expect("wasi variant source_interface present");
            let vt = value.type_id;
            let val_local = alloc_local(next_local, locals, vt);
            stmts.push(let_stmt(&format!("{prefix}_val"), val_local, vt, value));

            // Push discriminant
            flat_args.push(variant_tag(local_ref(
                val_local,
                &format!("{prefix}_val"),
                vt,
            )));

            // Compute max flat payload count across all cases (the "join")
            let cases = wasi_registry
                .get_variant_cases_by_source(source, &n.name)
                .unwrap_or(&[]);
            let max_flat_count: usize = cases
                .iter()
                .map(|c| {
                    c.payload
                        .as_ref()
                        .map(|t| flatten_param_type(t, wasi_registry).len())
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0);

            if max_flat_count > 0 {
                // Allocate mutable locals for each payload flat slot, initialized to 0
                let mut payload_locals: Vec<(u32, TypeId)> = Vec::new();
                for i in 0..max_flat_count {
                    let local = alloc_local(next_local, locals, TypeTable::I32);
                    stmts.push(let_mut_stmt(
                        &format!("{prefix}_p{i}"),
                        local,
                        TypeTable::I32,
                        i32_const(0),
                    ));
                    payload_locals.push((local, TypeTable::I32));
                }

                // For each case with a payload, generate conditional flattening
                for (case_idx, case) in cases.iter().enumerate() {
                    if let Some(payload_ty) = &case.payload {
                        let payload_type_id = {
                            let mut tt = type_table.borrow_mut();
                            wasi_type_to_type_id(payload_ty, &mut tt, wasi_registry, wasi_package)
                        };
                        let payload_expr = variant_payload(
                            local_ref(val_local, &format!("{prefix}_val"), vt),
                            case_idx as u32,
                            payload_type_id,
                        );

                        let mut case_stmts = Vec::new();
                        let mut case_flat = Vec::new();
                        synthesize_flatten_value_to_flat_args(
                            payload_ty,
                            payload_expr,
                            &format!("{prefix}_c{case_idx}"),
                            next_local,
                            &mut case_stmts,
                            locals,
                            &mut case_flat,
                            wasi_registry,
                            wasi_package,
                            type_table,
                        );
                        // Assign case flat values to shared payload locals
                        for (i, flat_val) in case_flat.into_iter().enumerate() {
                            if i < payload_locals.len() {
                                let (pl, pt) = payload_locals[i];
                                case_stmts.push(expr_stmt(assign(
                                    local_ref(pl, &format!("{prefix}_p{i}"), pt),
                                    flat_val,
                                )));
                            }
                        }
                        stmts.push(if_stmt(
                            variant_test(
                                local_ref(val_local, &format!("{prefix}_val"), vt),
                                case_idx as u32,
                                &case.wado_name,
                            ),
                            block(case_stmts),
                            None,
                        ));
                    }
                }

                // Push all payload locals as flat args
                for (i, (pl, pt)) in payload_locals.iter().enumerate() {
                    flat_args.push(local_ref(*pl, &format!("{prefix}_p{i}"), *pt));
                }
            }
        }
        // Simple primitives / handles → pass through directly
        _ => {
            flat_args.push(value);
        }
    }
}

/// Flatten an `Option<T>` GC value to flat CM ABI args for sync function calls.
///
/// Produces: [discriminant (0=None, 1=Some), ...flatten(T)]
/// The discriminant and payload locals are mutable, initialized to 0,
/// and populated conditionally when the option is `Some`.
pub(super) fn synthesize_flatten_option_to_flat_args(
    inner_type: &Type,
    value: TirExpr,
    prefix: &str,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    flat_args: &mut Vec<TirExpr>,
    wasi_registry: &WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) {
    let vt = value.type_id;
    let val_local = alloc_local(next_local, locals, vt);
    stmts.push(let_stmt(&format!("{prefix}_optval"), val_local, vt, value));

    // CM ABI option discriminant: 0 = None, 1 = Some.
    // Use variant_test (ref.test) instead of variant_tag (struct.get)
    // because variant_tag traps on null refs.
    let disc_local = alloc_local(next_local, locals, TypeTable::I32);
    stmts.push(let_stmt(
        &format!("{prefix}_disc"),
        disc_local,
        TypeTable::I32,
        variant_test(
            local_ref(val_local, &format!("{prefix}_optval"), vt),
            0,
            "Some",
        ),
    ));
    flat_args.push(local_ref(
        disc_local,
        &format!("{prefix}_disc"),
        TypeTable::I32,
    ));

    // Compute inner flat types
    let inner_flat_types = flatten_param_type(inner_type, wasi_registry);
    if inner_flat_types.is_empty() {
        return;
    }

    // Allocate mutable locals for inner flats, initialized to 0
    let mut inner_locals: Vec<(u32, TypeId)> = Vec::new();
    for (i, &ft) in inner_flat_types.iter().enumerate() {
        let local = alloc_local(next_local, locals, ft);
        let zero = match ft {
            TypeTable::I64 => i64_const(0),
            _ => i32_const(0),
        };
        stmts.push(let_mut_stmt(&format!("{prefix}_inner{i}"), local, ft, zero));
        inner_locals.push((local, ft));
    }

    // If Some: extract payload and flatten it
    let inner_type_id = {
        let mut tt = type_table.borrow_mut();
        wasi_type_to_type_id(inner_type, &mut tt, wasi_registry, wasi_package)
    };
    let payload_expr = variant_payload(
        local_ref(val_local, &format!("{prefix}_optval"), vt),
        0, // case_index 0 = Some
        inner_type_id,
    );

    let mut some_stmts = Vec::new();
    let mut some_flat = Vec::new();
    synthesize_flatten_value_to_flat_args(
        inner_type,
        payload_expr,
        &format!("{prefix}_s"),
        next_local,
        &mut some_stmts,
        locals,
        &mut some_flat,
        wasi_registry,
        wasi_package,
        type_table,
    );
    // Assign flattened values to the mutable locals
    for (i, flat_val) in some_flat.into_iter().enumerate() {
        if i < inner_locals.len() {
            let (il, it) = inner_locals[i];
            some_stmts.push(expr_stmt(assign(
                local_ref(il, &format!("{prefix}_inner{i}"), it),
                flat_val,
            )));
        }
    }
    stmts.push(if_stmt(
        variant_test(
            local_ref(val_local, &format!("{prefix}_optval"), vt),
            0,
            "Some",
        ),
        block(some_stmts),
        None,
    ));

    // Push inner locals as flat args
    for (i, (il, it)) in inner_locals.iter().enumerate() {
        flat_args.push(local_ref(*il, &format!("{prefix}_inner{i}"), *it));
    }
}

/// Lower a WASI type value to linear memory at the given address.
pub(super) fn synthesize_lower_wasi_type_to_memory(
    ty: &Type,
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
    wasi_registry: &WasiRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) -> Vec<TirStmt> {
    let resolved = wasi_registry.resolve_type(ty);
    match &resolved {
        Type::Named(n) => {
            // CM record lowering: store each field at its offset, keyed on
            // the exact source interface the Named reference was resolved
            // to. Accepts both `wasi:*` and `core:kiln/*` sources so the
            // kiln generator's record surface (input-file, output-file,
            // response, raw-request) lowers through the same path as WASI
            // records. Callers (e.g. the `Array<T>` element lower)
            // populate `source_interface` via `type_id_to_ast_type` so
            // this lookup does not need a fallback path.
            let source = n.source_interface.as_deref();
            if let Some(fields) = source
                .and_then(|s| wasi_registry.get_struct_fields_with_wado_names_by_source(s, &n.name))
            {
                let resolved_fields: Vec<(String, Type)> = fields
                    .iter()
                    .map(|(wn, _, ft)| (wn.clone(), wasi_registry.resolve_type(ft)))
                    .collect();
                let mut stmts = Vec::new();
                let mut offset = 0u32;
                let mut max_align = 1u32;
                let value_type_id = value.type_id;
                let val_local = alloc_local(next_local, locals, value_type_id);
                stmts.push(let_stmt("__struct_val", val_local, value_type_id, value));

                for (field_idx, (wado_name, field_ty)) in resolved_fields.iter().enumerate() {
                    let fa =
                        crate::component_model::cm_align_with_registry(field_ty, wasi_registry);
                    let fs = crate::component_model::cm_size_with_registry(field_ty, wasi_registry);
                    offset = cm_abi::align_to(offset, fa);
                    let field_type_id = {
                        let mut tt = type_table.borrow_mut();
                        wasi_type_to_type_id(field_ty, &mut tt, wasi_registry, wasi_package)
                    };
                    let field_expr = TirExpr {
                        kind: TirExprKind::FieldAccess {
                            expr: Box::new(local_ref(val_local, "__struct_val", value_type_id)),
                            field_index: field_idx as u32,
                            field_name: wado_name.clone(),
                        },
                        type_id: field_type_id,
                        span: synth_span(),
                    };
                    let field_addr = if offset == 0 {
                        addr.clone()
                    } else {
                        binary_add(addr.clone(), i32_const(offset as i32))
                    };
                    stmts.extend(synthesize_lower_wasi_type_to_memory(
                        field_ty,
                        field_expr,
                        field_addr,
                        next_local,
                        locals,
                        wasi_registry,
                        wasi_package,
                        type_table,
                    ));
                    offset += fs;
                    max_align = max_align.max(fa);
                }
                return stmts;
            }
            // Fall through to synthesize_lower for primitives and simple types
            synthesize_lower(&resolved, value, addr, next_local, locals)
        }
        _ => synthesize_lower(&resolved, value, addr, next_local, locals),
    }
}
