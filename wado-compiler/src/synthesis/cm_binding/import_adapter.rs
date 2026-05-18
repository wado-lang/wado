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

use crate::ast::Type;
use crate::cm_abi;
use crate::component_model::{WasiFunctionInfo, WasiRegistry};
use crate::hashmap::IndexSet;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name::LocalMethodName;
use crate::tir::{
    CallArg, EffectRef, FunctionKind, FunctionRef, InlineHint, TirBinaryOp, TirBlock, TirExpr,
    TirExprKind, TirFunction, TirLocal, TirParam, TirStmt, TirStructField, TypeId, TypeTable,
};

use crate::synthesis::common::{
    alloc_local, assign, binary, block, break_stmt, builtin_call, cast, cm_raw_call, expr_stmt,
    generic_method_call, i32_const, i64_const, if_stmt, internal_call, let_mut_stmt, let_stmt,
    local_ref, loop_stmt, null_expr, return_stmt, synth_span,
};

use super::lift::{materialize_if_needed, synthesize_lift, try_lift_wasi_variant_or_enum};
use super::lower::{
    synthesize_flatten_option_to_flat_args, synthesize_flatten_value_to_flat_args,
    synthesize_lower, synthesize_lower_option_to_memory, synthesize_lower_tuple,
    synthesize_lower_wasi_variant_to_memory,
};
use super::types::{
    LiftContext, binary_add, cm_param_align, cm_param_size, cm_param_store_plan,
    flatten_param_type, needs_flat_result_lifting, wasi_type_to_type_id,
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

/// Canonical ABI: maximum number of flat return values before outptr is used.
const MAX_FLAT_RESULTS: usize = 1;

/// Synthesize lifting of a flat Result discriminant into a GC variant struct.
///
/// For `Result<(), ()>`: disc==0 → Ok, disc==1 → Err (no payloads)
/// For `Result<(), ErrorCode>`: disc==0 → Ok, disc!=0 → `Err(lift_error)`
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

        let ok_construct = if ok_is_unit {
            TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type: result_type_id,
                    case_index: ok_index,
                    case_name: ok_name.clone(),
                    payload: None,
                },
                result_type_id,
                synth_span(),
            )
        } else {
            // Ok with payload — flat result should use outptr instead
            // This shouldn't happen, but handle gracefully
            TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type: result_type_id,
                    case_index: ok_index,
                    case_name: ok_name.clone(),
                    payload: None,
                },
                result_type_id,
                synth_span(),
            )
        };

        let err_construct = if err_is_unit {
            TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type: result_type_id,
                    case_index: err_index,
                    case_name: err_name.clone(),
                    payload: None,
                },
                result_type_id,
                synth_span(),
            )
        } else {
            // Err with a flat payload — the remaining flat values encode the error.
            // Only lift when the error type is a named WASI variant/enum carrying
            // a resolved source_interface; otherwise fall back to a bare Err.
            let lifted_variant = if let Type::Named(n) = err_ty
                && let Some(source) = n.source_interface.as_deref()
                && source.starts_with("wasi:")
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
                        case_name: err_name.clone(),
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
                        case_name: err_name.clone(),
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
        is_pub: false,
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
fn wasi_return_type_id(func_info: &WasiFunctionInfo, wasi_registry: &WasiRegistry) -> TypeId {
    // Truly async imports (e.g., Client::send) use canon lower async and
    // return a subtask handle. Non-async imports with stream/future params
    // use sync lower (handles passed as i32, results returned directly).
    let needs_async_lower = func_info.is_async;
    if needs_async_lower {
        // Async canon lower: raw call returns subtask handle (i32)
        TypeTable::I32
    } else {
        let needs_outptr = func_info.return_type.as_ref().is_some_and(|rt| {
            cm_abi::cm_flat_types(rt).len() > MAX_FLAT_RESULTS
                || crate::component_model::wasi_named_type_return_needs_outptr(rt, wasi_registry)
        });
        if needs_outptr {
            // Outptr: raw call returns void; result is read from outptr
            TypeTable::UNIT
        } else if let Some(ty) = &func_info.return_type {
            // Flat return: use the core type
            match ty {
                Type::Named(n) => match n.name.as_str() {
                    "i32" | "u32" => TypeTable::I32,
                    "i64" | "u64" => TypeTable::I64,
                    "f32" => TypeTable::F32,
                    "f64" => TypeTable::F64,
                    "bool" => TypeTable::I32, // CM returns bool as i32
                    _ => TypeTable::I32,
                },
                _ => TypeTable::I32,
            }
        } else {
            TypeTable::UNIT
        }
    }
}

/// Synthesise the per-import CM lift function for an async import. Body
/// is built from `func_info.return_type` via [`synthesize_lift`] — the
/// same helper sync imports use, so generic calls inside (e.g.
/// `Array::with_capacity`) are visible to the monomorphizer.
fn synthesize_async_lift_function(
    name: String,
    func_info: &WasiFunctionInfo,
    inner_type_id: TypeId,
    wasi_registry: &WasiRegistry,
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
        span: synth_span(),
        default_expr: None,
    }];

    let lifted = if let Some(return_type) = &func_info.return_type {
        let resolved = wasi_registry.resolve_type(return_type);
        let lift_ctx = LiftContext {
            wasi_registry,
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
    func_info: &WasiFunctionInfo,
    wasi_registry: &WasiRegistry,
    type_table: &RefCell<TypeTable>,
    interner: &RefCell<ModuleSourceInterner>,
    owner_module: &ModuleSource,
    entry_source: &ModuleSource,
) -> AdapterArtifacts {
    let names = super::types::CmStdlibNames::from_type_table(&type_table.borrow());
    let name = binding_func_name(&func_info.interface_name, &func_info.method_name);
    let local_name = func_info.local_alias_name();

    // Derive outptr needs from return type using Canonical ABI layout.
    // Also check WASI variants with payload cases (e.g., Method with Other(String)):
    // cm_flat_types treats unknown named types as i32, missing their true flat count.
    //
    // For `async fn foo(...) -> AsyncCall<T>` imports, `func_info.return_type`
    // already stores the CM-ABI `T` (the registry strips the `AsyncCall<T>`
    // wrapper at registration time, see `WasiRegistry::register`). The
    // wrapping is re-applied below when emitting the Wado-visible adapter
    // return type.
    let cm_return_type: Option<Type> = func_info.return_type.clone();
    let needs_outptr = cm_return_type.as_ref().is_some_and(|rt| {
        cm_abi::cm_flat_types(rt).len() > MAX_FLAT_RESULTS
            || crate::component_model::wasi_named_type_return_needs_outptr(rt, wasi_registry)
    });
    let pkg = Some(func_info.package.as_str());
    let outptr_alloc = if needs_outptr {
        cm_return_type.as_ref().map(|rt| {
            // WASI variants need their registry-computed size/align, not the generic cm_size
            if let Type::Named(named) = rt
                && let Some(sa) = crate::component_model::wasi_variant_cm_size_align_scoped(
                    named,
                    wasi_registry,
                    pkg,
                )
            {
                return sa;
            }
            // Use registry-aware size/align for WASI structs and other complex types
            (
                crate::component_model::cm_size_with_registry_scoped(rt, wasi_registry, pkg),
                crate::component_model::cm_align_with_registry_scoped(rt, wasi_registry, pkg),
            )
        })
    } else {
        None
    };

    let mut next_local: u32 = 0;
    let mut params = Vec::new();
    let mut locals: Vec<TirLocal> = Vec::new();
    let mut body_stmts: Vec<TirStmt> = Vec::new();
    let mut flat_args: Vec<TirExpr> = Vec::new();
    // Per-import lift function returned alongside the adapter for CM
    // `async func` imports. The adapter writes a `FuncRef` to it into the
    // emitted `AsyncCall<T>`'s `__cm_lift` field; the caller is responsible
    // for adding it to the entry module.
    let mut auxiliary: Vec<Rc<RefCell<TirFunction>>> = Vec::new();

    // ---- Pass 1: Allocate all parameter locals (contiguous) ----
    // Wasm requires params at indices [0..n-1], so allocate them first.
    //
    // For types that the binding lowers internally (String, Array<u8>), we create
    // a single placeholder param. The binding body will lower them to flat CM args.
    //
    // For other types (handles, Option<T>, etc.), we create flat params matching
    // the CM ABI directly. The call site must flatten args before passing them.
    //
    // Track (start_param_idx, param_count) per WASI param for Pass 2 indexing.
    let mut param_mapping: Vec<(usize, usize)> = Vec::new();
    for (param_name, _, param_type) in &func_info.params {
        let flat_tys = flatten_param_type(param_type, wasi_registry, &names);
        if flat_tys.is_empty() {
            continue; // unit param, skip
        }
        let start = params.len();
        match param_type {
            // String: single placeholder param (binding body lowers to ptr+len)
            Type::Named(n) if n.name == names.string => {
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: TypeTable::I32,
                    local_index: next_local,
                    is_mut: false,
                    span: synth_span(),
                    default_expr: None,
                });
                locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
                next_local += 1;
                param_mapping.push((start, 1));
            }
            // Array<u8>: single placeholder param (binding body lowers to ptr+len)
            Type::Generic(g)
                if g.name == names.array
                    && g.args.len() == 1
                    && matches!(&g.args[0], Type::Named(n) if n.name == "u8") =>
            {
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: TypeTable::I32,
                    local_index: next_local,
                    is_mut: false,
                    span: synth_span(),
                    default_expr: None,
                });
                locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
                next_local += 1;
                param_mapping.push((start, 1));
            }
            // General Array<T>: single placeholder param (binding body lowers to ptr+len)
            Type::Generic(g) if g.name == names.array && g.args.len() == 1 => {
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: TypeTable::I32,
                    local_index: next_local,
                    is_mut: false,
                    span: synth_span(),
                    default_expr: None,
                });
                locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
                next_local += 1;
                param_mapping.push((start, 1));
            }
            // Struct (record) param: single GC reference, binding extracts fields
            Type::Named(n)
                if n.source_interface.as_deref().is_some_and(|s| {
                    s.starts_with("wasi:")
                        && wasi_registry
                            .get_struct_fields_by_source(s, &n.name)
                            .is_some()
                }) =>
            {
                let struct_type_id = {
                    let mut tt = type_table.borrow_mut();
                    wasi_type_to_type_id(param_type, &mut tt, wasi_registry, &func_info.package)
                };
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: struct_type_id,
                    local_index: next_local,
                    is_mut: false,
                    span: synth_span(),
                    default_expr: None,
                });
                locals.push(TirLocal::synth(next_local, struct_type_id, false));
                next_local += 1;
                param_mapping.push((start, 1));
            }
            // Variant param: single GC reference, binding lowers to flat args
            Type::Named(n)
                if n.source_interface.as_deref().is_some_and(|s| {
                    s.starts_with("wasi:")
                        && wasi_registry
                            .get_variant_cases_by_source(s, &n.name)
                            .is_some()
                }) =>
            {
                let variant_type_id = {
                    let mut tt = type_table.borrow_mut();
                    wasi_type_to_type_id(param_type, &mut tt, wasi_registry, &func_info.package)
                };
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: variant_type_id,
                    local_index: next_local,
                    is_mut: false,
                    span: synth_span(),
                    default_expr: None,
                });
                locals.push(TirLocal::synth(next_local, variant_type_id, false));
                next_local += 1;
                param_mapping.push((start, 1));
            }
            // Option<T>: single GC ref param (binding body lowers to discriminant + payload)
            Type::Generic(g) if g.name == "Option" && g.args.len() == 1 => {
                let option_type_id = {
                    let mut tt = type_table.borrow_mut();
                    wasi_type_to_type_id(param_type, &mut tt, wasi_registry, &func_info.package)
                };
                params.push(TirParam {
                    name: param_name.clone(),
                    type_id: option_type_id,
                    local_index: next_local,
                    is_mut: false,
                    span: synth_span(),
                    default_expr: None,
                });
                locals.push(TirLocal::synth(next_local, option_type_id, false));
                next_local += 1;
                param_mapping.push((start, 1));
            }
            // All other types: create flat params matching CM ABI
            _ => {
                for (j, flat_ty) in flat_tys.iter().enumerate() {
                    let name = if flat_tys.len() == 1 {
                        param_name.clone()
                    } else {
                        format!("{param_name}_flat{j}")
                    };
                    params.push(TirParam {
                        name,
                        type_id: *flat_ty,
                        local_index: next_local,
                        is_mut: false,
                        span: synth_span(),
                        default_expr: None,
                    });
                    locals.push(TirLocal::synth(next_local, *flat_ty, false));
                    next_local += 1;
                }
                param_mapping.push((start, flat_tys.len()));
            }
        }
    }

    // ---- Pass 2: Generate parameter lowering code ----
    // Intermediate locals (packed i64, etc.) are allocated after all params.
    let mut mapping_idx = 0usize;
    for (param_name, _, param_type) in &func_info.params {
        let flat_tys = flatten_param_type(param_type, wasi_registry, &names);
        if flat_tys.is_empty() {
            continue; // unit param, skip
        }
        let (start_idx, count) = param_mapping[mapping_idx];
        mapping_idx += 1;
        let param_local = params[start_idx].local_index;

        match param_type {
            // String param: accept Wado String, lower to (ptr, len) pair
            Type::Named(n) if n.name == names.string => {
                // Call cm_lower_string → packed i64
                let packed_local = next_local;
                let packed = internal_call(
                    "cm_lower_string",
                    vec![local_ref(param_local, param_name, TypeTable::I32)],
                    TypeTable::I64,
                );
                body_stmts.push(let_stmt(
                    &format!("__{param_name}_packed"),
                    packed_local,
                    TypeTable::I64,
                    packed,
                ));
                locals.push(TirLocal::synth(next_local, TypeTable::I64, false));
                next_local += 1;

                // ptr = packed as i32 (low 32 bits)
                flat_args.push(cast(
                    local_ref(
                        packed_local,
                        &format!("__{param_name}_packed"),
                        TypeTable::I64,
                    ),
                    TypeTable::I32,
                ));
                // len = (packed >> 32) as i32 (high 32 bits)
                flat_args.push(cast(
                    binary(
                        TirBinaryOp::Shr,
                        local_ref(
                            packed_local,
                            &format!("__{param_name}_packed"),
                            TypeTable::I64,
                        ),
                        i64_const(32),
                        TypeTable::I64,
                    ),
                    TypeTable::I32,
                ));
            }

            // Array<u8> param: accept Wado Array<u8>, lower to (ptr, len) pair
            Type::Generic(g)
                if g.name == names.array
                    && g.args.len() == 1
                    && matches!(&g.args[0], Type::Named(n) if n.name == "u8") =>
            {
                // Call cm_lower_array_u8 → packed i64
                let packed_local = next_local;
                let packed = internal_call(
                    "cm_lower_array_u8",
                    vec![local_ref(param_local, param_name, TypeTable::I32)],
                    TypeTable::I64,
                );
                body_stmts.push(let_stmt(
                    &format!("__{param_name}_packed"),
                    packed_local,
                    TypeTable::I64,
                    packed,
                ));
                locals.push(TirLocal::synth(next_local, TypeTable::I64, false));
                next_local += 1;

                // Split packed i64 → (ptr, len)
                flat_args.push(cast(
                    local_ref(
                        packed_local,
                        &format!("__{param_name}_packed"),
                        TypeTable::I64,
                    ),
                    TypeTable::I32,
                ));
                flat_args.push(cast(
                    binary(
                        TirBinaryOp::Shr,
                        local_ref(
                            packed_local,
                            &format!("__{param_name}_packed"),
                            TypeTable::I64,
                        ),
                        i64_const(32),
                        TypeTable::I64,
                    ),
                    TypeTable::I32,
                ));
            }

            // General Array<T> param: lower to (ptr, len) in linear memory
            Type::Generic(g) if g.name == names.array && g.args.len() == 1 => {
                let elem_type = &g.args[0];
                // Use registry-aware layout so named WASI struct/variant/enum/flags
                // element types walk at their true CM stride/alignment instead of
                // the i32-handle fallback in `cm_abi::cm_size`/`cm_align`.
                let elem_size = crate::component_model::cm_size_with_registry_scoped(
                    elem_type,
                    wasi_registry,
                    Some(&func_info.package),
                ) as i32;
                let elem_align = crate::component_model::cm_align_with_registry_scoped(
                    elem_type,
                    wasi_registry,
                    Some(&func_info.package),
                ) as i32;

                // Resolve proper TypeIds for the element and array types
                let (elem_type_id, array_type_id) = {
                    let mut tt = type_table.borrow_mut();
                    let elem_tid =
                        wasi_type_to_type_id(elem_type, &mut tt, wasi_registry, &func_info.package);
                    let array_tid = tt.make_array(elem_tid);
                    (elem_tid, array_tid)
                };

                // __len = Array<T>::len(param)
                let len_local = alloc_local(&mut next_local, &mut locals, TypeTable::I32);
                body_stmts.push(let_stmt(
                    &format!("__{param_name}_len"),
                    len_local,
                    TypeTable::I32,
                    generic_method_call(
                        local_ref(param_local, param_name, array_type_id),
                        &names.array,
                        "len",
                        ModuleSource::prelude(),
                        vec![],
                        TypeTable::I32,
                    ),
                ));

                // __base = realloc(0, 0, align, __len * elem_size)
                let base_local = alloc_local(&mut next_local, &mut locals, TypeTable::I32);
                body_stmts.push(let_stmt(
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
                                local_ref(
                                    len_local,
                                    &format!("__{param_name}_len"),
                                    TypeTable::I32,
                                ),
                                i32_const(elem_size),
                                TypeTable::I32,
                            ),
                        ],
                        TypeTable::I32,
                    ),
                ));

                // __i = 0; loop { if __i >= __len { break; } lower elem[__i]; __i += 1; }
                let i_local = alloc_local(&mut next_local, &mut locals, TypeTable::I32);
                body_stmts.push(let_mut_stmt(
                    &format!("__{param_name}_i"),
                    i_local,
                    TypeTable::I32,
                    i32_const(0),
                ));

                let mut loop_body = Vec::new();
                // break if __i >= __len
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
                let addr_local = alloc_local(&mut next_local, &mut locals, TypeTable::I32);
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
                let elem_local = alloc_local(&mut next_local, &mut locals, elem_type_id);
                let iv_info = LocalMethodName::new(
                    names.array.clone(),
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
                                module_source: ModuleSource::array(),
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
                // Lower element to linear memory at __addr
                let elem_ref = local_ref(elem_local, &format!("__{param_name}_elem"), elem_type_id);
                let addr_ref =
                    local_ref(addr_local, &format!("__{param_name}_addr"), TypeTable::I32);
                let lower_stmts = if let Type::Tuple(sub_elems) = elem_type {
                    synthesize_lower_tuple(
                        sub_elems,
                        elem_ref,
                        addr_ref,
                        &mut next_local,
                        &mut locals,
                        wasi_registry,
                        &func_info.package,
                        type_table,
                    )
                } else {
                    synthesize_lower(
                        elem_type,
                        elem_ref,
                        addr_ref,
                        &mut next_local,
                        &mut locals,
                        &names,
                    )
                };
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
                body_stmts.push(loop_stmt(block(loop_body)));

                // Push (base, len) as flat args
                flat_args.push(local_ref(
                    base_local,
                    &format!("__{param_name}_base"),
                    TypeTable::I32,
                ));
                flat_args.push(local_ref(
                    len_local,
                    &format!("__{param_name}_len"),
                    TypeTable::I32,
                ));
            }

            // Struct (record) param: extract fields as flat args
            Type::Named(n)
                if n.source_interface.as_deref().is_some_and(|s| {
                    s.starts_with("wasi:")
                        && wasi_registry
                            .get_struct_fields_by_source(s, &n.name)
                            .is_some()
                }) =>
            {
                let struct_type_id = params[start_idx].type_id;
                let source = n
                    .source_interface
                    .as_deref()
                    .expect("wasi struct source_interface present");
                let wado_fields = wasi_registry
                    .get_struct_fields_with_wado_names_by_source(source, &n.name)
                    .expect("struct fields_with_wado_names present when fields are");
                for (field_idx, (wado_name, _, field_ty)) in wado_fields.iter().enumerate() {
                    let field_type_id = {
                        let mut tt = type_table.borrow_mut();
                        wasi_type_to_type_id(field_ty, &mut tt, wasi_registry, &func_info.package)
                    };
                    flat_args.push(TirExpr {
                        kind: TirExprKind::FieldAccess {
                            expr: Box::new(local_ref(param_local, param_name, struct_type_id)),
                            field_index: field_idx as u32,
                            field_name: wado_name.clone(),
                        },
                        type_id: field_type_id,
                        span: synth_span(),
                    });
                }
            }
            // Variant param: for async, pass GC ref (lowered in Step 3 indirect params);
            // for sync, flatten directly to flat i32 args.
            Type::Named(n)
                if n.source_interface.as_deref().is_some_and(|s| {
                    s.starts_with("wasi:")
                        && wasi_registry
                            .get_variant_cases_by_source(s, &n.name)
                            .is_some()
                }) =>
            {
                if func_info.is_async {
                    let variant_type_id = params[start_idx].type_id;
                    flat_args.push(local_ref(param_local, param_name, variant_type_id));
                } else {
                    synthesize_flatten_value_to_flat_args(
                        param_type,
                        local_ref(param_local, param_name, params[start_idx].type_id),
                        &format!("__{param_name}"),
                        &mut next_local,
                        &mut body_stmts,
                        &mut locals,
                        &mut flat_args,
                        wasi_registry,
                        &func_info.package,
                        type_table,
                    );
                }
            }
            // Option<T>: for async, pass GC ref (lowered in Step 3 indirect params);
            // for sync, flatten directly to flat args.
            Type::Generic(g) if g.name == "Option" && g.args.len() == 1 => {
                if func_info.is_async {
                    let option_type_id = params[start_idx].type_id;
                    flat_args.push(local_ref(param_local, param_name, option_type_id));
                } else {
                    synthesize_flatten_option_to_flat_args(
                        &g.args[0],
                        local_ref(param_local, param_name, params[start_idx].type_id),
                        &format!("__{param_name}"),
                        &mut next_local,
                        &mut body_stmts,
                        &mut locals,
                        &mut flat_args,
                        wasi_registry,
                        &func_info.package,
                        type_table,
                    );
                }
            }
            // All other types: flat params passed through directly
            _ => {
                for j in 0..count {
                    let p = &params[start_idx + j];
                    flat_args.push(local_ref(p.local_index, &p.name, p.type_id));
                }
            }
        }
    }

    // ---- Handle outptr for async or complex returns ----
    // Track async outptr allocation info for later freeing.
    let mut async_outptr_info: Option<(u32, u32, u32)> = None; // (local_index, size, align)
    // Only truly async imports use canon lower async (callback-style).
    // Non-async imports with stream/future params use sync lower.
    let needs_async_lower = func_info.is_async;
    if needs_async_lower {
        // Callback-style async (not used by Wado):
        // - MAX_FLAT_ASYNC_PARAMS = 4 flat params before switching to indirect.
        // - If flat_args exceeds 4, all params are passed via a single params_ptr
        //   (pointer to a linear-memory buffer with all lowered params).
        // - Per CM spec flatten_functype: the results_ptr is only added when
        //   len(flat_results) > 0 (i.e., when there IS a return type).
        // - Async void functions have no results_ptr.
        const MAX_FLAT_ASYNC_PARAMS: usize = 4;

        // The CM-level result type (for layout) is the inner `T` of
        // `AsyncCall<T>` for async imports; the `func_info.return_type`
        // itself is the Wado-visible `AsyncCall<T>` wrapper.
        let has_results = cm_return_type.is_some();

        // Allocate the async results buffer via realloc (only when there are results).
        if has_results {
            let pkg = Some(func_info.package.as_str());
            let (async_result_size, async_result_align) = if let Some(return_type) = &cm_return_type
            {
                if let Type::Named(named) = return_type
                    && let Some(sa) = crate::component_model::wasi_variant_cm_size_align_scoped(
                        named,
                        wasi_registry,
                        pkg,
                    )
                {
                    sa
                } else {
                    (
                        crate::component_model::cm_size_with_registry_scoped(
                            return_type,
                            wasi_registry,
                            pkg,
                        ),
                        crate::component_model::cm_align_with_registry_scoped(
                            return_type,
                            wasi_registry,
                            pkg,
                        ),
                    )
                }
            } else {
                unreachable!()
            };
            let async_outptr_local = next_local;
            body_stmts.push(let_stmt(
                "__async_outptr",
                async_outptr_local,
                TypeTable::I32,
                builtin_call(
                    "realloc",
                    vec![
                        i32_const(0),
                        i32_const(0),
                        i32_const(async_result_align as i32),
                        i32_const(async_result_size as i32),
                    ],
                    TypeTable::I32,
                ),
            ));
            locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
            next_local += 1;
            async_outptr_info = Some((async_outptr_local, async_result_size, async_result_align));
        }

        // Force indirect path when variant or Option params are present
        // (they need memory lowering, not direct flat passing).
        let has_variant_params = func_info.params.iter().any(|(_, _, ty)| {
            matches!(ty, Type::Named(n) if n
                .source_interface
                .as_deref()
                .is_some_and(|s| s.starts_with("wasi:")
                    && wasi_registry
                        .get_variant_cases_by_source(s, &n.name)
                        .is_some()))
                || matches!(ty, Type::Generic(g) if g.name == "Option" && g.args.len() == 1)
        });

        if flat_args.len() > MAX_FLAT_ASYNC_PARAMS || has_variant_params {
            // Indirect calling: write all params to a memory buffer using CM layout.
            // The buffer layout follows the Component Model Canonical ABI spec,
            // which uses component-level type sizes (e.g., flags with ≤8 labels = 1 byte,
            // enums with ≤256 cases = 1 byte), NOT flat type sizes (all i32 = 4 bytes).

            // Step 1: Compute buffer layout using CM component-level param types.
            let mut buf_offset = 0u32;
            let mut buf_max_align = 1u32;
            let mut param_offsets: Vec<u32> = Vec::with_capacity(func_info.params.len());
            for (_, _, ty) in &func_info.params {
                let sz = cm_param_size(ty, wasi_registry);
                let al = cm_param_align(ty, wasi_registry);
                buf_offset = (buf_offset + al - 1) & !(al - 1);
                param_offsets.push(buf_offset);
                buf_offset += sz;
                buf_max_align = buf_max_align.max(al);
            }
            let buf_total_size = (buf_offset + buf_max_align - 1) & !(buf_max_align - 1);

            // Step 2: Allocate the params buffer.
            let params_buf_local = next_local;
            body_stmts.push(let_stmt(
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
            locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
            next_local += 1;

            // Step 3: Write each param's values to the buffer at CM-computed offsets.
            let mut flat_idx = 0;
            for (param_idx, (_, _, ty)) in func_info.params.iter().enumerate() {
                let base_offset = param_offsets[param_idx];
                // WASI variants: lower directly to the buffer using registry-aware layout
                if let Type::Named(n) = ty
                    && let Some(source) = n.source_interface.as_deref()
                    && source.starts_with("wasi:")
                    && wasi_registry
                        .get_variant_cases_by_source(source, &n.name)
                        .is_some()
                {
                    let buf_addr = if base_offset == 0 {
                        local_ref(params_buf_local, "__params_buf", TypeTable::I32)
                    } else {
                        binary_add(
                            local_ref(params_buf_local, "__params_buf", TypeTable::I32),
                            i32_const(base_offset as i32),
                        )
                    };
                    // flat_args has one entry for this variant (the GC ref from Pass 2)
                    let variant_value = flat_args[flat_idx].clone();
                    flat_idx += 1;
                    synthesize_lower_wasi_variant_to_memory(
                        n,
                        source,
                        variant_value,
                        buf_addr,
                        &mut next_local,
                        &mut body_stmts,
                        &mut locals,
                        wasi_registry,
                        &func_info.package,
                        type_table,
                    );
                    continue;
                }
                // Option<T>: lower directly to the buffer
                if let Type::Generic(g) = ty
                    && g.name == "Option"
                    && g.args.len() == 1
                {
                    let buf_addr = if base_offset == 0 {
                        local_ref(params_buf_local, "__params_buf", TypeTable::I32)
                    } else {
                        binary_add(
                            local_ref(params_buf_local, "__params_buf", TypeTable::I32),
                            i32_const(base_offset as i32),
                        )
                    };
                    let option_value = flat_args[flat_idx].clone();
                    flat_idx += 1;
                    synthesize_lower_option_to_memory(
                        &g.args[0],
                        option_value,
                        buf_addr,
                        &mut next_local,
                        &mut body_stmts,
                        &mut locals,
                        wasi_registry,
                        &func_info.package,
                        type_table,
                    );
                    continue;
                }
                let stores = cm_param_store_plan(ty, wasi_registry, &names);
                for (sub_offset, store_name) in &stores {
                    let offset = base_offset + sub_offset;
                    let addr = if offset == 0 {
                        local_ref(params_buf_local, "__params_buf", TypeTable::I32)
                    } else {
                        binary(
                            TirBinaryOp::Add,
                            local_ref(params_buf_local, "__params_buf", TypeTable::I32),
                            i32_const(offset as i32),
                            TypeTable::I32,
                        )
                    };
                    body_stmts.push(expr_stmt(builtin_call(
                        store_name,
                        vec![addr, flat_args[flat_idx].clone()],
                        TypeTable::UNIT,
                    )));
                    flat_idx += 1;
                }
            }

            // Replace flat_args with params_buf (+ async_outptr if results exist).
            flat_args = vec![local_ref(params_buf_local, "__params_buf", TypeTable::I32)];
            if let Some((outptr_local, _, _)) = async_outptr_info {
                flat_args.push(local_ref(outptr_local, "__async_outptr", TypeTable::I32));
            }
        } else {
            // Direct calling: params fit within MAX_FLAT_ASYNC_PARAMS.
            // Only add outptr if there are results.
            if let Some((outptr_local, _, _)) = async_outptr_info {
                flat_args.push(local_ref(outptr_local, "__async_outptr", TypeTable::I32));
            }
        }
    } else if let Some((size, align)) = outptr_alloc {
        // Allocate outptr via realloc
        let outptr_local = next_local;
        let outptr_alloc = builtin_call(
            "realloc",
            vec![
                i32_const(0),            // old_ptr
                i32_const(0),            // old_size
                i32_const(align as i32), // align
                i32_const(size as i32),  // new_size
            ],
            TypeTable::I32,
        );
        body_stmts.push(let_stmt(
            "__outptr",
            outptr_local,
            TypeTable::I32,
            outptr_alloc,
        ));
        locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
        next_local += 1;

        flat_args.push(local_ref(outptr_local, "__outptr", TypeTable::I32));
    }

    // ---- Build CmRawCall ----
    let raw_call_return_type = wasi_return_type_id(func_info, wasi_registry);
    let raw_call_expr = cm_raw_call(&local_name, flat_args, raw_call_return_type);

    // ---- Handle result ----
    // The binding's return type to the Wado caller:
    let adapter_return_type;

    // Async/streaming path: functions lowered with `async` canon option.
    // This covers both truly async functions (func_info.is_async) and sync
    // functions with streaming params (Stream/Future) that require async lowering.
    // Non-async functions with streaming params complete synchronously (RETURNED
    // status), so wait_for_subtask is a no-op. The result is always written to the
    // outptr and lifted via synthesize_lift based on the return type metadata.
    if needs_async_lower {
        // WASI P3 async calling convention: the lowered function returns a
        // packed subtask handle/status `(subtask_handle << 4) | status`. The
        // result (if any) is written to the async outptr buffer when the
        // subtask eventually reaches `Status::Returned`.
        //
        // For Wado-level `async fn foo(...) -> AsyncCall<T>` imports, the
        // adapter does NOT wait for the subtask or lift the result here.
        // Instead it packages `(packed_handle, outptr, size, align,
        // __cm_lift_fn)` into an `AsyncCall<T>` struct and returns it
        // immediately, letting the caller interleave stream-parameter
        // writes with the host subtask before explicitly `.wait()`-ing.
        // `AsyncCall<T>::wait` then performs the wait + free; the lift
        // itself runs in the per-import `__cm_lift__*` function emitted
        // alongside this adapter and reached through `__cm_lift`.
        let subtask_local = next_local;
        locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
        next_local += 1;
        body_stmts.push(let_stmt(
            "__subtask_packed",
            subtask_local,
            TypeTable::I32,
            raw_call_expr,
        ));

        // Assemble the AsyncCall<T> struct fields.
        let (outptr_expr, size_expr, align_expr) =
            if let Some((outptr_local, outptr_size, outptr_align)) = async_outptr_info {
                (
                    local_ref(outptr_local, "__async_outptr", TypeTable::I32),
                    i32_const(outptr_size as i32),
                    i32_const(outptr_align as i32),
                )
            } else {
                // Void async import: no outptr. Carry zeroes so the struct
                // layout is uniform; `AsyncCall<()>::wait` is a no-op.
                (i32_const(0), i32_const(0), i32_const(0))
            };

        // Determine the type argument T for AsyncCall<T>. The CM-level
        // result type (inner T) was computed in `cm_return_type`; for
        // void async we use `()`.
        let inner_type_id = if let Some(return_type) = &cm_return_type {
            let resolved = wasi_registry.resolve_type(return_type);
            wasi_type_to_type_id(
                &resolved,
                &mut type_table.borrow_mut(),
                wasi_registry,
                &func_info.package,
            )
        } else {
            TypeTable::UNIT
        };
        let subtask_type = type_table.borrow_mut().make_async_call(inner_type_id);

        // Per-import lift function: `AsyncCall<T>::wait` calls back
        // through this `FuncRef` to materialise the result.
        let lift_fn_name = lift_func_name(&func_info.interface_name, &func_info.method_name);
        let lift_fn = synthesize_async_lift_function(
            lift_fn_name.clone(),
            func_info,
            inner_type_id,
            wasi_registry,
            type_table,
            interner,
        );
        let lift_fn_type = type_table.borrow_mut().make_function(
            vec![TypeTable::I32],
            inner_type_id,
            Vec::new(),
            Vec::new(),
        );
        let lift_fn_ref = TirExpr::new(
            TirExprKind::FuncRef {
                module_source: entry_source.clone(),
                name: lift_fn_name,
            },
            lift_fn_type,
            synth_span(),
        );

        let subtask_struct = TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type: subtask_type,
                struct_name: "AsyncCall".to_string(),
                fields: vec![
                    TirStructField {
                        name: "__cm_packed".to_string(),
                        value: local_ref(subtask_local, "__subtask_packed", TypeTable::I32),
                        field_index: 0,
                    },
                    TirStructField {
                        name: "__cm_outptr".to_string(),
                        value: outptr_expr,
                        field_index: 1,
                    },
                    TirStructField {
                        name: "__cm_size".to_string(),
                        value: size_expr,
                        field_index: 2,
                    },
                    TirStructField {
                        name: "__cm_align".to_string(),
                        value: align_expr,
                        field_index: 3,
                    },
                    TirStructField {
                        name: "__cm_lift".to_string(),
                        value: lift_fn_ref,
                        field_index: 4,
                    },
                ],
            },
            subtask_type,
            synth_span(),
        );
        body_stmts.push(return_stmt(Some(subtask_struct)));
        adapter_return_type = subtask_type;
        auxiliary.push(lift_fn);
    } else if let Some((alloc_size, alloc_align)) = outptr_alloc {
        body_stmts.push(expr_stmt(raw_call_expr));
        let outptr_local = next_local - 1;

        let return_type = func_info.return_type.as_ref().unwrap();
        let resolved = wasi_registry.resolve_type(return_type);

        // Inline lifting for all types, including list<T> which uses
        // Array::<T>::with_capacity() and .push() with proper monomorphization info.
        let lift_ctx = LiftContext {
            wasi_registry,
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

        // Materialize the lifted value into a local before freeing if it
        // contains a bare memory load (e.g., i32.load from the outptr buffer).
        // Complex types are already materialized into locals by synthesize_lift.
        let lifted = materialize_if_needed(lifted, &mut next_local, &mut body_stmts, &mut locals);

        // Free the outptr
        body_stmts.push(expr_stmt(builtin_call(
            "realloc",
            vec![
                local_ref(outptr_local, "__outptr", TypeTable::I32),
                i32_const(alloc_size as i32),
                i32_const(alloc_align as i32),
                i32_const(0),
            ],
            TypeTable::I32,
        )));

        let lifted_type_id = lifted.type_id;
        body_stmts.push(return_stmt(Some(lifted)));
        adapter_return_type = lifted_type_id; // real type, fixed up at call site if needed
    } else if let Some(return_type) = &func_info.return_type {
        let resolved = wasi_registry.resolve_type(return_type);
        if needs_flat_result_lifting(&resolved) {
            // Flat return with complex type (e.g., Result<(), ()>): the raw call returns
            // an i32 discriminant on the stack, but the binding needs to return a GC struct.
            // Synthesize VariantConstruct from the discriminant.
            let disc_local = alloc_local(&mut next_local, &mut locals, TypeTable::I32);
            body_stmts.push(let_stmt(
                "__disc",
                disc_local,
                TypeTable::I32,
                raw_call_expr,
            ));

            // Resolve the concrete `Result<T, E>` TypeId so the binding's
            // intermediate local and `VariantConstruct` exprs match the
            // declared return type. Without this, the local was declared as
            // `TypeTable::I32` and back-patched later by `type_fixup`,
            // which produced invalid TIR if any consumer ran first.
            let result_type_id = {
                let mut tt = type_table.borrow_mut();
                wasi_type_to_type_id(&resolved, &mut tt, wasi_registry, &func_info.package)
            };
            let result_local = alloc_local(&mut next_local, &mut locals, result_type_id);
            body_stmts.push(let_mut_stmt(
                "__result_val",
                result_local,
                result_type_id,
                null_expr(result_type_id),
            ));

            let lift_ctx = LiftContext {
                wasi_registry,
                type_table,
                cm_package: &func_info.package,
                interner,
            };
            let lifted = synthesize_lift_flat_result(
                &resolved,
                local_ref(disc_local, "__disc", TypeTable::I32),
                result_local,
                result_type_id,
                &mut next_local,
                &mut body_stmts,
                &mut locals,
                &lift_ctx,
            );
            let lifted_type_id = lifted.type_id;
            body_stmts.push(return_stmt(Some(lifted)));
            adapter_return_type = lifted_type_id;
        } else {
            // Truly flat return (primitive): cm_raw_call directly returns the value
            body_stmts.push(return_stmt(Some(raw_call_expr)));
            adapter_return_type = raw_call_return_type;
        }
    } else {
        // No return: just call
        body_stmts.push(expr_stmt(raw_call_expr));
        adapter_return_type = TypeTable::UNIT;
    }

    let body = block(body_stmts);

    let binding =
        make_binding_function(name, params, adapter_return_type, body, next_local, locals);
    // Resources and effects are unified at the effect-system level: every
    // operation on `<E>` (whether `<E>` is declared as `effect` or `resource`)
    // requires the caller to hold `with <E>`. The binding for a CM-imported
    // operation therefore carries its owning name as its single concrete
    // effect. The propagation closure (built in `effect_check`) walks
    // operation signatures separately, so additional resources reachable
    // through `<E>`'s operations are admitted without listing them here.
    {
        let mut b = binding.borrow_mut();
        b.effects.push(EffectRef::Concrete {
            name: func_info.interface_name.clone(),
            module_source: owner_module.clone(),
        });
    }
    AdapterArtifacts {
        adapter: binding,
        auxiliary,
    }
}
