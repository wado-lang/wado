//! CM resource method rewriting and stream-read binding synthesis.
//!
//! Two passes that run before adapter synthesis:
//!
//! - [`synthesize_record_stream_reads`] generates a binding function
//!   `__cm_stream_read_<T>` for every distinct WASI record `T` referenced
//!   by `Stream<T>::read()`, so the rewriter has a callable target.
//! - [`rewrite_cm_resource_methods`] walks every TIR function body and
//!   rewrites `#[cm("...")]` resource method calls into the appropriate
//!   raw / internal / entry-module call, before downstream phases see a
//!   `cm_name`-tagged `MethodCall` they don't know how to translate.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::{AstId, NamedType, Type};
use crate::component_model::CmInterfaceRegistry;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name::LocalMethodName;
use crate::package::Package;
use crate::tir::{
    CallArg, FunctionKind, FunctionRef, InlineHint, MonomorphInfo, ResolvedType, TirBinaryOp,
    TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal, TirParam, TirStmt, TirStmtKind, TypeId,
    TypeTable,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};

use crate::synthesis::common::{
    alloc_named_local, assign, binary, break_stmt, builtin_call, cast, cm_raw_call, entry_call,
    expr_stmt, i32_const, if_stmt, internal_call, let_mut_stmt, let_stmt, local_ref, loop_stmt,
    option_none, option_some, return_stmt, synth_span,
};
use crate::wir::{CanonicalIntrinsic, CmFuturePayload, CmStreamPayload};

use super::synthesize_lift;
use super::types::{CmStdlibNames, LiftContext, LowerContext, binary_add, type_id_to_ast_type};

/// CM async built-ins (`stream-read`, `stream-write`, `future-read`, …)
/// pack their result as `(count << 4) | status`, with `-1` meaning BLOCKED.
const CM_PACKED_COUNT_SHIFT: i32 = 4;
const CM_PACKED_STATUS_MASK: i32 = 0xF;
const CM_BLOCKED: i32 = -1;

/// `result >> 4`: the element count of a packed CM async result.
fn packed_count(result: TirExpr) -> TirExpr {
    binary(
        TirBinaryOp::Shr,
        result,
        i32_const(CM_PACKED_COUNT_SHIFT),
        TypeTable::I32,
    )
}

/// `result & 0xF`: the status bits of a packed CM async result.
fn packed_status(result: TirExpr) -> TirExpr {
    binary(
        TirBinaryOp::BitAnd,
        result,
        i32_const(CM_PACKED_STATUS_MASK),
        TypeTable::I32,
    )
}

/// `result == -1`: the BLOCKED sentinel of a CM async built-in.
fn is_blocked(result: TirExpr) -> TirExpr {
    binary(
        TirBinaryOp::Eq,
        result,
        i32_const(CM_BLOCKED),
        TypeTable::BOOL,
    )
}

/// Everything a per-key binding synthesizer needs from the [`Package`].
struct SynthCtx<'a> {
    cm_interface_registry: &'a CmInterfaceRegistry,
    type_table: &'a RefCell<TypeTable>,
    interner: &'a RefCell<ModuleSourceInterner>,
}

/// Applies `find` to every expression, recording helper-name → key per match.
struct BindingFinder<'a, K, F: Fn(&TypeTable, &TirExpr) -> Option<(String, K)>> {
    tt: &'a TypeTable,
    find: &'a F,
    results: &'a mut IndexMap<String, K>,
}

impl<K, F: Fn(&TypeTable, &TirExpr) -> Option<(String, K)>> TirRefVisitor
    for BindingFinder<'_, K, F>
{
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let Some((name, key)) = (self.find)(self.tt, expr) {
            self.results.entry(name).or_insert(key);
        }
        self.walk_expr(expr);
    }
}

/// Shared driver for the `synthesize_*` binding passes: walk every TIR
/// function body with `find` (an exhaustive [`TirRefVisitor`] traversal whose
/// coverage matches the rewriter's, so a call nested in an `if`-expression
/// branch still gets its helper generated), dedupe matches by helper name,
/// synthesize one function per key, and add them to the entry module — not
/// `values().next()`, which is not guaranteed to be the entry module. Calls
/// synthesized by `rewrite_cm_resource_methods` target the entry module via
/// `entry_call`, so the helpers must live there for resolution to succeed in
/// `wir_build`.
fn synthesize_entry_bindings<K>(
    project: &mut Package,
    find: impl Fn(&TypeTable, &TirExpr) -> Option<(String, K)>,
    synthesize: impl Fn(K, &SynthCtx) -> TirFunction,
) {
    let mut needed: IndexMap<String, K> = IndexMap::default();
    for module in project.tir_modules.values() {
        let tt = module.type_table.borrow();
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                BindingFinder {
                    tt: &tt,
                    find: &find,
                    results: &mut needed,
                }
                .visit_block(body);
            }
        }
    }
    if needed.is_empty() {
        return;
    }

    let entry_source = project.entry_module_source.clone();
    let type_table = project
        .tir_modules
        .get(&entry_source)
        .expect("entry module must exist in tir_modules")
        .type_table
        .clone();
    let ctx = SynthCtx {
        cm_interface_registry: &project.cm_interface_registry,
        type_table: &type_table,
        interner: &project.interner,
    };
    let new_functions: Vec<Rc<RefCell<TirFunction>>> = needed
        .into_iter()
        .map(|(_, key)| Rc::new(RefCell::new(synthesize(key, &ctx))))
        .collect();

    let entry_module = project
        .tir_modules
        .get_mut(&entry_source)
        .expect("entry module must exist in tir_modules");
    entry_module.functions.extend(new_functions);
}

/// Generate binding functions for Stream<T>.`read()` where T is a non-u8 WASI record type.
///
/// For each unique stream element type T found in stream-read calls, generates a
/// TIR function `__cm_stream_read_<T>` that:
/// 1. Calls `cm_stream_read_raw(handle, max, elem_size, elem_align)` to get raw buffer
/// 2. Loops through the buffer, lifting each record from linear memory
/// 3. Constructs `List<T>` and returns it
pub(super) fn synthesize_record_stream_reads(project: &mut Package) {
    synthesize_entry_bindings(
        project,
        |tt, expr| {
            let (elem, list) = record_stream_read_element(tt, expr)?;
            Some((
                record_stream_read_func_name(&tt.base_type_name(elem)),
                (elem, list),
            ))
        },
        |(elem_type_id, array_type_id), ctx| {
            synthesize_record_stream_read_func(elem_type_id, array_type_id, ctx)
        },
    );
}

fn synthesize_record_stream_read_func(
    elem_type_id: TypeId,
    array_type_id: TypeId,
    ctx: &SynthCtx,
) -> TirFunction {
    let registry = ctx.cm_interface_registry;
    let elem_name = ctx.type_table.borrow().base_type_name(elem_type_id);
    let source = registry
        .find_wasi_struct_source(&elem_name)
        .unwrap_or_else(|| {
            panic!(
                "record `{elem_name}` used as a stream-read element has no defining \
                 `wasi:*` interface in the CM interface registry; cannot synthesize \
                 its stream-read binding"
            )
        })
        .to_string();
    if registry
        .get_struct_fields_by_source(&source, &elem_name)
        .is_none()
    {
        panic!(
            "fields of record `{elem_name}` (interface `{source}`) are not registered \
             in the CM interface registry; cannot lift its stream-read elements"
        );
    }
    let ast_type = Type::Named(NamedType {
        id: AstId::fresh(),
        name: elem_name.clone(),
        span: synth_span(),
        source_interface: Some(source.clone()),
    });
    // Scope element layout to the record's own package (from `source`) so
    // nested field names resolve the same way the scoped lift reads them.
    let elem_pkg = super::types::wasi_package_from_cm_source(&source);
    let elem_size =
        crate::component_model::cm_size_with_registry_scoped(&ast_type, registry, elem_pkg) as i32;
    let elem_align =
        crate::component_model::cm_align_with_registry_scoped(&ast_type, registry, elem_pkg) as i32;
    let cm_record_name = registry
        .get_struct_cm_name_by_source(&source, &elem_name)
        .unwrap_or(&elem_name)
        .to_string();
    synthesize_stream_read_func(
        record_stream_read_func_name(&elem_name),
        format!("stream-read:{cm_record_name}"),
        &elem_name,
        elem_type_id,
        array_type_id,
        elem_size,
        elem_align,
        &ast_type,
        "filesystem",
        registry,
        ctx.type_table,
        ctx.interner,
    )
}

/// Generate the per-payload `Future<T>::read()` binding functions.
///
/// For each distinct future payload `T` consumed by `future-read`, synthesizes
/// `__cm_future_read_<mangle(T)>(handle: i32) -> Option<T>` which allocates a
/// CM buffer (sized via `cm_abi`), calls the payload-parameterized
/// `future-read` canonical, handles BLOCKED via `future_await_blocked`, lifts the
/// payload with the shared `synthesize_lift`, and wraps it in `Option`. This
/// replaces the hand-rolled WIR-build lift with its hardcoded CM offsets.
pub(super) fn synthesize_future_reads(project: &mut Package) {
    synthesize_entry_bindings(
        project,
        |tt, expr| {
            let (payload, option) = future_read_payload(tt, expr)?;
            Some((future_read_func_name(tt, payload), (payload, option)))
        },
        |(payload_type_id, option_type_id), ctx| {
            synthesize_future_read_func(payload_type_id, option_type_id, ctx)
        },
    );
}

/// Generate the per-payload `FutureWritable<T>::write()` binding functions,
/// mirroring [`synthesize_future_reads`].
///
/// On BLOCKED, transmission shapes (write-completion, trailers) await the host
/// reader and free the buffer, but value payloads leave the buffer alive and
/// return: their reader is another task in the same instance, so busy-waiting
/// would deadlock the async executor.
pub(super) fn synthesize_future_writes(project: &mut Package) {
    synthesize_entry_bindings(
        project,
        |tt, expr| {
            let payload = future_write_payload(tt, expr)?;
            Some((future_write_func_name(tt, payload), payload))
        },
        synthesize_future_write_func,
    );
}

/// The payload type of a `future-write` method call, or `None` if the
/// expression is not a future-write.
fn future_write_payload(tt: &TypeTable, expr: &TirExpr) -> Option<TypeId> {
    let (func, receiver) = match &expr.kind {
        TirExprKind::MethodCall { func, receiver, .. } => (func, receiver),
        _ => return None,
    };
    if func.method_info.as_ref().and_then(|m| m.cm_name.as_deref()) != Some("future-write") {
        return None;
    }
    let mut recv = receiver.type_id;
    while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = tt.get(recv) {
        recv = *inner;
    }
    tt.generic_type_args(recv)?.first().copied()
}

/// The `__cm_future_write_*` helper name for a payload type.
fn future_write_func_name(tt: &TypeTable, payload_type_id: TypeId) -> String {
    format!(
        "__cm_future_write_{}",
        tt.mangle_type_arg_for_generic(payload_type_id)
    )
}

/// Generate per-element `StreamWritable<T>::write()` binding functions for
/// scalar / structural (non-`u8`, non-record) element types, mirroring
/// [`synthesize_future_writes`]. Each lowers a `List<T>` into a CM element
/// buffer (via `synthesize_lower_list_to_buffer`) and calls the
/// element-parameterized `stream-write` canonical, waiting for the reader on
/// BLOCKED (streams deliver element-by-element, so the buffer must survive the
/// wait — the function runs in an `async` task).
pub(super) fn synthesize_stream_writes(project: &mut Package) {
    synthesize_entry_bindings(
        project,
        |tt, expr| {
            let elem = stream_write_value_element(tt, expr)?;
            Some((stream_write_func_name(tt, elem), elem))
        },
        synthesize_stream_write_func,
    );
}

/// The stream-write element type for a scalar / structural `stream-write`, or
/// `None` for `u8` and record streams (handled elsewhere).
fn stream_write_value_element(tt: &TypeTable, expr: &TirExpr) -> Option<TypeId> {
    let (func, receiver) = match &expr.kind {
        TirExprKind::MethodCall { func, receiver, .. } => (func, receiver),
        _ => return None,
    };
    if func.method_info.as_ref().and_then(|m| m.cm_name.as_deref()) != Some("stream-write") {
        return None;
    }
    let mut recv = receiver.type_id;
    while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = tt.get(recv) {
        recv = *inner;
    }
    let elem = *tt.generic_type_args(recv)?.first()?;
    matches!(
        crate::component_model::classify_stream_payload(tt, elem),
        CmStreamPayload::Value(_)
    )
    .then_some(elem)
}

fn stream_write_func_name(tt: &TypeTable, elem_type_id: TypeId) -> String {
    format!(
        "__cm_stream_write_{}",
        tt.mangle_type_arg_for_generic(elem_type_id)
    )
}

/// Generate per-element `StreamReadable<T>::read()` binding functions for
/// scalar / structural (non-`u8`, non-WASI-record) element types, mirroring
/// [`synthesize_stream_writes`]. Each reads up to `max` elements into a CM
/// buffer via the element-parameterized `stream-read` canonical, lifts each
/// element with the shared `synthesize_lift`, and returns `List<T>`. An empty
/// result signals EOF to the caller.
pub(super) fn synthesize_stream_reads(project: &mut Package) {
    synthesize_entry_bindings(
        project,
        |tt, expr| {
            let elem = stream_read_value_element(tt, expr)?;
            Some((stream_read_value_func_name(tt, elem), elem))
        },
        synthesize_stream_read_value_func,
    );
}

/// The stream-read element type for a value-payload `stream-read`, or `None`
/// for `u8` and WASI record streams (handled by their own paths).
fn stream_read_value_element(tt: &TypeTable, expr: &TirExpr) -> Option<TypeId> {
    let func = match &expr.kind {
        TirExprKind::MethodCall { func, .. } | TirExprKind::Call { func, .. } => func,
        _ => return None,
    };
    if func.method_info.as_ref().and_then(|m| m.cm_name.as_deref()) != Some("stream-read") {
        return None;
    }
    if is_u8_array_type(expr.type_id, tt) {
        return None;
    }
    let elem = *tt.generic_type_args(expr.type_id)?.first()?;
    matches!(
        crate::component_model::classify_stream_payload(tt, elem),
        CmStreamPayload::Value(_)
    )
    .then_some(elem)
}

/// The `__cm_stream_read_val_*` helper name for an element type.
fn stream_read_value_func_name(tt: &TypeTable, elem_type_id: TypeId) -> String {
    format!(
        "__cm_stream_read_val_{}",
        tt.mangle_type_arg_for_generic(elem_type_id)
    )
}

fn synthesize_stream_write_func(elem_type_id: TypeId, ctx: &SynthCtx) -> TirFunction {
    let cm_interface_registry = ctx.cm_interface_registry;
    let type_table = ctx.type_table;
    let list_type_id = type_table.borrow_mut().make_list(elem_type_id);
    let (func_name, write_name, elem_ast, elem_size, elem_align) = {
        let tt = type_table.borrow();
        let func_name = stream_write_func_name(&tt, elem_type_id);
        let payload = crate::component_model::classify_stream_payload(&tt, elem_type_id);
        let write_name = CanonicalIntrinsic::StreamWrite(payload).import_name();
        let elem_ast = type_id_to_ast_type(elem_type_id, &tt, cm_interface_registry);
        // Match the package the element buffer is packed with below
        // (`synthesize_lower_list_to_buffer`, wasi_package "cli"), so the retry
        // pointer stride can't disagree with the buffer's element size.
        let size = crate::component_model::cm_size_with_registry_scoped(
            &elem_ast,
            cm_interface_registry,
            Some("cli"),
        ) as i32;
        let align = crate::component_model::cm_align_with_registry_scoped(
            &elem_ast,
            cm_interface_registry,
            Some("cli"),
        ) as i32;
        (func_name, write_name, elem_ast, size, align)
    };

    let mut next_local: u32 = 0;
    let mut locals: Vec<TirLocal> = Vec::new();
    let mut stmts: Vec<TirStmt> = Vec::new();

    // Params: handle (i32), data (List<T>).
    let handle_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("handle".to_string()),
        TypeTable::I32,
        false,
    );
    let data_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("data".to_string()),
        list_type_id,
        false,
    );

    // Lower the list into a CM element buffer: (ptr, element count).
    let lower_ctx = LowerContext {
        cm_interface_registry,
        type_table,
        wasi_package: "cli",
        names: CmStdlibNames::from_compiler_items(type_table.borrow().compiler_items()),
    };
    let (lower_stmts, ptr_local, count_local) = super::lower::synthesize_lower_list_to_buffer(
        &elem_ast,
        local_ref(data_idx, "data", list_type_id),
        &mut next_local,
        &mut locals,
        &lower_ctx,
    );
    stmts.extend(lower_stmts);

    // A reader may take fewer elements than offered, so `stream.write` can copy
    // only a prefix and report COMPLETED. Loop, advancing past the elements
    // already written, until the whole buffer is sent or the reader drops.
    let offset_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("offset".to_string()),
        TypeTable::I32,
        true,
    );
    stmts.push(let_mut_stmt(
        "offset",
        offset_idx,
        TypeTable::I32,
        i32_const(0),
    ));
    let result_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("result".to_string()),
        TypeTable::I32,
        true,
    );
    stmts.push(let_mut_stmt(
        "result",
        result_idx,
        TypeTable::I32,
        i32_const(0),
    ));

    let offset_ref = || local_ref(offset_idx, "offset", TypeTable::I32);
    let result_ref = || local_ref(result_idx, "result", TypeTable::I32);
    let count_ref = || local_ref(count_local, "__list_len", TypeTable::I32);

    // cur_ptr = base + offset * elem_size
    let cur_ptr = binary(
        TirBinaryOp::Add,
        local_ref(ptr_local, "__list_base", TypeTable::I32),
        binary(
            TirBinaryOp::Mul,
            offset_ref(),
            i32_const(elem_size),
            TypeTable::I32,
        ),
        TypeTable::I32,
    );
    // remaining = count - offset
    let remaining = binary(TirBinaryOp::Sub, count_ref(), offset_ref(), TypeTable::I32);

    let loop_body = TirBlock {
        stmts: vec![
            // if offset >= count { break }
            if_stmt(
                binary(
                    TirBinaryOp::GtEq,
                    offset_ref(),
                    count_ref(),
                    TypeTable::BOOL,
                ),
                TirBlock {
                    stmts: vec![break_stmt()],
                    span: synth_span(),
                },
                None,
            ),
            // result = stream-write:<elem>(handle, cur_ptr, remaining)
            expr_stmt(assign(
                result_ref(),
                cm_raw_call(
                    &write_name,
                    vec![
                        local_ref(handle_idx, "handle", TypeTable::I32),
                        cur_ptr,
                        remaining,
                    ],
                    TypeTable::I32,
                ),
            )),
            // if result == BLOCKED { result = wait_for_blocked(handle) }
            await_if_blocked(result_idx, "result", handle_idx, "wait_for_blocked"),
            // offset += packed count (the copied-element count)
            expr_stmt(assign(
                offset_ref(),
                binary(
                    TirBinaryOp::Add,
                    offset_ref(),
                    packed_count(result_ref()),
                    TypeTable::I32,
                ),
            )),
            // if packed count == 0 || packed status != 0 { break }
            // No progress, or the reader/stream closed (DROPPED/CANCELLED).
            if_stmt(
                binary(
                    TirBinaryOp::Or,
                    binary(
                        TirBinaryOp::Eq,
                        packed_count(result_ref()),
                        i32_const(0),
                        TypeTable::BOOL,
                    ),
                    binary(
                        TirBinaryOp::NotEq,
                        packed_status(result_ref()),
                        i32_const(0),
                        TypeTable::BOOL,
                    ),
                    TypeTable::BOOL,
                ),
                TirBlock {
                    stmts: vec![break_stmt()],
                    span: synth_span(),
                },
                None,
            ),
        ],
        span: synth_span(),
    };
    stmts.push(loop_stmt(loop_body));

    // Free the element buffer: realloc(ptr, count * elem_size, elem_align, 0).
    let byte_count = binary(
        TirBinaryOp::Mul,
        local_ref(count_local, "__list_len", TypeTable::I32),
        i32_const(elem_size),
        TypeTable::I32,
    );
    let freed_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("__freed".to_string()),
        TypeTable::I32,
        false,
    );
    stmts.push(let_stmt(
        "__freed",
        freed_idx,
        TypeTable::I32,
        builtin_call(
            "realloc",
            vec![
                local_ref(ptr_local, "__list_base", TypeTable::I32),
                byte_count,
                i32_const(elem_align),
                i32_const(0),
            ],
            TypeTable::I32,
        ),
    ));

    TirFunction {
        module_source: ModuleSource::default(),
        name: func_name,
        visibility: crate::ast::Visibility::Private,
        is_export: false,
        is_async: false,
        type_params: vec![],
        impl_type_params: vec![],
        monomorph_info: None,
        method_info: None,
        params: vec![
            TirParam {
                name: "handle".to_string(),
                local_index: handle_idx,
                type_id: TypeTable::I32,
                is_mut: false,
                is_mut_ref: false,
                span: synth_span(),
            },
            TirParam {
                name: "data".to_string(),
                local_index: data_idx,
                type_id: list_type_id,
                is_mut: false,
                is_mut_ref: false,
                span: synth_span(),
            },
        ],
        return_type: TypeTable::UNIT,
        task_return_type: None,
        effects: vec![],
        stores: vec![],
        body: Some(TirBlock {
            stmts,
            span: synth_span(),
        }),
        span: synth_span(),
        local_count: next_local,
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
    }
}

/// `if <status> == BLOCKED { <status> = <awaiter>(handle) }` — re-reads the
/// packed result after the host signals readiness.
fn await_if_blocked(status_idx: u32, status_name: &str, handle_idx: u32, awaiter: &str) -> TirStmt {
    if_stmt(
        is_blocked(local_ref(status_idx, status_name, TypeTable::I32)),
        TirBlock {
            stmts: vec![expr_stmt(assign(
                local_ref(status_idx, status_name, TypeTable::I32),
                internal_call(
                    awaiter,
                    vec![local_ref(handle_idx, "handle", TypeTable::I32)],
                    TypeTable::I32,
                ),
            ))],
            span: synth_span(),
        },
        None,
    )
}

fn free_cm_buffer(
    ptr_idx: u32,
    size: i32,
    align: i32,
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
) -> TirStmt {
    let freed_idx = alloc_named_local(
        next_local,
        locals,
        Some("__freed".to_string()),
        TypeTable::I32,
        false,
    );
    let_stmt(
        "__freed",
        freed_idx,
        TypeTable::I32,
        builtin_call(
            "realloc",
            vec![
                local_ref(ptr_idx, "ptr", TypeTable::I32),
                i32_const(size),
                i32_const(align),
                i32_const(0),
            ],
            TypeTable::I32,
        ),
    )
}

fn synthesize_future_write_func(payload_type_id: TypeId, ctx: &SynthCtx) -> TirFunction {
    let cm_interface_registry = ctx.cm_interface_registry;
    let type_table = ctx.type_table;
    let (func_name, write_name, cm_package, payload_ast, size, align, awaits_reader) = {
        let tt = type_table.borrow();
        let func_name = future_write_func_name(&tt, payload_type_id);
        let payload = crate::component_model::classify_future_payload(&tt, payload_type_id);
        let write_name = CanonicalIntrinsic::FutureWrite(payload.clone()).import_name();
        let cm_package = future_payload_package(&payload);
        let payload_ast = type_id_to_ast_type(payload_type_id, &tt, cm_interface_registry);
        let size = crate::component_model::cm_size_with_registry_scoped(
            &payload_ast,
            cm_interface_registry,
            Some(&cm_package),
        ) as i32;
        let align = crate::component_model::cm_align_with_registry_scoped(
            &payload_ast,
            cm_interface_registry,
            Some(&cm_package),
        ) as i32;
        let awaits_reader = matches!(
            payload,
            CmFuturePayload::Trailers | CmFuturePayload::Transmission(_)
        );
        (
            func_name,
            write_name,
            cm_package,
            payload_ast,
            size,
            align,
            awaits_reader,
        )
    };

    let mut next_local: u32 = 0;
    let mut locals: Vec<TirLocal> = Vec::new();
    let mut stmts: Vec<TirStmt> = Vec::new();

    // Params: handle (i32), value (T).
    let handle_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("handle".to_string()),
        TypeTable::I32,
        false,
    );
    let value_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("value".to_string()),
        payload_type_id,
        false,
    );

    // let ptr = realloc(0, 0, align, size)
    let ptr_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("ptr".to_string()),
        TypeTable::I32,
        false,
    );
    stmts.push(let_stmt(
        "ptr",
        ptr_idx,
        TypeTable::I32,
        builtin_call(
            "realloc",
            vec![
                i32_const(0),
                i32_const(0),
                i32_const(align),
                i32_const(size),
            ],
            TypeTable::I32,
        ),
    ));

    // Lower `value` into the buffer using the shared registry-backed lowerer.
    let lower_ctx = LowerContext {
        cm_interface_registry,
        type_table,
        wasi_package: &cm_package,
        names: CmStdlibNames::from_compiler_items(type_table.borrow().compiler_items()),
    };
    stmts.extend(super::lower::synthesize_lower_wasi_type_to_memory(
        &payload_ast,
        local_ref(value_idx, "value", payload_type_id),
        local_ref(ptr_idx, "ptr", TypeTable::I32),
        &mut next_local,
        &mut locals,
        &lower_ctx,
    ));

    let written_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("__written".to_string()),
        TypeTable::I32,
        awaits_reader,
    );
    let write_binding = if awaits_reader {
        let_mut_stmt
    } else {
        let_stmt
    };
    stmts.push(write_binding(
        "__written",
        written_idx,
        TypeTable::I32,
        cm_raw_call(
            &write_name,
            vec![
                local_ref(handle_idx, "handle", TypeTable::I32),
                local_ref(ptr_idx, "ptr", TypeTable::I32),
            ],
            TypeTable::I32,
        ),
    ));

    if awaits_reader {
        stmts.push(await_if_blocked(
            written_idx,
            "__written",
            handle_idx,
            "future_await_blocked",
        ));
        stmts.push(free_cm_buffer(
            ptr_idx,
            size,
            align,
            &mut next_local,
            &mut locals,
        ));
    }

    TirFunction {
        module_source: ModuleSource::default(),
        name: func_name,
        visibility: crate::ast::Visibility::Private,
        is_export: false,
        is_async: false,
        type_params: vec![],
        impl_type_params: vec![],
        monomorph_info: None,
        method_info: None,
        params: vec![
            TirParam {
                name: "handle".to_string(),
                local_index: handle_idx,
                type_id: TypeTable::I32,
                is_mut: false,
                is_mut_ref: false,
                span: synth_span(),
            },
            TirParam {
                name: "value".to_string(),
                local_index: value_idx,
                type_id: payload_type_id,
                is_mut: false,
                is_mut_ref: false,
                span: synth_span(),
            },
        ],
        return_type: TypeTable::UNIT,
        task_return_type: None,
        effects: vec![],
        stores: vec![],
        body: Some(TirBlock {
            stmts,
            span: synth_span(),
        }),
        span: synth_span(),
        local_count: next_local,
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
    }
}

/// The `(payload, Option<payload>)` types of a `future-read` call, or `None`
/// if the expression is not a future-read.
fn future_read_payload(tt: &TypeTable, expr: &TirExpr) -> Option<(TypeId, TypeId)> {
    let func = match &expr.kind {
        TirExprKind::MethodCall { func, .. } | TirExprKind::Call { func, .. } => func,
        _ => return None,
    };
    if func.method_info.as_ref().and_then(|m| m.cm_name.as_deref()) != Some("future-read") {
        return None;
    }
    let payload_type_id = *tt.generic_type_args(expr.type_id)?.first()?;
    Some((payload_type_id, expr.type_id))
}

/// The `__cm_future_read_*` helper name for a payload type.
fn future_read_func_name(tt: &TypeTable, payload_type_id: TypeId) -> String {
    format!(
        "__cm_future_read_{}",
        tt.mangle_type_arg_for_generic(payload_type_id)
    )
}

/// CM package scope for lifting a future payload (biases named-type source
/// resolution in `synthesize_lift`).
fn future_payload_package(payload: &CmFuturePayload) -> String {
    match payload {
        CmFuturePayload::Transmission(src) => src.clone(),
        CmFuturePayload::Trailers => "http".to_string(),
        CmFuturePayload::Scalar(_) => "cli".to_string(),
        // General value payloads carry no WASI scope; named types in the
        // payload resolve through the registry against the entry package.
        CmFuturePayload::Value(_) => "cli".to_string(),
    }
}

fn synthesize_future_read_func(
    payload_type_id: TypeId,
    option_type_id: TypeId,
    ctx: &SynthCtx,
) -> TirFunction {
    let cm_interface_registry = ctx.cm_interface_registry;
    let type_table = ctx.type_table;
    let (func_name, read_name, cm_package, payload_ast, size, align) = {
        let tt = type_table.borrow();
        let func_name = future_read_func_name(&tt, payload_type_id);
        let payload = crate::component_model::classify_future_payload(&tt, payload_type_id);
        let read_name = CanonicalIntrinsic::FutureRead(payload.clone()).import_name();
        let cm_package = future_payload_package(&payload);
        let payload_ast = type_id_to_ast_type(payload_type_id, &tt, cm_interface_registry);
        let size = crate::component_model::cm_size_with_registry_scoped(
            &payload_ast,
            cm_interface_registry,
            Some(&cm_package),
        ) as i32;
        let align = crate::component_model::cm_align_with_registry_scoped(
            &payload_ast,
            cm_interface_registry,
            Some(&cm_package),
        ) as i32;
        (func_name, read_name, cm_package, payload_ast, size, align)
    };

    let mut next_local: u32 = 0;
    let mut locals: Vec<TirLocal> = Vec::new();
    let mut stmts: Vec<TirStmt> = Vec::new();

    // Param: handle (i32).
    let handle_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("handle".to_string()),
        TypeTable::I32,
        false,
    );

    // let ptr = realloc(0, 0, align, size)
    let ptr_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("ptr".to_string()),
        TypeTable::I32,
        false,
    );
    stmts.push(let_stmt(
        "ptr",
        ptr_idx,
        TypeTable::I32,
        builtin_call(
            "realloc",
            vec![
                i32_const(0),
                i32_const(0),
                i32_const(align),
                i32_const(size),
            ],
            TypeTable::I32,
        ),
    ));

    // let mut status = future-read:<payload>(handle, ptr)
    let status_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("status".to_string()),
        TypeTable::I32,
        true,
    );
    stmts.push(let_mut_stmt(
        "status",
        status_idx,
        TypeTable::I32,
        cm_raw_call(
            &read_name,
            vec![
                local_ref(handle_idx, "handle", TypeTable::I32),
                local_ref(ptr_idx, "ptr", TypeTable::I32),
            ],
            TypeTable::I32,
        ),
    ));

    stmts.push(await_if_blocked(
        status_idx,
        "status",
        handle_idx,
        "future_await_blocked",
    ));

    // let mut result: Option<T> = None
    let result_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("result".to_string()),
        option_type_id,
        true,
    );
    let none_val = option_none(option_type_id, type_table.borrow().compiler_items());
    stmts.push(let_mut_stmt("result", result_idx, option_type_id, none_val));

    // if packed status == 0 { result = Some(<lifted payload>); } else { /* None */ }
    let cond = binary(
        TirBinaryOp::Eq,
        packed_status(local_ref(status_idx, "status", TypeTable::I32)),
        i32_const(0),
        TypeTable::BOOL,
    );
    let mut some_stmts: Vec<TirStmt> = Vec::new();
    let lift_ctx = LiftContext {
        cm_interface_registry,
        type_table,
        cm_package: &cm_package,
        interner: ctx.interner,
    };
    let lifted = synthesize_lift(
        &payload_ast,
        local_ref(ptr_idx, "ptr", TypeTable::I32),
        &mut next_local,
        &mut some_stmts,
        &mut locals,
        &lift_ctx,
    );
    let some_val = option_some(lifted, option_type_id, type_table.borrow().compiler_items());
    some_stmts.push(expr_stmt(assign(
        local_ref(result_idx, "result", option_type_id),
        some_val,
    )));
    stmts.push(if_stmt(
        cond,
        TirBlock {
            stmts: some_stmts,
            span: synth_span(),
        },
        None,
    ));

    // Free the CM buffer (after the lift has read from it).
    stmts.push(free_cm_buffer(
        ptr_idx,
        size,
        align,
        &mut next_local,
        &mut locals,
    ));

    stmts.push(return_stmt(Some(local_ref(
        result_idx,
        "result",
        option_type_id,
    ))));

    TirFunction {
        module_source: ModuleSource::default(),
        name: func_name,
        visibility: crate::ast::Visibility::Private,
        is_export: false,
        is_async: false,
        type_params: vec![],
        impl_type_params: vec![],
        monomorph_info: None,
        method_info: None,
        params: vec![TirParam {
            name: "handle".to_string(),
            local_index: handle_idx,
            type_id: TypeTable::I32,
            is_mut: false,
            is_mut_ref: false,
            span: synth_span(),
        }],
        return_type: option_type_id,
        task_return_type: None,
        effects: vec![],
        stores: vec![],
        body: Some(TirBlock {
            stmts,
            span: synth_span(),
        }),
        span: synth_span(),
        local_count: next_local,
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
    }
}

/// The `(element, List<element>)` types of a WASI-record `stream-read`, or
/// `None` for `u8` and value-payload streams (handled by their own paths).
fn record_stream_read_element(tt: &TypeTable, expr: &TirExpr) -> Option<(TypeId, TypeId)> {
    let func = match &expr.kind {
        TirExprKind::MethodCall { func, .. } | TirExprKind::Call { func, .. } => func,
        _ => return None,
    };
    if func.method_info.as_ref().and_then(|m| m.cm_name.as_deref()) != Some("stream-read") {
        return None;
    }
    if is_u8_array_type(expr.type_id, tt) {
        return None;
    }
    let elem_type_id = *tt.generic_type_args(expr.type_id)?.first()?;
    if matches!(
        crate::component_model::classify_stream_payload(tt, elem_type_id),
        CmStreamPayload::Value(_)
    ) {
        return None;
    }
    Some((elem_type_id, expr.type_id))
}

/// The `__cm_stream_read_<record>` helper name for a WASI record element.
fn record_stream_read_func_name(elem_name: &str) -> String {
    format!("__cm_stream_read_{elem_name}")
}

/// Shared stream-read loop generator. `func_name` / `stream_read_name` /
/// `payload_ast` / `cm_package` / `elem_inst_name` are precomputed by the
/// caller so this body serves both the WASI-record path
/// ([`synthesize_record_stream_reads`]) and the value-payload path
/// ([`synthesize_stream_reads`]). `elem_inst_name` is the type name used to
/// instantiate `List<…>` for the result builder.
#[allow(clippy::too_many_arguments)]
fn synthesize_stream_read_func(
    func_name: String,
    stream_read_name: String,
    elem_inst_name: &str,
    elem_type_id: TypeId,
    array_type_id: TypeId,
    elem_size: i32,
    elem_align: i32,
    payload_ast: &Type,
    cm_package: &str,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &RefCell<TypeTable>,
    interner: &RefCell<ModuleSourceInterner>,
) -> TirFunction {
    let list_struct_name =
        super::types::CmStdlibNames::from_compiler_items(type_table.borrow().compiler_items())
            .array;

    let mut next_local: u32 = 0;
    let mut locals: Vec<TirLocal> = Vec::new();
    let mut stmts: Vec<TirStmt> = Vec::new();

    // Params: handle (i32), max (i32)
    let handle_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("handle".to_string()),
        TypeTable::I32,
        false,
    );
    let max_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("max".to_string()),
        TypeTable::I32,
        false,
    );

    // let byte_count = max * elem_size
    let byte_count_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("byte_count".to_string()),
        TypeTable::I32,
        false,
    );
    let byte_count = binary(
        TirBinaryOp::Mul,
        local_ref(max_idx, "max", TypeTable::I32),
        i32_const(elem_size),
        TypeTable::I32,
    );
    stmts.push(let_stmt(
        "byte_count",
        byte_count_idx,
        TypeTable::I32,
        byte_count,
    ));

    // let ptr = realloc(0, 0, elem_align, byte_count)
    let ptr_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("ptr".to_string()),
        TypeTable::I32,
        false,
    );
    let alloc_call = builtin_call(
        "realloc",
        vec![
            i32_const(0),
            i32_const(0),
            i32_const(elem_align),
            local_ref(byte_count_idx, "byte_count", TypeTable::I32),
        ],
        TypeTable::I32,
    );
    stmts.push(let_stmt("ptr", ptr_idx, TypeTable::I32, alloc_call));

    // let mut result = stream-read:directory-entry(handle, ptr, max)
    let result_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("result".to_string()),
        TypeTable::I32,
        false,
    );
    let stream_read_call = cm_raw_call(
        &stream_read_name,
        vec![
            local_ref(handle_idx, "handle", TypeTable::I32),
            local_ref(ptr_idx, "ptr", TypeTable::I32),
            local_ref(max_idx, "max", TypeTable::I32),
        ],
        TypeTable::I32,
    );
    stmts.push(let_mut_stmt(
        "result",
        result_idx,
        TypeTable::I32,
        stream_read_call,
    ));

    // if result == BLOCKED { result = wait_for_blocked(handle); }
    stmts.push(await_if_blocked(
        result_idx,
        "result",
        handle_idx,
        "wait_for_blocked",
    ));

    // let count = packed count of result
    let count_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("count".to_string()),
        TypeTable::I32,
        false,
    );
    let count_expr = packed_count(local_ref(result_idx, "result", TypeTable::I32));
    stmts.push(let_stmt("count", count_idx, TypeTable::I32, count_expr));

    // let mut arr = List::<T>::with_capacity(count), filled element by element
    let arr_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("arr".to_string()),
        array_type_id,
        false,
    );

    // Create empty array via List<T>::with_capacity(count)
    let empty_arr = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::list(),
                name: format!("{list_struct_name}<{elem_inst_name}>::with_capacity"),
                monomorph_info: Some(MonomorphInfo {
                    generic_name: "List::with_capacity".to_string(),
                    impl_type_args: vec![elem_type_id],
                    method_type_args: vec![],
                    is_blanket: false,
                }),
                method_info: Some(LocalMethodName {
                    struct_name: format!("{list_struct_name}<{elem_inst_name}>"),
                    receiver: crate::name::Receiver::classify(&list_struct_name),
                    trait_name: None,
                    base_trait_name: None,
                    base_trait_module: None,
                    trait_type_args: vec![],
                    method_name: "with_capacity".to_string(),
                    method_type_args: vec![],
                    is_type_param_receiver: false,
                    is_ref_impl: false,
                    cm_name: None,
                }),
            },
            type_args: vec![],
            args: vec![CallArg::new(
                local_ref(count_idx, "count", TypeTable::I32),
                false,
            )],
        },
        array_type_id,
        synth_span(),
    );
    stmts.push(let_mut_stmt("arr", arr_idx, array_type_id, empty_arr));

    // let mut i = 0
    let i_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("i".to_string()),
        TypeTable::I32,
        false,
    );
    stmts.push(let_mut_stmt("i", i_idx, TypeTable::I32, i32_const(0)));

    // Loop body: while i < count
    let mut loop_body_stmts = Vec::new();

    // if i >= count { break; }
    let break_cond = binary(
        TirBinaryOp::GtEq,
        local_ref(i_idx, "i", TypeTable::I32),
        local_ref(count_idx, "count", TypeTable::I32),
        TypeTable::BOOL,
    );
    loop_body_stmts.push(if_stmt(
        break_cond,
        TirBlock {
            stmts: vec![break_stmt()],
            span: synth_span(),
        },
        None,
    ));

    // let addr = ptr + i * elem_size
    let addr_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("addr".to_string()),
        TypeTable::I32,
        false,
    );
    let offset = binary(
        TirBinaryOp::Mul,
        local_ref(i_idx, "i", TypeTable::I32),
        i32_const(elem_size),
        TypeTable::I32,
    );
    let addr = binary_add(local_ref(ptr_idx, "ptr", TypeTable::I32), offset);
    loop_body_stmts.push(let_stmt("addr", addr_idx, TypeTable::I32, addr));

    // Lift each element from linear memory at addr.
    let lift_ctx = LiftContext {
        cm_interface_registry,
        type_table,
        cm_package,
        interner,
    };
    let lifted_elem = synthesize_lift(
        payload_ast,
        local_ref(addr_idx, "addr", TypeTable::I32),
        &mut next_local,
        &mut loop_body_stmts,
        &mut locals,
        &lift_ctx,
    );

    // arr.push(elem)
    let elem_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("elem".to_string()),
        elem_type_id,
        false,
    );
    loop_body_stmts.push(let_stmt("elem", elem_idx, elem_type_id, lifted_elem));

    let push_call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(local_ref(arr_idx, "arr", array_type_id)),
            FunctionRef {
                module_source: ModuleSource::list(),
                name: format!("{list_struct_name}<{elem_inst_name}>::push"),
                monomorph_info: Some(MonomorphInfo {
                    generic_name: "List::push".to_string(),
                    impl_type_args: vec![elem_type_id],
                    method_type_args: vec![],
                    is_blanket: false,
                }),
                method_info: Some(LocalMethodName {
                    struct_name: format!("{list_struct_name}<{elem_inst_name}>"),
                    receiver: crate::name::Receiver::classify(&list_struct_name),
                    trait_name: None,
                    base_trait_name: None,
                    base_trait_module: None,
                    trait_type_args: vec![],
                    method_name: "push".to_string(),
                    method_type_args: vec![],
                    is_type_param_receiver: false,
                    is_ref_impl: false,
                    cm_name: None,
                }),
            },
            vec![],
            vec![CallArg::new(
                local_ref(elem_idx, "elem", elem_type_id),
                false,
            )],
        ),
        TypeTable::UNIT,
        synth_span(),
    );
    loop_body_stmts.push(expr_stmt(push_call));

    // i += 1
    let increment = assign(
        local_ref(i_idx, "i", TypeTable::I32),
        binary_add(local_ref(i_idx, "i", TypeTable::I32), i32_const(1)),
    );
    loop_body_stmts.push(expr_stmt(increment));

    stmts.push(loop_stmt(TirBlock {
        stmts: loop_body_stmts,
        span: synth_span(),
    }));

    // Free buffer: realloc(ptr, byte_count, elem_align, 0)
    let free_call = builtin_call(
        "realloc",
        vec![
            local_ref(ptr_idx, "ptr", TypeTable::I32),
            local_ref(byte_count_idx, "byte_count", TypeTable::I32),
            i32_const(elem_align),
            i32_const(0),
        ],
        TypeTable::I32,
    );
    let freed_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("__freed".to_string()),
        TypeTable::I32,
        false,
    );
    stmts.push(let_stmt("__freed", freed_idx, TypeTable::I32, free_call));

    // return arr
    stmts.push(return_stmt(Some(local_ref(arr_idx, "arr", array_type_id))));

    TirFunction {
        module_source: ModuleSource::default(),
        name: func_name,
        visibility: crate::ast::Visibility::Private,
        is_export: false,
        is_async: false,
        type_params: vec![],
        impl_type_params: vec![],
        monomorph_info: None,
        method_info: None,
        params: vec![
            TirParam {
                name: "handle".to_string(),
                local_index: handle_idx,
                type_id: TypeTable::I32,
                is_mut: false,
                is_mut_ref: false,
                span: synth_span(),
            },
            TirParam {
                name: "max".to_string(),
                local_index: max_idx,
                type_id: TypeTable::I32,
                is_mut: false,
                is_mut_ref: false,
                span: synth_span(),
            },
        ],
        return_type: array_type_id,
        task_return_type: None,
        effects: vec![],
        stores: vec![],
        body: Some(TirBlock {
            stmts,
            span: synth_span(),
        }),
        span: synth_span(),
        local_count: next_local,
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
    }
}

/// Synthesize `__cm_stream_read_val_<mangle>(handle, max) -> List<T>` for a
/// value-payload element type, delegating the read loop to the shared
/// [`synthesize_stream_read_func`]. The canonical name and CM layout come from
/// the general `classify_stream_payload` / `cm_*_with_registry_scoped` path,
/// matching the `val-` naming the codegen builds for `CmStreamPayload::Value`.
fn synthesize_stream_read_value_func(elem_type_id: TypeId, ctx: &SynthCtx) -> TirFunction {
    let cm_interface_registry = ctx.cm_interface_registry;
    let type_table = ctx.type_table;
    let array_type_id = type_table.borrow_mut().make_list(elem_type_id);
    let (func_name, read_name, elem_inst_name, payload_ast, elem_size, elem_align) = {
        let tt = type_table.borrow();
        let func_name = stream_read_value_func_name(&tt, elem_type_id);
        let payload = crate::component_model::classify_stream_payload(&tt, elem_type_id);
        let read_name = CanonicalIntrinsic::StreamRead(payload).import_name();
        let elem_inst_name = tt.base_type_name(elem_type_id);
        let payload_ast = type_id_to_ast_type(elem_type_id, &tt, cm_interface_registry);
        let size = crate::component_model::cm_size_with_registry_scoped(
            &payload_ast,
            cm_interface_registry,
            Some("cli"),
        ) as i32;
        let align = crate::component_model::cm_align_with_registry_scoped(
            &payload_ast,
            cm_interface_registry,
            Some("cli"),
        ) as i32;
        (
            func_name,
            read_name,
            elem_inst_name,
            payload_ast,
            size,
            align,
        )
    };

    synthesize_stream_read_func(
        func_name,
        read_name,
        &elem_inst_name,
        elem_type_id,
        array_type_id,
        elem_size,
        elem_align,
        &payload_ast,
        "cli",
        cm_interface_registry,
        type_table,
        ctx.interner,
    )
}

/// Dispatch target for a rewritten CM resource method call.
#[derive(Clone, Copy)]
enum BindingTarget {
    /// Direct `CmRawCall` to the canonical Wasm import (simple operations).
    Raw,
    /// Call to an `internal.wado` binding function (complex operations).
    Internal,
    /// Call to a synthesized binding function in the entry module.
    Entry,
}

/// Determine the binding target and function name for a CM resource method,
/// or `None` if not handled here (falls through to WIR translate).
fn cm_binding_function(cm_name: &str) -> Option<(BindingTarget, &'static str)> {
    use BindingTarget::{Internal, Raw};
    match cm_name {
        // Simple drops → direct CmRawCall (non-parameterized)
        "stream-drop-readable" => Some((Raw, "stream-drop-readable")),
        "stream-drop-writable" => Some((Raw, "stream-drop-writable")),
        "waitable-set-drop" => Some((Raw, "waitable-set-drop")),
        "subtask-drop" => Some((Raw, "subtask-drop")),
        "error-context-drop" => Some((Raw, "error-context-drop")),

        // Simple cancel → direct CmRawCall (non-parameterized)
        "stream-cancel-read" => Some((Raw, "stream-cancel-read")),
        "stream-cancel-write" => Some((Raw, "stream-cancel-write")),
        "subtask-cancel" => Some((Raw, "subtask-cancel")),

        // Future drops/cancels are parameterized by payload type — leave for WIR translate

        // waitable-join: void canonical, returns the handle as Waitable
        "waitable-join" => Some((Internal, "cm_waitable_join")),

        // Simple constructors → direct CmRawCall (returns i32 handle)
        "waitable-set-new" => Some((Raw, "waitable-set-new")),

        // Complex operations → internal binding functions
        "stream-read" => Some((Internal, "cm_stream_read_u8")),
        "stream-write" => Some((Internal, "cm_stream_write_u8")),
        "stream-write-raw" => Some((Internal, "cm_stream_write_raw_u8")),
        "error-context-new" => Some((Internal, "cm_error_context_new")),
        "error-context-debug-message" => Some((Internal, "cm_error_context_debug_message")),
        "waitable-set-wait" => Some((Internal, "cm_waitable_set_wait")),
        "waitable-set-poll" => Some((Internal, "cm_waitable_set_poll")),

        _ => None,
    }
}

/// Rewrite all #[cm("...")] resource method calls in the project.
pub(super) fn rewrite_cm_resource_methods(project: &mut Package) {
    let entry_source = project.entry_module_source.clone();
    let cm_interface_registry = &project.cm_interface_registry;
    for module in project.tir_modules.values() {
        let type_table = module.type_table.clone();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                rewrite_cm_methods_in_block(
                    body,
                    &type_table.borrow(),
                    &entry_source,
                    cm_interface_registry,
                );
            }
        }
    }
}

fn rewrite_cm_methods_in_block(
    block: &mut TirBlock,
    tt: &TypeTable,
    entry_source: &ModuleSource,
    cm_interface_registry: &CmInterfaceRegistry,
) {
    CmMethodRewriter {
        tt,
        entry_source,
        cm_interface_registry,
    }
    .visit_block(block);
}

/// Rewrites `#[cm("...")]` resource method calls into the appropriate
/// raw / internal / entry-module call. Detection runs post-order — after the
/// exhaustive `TirMutVisitor` walk has rewritten any nested calls — matching
/// the previous hand-written walker's "recurse first, then rewrite" order.
struct CmMethodRewriter<'a> {
    tt: &'a TypeTable,
    entry_source: &'a ModuleSource,
    cm_interface_registry: &'a CmInterfaceRegistry,
}

impl TirMutVisitor for CmMethodRewriter<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, type_id, .. } => {
                let old_type = value.type_id;
                self.visit_expr(value);
                // A streaming rewrite can retype the let value (e.g. to i32);
                // keep the binding's recorded type in sync.
                if value.type_id != old_type {
                    *type_id = value.type_id;
                }
            }
            // `TaskReturn` is normally stripped before this pass; descend into
            // its value defensively rather than tripping the walk's guard.
            TirStmtKind::TaskReturn { value } => self.visit_expr(value),
            _ => self.walk_stmt(stmt),
        }
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        // Recurse first so nested CM calls are rewritten before this node.
        self.walk_expr(expr);

        let cm_name = match &expr.kind {
            TirExprKind::MethodCall { func, .. } | TirExprKind::Call { func, .. } => {
                func.method_info.as_ref().and_then(|m| m.cm_name.clone())
            }
            _ => return,
        };
        let Some(cm_name) = cm_name else {
            return;
        };

        // future-new / stream-new: emit the payload-parameterized canonical as a
        // `CmRawCall` (returns the packed i64) and pass it to the `core:rt` pair
        // splitter, which casts the two halves to the readable/writable handles.
        if matches!(cm_name.as_str(), "future-new" | "stream-new") {
            rewrite_cm_new(expr, self.tt, cm_name == "future-new");
            return;
        }
        if cm_name == "future-write"
            && let Some(payload_type_id) = future_write_payload(self.tt, expr)
        {
            let func_name = future_write_func_name(self.tt, payload_type_id);
            rewrite_cm_instance_method(expr, BindingTarget::Entry, &func_name, self.entry_source);
            return;
        }
        // Scalar / structural stream writes call a generated per-element binding;
        // `u8` and record streams fall through to the existing paths.
        if cm_name == "stream-write"
            && let Some(elem_type_id) = stream_write_value_element(self.tt, expr)
        {
            let func_name = stream_write_func_name(self.tt, elem_type_id);
            rewrite_cm_instance_method(expr, BindingTarget::Entry, &func_name, self.entry_source);
            return;
        }
        // Future reads call a generated per-payload binding function.
        if cm_name == "future-read" {
            if let Some((payload_type_id, _)) = future_read_payload(self.tt, expr) {
                let func_name = future_read_func_name(self.tt, payload_type_id);
                rewrite_cm_instance_method(
                    expr,
                    BindingTarget::Entry,
                    &func_name,
                    self.entry_source,
                );
            }
            return;
        }
        // Value-payload stream reads call a generated per-element binding;
        // WASI-record reads fall through to the `__cm_stream_read_<record>` path.
        if let Some(elem_type_id) = stream_read_value_element(self.tt, expr) {
            let func_name = stream_read_value_func_name(self.tt, elem_type_id);
            rewrite_cm_instance_method(expr, BindingTarget::Entry, &func_name, self.entry_source);
            return;
        }
        // WASI-record stream reads call a generated binding function.
        if cm_name == "stream-read" {
            if let Some((elem_type_id, _)) = record_stream_read_element(self.tt, expr) {
                let elem_name = self.tt.base_type_name(elem_type_id);
                let func_name = record_stream_read_func_name(&elem_name);
                rewrite_cm_instance_method(
                    expr,
                    BindingTarget::Entry,
                    &func_name,
                    self.entry_source,
                );
                return;
            }
            if !is_u8_array_type(expr.type_id, self.tt) {
                return;
            }
        }
        // Stream ops on non-u8 types: parameterize the canonical name and
        // rewrite as a CmRawCall directly (the name is dynamic).
        if is_stream_cm_method(&cm_name) {
            let parameterized =
                parameterize_stream_cm_name(&cm_name, expr, self.tt, self.cm_interface_registry);
            if parameterized != cm_name {
                rewrite_cm_instance_method(
                    expr,
                    BindingTarget::Raw,
                    &parameterized,
                    self.entry_source,
                );
                return;
            }
        }
        // Future drop / cancel: parameterize the canonical name by the future's
        // payload and rewrite as a raw CmRawCall (mirrors the stream path above).
        if let Some(parameterized) = parameterize_future_cm_name(&cm_name, expr, self.tt) {
            rewrite_cm_instance_method(expr, BindingTarget::Raw, &parameterized, self.entry_source);
            return;
        }
        // Look up the binding function for everything else.
        let Some((target, func_name)) = cm_binding_function(&cm_name) else {
            // Not handled by synthesis yet — falls through to WIR translate.
            return;
        };
        match &mut expr.kind {
            TirExprKind::MethodCall { .. } => {
                rewrite_cm_instance_method(expr, target, func_name, self.entry_source);
            }
            TirExprKind::Call { .. } => {
                rewrite_cm_static_method(expr, target, func_name, self.entry_source);
            }
            _ => {}
        }
    }
}

/// Rewrite a CM instance method call (receiver.method(args)) to a builtin/internal call.
/// The receiver is cast to i32 (resource handle) and passed as the first argument.
fn rewrite_cm_instance_method(
    expr: &mut TirExpr,
    target: BindingTarget,
    func_name: &str,
    entry_source: &ModuleSource,
) {
    let TirExprKind::MethodCall { receiver, args, .. } = &mut expr.kind else {
        return;
    };

    // Take ownership of receiver and args
    let taken_receiver = std::mem::replace(
        receiver.as_mut(),
        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
    );
    let taken_args: Vec<TirExpr> = std::mem::take(args).into_iter().map(|a| a.expr).collect();

    // Cast receiver to i32 (resource handle)
    let handle = cast(taken_receiver, TypeTable::I32);

    // Build argument list: handle first, then the rest
    let mut all_args = vec![handle];
    all_args.extend(taken_args);

    // Create the replacement call
    let new_expr = match target {
        BindingTarget::Raw => cm_raw_call(func_name, all_args, expr.type_id),
        BindingTarget::Internal => internal_call(func_name, all_args, expr.type_id),
        BindingTarget::Entry => entry_call(func_name, all_args, expr.type_id, entry_source.clone()),
    };

    *expr = new_expr;
}

/// Rewrite a CM static method call (`Type::method(args)`) to a raw/internal call.
fn rewrite_cm_static_method(
    expr: &mut TirExpr,
    target: BindingTarget,
    func_name: &str,
    entry_source: &ModuleSource,
) {
    let TirExprKind::Call { args, .. } = &mut expr.kind else {
        return;
    };

    let taken_args: Vec<TirExpr> = std::mem::take(args).into_iter().map(|a| a.expr).collect();

    let new_expr = match target {
        BindingTarget::Raw => cm_raw_call(func_name, taken_args, expr.type_id),
        BindingTarget::Internal => internal_call(func_name, taken_args, expr.type_id),
        BindingTarget::Entry => {
            entry_call(func_name, taken_args, expr.type_id, entry_source.clone())
        }
    };

    *expr = new_expr;
}

/// Check if a CM method name is a stream operation.
fn is_stream_cm_method(cm_name: &str) -> bool {
    matches!(
        cm_name,
        "stream-drop-readable"
            | "stream-drop-writable"
            | "stream-cancel-read"
            | "stream-cancel-write"
    )
}

/// Rewrite a `Future::<T>::new()` / `Stream::<T>::new()` static call into
/// `rt::cm_{future,stream}_pair::<T>(<canonical-new CmRawCall>)`. The canonical
/// name is parameterized by the payload (computed here from the call's concrete
/// type arg); the i64 handle split lives in `core:rt`.
fn rewrite_cm_new(expr: &mut TirExpr, tt: &TypeTable, is_future: bool) {
    let TirExprKind::Call { func, .. } = &expr.kind else {
        return;
    };
    let Some(payload_tid) = func
        .monomorph_info
        .as_ref()
        .and_then(|m| m.impl_type_args.first().copied())
    else {
        return;
    };
    let (canonical_name, helper) = if is_future {
        let payload = crate::component_model::classify_future_payload(tt, payload_tid);
        (
            CanonicalIntrinsic::FutureNew(payload).import_name(),
            "cm_future_pair",
        )
    } else {
        let payload = crate::component_model::classify_stream_payload(tt, payload_tid);
        (
            CanonicalIntrinsic::StreamNew(payload).import_name(),
            "cm_stream_pair",
        )
    };
    let result_type = expr.type_id;
    let packed = cm_raw_call(&canonical_name, vec![], TypeTable::I64);
    *expr = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::rt(),
                name: helper.to_string(),
                monomorph_info: Some(MonomorphInfo {
                    generic_name: helper.to_string(),
                    impl_type_args: vec![payload_tid],
                    method_type_args: vec![],
                    is_blanket: false,
                }),
                method_info: None,
            },
            type_args: vec![payload_tid],
            args: vec![CallArg::new(packed, false)],
        },
        result_type,
        synth_span(),
    );
}

/// Parameterize a future drop / cancel canonical name by the receiver's payload
/// (`future-drop-readable` → `future-drop-readable:val-point`, …), matching the
/// `import_name` codegen builds. Returns `None` if `cm_name` is not a future
/// drop/cancel op or the receiver is not a `Future<T>` / `FutureWritable<T>`
/// method call.
fn parameterize_future_cm_name(cm_name: &str, expr: &TirExpr, tt: &TypeTable) -> Option<String> {
    // Match the op first so the canonical-name list lives in exactly one place
    // and non-future-handle methods skip the receiver classification below.
    let make = match cm_name {
        "future-drop-readable" => CanonicalIntrinsic::FutureDropReadable,
        "future-drop-writable" => CanonicalIntrinsic::FutureDropWritable,
        "future-cancel-read" => CanonicalIntrinsic::FutureCancelRead,
        "future-cancel-write" => CanonicalIntrinsic::FutureCancelWrite,
        _ => return None,
    };
    let TirExprKind::MethodCall { receiver, .. } = &expr.kind else {
        return None;
    };
    let mut type_id = receiver.type_id;
    while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = tt.get(type_id) {
        type_id = *inner;
    }
    let payload_tid = *tt.generic_type_args(type_id)?.first()?;
    let payload = crate::component_model::classify_future_payload(tt, payload_tid);
    Some(make(payload).import_name())
}

/// Parameterize a stream CM name based on the receiver type.
/// For non-u8 streams (e.g., `Stream<DirectoryEntry>`), appends the CM record name
/// (e.g., "stream-drop-readable:directory-entry").
///
/// Prefers the canonical CM name registered via `#[cm("…")]` so that
/// non-mechanical mappings (e.g., `DNSRecord` → `dns-record`) and
/// preserved acronyms aren't mangled. Falls back to a `PascalCase` →
/// kebab-case conversion only for receiver element types not in the
/// registry (user-authored streams).
fn parameterize_stream_cm_name(
    cm_name: &str,
    expr: &TirExpr,
    tt: &TypeTable,
    cm_interface_registry: &CmInterfaceRegistry,
) -> String {
    // Get the receiver's type from the method call
    let receiver_type_id = match &expr.kind {
        TirExprKind::MethodCall { receiver, .. } => receiver.type_id,
        _ => return cm_name.to_string(),
    };
    // Resolve through references: &Stream<T> → Stream<T>
    let mut type_id = receiver_type_id;
    while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = tt.get(type_id) {
        type_id = *inner;
    }
    // Extract element type from Stream<T>
    if let Some(type_args) = tt.generic_type_args(type_id)
        && let Some(&elem) = type_args.first()
    {
        let elem_name = tt.base_type_name(elem);
        if elem_name != "u8" {
            // Scalar / structural elements use the general `val-` payload name,
            // matching `CmStreamPayload::Value`; named records keep their CM name.
            if let Some(payload) = crate::component_model::cm_payload_type_from_type_id(tt, elem) {
                return format!("{cm_name}:val-{}", payload.name_suffix());
            }
            // The element's declaring interface keys the CM-name lookup. Its
            // `module_source` is the loader identity (a `.wado` path); the
            // registry bridges it to the versioned `#[cm(...)]` key.
            let elem_source = match tt.get(tt.get_ultimate_base_type(elem)) {
                ResolvedType::Struct { module_source, .. }
                | ResolvedType::Variant { module_source, .. }
                | ResolvedType::Enum { module_source, .. }
                | ResolvedType::Flags { module_source, .. }
                | ResolvedType::Resource { module_source, .. } => Some(module_source.to_string()),
                _ => None,
            };
            let cm_elem = elem_source
                .as_deref()
                .and_then(|source| registered_cm_name(&elem_name, source, cm_interface_registry))
                .unwrap_or_else(|| pascal_to_kebab(&elem_name));
            return format!("{cm_name}:{cm_elem}");
        }
    }
    cm_name.to_string()
}

/// Look up the canonical `#[cm("…")]` CM name for a Wado type declared in the
/// interface identified by the coarse `module_source` string, across the
/// registry's stream-eligible categories (struct, resource, variant, enum,
/// flags). Returns `None` for an unregistered name.
fn registered_cm_name(
    name: &str,
    module_source: &str,
    registry: &CmInterfaceRegistry,
) -> Option<String> {
    registry
        .get_struct_cm_name_by_module(module_source, name)
        .or_else(|| registry.get_resource_cm_name_by_module(module_source, name))
        .or_else(|| registry.get_variant_cm_name_by_module(module_source, name))
        .or_else(|| registry.get_enum_cm_name_by_module(module_source, name))
        .or_else(|| registry.get_flags_cm_name_by_module(module_source, name))
        .map(str::to_string)
}

/// Mechanical `PascalCase` → kebab-case fallback for stream element types
/// not registered with a `#[cm("…")]` name (i.e., user-authored types).
fn pascal_to_kebab(name: &str) -> String {
    name.chars().fold(String::new(), |mut s, c| {
        if c.is_uppercase() && !s.is_empty() {
            s.push('-');
        }
        s.push(c.to_ascii_lowercase());
        s
    })
}

/// Check if a `TypeId` represents `List<u8>`, structurally — never via the
/// rendered type name, which a stdlib rename or newtype alias would break.
fn is_u8_array_type(type_id: TypeId, tt: &TypeTable) -> bool {
    tt.as_list(type_id).is_some_and(|elem| {
        matches!(
            tt.get(elem),
            ResolvedType::Primitive(crate::tir::PrimitiveType::U8)
        )
    })
}
