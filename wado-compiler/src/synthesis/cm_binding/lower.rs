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

use crate::ast::{NamedType, Type};
use crate::cm_abi;
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName};
use crate::tir::{
    CallArg, FunctionRef, ResolvedType, TirBinaryOp, TirExpr, TirExprKind, TirLocal, TirMatchArm,
    TirPattern, TirStmt, TirStmtKind, TypeId, TypeTable,
};

use crate::synthesis::common::{
    alloc_local, assign, binary, block, break_stmt, builtin_call, cast, expr_stmt,
    generic_method_call, i32_const, i64_const, if_stmt, internal_call, let_mut_stmt, let_stmt,
    local_ref, loop_stmt, split_packed_ptr_len, synth_span,
};

use super::types::{
    LowerContext, binary_add, cm_type_to_type_id, cm_val_type_from_type_id, coerce_flat_lower,
    field_access, flatten_param_type, is_unit_type, kebab_to_pascal, variant_tag, variant_test,
};

/// Join two CM flat slot types via the single Canonical ABI join
/// ([`cm_abi::CmValType::join`]) so a lowered flat arg matches the core import's
/// signature: `{i32, f32}` widens to `i32`, any other mismatch to `i64`.
fn join_flat_slot(a: Option<TypeId>, b: Option<TypeId>) -> TypeId {
    super::types::cm_val_type_to_type_id(cm_abi::CmValType::join(
        a.map(cm_val_type_from_type_id),
        b.map(cm_val_type_from_type_id),
    ))
}

/// Zero constant matching a CM flat slot type (i32/i64/f32/f64).
fn flat_slot_zero(tid: TypeId) -> TirExpr {
    if tid == TypeTable::I64 {
        i64_const(0)
    } else if tid == TypeTable::F32 {
        super::types::cm_zero(cm_abi::CmValType::F32)
    } else if tid == TypeTable::F64 {
        super::types::cm_zero(cm_abi::CmValType::F64)
    } else {
        i32_const(0)
    }
}

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
    ctx: &LowerContext<'_>,
) -> Vec<TirStmt> {
    match ty {
        Type::Named(named) if named.name == ctx.names.string => {
            // cm_lower_string(value) returns packed i64: (ptr | (len << 32))
            let packed_local = *next_local;
            locals.push(TirLocal::synth(*next_local, TypeTable::I64, false));
            *next_local += 1;
            let packed = internal_call("cm_lower_string", vec![value], TypeTable::I64);
            let mut stmts = vec![let_stmt("__packed", packed_local, TypeTable::I64, packed)];

            let (ptr, len) =
                split_packed_ptr_len(local_ref(packed_local, "__packed", TypeTable::I64));
            // Store ptr (low 32 bits) at addr, len (high 32 bits) at addr + 4.
            stmts.push(expr_stmt(builtin_call(
                "i32_store",
                vec![addr.clone(), ptr],
                TypeTable::UNIT,
            )));
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
            // Unknown named types: treat as i32 handles (enums, resources)
            _ => vec![expr_stmt(builtin_call(
                "i32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))],
        },
        // Own<T>, Borrow<T>, Stream<T>, Future<T>: i32 handles
        Type::Generic(g) if matches!(g.name.as_str(), "Stream" | "Future" | "Own" | "Borrow") => {
            vec![expr_stmt(builtin_call(
                "i32_store",
                vec![addr, value],
                TypeTable::UNIT,
            ))]
        }
        Type::Generic(g) => panic!(
            "`{}` must lower via synthesize_lower_wasi_type_to_memory, \
             which handles aggregate layout; synthesize_lower covers primitives only",
            g.name
        ),
        Type::Tuple(elems) if elems.is_empty() => vec![],
        Type::Tuple(_) => panic!(
            "tuples must lower via synthesize_lower_tuple, \
             which computes element offsets; synthesize_lower covers primitives only"
        ),
        Type::Reference(_) | Type::MutReference(_) => vec![expr_stmt(builtin_call(
            "i32_store",
            vec![addr, value],
            TypeTable::UNIT,
        ))],
        other => panic!("unsupported type for CM memory lower: {other:?}"),
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
    ctx: &LowerContext<'_>,
) -> Vec<TirStmt> {
    let layout = cm_abi::layout_tuple_with_registry_scoped(
        elems,
        ctx.cm_interface_registry,
        Some(ctx.wasi_package),
    );
    let mut stmts = Vec::new();

    // Element TypeIds come from the tuple's own type arguments — the
    // elaborator-registered types with correct module sources. Re-deriving them
    // via `cm_type_to_type_id` can't resolve a lib-local struct's source and
    // would fall back to i32, breaking FieldAccess on aggregate elements.
    let elem_type_ids = ctx
        .type_table
        .borrow()
        .as_tuple(value.type_id)
        .unwrap_or_default();

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

        // Prefer the registered element TypeId; fall back only if the tuple
        // type lacks args (shouldn't happen for a well-formed tuple value).
        let field_type_id = elem_type_ids.get(i).copied().unwrap_or_else(|| {
            let mut tt = ctx.type_table.borrow_mut();
            cm_type_to_type_id(
                elem_ty,
                &mut tt,
                ctx.cm_interface_registry,
                ctx.wasi_package,
            )
        });

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

        // Recursively lower each element through the full registry-aware
        // memory lowerer so aggregate elements (records, variants, options,
        // lists, results, nested tuples) lay out their payload correctly
        // rather than being stored as an opaque i32 GC ref.
        stmts.extend(synthesize_lower_wasi_type_to_memory(
            elem_ty, field_expr, field_addr, next_local, locals, ctx,
        ));
    }

    stmts
}

/// A variant case for memory lowering: its Wado (Pascal) case name and, for a
/// payload-bearing case, the payload's CM AST `Type` and GC `TypeId`.
pub(super) type CmMemCase = (String, Option<(Type, TypeId)>);

/// Lower a variant GC value to a linear-memory buffer at `addr`, given its cases.
///
/// Memory layout (Canonical ABI): a 1-byte discriminant at offset 0, then the
/// active case's payload at `align_to(1, max_payload_align)`; each case lowers
/// via a `Match` arm. `disc_of` builds the discriminant byte from the
/// materialised value — registry variants use `variant_tag` (the full tag),
/// while `Result` uses `variant_test` on its Err case. Shared by the
/// registry-backed variant path and the `Result` path; `Option` keeps its own
/// helper (its `None` is a null ref that would trap `variant_tag`).
fn synthesize_lower_variant_to_memory(
    value: TirExpr,
    addr: TirExpr,
    cases: Vec<CmMemCase>,
    disc_of: impl Fn(TirExpr) -> TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    ctx: &LowerContext<'_>,
) {
    let value_type_id = value.type_id;

    let value_local = alloc_local(next_local, locals, value_type_id);
    stmts.push(let_stmt("__variant_val", value_local, value_type_id, value));

    stmts.push(expr_stmt(builtin_call(
        "i32_store8",
        vec![
            addr.clone(),
            disc_of(local_ref(value_local, "__variant_val", value_type_id)),
        ],
        TypeTable::UNIT,
    )));

    let payload_offset = cm_abi::variant_payload_offset_with_registry_scoped(
        cases
            .iter()
            .filter_map(|(_, p)| p.as_ref().map(|(ty, _)| ty)),
        ctx.cm_interface_registry,
        Some(ctx.wasi_package),
    );
    let payload_addr = if payload_offset == 0 {
        addr
    } else {
        binary_add(addr, i32_const(payload_offset as i32))
    };

    let span = synth_span();
    let mut arms: Vec<TirMatchArm> = Vec::with_capacity(cases.len());
    for (case_name, payload) in cases {
        if let Some((payload_ty, payload_type_id)) = payload {
            let binding_local = alloc_local(next_local, locals, payload_type_id);
            let binding_name = format!("__variant_payload_{binding_local}");
            let payload_expr = local_ref(binding_local, &binding_name, payload_type_id);

            let case_stmts = synthesize_lower_wasi_type_to_memory(
                &payload_ty,
                payload_expr,
                payload_addr.clone(),
                next_local,
                locals,
                ctx,
            );

            arms.push(TirMatchArm {
                pattern: TirPattern::Variant {
                    enum_type: value_type_id,
                    variant_name: case_name,
                    bindings: vec![TirPattern::Binding {
                        name: binding_name,
                        local_index: binding_local,
                        type_id: payload_type_id,
                    }],
                    payload_type: payload_type_id,
                },
                guard: None,
                body: TirExpr::new(
                    TirExprKind::Block(crate::tir::TirBlock::new(case_stmts, span)),
                    TypeTable::UNIT,
                    span,
                ),
                span,
            });
        } else {
            arms.push(TirMatchArm {
                pattern: TirPattern::Variant {
                    enum_type: value_type_id,
                    variant_name: case_name,
                    bindings: Vec::new(),
                    payload_type: TypeTable::UNIT,
                },
                guard: None,
                body: TirExpr::new(
                    TirExprKind::Block(crate::tir::TirBlock::new(Vec::new(), span)),
                    TypeTable::UNIT,
                    span,
                ),
                span,
            });
        }
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

/// Lower a registry-backed variant (WASI or lib-local) GC value to memory.
/// Resolves the cases from the registry, then delegates to
/// [`synthesize_lower_variant_to_memory`].
pub(super) fn synthesize_lower_wasi_variant_to_memory(
    named: &NamedType,
    source: &str,
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    ctx: &LowerContext<'_>,
) {
    let name = named.name.as_str();
    let Some(cases) = ctx
        .cm_interface_registry
        .get_variant_cases_by_source(source, name)
    else {
        // Fallback: store as i32
        stmts.push(expr_stmt(builtin_call(
            "i32_store8",
            vec![addr, variant_tag(value)],
            TypeTable::UNIT,
        )));
        return;
    };
    let mem_cases: Vec<CmMemCase> = cases
        .iter()
        .cloned()
        .map(|case| {
            let payload = case.payload.map(|ty| {
                let tid = {
                    let mut tt = ctx.type_table.borrow_mut();
                    cm_type_to_type_id(&ty, &mut tt, ctx.cm_interface_registry, ctx.wasi_package)
                };
                (ty, tid)
            });
            (kebab_to_pascal(&case.cm_name), payload)
        })
        .collect();
    synthesize_lower_variant_to_memory(
        value,
        addr,
        mem_cases,
        variant_tag,
        next_local,
        stmts,
        locals,
        ctx,
    );
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
    ctx: &LowerContext<'_>,
) {
    let value_type_id = value.type_id;
    let names = &ctx.names;

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
                names.some_index,
                &names.some_name,
            ),
        ],
        TypeTable::UNIT,
    )));

    let payload_offset = cm_abi::layout_option_with_registry_scoped(
        inner_type,
        ctx.cm_interface_registry,
        Some(ctx.wasi_package),
    )
    .offsets[1];

    let payload_addr = if payload_offset == 0 {
        addr
    } else {
        binary_add(addr, i32_const(payload_offset as i32))
    };

    // Lower the Some payload to memory by binding it inside a TIR
    // `Match` arm — canonical post-Phase 10 shape. The None arm is
    // empty; the Match's exhaustiveness on `Option<T>` removes the
    // need for a wildcard else.
    //
    // Take the payload TypeId from the Option's own type argument (the
    // elaborator-registered type, correct module source) rather than
    // re-deriving it via `cm_type_to_type_id`, which can't resolve a lib-local
    // struct source and would fall back to i32.
    let inner_type_id = {
        let tt = ctx.type_table.borrow();
        match tt.get(value_type_id) {
            crate::tir::ResolvedType::GenericInstance { type_args, .. }
                if !type_args.is_empty() =>
            {
                type_args[0]
            }
            _ => {
                drop(tt);
                cm_type_to_type_id(
                    inner_type,
                    &mut ctx.type_table.borrow_mut(),
                    ctx.cm_interface_registry,
                    ctx.wasi_package,
                )
            }
        }
    };

    let payload_binding_local = alloc_local(next_local, locals, inner_type_id);
    let payload_binding_name = format!("__opt_payload_{payload_binding_local}");
    let payload_expr = local_ref(payload_binding_local, &payload_binding_name, inner_type_id);

    let case_stmts = synthesize_lower_wasi_type_to_memory(
        inner_type,
        payload_expr,
        payload_addr,
        next_local,
        locals,
        ctx,
    );

    let span = synth_span();
    let some_arm = TirMatchArm {
        pattern: TirPattern::Variant {
            enum_type: value_type_id,
            variant_name: names.some_name.clone(),
            bindings: vec![TirPattern::Binding {
                name: payload_binding_name,
                local_index: payload_binding_local,
                type_id: inner_type_id,
            }],
            payload_type: inner_type_id,
        },
        guard: None,
        body: TirExpr::new(
            TirExprKind::Block(crate::tir::TirBlock::new(case_stmts, span)),
            TypeTable::UNIT,
            span,
        ),
        span,
    };
    let none_arm = TirMatchArm {
        pattern: TirPattern::Variant {
            enum_type: value_type_id,
            variant_name: names.none_name.clone(),
            bindings: Vec::new(),
            payload_type: TypeTable::UNIT,
        },
        guard: None,
        body: TirExpr::new(
            TirExprKind::Block(crate::tir::TirBlock::new(Vec::new(), span)),
            TypeTable::UNIT,
            span,
        ),
        span,
    };
    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(local_ref(value_local, "__opt_val", value_type_id)),
            arms: vec![some_arm, none_arm],
        },
        TypeTable::UNIT,
        span,
    );
    stmts.push(TirStmt::new(TirStmtKind::Expr(match_expr), span));
}

/// Lower a `Result<T, E>` (GC value) to a CM `result` at `addr`. `Result` is a
/// 2-case variant (Ok=0, Err=1), so it shares [`synthesize_lower_variant_to_memory`]:
/// build its case list (Ok/Err names from compiler items, payload types from the
/// instance's type args; unit arms carry no payload) and delegate.
pub(super) fn synthesize_lower_result_to_memory(
    ok_type: &Type,
    err_type: &Type,
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    ctx: &LowerContext<'_>,
) {
    let value_type_id = value.type_id;
    let (ok_name, err_name, err_index, ok_tid, err_tid) = {
        let tt = ctx.type_table.borrow();
        let (_, _, ok_name, _) = tt
            .compiler_items()
            .require_variant_case(crate::compiler_item::CompilerItem::ResultOk);
        let (_, _, err_name, err_index) = tt
            .compiler_items()
            .require_variant_case(crate::compiler_item::CompilerItem::ResultErr);
        let (ok_tid, err_tid) = match tt.get(value_type_id) {
            ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
                (type_args[0], type_args[1])
            }
            _ => (TypeTable::UNIT, TypeTable::UNIT),
        };
        (
            ok_name.to_string(),
            err_name.to_string(),
            err_index,
            ok_tid,
            err_tid,
        )
    };

    let arm = |name: String, ty: &Type, tid: TypeId| -> CmMemCase {
        if is_unit_type(ty) {
            (name, None)
        } else {
            (name, Some((ty.clone(), tid)))
        }
    };
    let cases = vec![
        arm(ok_name, ok_type, ok_tid),
        arm(err_name.clone(), err_type, err_tid),
    ];

    // disc byte: `variant_test(Err)` → 1 when Err, 0 when Ok. (`variant_tag`,
    // used for registry variants, mis-resolves the generic `Result` struct.)
    let disc_of = move |v: TirExpr| variant_test(v, err_index, &err_name);
    synthesize_lower_variant_to_memory(value, addr, cases, disc_of, next_local, stmts, locals, ctx);
}

/// Lower a `List<T>` (GC value) to a CM `list` element buffer in linear memory,
/// returning the statements plus the locals holding `(base_ptr, len)`.
///
/// Lowers each element via the recursive [`synthesize_lower_wasi_type_to_memory`]
/// so ref-bearing elements lower correctly rather than as opaque `i32` handles.
pub(super) fn synthesize_lower_list_to_buffer(
    elem_type: &Type,
    value: TirExpr,
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
    ctx: &LowerContext<'_>,
) -> (Vec<TirStmt>, u32, u32) {
    let names = &ctx.names;
    let list_type_id = value.type_id;
    let elem_resolved = ctx.cm_interface_registry.resolve_type(elem_type);

    let elem_size = crate::component_model::cm_size_with_registry_scoped(
        &elem_resolved,
        ctx.cm_interface_registry,
        Some(ctx.wasi_package),
    ) as i32;
    let elem_align = crate::component_model::cm_align_with_registry_scoped(
        &elem_resolved,
        ctx.cm_interface_registry,
        Some(ctx.wasi_package),
    ) as i32;
    // Take the element TypeId from the list's own type arguments — it is the
    // elaborator-registered type (correct module source), unlike a fresh
    // `cm_type_to_type_id`, which can't resolve a lib-local struct's source and
    // would fall back to i32.
    let elem_type_id = {
        let tt = ctx.type_table.borrow();
        match tt.get(list_type_id) {
            crate::tir::ResolvedType::GenericInstance { type_args, .. }
                if !type_args.is_empty() =>
            {
                type_args[0]
            }
            _ => {
                drop(tt);
                let mut tt = ctx.type_table.borrow_mut();
                cm_type_to_type_id(
                    &elem_resolved,
                    &mut tt,
                    ctx.cm_interface_registry,
                    ctx.wasi_package,
                )
            }
        }
    };

    let mut stmts = Vec::new();

    // Materialize the list so we can call len/index_value on it repeatedly.
    let list_local = alloc_local(next_local, locals, list_type_id);
    stmts.push(let_stmt("__list_val", list_local, list_type_id, value));

    // __len = List::len(list)
    let len_local = alloc_local(next_local, locals, TypeTable::I32);
    stmts.push(let_stmt(
        "__list_len",
        len_local,
        TypeTable::I32,
        generic_method_call(
            local_ref(list_local, "__list_val", list_type_id),
            &names.array,
            "len",
            ModuleSource::list(),
            vec![],
            TypeTable::I32,
        ),
    ));

    // __base = realloc(0, 0, elem_align, __len * elem_size)
    let base_local = alloc_local(next_local, locals, TypeTable::I32);
    stmts.push(let_stmt(
        "__list_base",
        base_local,
        TypeTable::I32,
        builtin_call(
            "realloc",
            vec![
                i32_const(0),
                i32_const(0),
                i32_const(elem_align),
                binary(
                    TirBinaryOp::Mul,
                    local_ref(len_local, "__list_len", TypeTable::I32),
                    i32_const(elem_size),
                    TypeTable::I32,
                ),
            ],
            TypeTable::I32,
        ),
    ));

    // __i = 0; loop { if __i >= __len break; lower elem[__i] at __base + __i*size; __i += 1 }
    let i_local = alloc_local(next_local, locals, TypeTable::I32);
    stmts.push(let_mut_stmt(
        "__list_i",
        i_local,
        TypeTable::I32,
        i32_const(0),
    ));

    let mut loop_body = Vec::new();
    loop_body.push(if_stmt(
        binary(
            TirBinaryOp::GtEq,
            local_ref(i_local, "__list_i", TypeTable::I32),
            local_ref(len_local, "__list_len", TypeTable::I32),
            TypeTable::BOOL,
        ),
        block(vec![break_stmt()]),
        None,
    ));
    let addr_local = alloc_local(next_local, locals, TypeTable::I32);
    loop_body.push(let_stmt(
        "__list_addr",
        addr_local,
        TypeTable::I32,
        binary(
            TirBinaryOp::Add,
            local_ref(base_local, "__list_base", TypeTable::I32),
            binary(
                TirBinaryOp::Mul,
                local_ref(i_local, "__list_i", TypeTable::I32),
                i32_const(elem_size),
                TypeTable::I32,
            ),
            TypeTable::I32,
        ),
    ));
    // __elem = list.index_value(__i)
    let elem_local = alloc_local(next_local, locals, elem_type_id);
    let iv_info = LocalMethodName::new(
        FqTypeName::from_mangled(names.array.clone()),
        Some("IndexValue<i32>".to_string()),
        "index_value".to_string(),
    );
    let iv_mangled = iv_info.to_mangled_name();
    loop_body.push(let_stmt(
        "__list_elem",
        elem_local,
        elem_type_id,
        TirExpr::new(
            TirExprKind::method_call(
                Box::new(local_ref(list_local, "__list_val", list_type_id)),
                FunctionRef {
                    module_source: ModuleSource::list(),
                    name: iv_mangled,
                    monomorph_info: None,
                    method_info: Some(iv_info),
                },
                vec![],
                vec![CallArg::new(
                    local_ref(i_local, "__list_i", TypeTable::I32),
                    false,
                )],
            ),
            elem_type_id,
            synth_span(),
        ),
    ));
    loop_body.extend(synthesize_lower_wasi_type_to_memory(
        &elem_resolved,
        local_ref(elem_local, "__list_elem", elem_type_id),
        local_ref(addr_local, "__list_addr", TypeTable::I32),
        next_local,
        locals,
        ctx,
    ));
    loop_body.push(expr_stmt(assign(
        local_ref(i_local, "__list_i", TypeTable::I32),
        binary(
            TirBinaryOp::Add,
            local_ref(i_local, "__list_i", TypeTable::I32),
            i32_const(1),
            TypeTable::I32,
        ),
    )));
    stmts.push(loop_stmt(block(loop_body)));

    (stmts, base_local, len_local)
}

/// Lower a `List<T>` (GC value) to a CM `list` at `addr`: an element buffer in
/// linear memory plus the `(ptr, len)` pair stored at `addr` / `addr + 4`.
pub(super) fn synthesize_lower_list_to_memory(
    elem_type: &Type,
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
    ctx: &LowerContext<'_>,
) -> Vec<TirStmt> {
    let (mut stmts, base_local, len_local) =
        synthesize_lower_list_to_buffer(elem_type, value, next_local, locals, ctx);
    stmts.push(expr_stmt(builtin_call(
        "i32_store",
        vec![
            addr.clone(),
            local_ref(base_local, "__list_base", TypeTable::I32),
        ],
        TypeTable::UNIT,
    )));
    stmts.push(expr_stmt(builtin_call(
        "i32_store",
        vec![
            binary_add(addr, i32_const(4)),
            local_ref(len_local, "__list_len", TypeTable::I32),
        ],
        TypeTable::UNIT,
    )));
    stmts
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
    ctx: &LowerContext<'_>,
) {
    let names = &ctx.names;
    let resolved = ctx.cm_interface_registry.resolve_type(ty);
    match &resolved {
        // String → cm_lower_string → packed i64 → (ptr, len)
        Type::Named(n) if n.name == names.string => {
            let packed_local = alloc_local(next_local, locals, TypeTable::I64);
            stmts.push(let_stmt(
                &format!("{prefix}_packed"),
                packed_local,
                TypeTable::I64,
                internal_call("cm_lower_string", vec![value], TypeTable::I64),
            ));
            let (ptr, len) = split_packed_ptr_len(local_ref(
                packed_local,
                &format!("{prefix}_packed"),
                TypeTable::I64,
            ));
            flat_args.push(ptr);
            flat_args.push(len);
        }
        // Enum → variant_tag (single i32). The registry lookup is the gate:
        // every source it holds is a CM interface, so a prefix check would be
        // redundant (same for the record/variant gates below).
        Type::Named(n)
            if n.source_interface.as_deref().is_some_and(|s| {
                ctx.cm_interface_registry
                    .get_enum_variants_by_source(s, &n.name)
                    .is_some()
            }) =>
        {
            flat_args.push(variant_tag(value));
        }
        // Variant → disc + join of all case payload flats
        Type::Named(n)
            if n.source_interface.as_deref().is_some_and(|s| {
                ctx.cm_interface_registry
                    .get_variant_cases_by_source(s, &n.name)
                    .is_some()
            }) =>
        {
            let source = n
                .source_interface
                .as_deref()
                .expect("CM variant source_interface present");
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
            let cases = ctx
                .cm_interface_registry
                .get_variant_cases_by_source(source, &n.name)
                .unwrap_or(&[]);
            let max_flat_count: usize = cases
                .iter()
                .map(|c| {
                    c.payload
                        .as_ref()
                        .map(|t| flatten_param_type(t, ctx.cm_interface_registry, names).len())
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0);

            if max_flat_count > 0 {
                // Per-slot joined flat types for the payload (skip the leading
                // discriminant slot). Using these — rather than hardcoding i32 —
                // keeps an i64/f64-carrying case from being truncated into an i32
                // local, and matches the flat signature `flatten_param_type`
                // declares for this variant.
                let payload_slot_types: Vec<TypeId> = {
                    let joined = flatten_param_type(&resolved, ctx.cm_interface_registry, names);
                    joined.get(1..).map(<[TypeId]>::to_vec).unwrap_or_default()
                };
                // Allocate mutable locals for each payload flat slot, zero-init.
                let mut payload_locals: Vec<(u32, TypeId)> = Vec::new();
                for i in 0..max_flat_count {
                    let slot_ty = payload_slot_types.get(i).copied().unwrap_or(TypeTable::I32);
                    let local = alloc_local(next_local, locals, slot_ty);
                    stmts.push(let_mut_stmt(
                        &format!("{prefix}_p{i}"),
                        local,
                        slot_ty,
                        flat_slot_zero(slot_ty),
                    ));
                    payload_locals.push((local, slot_ty));
                }

                // Single TIR Match over the materialised variant: one
                // arm per case. Payload-bearing arms bind the payload
                // and assign the flattened values to the shared
                // `payload_locals`; unit-payload arms have empty bodies.
                // Variant Match is exhaustive at TIR level so no
                // wildcard arm is needed.
                let span = synth_span();
                let mut arms: Vec<TirMatchArm> = Vec::with_capacity(cases.len());
                for case in cases {
                    let case_name = case.wado_name.clone();
                    if let Some(payload_ty) = &case.payload {
                        let payload_type_id = {
                            let mut tt = ctx.type_table.borrow_mut();
                            cm_type_to_type_id(
                                payload_ty,
                                &mut tt,
                                ctx.cm_interface_registry,
                                ctx.wasi_package,
                            )
                        };
                        let binding_local = alloc_local(next_local, locals, payload_type_id);
                        let binding_name = format!("__variant_payload_{binding_local}");
                        let payload_expr = local_ref(binding_local, &binding_name, payload_type_id);

                        let mut case_stmts = Vec::new();
                        let mut case_flat = Vec::new();
                        synthesize_flatten_value_to_flat_args(
                            payload_ty,
                            payload_expr,
                            &format!("{prefix}_c{}", case_name.as_str()),
                            next_local,
                            &mut case_stmts,
                            locals,
                            &mut case_flat,
                            ctx,
                        );
                        for (i, flat_val) in case_flat.into_iter().enumerate() {
                            if i < payload_locals.len() {
                                let (pl, pt) = payload_locals[i];
                                // Join this case's payload flat into the shared
                                // slot, bit-reinterpreting when the join widened
                                // the slot to a different core class (e.g. an
                                // `f32` case sharing an `i32`-joined slot).
                                let flat_val_ty = flat_val.type_id;
                                let v = coerce_flat_lower(
                                    flat_val,
                                    cm_val_type_from_type_id(flat_val_ty),
                                    cm_val_type_from_type_id(pt),
                                );
                                case_stmts.push(expr_stmt(assign(
                                    local_ref(pl, &format!("{prefix}_p{i}"), pt),
                                    v,
                                )));
                            }
                        }

                        arms.push(TirMatchArm {
                            pattern: TirPattern::Variant {
                                enum_type: vt,
                                variant_name: case_name,
                                bindings: vec![TirPattern::Binding {
                                    name: binding_name,
                                    local_index: binding_local,
                                    type_id: payload_type_id,
                                }],
                                payload_type: payload_type_id,
                            },
                            guard: None,
                            body: TirExpr::new(
                                TirExprKind::Block(crate::tir::TirBlock::new(case_stmts, span)),
                                TypeTable::UNIT,
                                span,
                            ),
                            span,
                        });
                    } else {
                        arms.push(TirMatchArm {
                            pattern: TirPattern::Variant {
                                enum_type: vt,
                                variant_name: case_name,
                                bindings: Vec::new(),
                                payload_type: TypeTable::UNIT,
                            },
                            guard: None,
                            body: TirExpr::new(
                                TirExprKind::Block(crate::tir::TirBlock::new(Vec::new(), span)),
                                TypeTable::UNIT,
                                span,
                            ),
                            span,
                        });
                    }
                }
                if !arms.is_empty() {
                    let match_expr = TirExpr::new(
                        TirExprKind::Match {
                            expr: Box::new(local_ref(val_local, &format!("{prefix}_val"), vt)),
                            arms,
                        },
                        TypeTable::UNIT,
                        span,
                    );
                    stmts.push(TirStmt::new(TirStmtKind::Expr(match_expr), span));
                }

                // Push all payload locals as flat args
                for (i, (pl, pt)) in payload_locals.iter().enumerate() {
                    flat_args.push(local_ref(*pl, &format!("{prefix}_p{i}"), *pt));
                }
            }
        }
        // Option<T> → discriminant i32 + flatten(T). Delegates to the
        // dedicated option flattener, which conditionally lowers the Some
        // payload via a Match. Reached for an `Option` field of a CM record
        // (e.g. `span: Option<SourceSpan>` on the kiln-host Diagnostic).
        Type::Generic(g) if g.name == names.option && g.args.len() == 1 => {
            synthesize_flatten_option_to_flat_args(
                &g.args[0], value, prefix, next_local, stmts, locals, flat_args, ctx,
            );
        }
        // The core `Result` is not a CM-registry variant, so it never matches
        // the variant arm above and needs its own flattener.
        Type::Generic(g) if g.name == names.result && g.args.len() == 2 => {
            synthesize_flatten_result_to_flat_args(
                &g.args[0], &g.args[1], value, prefix, next_local, stmts, locals, flat_args, ctx,
            );
        }
        // List<T> → (ptr, len): lower into an element buffer, push base/len.
        Type::Generic(g) if g.name == names.array && g.args.len() == 1 => {
            let (list_stmts, base_local, len_local) =
                synthesize_lower_list_to_buffer(&g.args[0], value, next_local, locals, ctx);
            stmts.extend(list_stmts);
            flat_args.push(local_ref(base_local, "__list_base", TypeTable::I32));
            flat_args.push(local_ref(len_local, "__list_len", TypeTable::I32));
        }
        Type::Tuple(elems) if !elems.is_empty() => synthesize_flatten_tuple_to_flat_args(
            elems, value, prefix, next_local, stmts, locals, flat_args, ctx,
        ),
        // CM record → its fields' flat args (recursion handles nested records).
        Type::Named(n)
            if n.source_interface.as_deref().is_some_and(|s| {
                ctx.cm_interface_registry
                    .get_struct_fields_with_wado_names_by_source(s, &n.name)
                    .is_some()
            }) =>
        {
            let source = n.source_interface.as_deref().expect("matched above");
            let fields = ctx
                .cm_interface_registry
                .get_struct_fields_with_wado_names_by_source(source, &n.name)
                .expect("matched above");
            let value_type_id = value.type_id;
            let val_local = alloc_local(next_local, locals, value_type_id);
            let val_name = format!("{prefix}_rec");
            stmts.push(let_stmt(&val_name, val_local, value_type_id, value));
            flatten_cm_record_fields(
                fields,
                val_local,
                &val_name,
                value_type_id,
                prefix,
                next_local,
                stmts,
                locals,
                flat_args,
                ctx,
            );
        }
        // Primitive, flags, resource, or borrow handle: the value is the single flat arg.
        Type::Named(_) | Type::Reference(_) | Type::MutReference(_) => flat_args.push(value),
        // Stream / Future / Own / Borrow are i32 handles (single flat arg).
        Type::Generic(g) if matches!(g.name.as_str(), "Stream" | "Future" | "Own" | "Borrow") => {
            flat_args.push(value);
        }
        other => panic!("unsupported type for CM value flattening: {other:?}"),
    }
}

/// Flatten a tuple value (GC ref) into its elements' flat CM ABI args, in order.
fn synthesize_flatten_tuple_to_flat_args(
    elems: &[Type],
    value: TirExpr,
    prefix: &str,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    flat_args: &mut Vec<TirExpr>,
    ctx: &LowerContext<'_>,
) {
    let tuple_type_id = value.type_id;
    let val_local = alloc_local(next_local, locals, tuple_type_id);
    let val_name = format!("{prefix}_tup");
    stmts.push(let_stmt(&val_name, val_local, tuple_type_id, value));
    let elem_type_ids: Vec<TypeId> = ctx
        .type_table
        .borrow()
        .as_tuple(tuple_type_id)
        .unwrap_or_default();
    for (i, elem_ty) in elems.iter().enumerate() {
        let elem_type_id = elem_type_ids.get(i).copied().unwrap_or_else(|| {
            let mut tt = ctx.type_table.borrow_mut();
            cm_type_to_type_id(
                elem_ty,
                &mut tt,
                ctx.cm_interface_registry,
                ctx.wasi_package,
            )
        });
        let elem_expr = field_access(
            local_ref(val_local, &val_name, tuple_type_id),
            &i.to_string(),
            i as u32,
            elem_type_id,
        );
        synthesize_flatten_value_to_flat_args(
            elem_ty,
            elem_expr,
            &format!("{prefix}_e{i}"),
            next_local,
            stmts,
            locals,
            flat_args,
            ctx,
        );
    }
}

/// Flatten a CM record value (already bound to `val_local` of `value_type_id`)
/// into its fields' flat CM-ABI args, in declared order. Each field recurses
/// through [`synthesize_flatten_value_to_flat_args`], so String / Option /
/// enum / nested-record fields expand to their own flat slots. Shared by the
/// nested-field path here and the top-level struct-param path in
/// `import_adapter`, so both flatten a record by exactly the same rule.
pub(super) fn flatten_cm_record_fields(
    fields: &[(String, String, Type)],
    val_local: u32,
    val_name: &str,
    value_type_id: TypeId,
    prefix: &str,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    flat_args: &mut Vec<TirExpr>,
    ctx: &LowerContext<'_>,
) {
    for (field_idx, (wado_name, _, field_ty)) in fields.iter().enumerate() {
        let resolved_field = ctx.cm_interface_registry.resolve_type(field_ty);
        let field_type_id = {
            let mut tt = ctx.type_table.borrow_mut();
            cm_type_to_type_id(
                &resolved_field,
                &mut tt,
                ctx.cm_interface_registry,
                ctx.wasi_package,
            )
        };
        let field_expr = field_access(
            local_ref(val_local, val_name, value_type_id),
            wado_name,
            field_idx as u32,
            field_type_id,
        );
        synthesize_flatten_value_to_flat_args(
            &resolved_field,
            field_expr,
            &format!("{prefix}_f{field_idx}"),
            next_local,
            stmts,
            locals,
            flat_args,
            ctx,
        );
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
    ctx: &LowerContext<'_>,
) {
    let names = &ctx.names;
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
            names.some_index,
            &names.some_name,
        ),
    ));
    flat_args.push(local_ref(
        disc_local,
        &format!("{prefix}_disc"),
        TypeTable::I32,
    ));

    // Compute inner flat types
    let inner_flat_types = flatten_param_type(inner_type, ctx.cm_interface_registry, names);
    if inner_flat_types.is_empty() {
        return;
    }

    // Zero-init by slot type: an `f32`/`f64` slot needs a float zero, not an i32 zero.
    let mut inner_locals: Vec<(u32, TypeId)> = Vec::new();
    for (i, &ft) in inner_flat_types.iter().enumerate() {
        let local = alloc_local(next_local, locals, ft);
        stmts.push(let_mut_stmt(
            &format!("{prefix}_inner{i}"),
            local,
            ft,
            flat_slot_zero(ft),
        ));
        inner_locals.push((local, ft));
    }

    // Flatten the Some payload via a TIR Match that binds the inner
    // value. The None arm is empty; variant `Match` is exhaustive on
    // `Option<T>` so no wildcard is required.
    let inner_type_id = {
        let mut tt = ctx.type_table.borrow_mut();
        cm_type_to_type_id(
            inner_type,
            &mut tt,
            ctx.cm_interface_registry,
            ctx.wasi_package,
        )
    };
    let payload_binding_local = alloc_local(next_local, locals, inner_type_id);
    let payload_binding_name = format!("__opt_payload_{payload_binding_local}");
    let payload_expr = local_ref(payload_binding_local, &payload_binding_name, inner_type_id);

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
        ctx,
    );
    for (i, flat_val) in some_flat.into_iter().enumerate() {
        if i < inner_locals.len() {
            let (il, it) = inner_locals[i];
            some_stmts.push(expr_stmt(assign(
                local_ref(il, &format!("{prefix}_inner{i}"), it),
                flat_val,
            )));
        }
    }

    let span = synth_span();
    let some_arm = TirMatchArm {
        pattern: TirPattern::Variant {
            enum_type: vt,
            variant_name: names.some_name.clone(),
            bindings: vec![TirPattern::Binding {
                name: payload_binding_name,
                local_index: payload_binding_local,
                type_id: inner_type_id,
            }],
            payload_type: inner_type_id,
        },
        guard: None,
        body: TirExpr::new(
            TirExprKind::Block(crate::tir::TirBlock::new(some_stmts, span)),
            TypeTable::UNIT,
            span,
        ),
        span,
    };
    let none_arm = TirMatchArm {
        pattern: TirPattern::Variant {
            enum_type: vt,
            variant_name: names.none_name.clone(),
            bindings: Vec::new(),
            payload_type: TypeTable::UNIT,
        },
        guard: None,
        body: TirExpr::new(
            TirExprKind::Block(crate::tir::TirBlock::new(Vec::new(), span)),
            TypeTable::UNIT,
            span,
        ),
        span,
    };
    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(local_ref(val_local, &format!("{prefix}_optval"), vt)),
            arms: vec![some_arm, none_arm],
        },
        TypeTable::UNIT,
        span,
    );
    stmts.push(TirStmt::new(TirStmtKind::Expr(match_expr), span));

    // Push inner locals as flat args
    for (i, (il, it)) in inner_locals.iter().enumerate() {
        flat_args.push(local_ref(*il, &format!("{prefix}_inner{i}"), *it));
    }
}

/// Flatten a `Result<T, E>` GC value to flat CM ABI args. Unlike `Option`, reads
/// the discriminant with `variant_tag` (`struct.get`) — a `Result` is never null.
pub(super) fn synthesize_flatten_result_to_flat_args(
    ok_type: &Type,
    err_type: &Type,
    value: TirExpr,
    prefix: &str,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    flat_args: &mut Vec<TirExpr>,
    ctx: &LowerContext<'_>,
) {
    let names = &ctx.names;
    let vt = value.type_id;
    let val_local = alloc_local(next_local, locals, vt);
    stmts.push(let_stmt(&format!("{prefix}_resval"), val_local, vt, value));

    // Discriminant: 0 = Ok, 1 = Err.
    let disc_local = alloc_local(next_local, locals, TypeTable::I32);
    stmts.push(let_stmt(
        &format!("{prefix}_disc"),
        disc_local,
        TypeTable::I32,
        variant_tag(local_ref(val_local, &format!("{prefix}_resval"), vt)),
    ));
    flat_args.push(local_ref(
        disc_local,
        &format!("{prefix}_disc"),
        TypeTable::I32,
    ));

    let ok_flat = flatten_param_type(ok_type, ctx.cm_interface_registry, names);
    let err_flat = flatten_param_type(err_type, ctx.cm_interface_registry, names);
    let max_len = ok_flat.len().max(err_flat.len());
    if max_len == 0 {
        return;
    }

    let mut payload_locals: Vec<(u32, TypeId)> = Vec::new();
    for i in 0..max_len {
        let slot_ty = join_flat_slot(ok_flat.get(i).copied(), err_flat.get(i).copied());
        let local = alloc_local(next_local, locals, slot_ty);
        stmts.push(let_mut_stmt(
            &format!("{prefix}_p{i}"),
            local,
            slot_ty,
            flat_slot_zero(slot_ty),
        ));
        payload_locals.push((local, slot_ty));
    }

    let span = synth_span();
    let ok_arm = flatten_result_case_arm(
        names.ok_name.clone(),
        ok_type,
        vt,
        &payload_locals,
        prefix,
        next_local,
        locals,
        ctx,
    );
    let err_arm = flatten_result_case_arm(
        names.err_name.clone(),
        err_type,
        vt,
        &payload_locals,
        prefix,
        next_local,
        locals,
        ctx,
    );
    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(local_ref(val_local, &format!("{prefix}_resval"), vt)),
            arms: vec![ok_arm, err_arm],
        },
        TypeTable::UNIT,
        span,
    );
    stmts.push(TirStmt::new(TirStmtKind::Expr(match_expr), span));

    for (i, (pl, pt)) in payload_locals.iter().enumerate() {
        flat_args.push(local_ref(*pl, &format!("{prefix}_p{i}"), *pt));
    }
}

fn flatten_result_case_arm(
    case_name: String,
    payload_type: &Type,
    enum_type: TypeId,
    payload_locals: &[(u32, TypeId)],
    prefix: &str,
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
    ctx: &LowerContext<'_>,
) -> TirMatchArm {
    let span = synth_span();

    // Empty flat list ⇒ unit payload: a binding-free arm.
    if flatten_param_type(payload_type, ctx.cm_interface_registry, &ctx.names).is_empty() {
        return TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type,
                variant_name: case_name,
                bindings: Vec::new(),
                payload_type: TypeTable::UNIT,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(crate::tir::TirBlock::new(Vec::new(), span)),
                TypeTable::UNIT,
                span,
            ),
            span,
        };
    }

    let payload_type_id = {
        let mut tt = ctx.type_table.borrow_mut();
        cm_type_to_type_id(
            payload_type,
            &mut tt,
            ctx.cm_interface_registry,
            ctx.wasi_package,
        )
    };
    let binding_local = alloc_local(next_local, locals, payload_type_id);
    let binding_name = format!("__result_payload_{binding_local}");
    let payload_expr = local_ref(binding_local, &binding_name, payload_type_id);

    let mut case_stmts = Vec::new();
    let mut case_flat = Vec::new();
    synthesize_flatten_value_to_flat_args(
        payload_type,
        payload_expr,
        &format!("{prefix}_c{case_name}"),
        next_local,
        &mut case_stmts,
        locals,
        &mut case_flat,
        ctx,
    );
    for (i, flat_val) in case_flat.into_iter().enumerate() {
        if i < payload_locals.len() {
            let (pl, pt) = payload_locals[i];
            // Bit-reinterpret into the joined slot; a numeric cast would corrupt
            // a value sharing a different-class slot (f32 1.5 -> i32 1).
            let flat_val_ty = flat_val.type_id;
            let v = coerce_flat_lower(
                flat_val,
                cm_val_type_from_type_id(flat_val_ty),
                cm_val_type_from_type_id(pt),
            );
            case_stmts.push(expr_stmt(assign(
                local_ref(pl, &format!("{prefix}_p{i}"), pt),
                v,
            )));
        }
    }

    TirMatchArm {
        pattern: TirPattern::Variant {
            enum_type,
            variant_name: case_name,
            bindings: vec![TirPattern::Binding {
                name: binding_name,
                local_index: binding_local,
                type_id: payload_type_id,
            }],
            payload_type: payload_type_id,
        },
        guard: None,
        body: TirExpr::new(
            TirExprKind::Block(crate::tir::TirBlock::new(case_stmts, span)),
            TypeTable::UNIT,
            span,
        ),
        span,
    }
}

/// Lower a WASI type value to linear memory at the given address.
pub(super) fn synthesize_lower_wasi_type_to_memory(
    ty: &Type,
    value: TirExpr,
    addr: TirExpr,
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
    ctx: &LowerContext<'_>,
) -> Vec<TirStmt> {
    let names = &ctx.names;
    let resolved = ctx.cm_interface_registry.resolve_type(ty);
    match &resolved {
        Type::Named(n) => {
            // CM record lowering: store each field at its offset, keyed on
            // the exact source interface the Named reference was resolved
            // to. Accepts both `wasi:*` and `core:kiln/*` sources so the
            // kiln generator's record surface (input-file, output-file,
            // response, raw-request) lowers through the same path as WASI
            // records. Callers (e.g. the `List<T>` element lower)
            // populate `source_interface` via `type_id_to_ast_type` so
            // this lookup does not need a fallback path.
            // Resolve via the registry so lib-local records (no
            // `source_interface` on the nested type) are found by name under
            // their package's default-interface FQ, like WASI/kiln records.
            let source = ctx
                .cm_interface_registry
                .resolve_cm_source_for(n, Some(ctx.wasi_package));
            if let Some(fields) = source.and_then(|s| {
                ctx.cm_interface_registry
                    .get_struct_fields_with_wado_names_by_source(s, &n.name)
            }) {
                let resolved_fields: Vec<(String, Type)> = fields
                    .iter()
                    .map(|(wn, _, ft)| (wn.clone(), ctx.cm_interface_registry.resolve_type(ft)))
                    .collect();
                let offsets = cm_abi::layout_fields_with_registry_scoped(
                    resolved_fields.iter().map(|(_, ty)| ty),
                    ctx.cm_interface_registry,
                    Some(ctx.wasi_package),
                )
                .offsets;
                let mut stmts = Vec::new();
                let value_type_id = value.type_id;
                let val_local = alloc_local(next_local, locals, value_type_id);
                stmts.push(let_stmt("__struct_val", val_local, value_type_id, value));

                for (field_idx, (wado_name, field_ty)) in resolved_fields.iter().enumerate() {
                    let offset = offsets[field_idx];
                    let field_type_id = {
                        let mut tt = ctx.type_table.borrow_mut();
                        cm_type_to_type_id(
                            field_ty,
                            &mut tt,
                            ctx.cm_interface_registry,
                            ctx.wasi_package,
                        )
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
                        field_ty, field_expr, field_addr, next_local, locals, ctx,
                    ));
                }
                return stmts;
            }
            // Named `variant` (sum type with payloads): lower disc + active
            // case payload via the registry-backed variant helper, keyed on the
            // same resolved source. Lib-local variants register their cases
            // under the package's default-interface FQ, like WASI variants.
            if let Some(src) = source
                && ctx
                    .cm_interface_registry
                    .get_variant_cases_by_source(src, &n.name)
                    .is_some()
            {
                let mut stmts = Vec::new();
                synthesize_lower_wasi_variant_to_memory(
                    n, src, value, addr, next_local, &mut stmts, locals, ctx,
                );
                return stmts;
            }
            // Fall through to synthesize_lower for primitives and simple types
            synthesize_lower(&resolved, value, addr, next_local, locals, ctx)
        }
        Type::Tuple(elems) if !elems.is_empty() => {
            synthesize_lower_tuple(elems, value, addr, next_local, locals, ctx)
        }
        Type::Generic(g) if g.name == names.option && g.args.len() == 1 => {
            let inner = ctx.cm_interface_registry.resolve_type(&g.args[0]);
            let mut stmts = Vec::new();
            synthesize_lower_option_to_memory(
                &inner, value, addr, next_local, &mut stmts, locals, ctx,
            );
            stmts
        }
        Type::Generic(g) if g.name == names.array && g.args.len() == 1 => {
            synthesize_lower_list_to_memory(&g.args[0], value, addr, next_local, locals, ctx)
        }
        Type::Generic(g) if g.name == names.result && g.args.len() == 2 => {
            let ok = ctx.cm_interface_registry.resolve_type(&g.args[0]);
            let err = ctx.cm_interface_registry.resolve_type(&g.args[1]);
            let mut stmts = Vec::new();
            synthesize_lower_result_to_memory(
                &ok, &err, value, addr, next_local, &mut stmts, locals, ctx,
            );
            stmts
        }
        _ => synthesize_lower(&resolved, value, addr, next_local, locals, ctx),
    }
}
