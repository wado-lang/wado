//! Import-side CM adapter synthesis.
//!
//! Generates a `__cm_binding__<iface>_<method>` TIR function for every WASI
//! import the program references. The body lowers Wado-typed args to the
//! flat CM ABI shape, performs the `cm_raw_call`, and (for sync imports)
//! lifts the result back. Truly async imports return an `AsyncCall<T>`
//! struct holding the subtask handle and outptr; the caller is responsible
//! for `wait()`-ing on it.
//!
//! Entry points used by the driver: [`binding_func_name`] for the synthesized
//! function name and [`synthesize_adapter`] for the body. The adapter shares
//! [`make_binding_function`] with export-side synthesis, hence its
//! `pub(super)` visibility.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::{NamedType, Type};
use crate::component_model::{CmFunctionInfo, CmInterfaceRegistry};
use crate::hashmap::IndexSet;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name::LocalMethodName;
use crate::tir::{
    CallArg, EffectRef, FunctionKind, FunctionRef, InlineHint, TirBinaryOp, TirBlock, TirExpr,
    TirExprKind, TirFunction, TirLocal, TirParam, TirStmt, TirStructField, TypeId, TypeTable,
};

use crate::synthesis::common::{
    alloc_local, assign, binary, block, break_stmt, builtin_call, cm_raw_call, expr_stmt,
    generic_method_call, i32_const, if_stmt, internal_call, let_mut_stmt, let_stmt, local_ref,
    loop_stmt, null_expr, return_stmt, split_packed_ptr_len, synth_span,
};

use super::lift::{materialize_if_needed, synthesize_lift, try_lift_wasi_variant_or_enum};
use super::lower::{
    flatten_cm_record_fields, synthesize_flatten_option_to_flat_args,
    synthesize_flatten_result_to_flat_args, synthesize_flatten_value_to_flat_args,
    synthesize_lower_option_to_memory, synthesize_lower_wasi_type_to_memory,
    synthesize_lower_wasi_variant_to_memory,
};
use super::types::{
    CmStdlibNames, LiftContext, LowerContext, binary_add, cm_param_store_plan, cm_type_to_type_id,
    cm_val_type_to_type_id, flatten_param_type, needs_flat_result_lifting,
};

/// Build the binding function name for a WASI import.
pub fn binding_func_name(interface_name: &str, method_name: &str) -> String {
    format!("__cm_binding__{interface_name}_{method_name}")
}

/// Per-import lift function name. Pointed to by `AsyncCall<T>::__cm_lift`.
fn lift_func_name(interface_name: &str, method_name: &str) -> String {
    format!("__cm_lift__{interface_name}_{method_name}")
}

/// Functions produced by [`synthesize_adapter`] for a single WASI import:
/// the user-visible `__cm_binding__*` adapter, plus any auxiliaries (for
/// async imports, the `__cm_lift__*` function the adapter's `AsyncCall<T>`
/// dispatches through).
pub(super) struct AdapterArtifacts {
    pub adapter: Rc<RefCell<TirFunction>>,
    pub auxiliary: Vec<Rc<RefCell<TirFunction>>>,
}

/// Synthesize lifting of a flat Result discriminant into a GC variant struct.
///
/// Only reached for a Result that flattens to a bare discriminant (one flat
/// slot) — i.e. `Result<(), ()>`: disc==0 → Ok, disc==1 → Err, neither
/// carrying a payload. Any payload-bearing Result flattens to >1 slot and is
/// lifted through the outptr return path instead, so it never arrives here.
/// (The non-unit Err branch below therefore stays defensive — see the
/// `debug_assert!` on `ok_is_unit`.)
///
/// `result_type_id` is the resolved `Result<T, E>` `TypeId` shared with
/// the caller's `result_local`; the emitted `VariantConstruct` exprs use
/// it directly so no `TypeTable::I32` placeholder leaks downstream.
fn synthesize_lift_flat_result(
    ty: &Type,
    disc_expr: TirExpr,
    result_local: u32,
    result_type_id: TypeId,
    next_local: &mut u32,
    stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    ctx: &LiftContext<'_>,
) -> TirExpr {
    if let Type::Generic(g) = ty
        && g.name == "Result"
        && g.args.len() == 2
    {
        let ok_ty = &g.args[0];
        let err_ty = &g.args[1];

        let ok_is_unit = matches!(ok_ty, Type::Named(n) if n.name == "()")
            || matches!(ok_ty, Type::Tuple(elems) if elems.is_empty());
        let err_is_unit = matches!(err_ty, Type::Named(n) if n.name == "()")
            || matches!(err_ty, Type::Tuple(elems) if elems.is_empty());

        let (ok_name, ok_index, err_name, err_index) = {
            let tt = ctx.type_table.borrow();
            let items = tt.compiler_items();
            let (_, _, ok_n, ok_i) =
                items.require_variant_case(crate::compiler_item::CompilerItem::ResultOk);
            let (_, _, err_n, err_i) =
                items.require_variant_case(crate::compiler_item::CompilerItem::ResultErr);
            (ok_n.to_string(), ok_i, err_n.to_string(), err_i)
        };

        // A flat (non-outptr) Result reaches here only when it flattens to a
        // bare discriminant, i.e. the Ok payload carries no flat slots (the
        // unit case). A non-unit Ok payload is routed through the outptr path
        // instead, so it must not appear here — guard the invariant rather
        // than silently dropping the payload.
        debug_assert!(
            ok_is_unit,
            "flat Result lift reached with a non-unit Ok payload; \
             expected the outptr return path to handle it"
        );
        let ok_construct = TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type: result_type_id,
                case_index: ok_index,
                case_name: ok_name,
                payload: None,
            },
            result_type_id,
            synth_span(),
        );

        let err_construct = if err_is_unit {
            TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type: result_type_id,
                    case_index: err_index,
                    case_name: err_name,
                    payload: None,
                },
                result_type_id,
                synth_span(),
            )
        } else {
            // Err with a flat payload — the remaining flat values encode the error.
            // `try_lift_wasi_variant_or_enum` returns None for non-CM types, so
            // we fall back to a bare Err.
            let lifted_variant = if let Type::Named(n) = err_ty
                && let Some(source) = n.source_interface.as_deref()
            {
                try_lift_wasi_variant_or_enum(
                    n,
                    source,
                    disc_expr.clone(),
                    next_local,
                    stmts,
                    locals,
                    ctx,
                )
            } else {
                None
            };
            if let Some(lifted) = lifted_variant {
                TirExpr::new(
                    TirExprKind::VariantConstruct {
                        variant_type: result_type_id,
                        case_index: err_index,
                        case_name: err_name,
                        payload: Some(Box::new(lifted)),
                    },
                    result_type_id,
                    synth_span(),
                )
            } else {
                TirExpr::new(
                    TirExprKind::VariantConstruct {
                        variant_type: result_type_id,
                        case_index: err_index,
                        case_name: err_name,
                        payload: None,
                    },
                    result_type_id,
                    synth_span(),
                )
            }
        };

        stmts.push(if_stmt(
            binary(TirBinaryOp::Eq, disc_expr, i32_const(0), TypeTable::BOOL),
            block(vec![expr_stmt(assign(
                local_ref(result_local, "__result_val", result_type_id),
                ok_construct,
            ))]),
            Some(block(vec![expr_stmt(assign(
                local_ref(result_local, "__result_val", result_type_id),
                err_construct,
            ))])),
        ));

        return local_ref(result_local, "__result_val", result_type_id);
    }

    // Fallback: just return the discriminant as-is
    disc_expr
}

/// Create a `TirFunction` with default metadata fields.
pub(super) fn make_binding_function(
    name: String,
    params: Vec<TirParam>,
    return_type: TypeId,
    body: TirBlock,
    local_count: u32,
    locals: Vec<TirLocal>,
) -> Rc<RefCell<TirFunction>> {
    Rc::new(RefCell::new(TirFunction {
        module_source: ModuleSource::default(),
        name,
        visibility: crate::ast::Visibility::Private,
        is_export: false,
        is_async: false,
        type_params: vec![],
        impl_type_params: vec![],
        monomorph_info: None,
        method_info: None,
        params,
        return_type,
        task_return_type: None,
        effects: vec![],
        stores: vec![],
        body: Some(body),
        span: synth_span(),
        local_count,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: true,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        benign_effects: Vec::new(),
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
    }))
}

/// Map a WASI return type to the flat return `TypeId` for the binding.
/// Sync functions with outptr return void from the raw call itself.
fn wasi_return_type_id(
    func_info: &CmFunctionInfo,
    cm_interface_registry: &CmInterfaceRegistry,
) -> TypeId {
    // Truly async imports (e.g., Client::send) use canon lower async and
    // return a subtask handle. Non-async imports with stream/future params
    // use sync lower (handles passed as i32, results returned directly).
    let needs_async_lower = func_info.is_async;
    if needs_async_lower {
        // Async canon lower: raw call returns subtask handle (i32)
        TypeTable::I32
    } else {
        let needs_outptr = func_info.return_type.as_ref().is_some_and(|rt| {
            crate::component_model::cm_return_needs_outptr(rt, cm_interface_registry)
        });
        if needs_outptr {
            // Raw call returns void; the result is read from the outptr.
            TypeTable::UNIT
        } else if let Some(ty) = &func_info.return_type {
            // Flat return: `needs_outptr` is false, so `cm_flatten` yields at
            // most one value — a record flattening to `[i64]` resolves to `i64`,
            // not a blanket `i32`.
            match cm_interface_registry.cm_flatten(ty).first().copied() {
                Some(vt) => cm_val_type_to_type_id(vt),
                None => TypeTable::UNIT,
            }
        } else {
            TypeTable::UNIT
        }
    }
}

/// Synthesise the per-import CM lift function for an async import. Body
/// is built from `func_info.return_type` via [`synthesize_lift`] — the
/// same helper sync imports use, so generic calls inside (e.g.
/// `List::with_capacity`) are visible to the monomorphizer.
fn synthesize_async_lift_function(
    name: String,
    func_info: &CmFunctionInfo,
    inner_type_id: TypeId,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &RefCell<TypeTable>,
    interner: &RefCell<ModuleSourceInterner>,
) -> Rc<RefCell<TirFunction>> {
    let mut next_local: u32 = 0;
    let mut locals: Vec<TirLocal> = Vec::new();
    let mut body_stmts: Vec<TirStmt> = Vec::new();

    let outptr_local = next_local;
    locals.push(TirLocal::synth(outptr_local, TypeTable::I32, false));
    next_local += 1;
    let params = vec![TirParam {
        name: "__outptr".to_string(),
        type_id: TypeTable::I32,
        local_index: outptr_local,
        is_mut: false,
        is_mut_ref: false,
        span: synth_span(),
    }];

    let lifted = if let Some(return_type) = &func_info.return_type {
        let resolved = cm_interface_registry.resolve_type(return_type);
        let lift_ctx = LiftContext {
            cm_interface_registry,
            type_table,
            cm_package: &func_info.package,
            interner,
        };
        let lifted = synthesize_lift(
            &resolved,
            local_ref(outptr_local, "__outptr", TypeTable::I32),
            &mut next_local,
            &mut body_stmts,
            &mut locals,
            &lift_ctx,
        );
        materialize_if_needed(lifted, &mut next_local, &mut body_stmts, &mut locals)
    } else {
        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span())
    };
    body_stmts.push(return_stmt(Some(lifted)));

    make_binding_function(
        name,
        params,
        inner_type_id,
        block(body_stmts),
        next_local,
        locals,
    )
}

/// Build the `AsyncCall<T>` struct literal returned by async import paths.
/// Field order and indices mirror the stdlib `AsyncCall` declaration.
fn make_async_call_literal(
    subtask_type: TypeId,
    packed: TirExpr,
    outptr: TirExpr,
    size: TirExpr,
    align: TirExpr,
    lift: TirExpr,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: subtask_type,
            struct_name: "AsyncCall".to_string(),
            fields: vec![
                TirStructField {
                    name: "__cm_packed".to_string(),
                    value: packed,
                    field_index: 0,
                },
                TirStructField {
                    name: "__cm_outptr".to_string(),
                    value: outptr,
                    field_index: 1,
                },
                TirStructField {
                    name: "__cm_size".to_string(),
                    value: size,
                    field_index: 2,
                },
                TirStructField {
                    name: "__cm_align".to_string(),
                    value: align,
                    field_index: 3,
                },
                TirStructField {
                    name: "__cm_lift".to_string(),
                    value: lift,
                    field_index: 4,
                },
            ],
        },
        subtask_type,
        synth_span(),
    )
}

#[allow(clippy::too_many_arguments)]
fn synthesize_async_wrap_function(
    name: String,
    func_info: &CmFunctionInfo,
    inner_type_id: TypeId,
    subtask_type: TypeId,
    outptr_size: u32,
    outptr_align: u32,
    lift_fn_ref: TirExpr,
    ctx: &LowerContext<'_>,
) -> Rc<RefCell<TirFunction>> {
    let span = synth_span();
    let mut next_local: u32 = 0;
    let mut locals: Vec<TirLocal> = Vec::new();
    let mut body_stmts: Vec<TirStmt> = Vec::new();

    let mut params: Vec<TirParam> = Vec::new();
    let value_local: Option<u32> = if inner_type_id == TypeTable::UNIT {
        None
    } else {
        let vl = next_local;
        locals.push(TirLocal::synth(vl, inner_type_id, false));
        next_local += 1;
        params.push(TirParam {
            name: "__value".to_string(),
            type_id: inner_type_id,
            local_index: vl,
            is_mut: false,
            is_mut_ref: false,
            span,
        });
        Some(vl)
    };

    let outptr_expr = if outptr_size > 0 {
        let value_local =
            value_local.expect("outptr_size > 0 implies a non-unit result value to lower");
        let outptr_local = next_local;
        locals.push(TirLocal::synth(outptr_local, TypeTable::I32, false));
        next_local += 1;
        body_stmts.push(let_stmt(
            "__outptr",
            outptr_local,
            TypeTable::I32,
            builtin_call(
                "realloc",
                vec![
                    i32_const(0),
                    i32_const(0),
                    i32_const(outptr_align as i32),
                    i32_const(outptr_size as i32),
                ],
                TypeTable::I32,
            ),
        ));
        let return_type = func_info
            .return_type
            .as_ref()
            .expect("outptr_size > 0 implies a result type");
        body_stmts.extend(synthesize_lower_wasi_type_to_memory(
            return_type,
            local_ref(value_local, "__value", inner_type_id),
            local_ref(outptr_local, "__outptr", TypeTable::I32),
            &mut next_local,
            &mut locals,
            ctx,
        ));
        local_ref(outptr_local, "__outptr", TypeTable::I32)
    } else {
        i32_const(0)
    };

    let completed = make_async_call_literal(
        subtask_type,
        i32_const(0),
        outptr_expr,
        i32_const(outptr_size as i32),
        i32_const(outptr_align as i32),
        lift_fn_ref,
    );
    body_stmts.push(return_stmt(Some(completed)));

    make_binding_function(
        name,
        params,
        subtask_type,
        block(body_stmts),
        next_local,
        locals,
    )
}

/// Lift a CM record that flattens to a single core value. The value is spilled
/// into a scratch buffer and lifted from that canonical memory image (the sole
/// non-unit leaf sits at offset 0), then the buffer is freed once the result is
/// materialized.
fn lift_flat_struct_return(
    resolved: &Type,
    flat: crate::cm_abi::CmValType,
    raw_call: TirExpr,
    raw_call_type: TypeId,
    next_local: &mut u32,
    body_stmts: &mut Vec<TirStmt>,
    locals: &mut Vec<TirLocal>,
    lift_ctx: &LiftContext<'_>,
) -> TirExpr {
    use crate::cm_abi::CmValType;
    let (store_op, byte_width) = match flat {
        CmValType::I32 => ("i32_store", 4),
        CmValType::I64 => ("i64_store", 8),
        CmValType::F32 => ("f32_store", 4),
        CmValType::F64 => ("f64_store", 8),
    };

    let flat_local = alloc_local(next_local, locals, raw_call_type);
    body_stmts.push(let_stmt("__flat", flat_local, raw_call_type, raw_call));

    let buf_local = alloc_local(next_local, locals, TypeTable::I32);
    body_stmts.push(let_stmt(
        "__flat_buf",
        buf_local,
        TypeTable::I32,
        builtin_call(
            "realloc",
            vec![
                i32_const(0),
                i32_const(0),
                i32_const(byte_width),
                i32_const(byte_width),
            ],
            TypeTable::I32,
        ),
    ));
    body_stmts.push(expr_stmt(builtin_call(
        store_op,
        vec![
            local_ref(buf_local, "__flat_buf", TypeTable::I32),
            local_ref(flat_local, "__flat", raw_call_type),
        ],
        TypeTable::UNIT,
    )));

    let lifted = synthesize_lift(
        resolved,
        local_ref(buf_local, "__flat_buf", TypeTable::I32),
        next_local,
        body_stmts,
        locals,
        lift_ctx,
    );
    let lifted = materialize_if_needed(lifted, next_local, body_stmts, locals);

    body_stmts.push(expr_stmt(builtin_call(
        "realloc",
        vec![
            local_ref(buf_local, "__flat_buf", TypeTable::I32),
            i32_const(byte_width),
            i32_const(byte_width),
            i32_const(0),
        ],
        TypeTable::I32,
    )));

    lifted
}

/// Synthesize a CM binding function for a WASI import.
///
/// The binding function:
/// 1. Accepts the same parameter types as the WASI function
/// 2. Lowers parameters to flat CM ABI (String → ptr/len, etc.)
/// 3. Calls the lowered WASI function via `CmRawCall`
/// 4. Lifts the result from flat CM ABI back to Wado types
/// 5. Returns the Wado-typed result
///
/// The binding's Wado-level return type matches the WASI function declaration.
/// All return types are lifted inline using `synthesize_lift` — no per-type
/// converter functions are needed.
///
/// For async imports the adapter additionally emits a sibling
/// [`synthesize_async_lift_function`]; both are returned via
/// [`AdapterArtifacts`].
pub(super) fn synthesize_adapter(
    func_info: &CmFunctionInfo,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &RefCell<TypeTable>,
    interner: &RefCell<ModuleSourceInterner>,
    owner_module: &ModuleSource,
    entry_source: &ModuleSource,
) -> AdapterArtifacts {
    let lower_ctx = LowerContext {
        cm_interface_registry,
        type_table,
        wasi_package: &func_info.package,
        names: CmStdlibNames::from_compiler_items(type_table.borrow().compiler_items()),
    };
    let mut builder = AdapterBuilder {
        func_info,
        lower_ctx,
        interner,
        entry_source,
        next_local: 0,
        params: Vec::new(),
        locals: Vec::new(),
        body_stmts: Vec::new(),
        flat_args: Vec::new(),
        auxiliary: Vec::new(),
    };

    let plans = builder.plan_params();
    builder.emit_param_lowering(&plans);

    let async_outptr = if func_info.is_async {
        builder.prepare_async_args(&plans)
    } else {
        None
    };
    let sync_outptr = if func_info.is_async {
        None
    } else {
        builder.alloc_sync_outptr()
    };

    let raw_call_return_type = wasi_return_type_id(func_info, cm_interface_registry);
    let raw_call = cm_raw_call(
        &func_info.local_alias_name(),
        std::mem::take(&mut builder.flat_args),
        raw_call_return_type,
    );

    let adapter_return_type = if func_info.is_async {
        builder.emit_async_result(raw_call, async_outptr)
    } else if let Some(outptr) = sync_outptr {
        builder.emit_outptr_result(raw_call, outptr)
    } else if let Some(return_type) = &func_info.return_type {
        builder.emit_flat_result(raw_call, raw_call_return_type, return_type)
    } else {
        builder.body_stmts.push(expr_stmt(raw_call));
        TypeTable::UNIT
    };

    let binding = make_binding_function(
        binding_func_name(&func_info.interface_name, &func_info.method_name),
        builder.params,
        adapter_return_type,
        block(builder.body_stmts),
        builder.next_local,
        builder.locals,
    );
    // Resources and effects are unified at the effect-system level: every
    // operation on `<E>` (whether `<E>` is declared as `effect` or `resource`)
    // requires the caller to hold `with <E>`. The binding for a CM-imported
    // operation therefore carries its owning name as its single concrete
    // effect. The propagation closure (built in `effect_check`) walks
    // operation signatures separately, so additional resources reachable
    // through `<E>`'s operations are admitted without listing them here.
    binding.borrow_mut().effects.push(EffectRef::Concrete {
        name: func_info.interface_name.clone(),
        module_source: owner_module.clone(),
    });
    AdapterArtifacts {
        adapter: binding,
        auxiliary: builder.auxiliary,
    }
}

/// How one WASI parameter reaches the flat CM ABI call. Classified once per
/// parameter by [`classify_param`]; both the adapter's parameter list and the
/// lowering code derive from the resulting [`ParamPlan`].
#[derive(Clone, Copy)]
enum ParamLowering<'a> {
    /// Flattens to no slots: no adapter param, nothing to lower.
    Unit,
    /// String / List<u8>: a single placeholder param; the stdlib `helper`
    /// packs (ptr, len) into an i64 that is split into two flat args.
    PackedPtrLen { helper: &'static str },
    /// General List<T>: single placeholder param; elements are lowered into a
    /// realloc'd linear-memory buffer passed as (ptr, len).
    ListBuffer { elem: &'a Type },
    /// WASI record: single GC-ref param; fields flatten into flat slots.
    RecordFlatten { named: &'a NamedType },
    /// WASI variant: single GC-ref param. Async passes the ref through (it is
    /// memory-lowered by the indirect params buffer); sync flattens it.
    Variant { named: &'a NamedType },
    /// Option<T>: single GC-ref param. Async passes the ref through; sync
    /// flattens to discriminant + payload slots.
    OptionValue { payload: &'a Type },
    /// Result<T, E>: single GC-ref param, flattened (sync only).
    ResultValue { ok: &'a Type, err: &'a Type },
    /// Non-empty tuple: single GC-ref param, flattened (sync only).
    TupleFlatten,
    /// Scalars/handles: flat params matching the CM ABI, forwarded unchanged.
    Direct,
}

/// Lowering plan for one WASI parameter. `first_param`/`param_count` locate
/// its adapter params (allocated during planning, so param locals stay
/// contiguous at indices [0..n-1] as Wasm requires).
struct ParamPlan<'a> {
    name: &'a str,
    ty: &'a Type,
    first_param: usize,
    param_count: usize,
    lowering: ParamLowering<'a>,
}

fn classify_param<'t>(
    param_type: &'t Type,
    registry: &CmInterfaceRegistry,
    names: &CmStdlibNames,
) -> ParamLowering<'t> {
    match param_type {
        Type::Named(n) if n.name == names.string => ParamLowering::PackedPtrLen {
            helper: "cm_lower_string",
        },
        Type::Generic(g)
            if g.name == names.array
                && g.args.len() == 1
                && matches!(&g.args[0], Type::Named(n) if n.name == "u8") =>
        {
            ParamLowering::PackedPtrLen {
                helper: "cm_lower_array_u8",
            }
        }
        Type::Generic(g) if g.name == names.array && g.args.len() == 1 => {
            ParamLowering::ListBuffer { elem: &g.args[0] }
        }
        Type::Named(n)
            if n.source_interface
                .as_deref()
                .is_some_and(|s| registry.get_struct_fields_by_source(s, &n.name).is_some()) =>
        {
            ParamLowering::RecordFlatten { named: n }
        }
        Type::Named(n)
            if n.source_interface
                .as_deref()
                .is_some_and(|s| registry.get_variant_cases_by_source(s, &n.name).is_some()) =>
        {
            ParamLowering::Variant { named: n }
        }
        Type::Generic(g) if g.name == names.option && g.args.len() == 1 => {
            ParamLowering::OptionValue {
                payload: &g.args[0],
            }
        }
        Type::Generic(g) if g.name == names.result && g.args.len() == 2 => {
            ParamLowering::ResultValue {
                ok: &g.args[0],
                err: &g.args[1],
            }
        }
        Type::Tuple(elems) if !elems.is_empty() => ParamLowering::TupleFlatten,
        // Scalars, plain enums/flags, and resource handles are a single flat
        // param forwarded unchanged; likewise `&self`/`&mut self` receivers
        // and the async/handle generics, all i32 handles. A `Named` here has
        // no registered struct/variant source (the arms above caught those),
        // so it is a scalar/enum/flags/resource — all Direct.
        Type::Named(_) | Type::Reference(_) | Type::MutReference(_) => ParamLowering::Direct,
        Type::Generic(g) if matches!(g.name.as_str(), "Stream" | "Future" | "Own" | "Borrow") => {
            ParamLowering::Direct
        }
        Type::Tuple(elems) if elems.is_empty() => ParamLowering::Direct,
        other => panic!("unsupported param type shape for CM import lowering: {other:?}"),
    }
}

/// A realloc'd result buffer: the local holding its address plus the
/// allocation's size/align (needed again to free it or to embed it in an
/// `AsyncCall<T>`).
#[derive(Clone, Copy)]
struct OutptrBuffer {
    local: u32,
    size: u32,
    align: u32,
}

/// CM Canonical ABI (size, align) of an import's return type, using the
/// registry-computed layout for named WASI variants (their generic `cm_size`
/// would be wrong) and registry-aware layout for structs and other complex
/// types.
fn cm_return_size_align(
    return_type: &Type,
    registry: &CmInterfaceRegistry,
    pkg: Option<&str>,
) -> (u32, u32) {
    if let Type::Named(named) = return_type
        && let Some(sa) = crate::component_model::cm_variant_size_align_scoped(named, registry, pkg)
    {
        return sa;
    }
    (
        crate::component_model::cm_size_with_registry_scoped(return_type, registry, pkg),
        crate::component_model::cm_align_with_registry_scoped(return_type, registry, pkg),
    )
}

fn params_buf_addr(params_buf_local: u32, offset: u32) -> TirExpr {
    let base = local_ref(params_buf_local, "__params_buf", TypeTable::I32);
    if offset == 0 {
        base
    } else {
        binary_add(base, i32_const(offset as i32))
    }
}

/// Accumulates the adapter function under construction: locals, params, body
/// statements, the flat CM call args, and auxiliary sibling functions.
struct AdapterBuilder<'a> {
    func_info: &'a CmFunctionInfo,
    lower_ctx: LowerContext<'a>,
    interner: &'a RefCell<ModuleSourceInterner>,
    entry_source: &'a ModuleSource,
    next_local: u32,
    params: Vec<TirParam>,
    locals: Vec<TirLocal>,
    body_stmts: Vec<TirStmt>,
    flat_args: Vec<TirExpr>,
    auxiliary: Vec<Rc<RefCell<TirFunction>>>,
}

impl<'a> AdapterBuilder<'a> {
    fn registry(&self) -> &'a CmInterfaceRegistry {
        self.lower_ctx.cm_interface_registry
    }

    fn lift_ctx(&self) -> LiftContext<'a> {
        let func_info = self.func_info;
        LiftContext {
            cm_interface_registry: self.lower_ctx.cm_interface_registry,
            type_table: self.lower_ctx.type_table,
            cm_package: &func_info.package,
            interner: self.interner,
        }
    }

    fn cm_type_id(&self, ty: &Type) -> TypeId {
        let mut tt = self.lower_ctx.type_table.borrow_mut();
        cm_type_to_type_id(
            ty,
            &mut tt,
            self.lower_ctx.cm_interface_registry,
            &self.func_info.package,
        )
    }

    fn push_param(&mut self, name: String, type_id: TypeId) {
        self.params.push(TirParam {
            name,
            type_id,
            local_index: self.next_local,
            is_mut: false,
            is_mut_ref: false,
            span: synth_span(),
        });
        self.locals
            .push(TirLocal::synth(self.next_local, type_id, false));
        self.next_local += 1;
    }

    /// Classify every WASI parameter once and allocate its adapter params.
    /// String/List params get a single i32 placeholder (the body lowers them
    /// to flat CM args); records/variants/options/results/tuples get a single
    /// GC-ref param; everything else gets flat params matching the CM ABI.
    fn plan_params(&mut self) -> Vec<ParamPlan<'a>> {
        let func_info = self.func_info;
        let mut plans = Vec::with_capacity(func_info.params.len());
        for (param_name, _, param_type) in &func_info.params {
            let flat_tys = flatten_param_type(
                param_type,
                self.lower_ctx.cm_interface_registry,
                &self.lower_ctx.names,
            );
            let lowering = if flat_tys.is_empty() {
                ParamLowering::Unit
            } else {
                classify_param(
                    param_type,
                    self.lower_ctx.cm_interface_registry,
                    &self.lower_ctx.names,
                )
            };
            let first_param = self.params.len();
            match lowering {
                ParamLowering::Unit => {}
                ParamLowering::PackedPtrLen { .. } | ParamLowering::ListBuffer { .. } => {
                    self.push_param(param_name.clone(), TypeTable::I32);
                }
                ParamLowering::RecordFlatten { .. }
                | ParamLowering::Variant { .. }
                | ParamLowering::OptionValue { .. }
                | ParamLowering::ResultValue { .. }
                | ParamLowering::TupleFlatten => {
                    let type_id = self.cm_type_id(param_type);
                    self.push_param(param_name.clone(), type_id);
                }
                ParamLowering::Direct => {
                    for (j, flat_ty) in flat_tys.iter().enumerate() {
                        let name = if flat_tys.len() == 1 {
                            param_name.clone()
                        } else {
                            format!("{param_name}_flat{j}")
                        };
                        self.push_param(name, *flat_ty);
                    }
                }
            }
            plans.push(ParamPlan {
                name: param_name,
                ty: param_type,
                first_param,
                param_count: self.params.len() - first_param,
                lowering,
            });
        }
        plans
    }

    /// Emit the per-parameter lowering code that turns adapter params into
    /// flat CM args. Intermediate locals land after all param locals.
    fn emit_param_lowering(&mut self, plans: &[ParamPlan<'a>]) {
        let func_info = self.func_info;
        for plan in plans {
            match plan.lowering {
                ParamLowering::Unit => {}
                ParamLowering::PackedPtrLen { helper } => {
                    let param_local = self.params[plan.first_param].local_index;
                    self.emit_packed_ptr_len(plan.name, param_local, helper);
                }
                ParamLowering::ListBuffer { elem } => {
                    let param_local = self.params[plan.first_param].local_index;
                    self.emit_list_buffer(plan.name, param_local, elem);
                }
                ParamLowering::RecordFlatten { named } => {
                    let source = named
                        .source_interface
                        .as_deref()
                        .expect("wasi struct source_interface present");
                    let wado_fields = self
                        .lower_ctx
                        .cm_interface_registry
                        .get_struct_fields_with_wado_names_by_source(source, &named.name)
                        .expect("struct fields_with_wado_names present when fields are");
                    let param = &self.params[plan.first_param];
                    let (param_local, struct_type_id) = (param.local_index, param.type_id);
                    // Flatten each field through the shared helper so a String /
                    // Option / nested-record / enum field expands to its own flat
                    // slots, matching the import's flattened signature.
                    flatten_cm_record_fields(
                        wado_fields,
                        param_local,
                        plan.name,
                        struct_type_id,
                        &format!("__{}", plan.name),
                        &mut self.next_local,
                        &mut self.body_stmts,
                        &mut self.locals,
                        &mut self.flat_args,
                        &self.lower_ctx,
                    );
                }
                ParamLowering::Variant { .. } => {
                    let param = &self.params[plan.first_param];
                    let param_ref = local_ref(param.local_index, plan.name, param.type_id);
                    if func_info.is_async {
                        // Async: pass the GC ref through; the indirect params
                        // buffer memory-lowers it.
                        self.flat_args.push(param_ref);
                    } else {
                        synthesize_flatten_value_to_flat_args(
                            plan.ty,
                            param_ref,
                            &format!("__{}", plan.name),
                            &mut self.next_local,
                            &mut self.body_stmts,
                            &mut self.locals,
                            &mut self.flat_args,
                            &self.lower_ctx,
                        );
                    }
                }
                ParamLowering::OptionValue { payload } => {
                    let param = &self.params[plan.first_param];
                    let param_ref = local_ref(param.local_index, plan.name, param.type_id);
                    if func_info.is_async {
                        // Async: pass the GC ref through; the indirect params
                        // buffer memory-lowers it.
                        self.flat_args.push(param_ref);
                    } else {
                        synthesize_flatten_option_to_flat_args(
                            payload,
                            param_ref,
                            &format!("__{}", plan.name),
                            &mut self.next_local,
                            &mut self.body_stmts,
                            &mut self.locals,
                            &mut self.flat_args,
                            &self.lower_ctx,
                        );
                    }
                }
                ParamLowering::ResultValue { ok, err } => {
                    // The async params-to-memory lowering for `Result` is unbuilt
                    // (no async CM import needs it yet); fail loud instead.
                    assert!(
                        !func_info.is_async,
                        "CM import '{}#{}' takes a `Result` parameter on an async \
                         function; async Result-param lowering is not yet implemented",
                        func_info.interface_name, func_info.method_name
                    );
                    let param = &self.params[plan.first_param];
                    let param_ref = local_ref(param.local_index, plan.name, param.type_id);
                    synthesize_flatten_result_to_flat_args(
                        ok,
                        err,
                        param_ref,
                        &format!("__{}", plan.name),
                        &mut self.next_local,
                        &mut self.body_stmts,
                        &mut self.locals,
                        &mut self.flat_args,
                        &self.lower_ctx,
                    );
                }
                ParamLowering::TupleFlatten => {
                    assert!(
                        !func_info.is_async,
                        "CM import '{}#{}' takes a tuple parameter on an async \
                         function; async tuple-param lowering is not yet implemented",
                        func_info.interface_name, func_info.method_name
                    );
                    let param = &self.params[plan.first_param];
                    let param_ref = local_ref(param.local_index, plan.name, param.type_id);
                    synthesize_flatten_value_to_flat_args(
                        plan.ty,
                        param_ref,
                        &format!("__{}", plan.name),
                        &mut self.next_local,
                        &mut self.body_stmts,
                        &mut self.locals,
                        &mut self.flat_args,
                        &self.lower_ctx,
                    );
                }
                ParamLowering::Direct => {
                    let range = plan.first_param..plan.first_param + plan.param_count;
                    for param in &self.params[range] {
                        let arg = local_ref(param.local_index, &param.name, param.type_id);
                        self.flat_args.push(arg);
                    }
                }
            }
        }
    }

    /// String / List<u8>: call the packing helper (→ packed i64) and split it
    /// into (ptr, len) flat args.
    fn emit_packed_ptr_len(&mut self, param_name: &str, param_local: u32, helper: &str) {
        let packed_name = format!("__{param_name}_packed");
        let packed_local = alloc_local(&mut self.next_local, &mut self.locals, TypeTable::I64);
        let packed = internal_call(
            helper,
            vec![local_ref(param_local, param_name, TypeTable::I32)],
            TypeTable::I64,
        );
        self.body_stmts
            .push(let_stmt(&packed_name, packed_local, TypeTable::I64, packed));

        let (ptr, len) =
            split_packed_ptr_len(local_ref(packed_local, &packed_name, TypeTable::I64));
        self.flat_args.push(ptr);
        self.flat_args.push(len);
    }

    /// General List<T>: lower each element into a realloc'd linear-memory
    /// buffer and pass (base, len) as flat args.
    fn emit_list_buffer(&mut self, param_name: &str, param_local: u32, elem_type: &Type) {
        let registry = self.lower_ctx.cm_interface_registry;
        let pkg = Some(self.func_info.package.as_str());
        // Use registry-aware layout so named WASI struct/variant/enum/flags
        // element types walk at their true CM stride/alignment instead of
        // the i32-handle fallback in `cm_abi::cm_size`/`cm_align`.
        let elem_size =
            crate::component_model::cm_size_with_registry_scoped(elem_type, registry, pkg) as i32;
        let elem_align =
            crate::component_model::cm_align_with_registry_scoped(elem_type, registry, pkg) as i32;

        let (elem_type_id, array_type_id) = {
            let mut tt = self.lower_ctx.type_table.borrow_mut();
            let elem_tid =
                cm_type_to_type_id(elem_type, &mut tt, registry, &self.func_info.package);
            let list_tid = tt.make_list(elem_tid);
            (elem_tid, list_tid)
        };

        // __len = List<T>::len(param)
        let len_local = alloc_local(&mut self.next_local, &mut self.locals, TypeTable::I32);
        let len_expr = generic_method_call(
            local_ref(param_local, param_name, array_type_id),
            &self.lower_ctx.names.array,
            "len",
            ModuleSource::list(),
            vec![],
            TypeTable::I32,
        );
        self.body_stmts.push(let_stmt(
            &format!("__{param_name}_len"),
            len_local,
            TypeTable::I32,
            len_expr,
        ));

        // __base = realloc(0, 0, align, __len * elem_size)
        let base_local = alloc_local(&mut self.next_local, &mut self.locals, TypeTable::I32);
        self.body_stmts.push(let_stmt(
            &format!("__{param_name}_base"),
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
                        local_ref(len_local, &format!("__{param_name}_len"), TypeTable::I32),
                        i32_const(elem_size),
                        TypeTable::I32,
                    ),
                ],
                TypeTable::I32,
            ),
        ));

        // __i = 0; loop { if __i >= __len { break; } lower elem[__i]; __i += 1; }
        let i_local = alloc_local(&mut self.next_local, &mut self.locals, TypeTable::I32);
        self.body_stmts.push(let_mut_stmt(
            &format!("__{param_name}_i"),
            i_local,
            TypeTable::I32,
            i32_const(0),
        ));

        let mut loop_body = Vec::new();
        loop_body.push(if_stmt(
            binary(
                TirBinaryOp::GtEq,
                local_ref(i_local, &format!("__{param_name}_i"), TypeTable::I32),
                local_ref(len_local, &format!("__{param_name}_len"), TypeTable::I32),
                TypeTable::BOOL,
            ),
            block(vec![break_stmt()]),
            None,
        ));
        // __addr = __base + __i * elem_size
        let addr_local = alloc_local(&mut self.next_local, &mut self.locals, TypeTable::I32);
        loop_body.push(let_stmt(
            &format!("__{param_name}_addr"),
            addr_local,
            TypeTable::I32,
            binary(
                TirBinaryOp::Add,
                local_ref(base_local, &format!("__{param_name}_base"), TypeTable::I32),
                binary(
                    TirBinaryOp::Mul,
                    local_ref(i_local, &format!("__{param_name}_i"), TypeTable::I32),
                    i32_const(elem_size),
                    TypeTable::I32,
                ),
                TypeTable::I32,
            ),
        ));
        // __elem = param[__i] (IndexValue trait method)
        let elem_local = alloc_local(&mut self.next_local, &mut self.locals, elem_type_id);
        let iv_info = LocalMethodName::new(
            self.lower_ctx.names.array.clone(),
            Some("IndexValue<i32>".to_string()),
            "index_value".to_string(),
        );
        let iv_mangled = iv_info.to_mangled_name();
        loop_body.push(let_stmt(
            &format!("__{param_name}_elem"),
            elem_local,
            elem_type_id,
            TirExpr::new(
                TirExprKind::method_call(
                    Box::new(local_ref(param_local, param_name, array_type_id)),
                    FunctionRef {
                        module_source: ModuleSource::list(),
                        name: iv_mangled,
                        monomorph_info: None,
                        method_info: Some(iv_info),
                    },
                    vec![],
                    vec![CallArg::new(
                        local_ref(i_local, &format!("__{param_name}_i"), TypeTable::I32),
                        false,
                    )],
                ),
                elem_type_id,
                synth_span(),
            ),
        ));
        // Use the full memory lowerer so aggregate elements lay out their
        // payload correctly instead of being stored as an i32.
        let elem_ref = local_ref(elem_local, &format!("__{param_name}_elem"), elem_type_id);
        let addr_ref = local_ref(addr_local, &format!("__{param_name}_addr"), TypeTable::I32);
        let lower_stmts = synthesize_lower_wasi_type_to_memory(
            elem_type,
            elem_ref,
            addr_ref,
            &mut self.next_local,
            &mut self.locals,
            &self.lower_ctx,
        );
        loop_body.extend(lower_stmts);
        // __i += 1
        loop_body.push(expr_stmt(assign(
            local_ref(i_local, &format!("__{param_name}_i"), TypeTable::I32),
            binary(
                TirBinaryOp::Add,
                local_ref(i_local, &format!("__{param_name}_i"), TypeTable::I32),
                i32_const(1),
                TypeTable::I32,
            ),
        )));
        self.body_stmts.push(loop_stmt(block(loop_body)));

        self.flat_args.push(local_ref(
            base_local,
            &format!("__{param_name}_base"),
            TypeTable::I32,
        ));
        self.flat_args.push(local_ref(
            len_local,
            &format!("__{param_name}_len"),
            TypeTable::I32,
        ));
    }

    /// Async canon lower argument setup: allocate the results buffer (only
    /// when the import returns a value — per CM `flatten_functype` the
    /// `results_ptr` exists only then) and switch to a single indirect params
    /// buffer when the flat params exceed the limit or need memory lowering.
    fn prepare_async_args(&mut self, plans: &[ParamPlan<'a>]) -> Option<OutptrBuffer> {
        // Callback-style async: MAX_FLAT_ASYNC_PARAMS flat params before
        // all params are passed via a single params_ptr.
        const MAX_FLAT_ASYNC_PARAMS: usize = 4;

        let async_outptr = self.alloc_async_outptr();

        // Variant and Option params force the indirect path: they need
        // memory lowering, not direct flat passing.
        let needs_indirect = self.flat_args.len() > MAX_FLAT_ASYNC_PARAMS
            || plans.iter().any(|p| {
                matches!(
                    p.lowering,
                    ParamLowering::Variant { .. } | ParamLowering::OptionValue { .. }
                )
            });
        if needs_indirect {
            self.emit_indirect_params_buffer(plans, async_outptr);
        } else if let Some(outptr) = async_outptr {
            self.flat_args
                .push(local_ref(outptr.local, "__async_outptr", TypeTable::I32));
        }
        async_outptr
    }

    /// Allocate the async results buffer. For `async fn ... -> AsyncCall<T>`
    /// imports `func_info.return_type` already stores the CM-ABI `T` (the
    /// registry strips `AsyncCall<T>` at registration), so its layout is the
    /// buffer layout.
    fn alloc_async_outptr(&mut self) -> Option<OutptrBuffer> {
        let return_type = self.func_info.return_type.as_ref()?;
        let (size, align) = cm_return_size_align(
            return_type,
            self.lower_ctx.cm_interface_registry,
            Some(self.func_info.package.as_str()),
        );
        let local = alloc_local(&mut self.next_local, &mut self.locals, TypeTable::I32);
        self.body_stmts.push(let_stmt(
            "__async_outptr",
            local,
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
        Some(OutptrBuffer { local, size, align })
    }

    /// Indirect async calling: write all params to one linear-memory buffer
    /// and replace the flat args with (`params_ptr`[, `results_ptr`]).
    ///
    /// The buffer layout follows the Component Model Canonical ABI spec,
    /// which uses component-level type sizes (e.g., flags with ≤8 labels =
    /// 1 byte, enums with ≤256 cases = 1 byte), NOT flat type sizes.
    fn emit_indirect_params_buffer(
        &mut self,
        plans: &[ParamPlan<'a>],
        async_outptr: Option<OutptrBuffer>,
    ) {
        let registry = self.lower_ctx.cm_interface_registry;

        // Size the buffer with the same package-scoped layout the writes below
        // use (via `self.lower_ctx`), so a same-named type resolved under the
        // package hint cannot make the allocation disagree with the bytes
        // written. `layout_tuple_*` lays a param sequence out exactly like the
        // buffer: each param aligned then placed, padded to the max align.
        let param_types: Vec<Type> = plans.iter().map(|plan| plan.ty.clone()).collect();
        let layout = crate::cm_abi::layout_tuple_with_registry_scoped(
            &param_types,
            registry,
            Some(self.lower_ctx.wasi_package),
        );
        let param_offsets = layout.offsets;
        let buf_max_align = layout.align;
        let buf_total_size = layout.size;

        let params_buf_local = alloc_local(&mut self.next_local, &mut self.locals, TypeTable::I32);
        self.body_stmts.push(let_stmt(
            "__params_buf",
            params_buf_local,
            TypeTable::I32,
            builtin_call(
                "realloc",
                vec![
                    i32_const(0),
                    i32_const(0),
                    i32_const(buf_max_align as i32),
                    i32_const(buf_total_size as i32),
                ],
                TypeTable::I32,
            ),
        ));

        // Write each param's values to the buffer at CM-computed offsets.
        let mut flat_idx = 0usize;
        for (plan, base_offset) in plans.iter().zip(param_offsets) {
            match plan.lowering {
                // WASI variants: lower directly to the buffer using
                // registry-aware layout. flat_args has one entry (the GC ref).
                ParamLowering::Variant { named } => {
                    let source = named
                        .source_interface
                        .as_deref()
                        .expect("classified WASI variant has a source interface");
                    let variant_value = self.flat_args[flat_idx].clone();
                    flat_idx += 1;
                    synthesize_lower_wasi_variant_to_memory(
                        named,
                        source,
                        variant_value,
                        params_buf_addr(params_buf_local, base_offset),
                        &mut self.next_local,
                        &mut self.body_stmts,
                        &mut self.locals,
                        &self.lower_ctx,
                    );
                }
                ParamLowering::OptionValue { payload } => {
                    let option_value = self.flat_args[flat_idx].clone();
                    flat_idx += 1;
                    synthesize_lower_option_to_memory(
                        payload,
                        option_value,
                        params_buf_addr(params_buf_local, base_offset),
                        &mut self.next_local,
                        &mut self.body_stmts,
                        &mut self.locals,
                        &self.lower_ctx,
                    );
                }
                ParamLowering::Unit
                | ParamLowering::PackedPtrLen { .. }
                | ParamLowering::ListBuffer { .. }
                | ParamLowering::RecordFlatten { .. }
                | ParamLowering::ResultValue { .. }
                | ParamLowering::TupleFlatten
                | ParamLowering::Direct => {
                    let stores = cm_param_store_plan(plan.ty, registry, &self.lower_ctx.names);
                    for (sub_offset, store_name) in &stores {
                        let addr = params_buf_addr(params_buf_local, base_offset + sub_offset);
                        let value = self.flat_args[flat_idx].clone();
                        flat_idx += 1;
                        self.body_stmts.push(expr_stmt(builtin_call(
                            store_name,
                            vec![addr, value],
                            TypeTable::UNIT,
                        )));
                    }
                }
            }
        }

        self.flat_args = vec![local_ref(params_buf_local, "__params_buf", TypeTable::I32)];
        if let Some(outptr) = async_outptr {
            self.flat_args
                .push(local_ref(outptr.local, "__async_outptr", TypeTable::I32));
        }
    }

    /// Sync imports whose return needs an outptr: allocate the buffer and
    /// append its address as the last flat arg.
    fn alloc_sync_outptr(&mut self) -> Option<OutptrBuffer> {
        let return_type = self.func_info.return_type.as_ref()?;
        if !crate::component_model::cm_return_needs_outptr(
            return_type,
            self.lower_ctx.cm_interface_registry,
        ) {
            return None;
        }
        let (size, align) = cm_return_size_align(
            return_type,
            self.lower_ctx.cm_interface_registry,
            Some(self.func_info.package.as_str()),
        );
        let local = alloc_local(&mut self.next_local, &mut self.locals, TypeTable::I32);
        self.body_stmts.push(let_stmt(
            "__outptr",
            local,
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
        self.flat_args
            .push(local_ref(local, "__outptr", TypeTable::I32));
        Some(OutptrBuffer { local, size, align })
    }

    /// Async result strategy.
    ///
    /// WASI P3 async calling convention: the lowered function returns a
    /// packed subtask handle/status `(subtask_handle << 4) | status`. The
    /// result (if any) is written to the async outptr buffer when the
    /// subtask eventually reaches `Status::Returned`.
    ///
    /// For Wado-level `async fn foo(...) -> AsyncCall<T>` imports, the
    /// adapter does NOT wait for the subtask or lift the result here.
    /// Instead it packages `(packed_handle, outptr, size, align,
    /// __cm_lift_fn)` into an `AsyncCall<T>` struct and returns it
    /// immediately, letting the caller interleave stream-parameter
    /// writes with the host subtask before explicitly `.wait()`-ing.
    /// `AsyncCall<T>::wait` then performs the wait + free; the lift
    /// itself runs in the per-import `__cm_lift__*` function emitted
    /// alongside this adapter and reached through `__cm_lift`.
    fn emit_async_result(
        &mut self,
        raw_call: TirExpr,
        async_outptr: Option<OutptrBuffer>,
    ) -> TypeId {
        let func_info = self.func_info;
        let subtask_local = alloc_local(&mut self.next_local, &mut self.locals, TypeTable::I32);
        self.body_stmts.push(let_stmt(
            "__subtask_packed",
            subtask_local,
            TypeTable::I32,
            raw_call,
        ));

        let (outptr_expr, size_expr, align_expr) = match async_outptr {
            Some(outptr) => (
                local_ref(outptr.local, "__async_outptr", TypeTable::I32),
                i32_const(outptr.size as i32),
                i32_const(outptr.align as i32),
            ),
            // Void async import: no outptr. Carry zeroes so the struct
            // layout is uniform; `AsyncCall<()>::wait` is a no-op.
            None => (i32_const(0), i32_const(0), i32_const(0)),
        };

        // The type argument T for AsyncCall<T> is the CM-level result type
        // (`func_info.return_type` stores the inner T); `()` for void async.
        let inner_type_id = if let Some(return_type) = &func_info.return_type {
            let resolved = self.registry().resolve_type(return_type);
            self.cm_type_id(&resolved)
        } else {
            TypeTable::UNIT
        };
        let subtask_type = self
            .lower_ctx
            .type_table
            .borrow_mut()
            .make_async_call(inner_type_id);

        // Per-import lift function: `AsyncCall<T>::wait` calls back
        // through this `FuncRef` to materialise the result.
        let lift_fn_name = lift_func_name(&func_info.interface_name, &func_info.method_name);
        let lift_fn = synthesize_async_lift_function(
            lift_fn_name.clone(),
            func_info,
            inner_type_id,
            self.registry(),
            self.lower_ctx.type_table,
            self.interner,
        );
        let lift_fn_type = self.lower_ctx.type_table.borrow_mut().make_function(
            vec![TypeTable::I32],
            inner_type_id,
            Vec::new(),
            Vec::new(),
        );
        let lift_fn_ref = TirExpr::new(
            TirExprKind::FuncRef {
                module_source: self.entry_source.clone(),
                name: lift_fn_name,
                type_args: Vec::new(),
            },
            lift_fn_type,
            synth_span(),
        );

        let subtask_struct = make_async_call_literal(
            subtask_type,
            local_ref(subtask_local, "__subtask_packed", TypeTable::I32),
            outptr_expr,
            size_expr,
            align_expr,
            lift_fn_ref.clone(),
        );
        self.body_stmts.push(return_stmt(Some(subtask_struct)));
        self.auxiliary.push(lift_fn);

        let (wrap_size, wrap_align) = match async_outptr {
            Some(outptr) => (outptr.size, outptr.align),
            None => (0, 0),
        };
        self.auxiliary.push(synthesize_async_wrap_function(
            crate::name::cm_wrap_async_func_name(&func_info.interface_name, &func_info.method_name),
            func_info,
            inner_type_id,
            subtask_type,
            wrap_size,
            wrap_align,
            lift_fn_ref,
            &self.lower_ctx,
        ));

        subtask_type
    }

    /// Outptr result strategy: the raw call returns void; lift the result
    /// from the outptr buffer, then free it.
    fn emit_outptr_result(&mut self, raw_call: TirExpr, outptr: OutptrBuffer) -> TypeId {
        self.body_stmts.push(expr_stmt(raw_call));

        let return_type = self
            .func_info
            .return_type
            .as_ref()
            .expect("outptr result implies a return type");
        let resolved = self.registry().resolve_type(return_type);

        // Inline lifting for all types, including list<T> which uses
        // List::<T>::with_capacity() and .push() with proper monomorphization info.
        let lift_ctx = self.lift_ctx();
        let lifted = synthesize_lift(
            &resolved,
            local_ref(outptr.local, "__outptr", TypeTable::I32),
            &mut self.next_local,
            &mut self.body_stmts,
            &mut self.locals,
            &lift_ctx,
        );

        // Materialize the lifted value into a local before freeing if it
        // contains a bare memory load (e.g., i32.load from the outptr buffer).
        // Complex types are already materialized into locals by synthesize_lift.
        let lifted = materialize_if_needed(
            lifted,
            &mut self.next_local,
            &mut self.body_stmts,
            &mut self.locals,
        );

        self.body_stmts.push(expr_stmt(builtin_call(
            "realloc",
            vec![
                local_ref(outptr.local, "__outptr", TypeTable::I32),
                i32_const(outptr.size as i32),
                i32_const(outptr.align as i32),
                i32_const(0),
            ],
            TypeTable::I32,
        )));

        let lifted_type_id = lifted.type_id;
        self.body_stmts.push(return_stmt(Some(lifted)));
        lifted_type_id // real type, fixed up at call site if needed
    }

    /// Flat result strategy: the raw call returns the value on the stack.
    /// A `Result<(), E>` discriminant is rebuilt into its GC variant, a
    /// record flattening to one core value is rebuilt into its GC struct,
    /// and everything else passes through.
    fn emit_flat_result(
        &mut self,
        raw_call: TirExpr,
        raw_call_type: TypeId,
        return_type: &Type,
    ) -> TypeId {
        let registry = self.registry();
        let resolved = registry.resolve_type(return_type);
        let return_flat = registry.cm_flatten(&resolved);
        // Enums/newtypes flatten to a scalar and pass through; only a struct
        // that flattens to one core value must be rebuilt into its GC form.
        let is_flat_struct = return_flat.len() == 1
            && matches!(&resolved, Type::Named(n)
            if registry
                .resolve_cm_source_for(n, Some(self.func_info.package.as_str()))
                .is_some_and(|s| {
                    registry.get_struct_fields_by_source(s, &n.name).is_some()
                }));
        if needs_flat_result_lifting(&resolved, &self.lower_ctx.names) {
            // Flat return with complex type (e.g., Result<(), ()>): the raw call returns
            // an i32 discriminant on the stack, but the binding needs to return a GC struct.
            // Synthesize VariantConstruct from the discriminant.
            let disc_local = alloc_local(&mut self.next_local, &mut self.locals, TypeTable::I32);
            self.body_stmts
                .push(let_stmt("__disc", disc_local, TypeTable::I32, raw_call));

            // Resolve the concrete `Result<T, E>` TypeId so the binding's
            // intermediate local and `VariantConstruct` exprs match the
            // declared return type. Without this, the local was declared as
            // `TypeTable::I32` and back-patched later by `type_fixup`,
            // which produced invalid TIR if any consumer ran first.
            let result_type_id = self.cm_type_id(&resolved);
            let result_local = alloc_local(&mut self.next_local, &mut self.locals, result_type_id);
            self.body_stmts.push(let_mut_stmt(
                "__result_val",
                result_local,
                result_type_id,
                null_expr(result_type_id),
            ));

            let lift_ctx = self.lift_ctx();
            let lifted = synthesize_lift_flat_result(
                &resolved,
                local_ref(disc_local, "__disc", TypeTable::I32),
                result_local,
                result_type_id,
                &mut self.next_local,
                &mut self.body_stmts,
                &mut self.locals,
                &lift_ctx,
            );
            let lifted_type_id = lifted.type_id;
            self.body_stmts.push(return_stmt(Some(lifted)));
            lifted_type_id
        } else if is_flat_struct {
            let lift_ctx = self.lift_ctx();
            let lifted = lift_flat_struct_return(
                &resolved,
                return_flat[0],
                raw_call,
                raw_call_type,
                &mut self.next_local,
                &mut self.body_stmts,
                &mut self.locals,
                &lift_ctx,
            );
            let lifted_type_id = lifted.type_id;
            self.body_stmts.push(return_stmt(Some(lifted)));
            lifted_type_id
        } else {
            self.body_stmts.push(return_stmt(Some(raw_call)));
            raw_call_type
        }
    }
}
