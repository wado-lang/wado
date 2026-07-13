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
    assign, binary, break_stmt, builtin_call, cast, cm_raw_call, entry_call, expr_stmt, i32_const,
    if_stmt, internal_call, let_mut_stmt, let_stmt, local_ref, loop_stmt, option_none, option_some,
    return_stmt, synth_span,
};
use crate::wir::{CanonicalIntrinsic, CmFuturePayload, CmStreamPayload};

use super::synthesize_lift;
use super::types::{LiftContext, binary_add, type_id_to_ast_type};

/// Generate binding functions for Stream<T>.`read()` where T is a non-u8 WASI record type.
///
/// For each unique stream element type T found in stream-read calls, generates a
/// TIR function `__cm_stream_read_<T>` that:
/// 1. Calls `cm_stream_read_raw(handle, max, elem_size, elem_align)` to get raw buffer
/// 2. Loops through the buffer, lifting each record from linear memory
/// 3. Constructs `List<T>` and returns it
///
/// The generated functions are added to the entry module so they can be called
/// by the CM resource method rewriter.
pub(super) fn synthesize_record_stream_reads(project: &mut Package) {
    let cm_interface_registry = &project.cm_interface_registry;
    // Find all non-u8 stream-read element types
    let mut needed_element_types: IndexMap<String, (TypeId, TypeId)> = IndexMap::default();
    for module in project.tir_modules.values() {
        let tt = module.type_table.borrow();
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                find_record_stream_reads(body, &tt, &mut needed_element_types);
            }
        }
    }
    if needed_element_types.is_empty() {
        return;
    }

    // Generate binding functions for each element type.
    // Use the actual entry module — not `values().next()`, which returns the
    // first module in the IndexMap and is not guaranteed to be the entry module.
    // Calls synthesized by `rewrite_cm_resource_methods` target the entry module
    // via `entry_call`, so the binding functions must live there for resolution
    // to succeed in wir_build.
    let entry_source = project.entry_module_source.clone();
    let entry_module = project
        .tir_modules
        .get(&entry_source)
        .expect("entry module must exist in tir_modules");
    let type_table = entry_module.type_table.clone();
    let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();

    for (elem_name, (elem_type_id, array_type_id)) in &needed_element_types {
        // Stream-record element types come from `find_record_stream_reads`,
        // which only produces WASI record names. Resolve the name to its
        // defining `wasi:*` interface and then fetch fields strictly.
        let Some(source) = cm_interface_registry.find_wasi_struct_source(elem_name) else {
            continue;
        };
        let source = source.to_string();
        let Some(fields) = cm_interface_registry.get_struct_fields_by_source(&source, elem_name)
        else {
            continue;
        };
        let ast_type = Type::Named(NamedType {
            id: AstId::fresh(),
            name: elem_name.clone(),
            span: synth_span(),
            source_interface: Some(source.clone()),
        });
        let elem_size =
            crate::component_model::cm_size_with_registry(&ast_type, cm_interface_registry) as i32;
        let elem_align =
            crate::component_model::cm_align_with_registry(&ast_type, cm_interface_registry) as i32;

        let cm_record_name = cm_interface_registry
            .get_struct_cm_name_by_source(&source, elem_name)
            .unwrap_or(elem_name)
            .to_string();
        let _ = fields;
        let func = synthesize_stream_read_func(
            format!("__cm_stream_read_{elem_name}"),
            format!("stream-read:{cm_record_name}"),
            elem_name,
            *elem_type_id,
            *array_type_id,
            elem_size,
            elem_align,
            &ast_type,
            "filesystem",
            cm_interface_registry,
            &type_table,
            &project.interner,
        );
        new_functions.push(Rc::new(RefCell::new(func)));
    }

    let entry_module = project
        .tir_modules
        .get_mut(&entry_source)
        .expect("entry module must exist in tir_modules");
    for func in new_functions {
        entry_module.functions.push(func);
    }
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
    let cm_interface_registry = &project.cm_interface_registry;
    // payload mangle -> (payload_type_id, option_type_id)
    let mut needed: IndexMap<String, (TypeId, TypeId)> = IndexMap::default();
    for module in project.tir_modules.values() {
        let tt = module.type_table.borrow();
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                FutureReadFinder {
                    tt: &tt,
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

    let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
    for (_, (payload_type_id, option_type_id)) in &needed {
        let func = synthesize_future_read_func(
            *payload_type_id,
            *option_type_id,
            cm_interface_registry,
            &type_table,
            &project.interner,
        );
        new_functions.push(Rc::new(RefCell::new(func)));
    }

    let entry_module = project
        .tir_modules
        .get_mut(&entry_source)
        .expect("entry module must exist in tir_modules");
    for func in new_functions {
        entry_module.functions.push(func);
    }
}

/// Generate the per-payload `FutureWritable<T>::write()` binding functions for
/// general (aggregate) payloads, mirroring [`synthesize_future_reads`].
///
/// For each distinct aggregate payload `T` written, synthesizes
/// `__cm_future_write_<mangle(T)>(handle: i32, value: T)` which allocates a CM
/// buffer (sized via `cm_abi`), lowers `value` into it with the shared
/// `synthesize_lower_wasi_type_to_memory`, and calls the payload-parameterized
/// `future-write` canonical. As with the scalar WIR path, the buffer is
/// intentionally left alive: the `async` canonical option returns BLOCKED until
/// the reader copies the value, so freeing here (or waiting) would be wrong.
pub(super) fn synthesize_future_writes(project: &mut Package) {
    let cm_interface_registry = &project.cm_interface_registry;
    // payload mangle -> payload_type_id
    let mut needed: IndexMap<String, TypeId> = IndexMap::default();
    for module in project.tir_modules.values() {
        let tt = module.type_table.borrow();
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                FutureWriteFinder {
                    tt: &tt,
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

    let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
    for (_, payload_type_id) in &needed {
        let func =
            synthesize_future_write_func(*payload_type_id, cm_interface_registry, &type_table);
        new_functions.push(Rc::new(RefCell::new(func)));
    }

    let entry_module = project
        .tir_modules
        .get_mut(&entry_source)
        .expect("entry module must exist in tir_modules");
    for func in new_functions {
        entry_module.functions.push(func);
    }
}

/// The future-write payload type for a value `future-write` method call, or
/// `None` if it is not a future-write or its payload is a transmission shape
/// (`Result<(), E>`, `Result<Option<resource>, E>`) that the WIR-build path
/// still lowers directly. Covers scalar and aggregate payloads alike — both go
/// through the synthesized `__cm_future_write_<T>` helper.
fn future_write_value_payload(tt: &TypeTable, expr: &TirExpr) -> Option<TypeId> {
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
    let payload = *tt.generic_type_args(recv)?.first()?;
    crate::component_model::cm_payload_type_from_type_id(tt, payload)
        .is_some()
        .then_some(payload)
}

/// The `__cm_future_write_*` helper name for a payload type. Finder, rewriter,
/// and synthesizer must agree on this mangle.
fn future_write_func_name(tt: &TypeTable, payload_type_id: TypeId) -> String {
    format!(
        "__cm_future_write_{}",
        tt.mangle_type_arg_for_generic(payload_type_id)
    )
}

struct FutureWriteFinder<'a> {
    tt: &'a TypeTable,
    results: &'a mut IndexMap<String, TypeId>,
}

/// Generate per-element `StreamWritable<T>::write()` binding functions for
/// scalar / structural (non-`u8`, non-record) element types, mirroring
/// [`synthesize_future_writes`]. Each lowers a `List<T>` into a CM element
/// buffer (via `synthesize_lower_list_to_buffer`) and calls the
/// element-parameterized `stream-write` canonical, waiting for the reader on
/// BLOCKED (streams deliver element-by-element, so the buffer must survive the
/// wait — the function runs in an `async` task).
pub(super) fn synthesize_stream_writes(project: &mut Package) {
    let cm_interface_registry = &project.cm_interface_registry;
    let mut needed: IndexMap<String, TypeId> = IndexMap::default();
    for module in project.tir_modules.values() {
        let tt = module.type_table.borrow();
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                StreamWriteFinder {
                    tt: &tt,
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

    let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
    for (_, elem_type_id) in &needed {
        new_functions.push(Rc::new(RefCell::new(synthesize_stream_write_func(
            *elem_type_id,
            cm_interface_registry,
            &type_table,
        ))));
    }
    let entry_module = project
        .tir_modules
        .get_mut(&entry_source)
        .expect("entry module must exist in tir_modules");
    for func in new_functions {
        entry_module.functions.push(func);
    }
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

struct StreamWriteFinder<'a> {
    tt: &'a TypeTable,
    results: &'a mut IndexMap<String, TypeId>,
}

impl TirRefVisitor for StreamWriteFinder<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let Some(elem) = stream_write_value_element(self.tt, expr) {
            let name = stream_write_func_name(self.tt, elem);
            self.results.entry(name).or_insert(elem);
        }
        self.walk_expr(expr);
    }
}

/// Generate per-element `StreamReadable<T>::read()` binding functions for
/// scalar / structural (non-`u8`, non-WASI-record) element types, mirroring
/// [`synthesize_stream_writes`]. Each reads up to `max` elements into a CM
/// buffer via the element-parameterized `stream-read` canonical, lifts each
/// element with the shared `synthesize_lift`, and returns `List<T>`. An empty
/// result signals EOF to the caller.
pub(super) fn synthesize_stream_reads(project: &mut Package) {
    let cm_interface_registry = &project.cm_interface_registry;
    let mut needed: IndexMap<String, TypeId> = IndexMap::default();
    for module in project.tir_modules.values() {
        let tt = module.type_table.borrow();
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                StreamReadValueFinder {
                    tt: &tt,
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

    let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
    for (_, elem_type_id) in &needed {
        new_functions.push(Rc::new(RefCell::new(synthesize_stream_read_value_func(
            *elem_type_id,
            cm_interface_registry,
            &type_table,
            &project.interner,
        ))));
    }
    let entry_module = project
        .tir_modules
        .get_mut(&entry_source)
        .expect("entry module must exist in tir_modules");
    for func in new_functions {
        entry_module.functions.push(func);
    }
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

/// The `__cm_stream_read_val_*` helper name for an element type. Finder,
/// rewriter, and synthesizer must agree on this mangle.
fn stream_read_value_func_name(tt: &TypeTable, elem_type_id: TypeId) -> String {
    format!(
        "__cm_stream_read_val_{}",
        tt.mangle_type_arg_for_generic(elem_type_id)
    )
}

struct StreamReadValueFinder<'a> {
    tt: &'a TypeTable,
    results: &'a mut IndexMap<String, TypeId>,
}

impl TirRefVisitor for StreamReadValueFinder<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let Some(elem) = stream_read_value_element(self.tt, expr) {
            let name = stream_read_value_func_name(self.tt, elem);
            self.results.entry(name).or_insert(elem);
        }
        self.walk_expr(expr);
    }
}

fn synthesize_stream_write_func(
    elem_type_id: TypeId,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &RefCell<TypeTable>,
) -> TirFunction {
    let list_type_id = type_table.borrow_mut().make_list(elem_type_id);
    let (func_name, write_name, elem_ast, elem_size, elem_align) = {
        let tt = type_table.borrow();
        let func_name = stream_write_func_name(&tt, elem_type_id);
        let payload = crate::component_model::classify_stream_payload(&tt, elem_type_id);
        let write_name = CanonicalIntrinsic::StreamWrite(payload).import_name();
        let elem_ast = type_id_to_ast_type(elem_type_id, &tt, cm_interface_registry);
        let size =
            crate::component_model::cm_size_with_registry(&elem_ast, cm_interface_registry) as i32;
        let align =
            crate::component_model::cm_align_with_registry(&elem_ast, cm_interface_registry) as i32;
        (func_name, write_name, elem_ast, size, align)
    };

    let mut next_local: u32 = 0;
    let mut locals: Vec<TirLocal> = Vec::new();
    let mut stmts: Vec<TirStmt> = Vec::new();

    let alloc = |next: &mut u32, locals: &mut Vec<TirLocal>, ty: TypeId, is_mut: bool| -> u32 {
        let idx = *next;
        *next += 1;
        locals.push(TirLocal::synth(*next, ty, is_mut));
        idx
    };

    // Params: handle (i32), data (List<T>).
    let handle_idx = alloc(&mut next_local, &mut locals, TypeTable::I32, false);
    let data_idx = alloc(&mut next_local, &mut locals, list_type_id, false);

    // Lower the list into a CM element buffer: (ptr, element count).
    let (lower_stmts, ptr_local, count_local) = super::lower::synthesize_lower_list_to_buffer(
        &elem_ast,
        local_ref(data_idx, "data", list_type_id),
        &mut next_local,
        &mut locals,
        cm_interface_registry,
        "cli",
        type_table,
    );
    stmts.extend(lower_stmts);

    // A reader may take fewer elements than offered, so `stream.write` can copy
    // only a prefix and report COMPLETED. Loop, advancing past the elements
    // already written, until the whole buffer is sent or the reader drops.
    let offset_idx = alloc(&mut next_local, &mut locals, TypeTable::I32, true);
    stmts.push(let_mut_stmt(
        "offset",
        offset_idx,
        TypeTable::I32,
        i32_const(0),
    ));
    let result_idx = alloc(&mut next_local, &mut locals, TypeTable::I32, true);
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
            // if result == -1 { result = wait_for_blocked(handle) }
            if_stmt(
                binary(
                    TirBinaryOp::Eq,
                    result_ref(),
                    i32_const(-1),
                    TypeTable::BOOL,
                ),
                TirBlock {
                    stmts: vec![expr_stmt(assign(
                        result_ref(),
                        internal_call(
                            "wait_for_blocked",
                            vec![local_ref(handle_idx, "handle", TypeTable::I32)],
                            TypeTable::I32,
                        ),
                    ))],
                    span: synth_span(),
                },
                None,
            ),
            // offset += result >> 4   (the copied-element count)
            expr_stmt(assign(
                offset_ref(),
                binary(
                    TirBinaryOp::Add,
                    offset_ref(),
                    binary(TirBinaryOp::Shr, result_ref(), i32_const(4), TypeTable::I32),
                    TypeTable::I32,
                ),
            )),
            // if (result >> 4) == 0 || (result & 0xf) != 0 { break }
            // No progress, or the reader/stream closed (DROPPED/CANCELLED).
            if_stmt(
                binary(
                    TirBinaryOp::Or,
                    binary(
                        TirBinaryOp::Eq,
                        binary(TirBinaryOp::Shr, result_ref(), i32_const(4), TypeTable::I32),
                        i32_const(0),
                        TypeTable::BOOL,
                    ),
                    binary(
                        TirBinaryOp::NotEq,
                        binary(
                            TirBinaryOp::BitAnd,
                            result_ref(),
                            i32_const(0xf),
                            TypeTable::I32,
                        ),
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
    let freed_idx = alloc(&mut next_local, &mut locals, TypeTable::I32, false);
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

impl TirRefVisitor for FutureWriteFinder<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let Some(payload_type_id) = future_write_value_payload(self.tt, expr) {
            let name = future_write_func_name(self.tt, payload_type_id);
            self.results.entry(name).or_insert(payload_type_id);
        }
        self.walk_expr(expr);
    }
}

fn synthesize_future_write_func(
    payload_type_id: TypeId,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &RefCell<TypeTable>,
) -> TirFunction {
    let (func_name, write_name, cm_package, payload_ast, size, align) = {
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
        (func_name, write_name, cm_package, payload_ast, size, align)
    };

    let mut next_local: u32 = 0;
    let mut locals: Vec<TirLocal> = Vec::new();
    let mut stmts: Vec<TirStmt> = Vec::new();

    let alloc = |next: &mut u32, locals: &mut Vec<TirLocal>, ty: TypeId, is_mut: bool| -> u32 {
        let idx = *next;
        *next += 1;
        locals.push(TirLocal::synth(*next, ty, is_mut));
        idx
    };

    // Params: handle (i32), value (T).
    let handle_idx = alloc(&mut next_local, &mut locals, TypeTable::I32, false);
    let value_idx = alloc(&mut next_local, &mut locals, payload_type_id, false);

    // let ptr = realloc(0, 0, align, size)
    let ptr_idx = alloc(&mut next_local, &mut locals, TypeTable::I32, false);
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
    stmts.extend(super::lower::synthesize_lower_wasi_type_to_memory(
        &payload_ast,
        local_ref(value_idx, "value", payload_type_id),
        local_ref(ptr_idx, "ptr", TypeTable::I32),
        &mut next_local,
        &mut locals,
        cm_interface_registry,
        &cm_package,
        type_table,
    ));

    // let __written = future-write:<payload>(handle, ptr)
    // The result is intentionally ignored and the buffer left alive (see above).
    let written_idx = alloc(&mut next_local, &mut locals, TypeTable::I32, false);
    stmts.push(let_stmt(
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

struct FutureReadFinder<'a> {
    tt: &'a TypeTable,
    results: &'a mut IndexMap<String, (TypeId, TypeId)>,
}

impl TirRefVisitor for FutureReadFinder<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        let cm_name = match &expr.kind {
            TirExprKind::MethodCall { func, .. } | TirExprKind::Call { func, .. } => {
                func.method_info.as_ref().and_then(|m| m.cm_name.clone())
            }
            _ => None,
        };
        if cm_name.as_deref() == Some("future-read")
            && let Some(type_args) = self.tt.generic_type_args(expr.type_id)
            && let Some(&payload_type_id) = type_args.first()
        {
            let name = future_read_func_name(self.tt, payload_type_id);
            self.results
                .entry(name)
                .or_insert((payload_type_id, expr.type_id));
        }
        self.walk_expr(expr);
    }
}

/// The `__cm_future_read_*` helper name for a payload type. Finder, rewriter,
/// and synthesizer must agree on this mangle.
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
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &RefCell<TypeTable>,
    interner: &RefCell<ModuleSourceInterner>,
) -> TirFunction {
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

    let alloc = |next: &mut u32, locals: &mut Vec<TirLocal>, ty: TypeId, is_mut: bool| -> u32 {
        let idx = *next;
        *next += 1;
        locals.push(TirLocal::synth(*next, ty, is_mut));
        idx
    };

    // Param: handle (i32).
    let handle_idx = alloc(&mut next_local, &mut locals, TypeTable::I32, false);

    // let ptr = realloc(0, 0, align, size)
    let ptr_idx = alloc(&mut next_local, &mut locals, TypeTable::I32, false);
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
    let status_idx = alloc(&mut next_local, &mut locals, TypeTable::I32, true);
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

    // if status == -1 { status = future_await_blocked(handle); }
    stmts.push(if_stmt(
        binary(
            TirBinaryOp::Eq,
            local_ref(status_idx, "status", TypeTable::I32),
            i32_const(-1),
            TypeTable::BOOL,
        ),
        TirBlock {
            stmts: vec![expr_stmt(assign(
                local_ref(status_idx, "status", TypeTable::I32),
                internal_call(
                    "future_await_blocked",
                    vec![local_ref(handle_idx, "handle", TypeTable::I32)],
                    TypeTable::I32,
                ),
            ))],
            span: synth_span(),
        },
        None,
    ));

    // let mut result: Option<T> = None
    let result_idx = alloc(&mut next_local, &mut locals, option_type_id, true);
    let none_val = option_none(option_type_id, type_table.borrow().compiler_items());
    stmts.push(let_mut_stmt("result", result_idx, option_type_id, none_val));

    // if (status & 0xF) == 0 { result = Some(<lifted payload>); } else { /* None */ }
    let cond = binary(
        TirBinaryOp::Eq,
        binary(
            TirBinaryOp::BitAnd,
            local_ref(status_idx, "status", TypeTable::I32),
            i32_const(0xF),
            TypeTable::I32,
        ),
        i32_const(0),
        TypeTable::BOOL,
    );
    let mut some_stmts: Vec<TirStmt> = Vec::new();
    let lift_ctx = LiftContext {
        cm_interface_registry,
        type_table,
        cm_package: &cm_package,
        interner,
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
    let freed_idx = alloc(&mut next_local, &mut locals, TypeTable::I32, false);
    stmts.push(let_stmt(
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

/// Find all stream-read method calls that return List<T> where T is not u8.
///
/// Coverage must match `rewrite_cm_methods_in_expr`, which descends into every
/// container expression (if/match/block/binary/...) when rewriting a record
/// `stream-read` into a call to `__cm_stream_read_<T>`. A `TirRefVisitor` gives
/// that exhaustive traversal for free, so a read nested in (e.g.) an
/// `if`-expression branch is still discovered and its binding function
/// synthesized — otherwise the rewrite would target a function that was never
/// generated, producing an unresolved-call panic at WIR build.
fn find_record_stream_reads(
    block: &TirBlock,
    tt: &TypeTable,
    results: &mut IndexMap<String, (TypeId, TypeId)>,
) {
    let mut finder = RecordStreamReadFinder { tt, results };
    finder.visit_block(block);
}

struct RecordStreamReadFinder<'a> {
    tt: &'a TypeTable,
    results: &'a mut IndexMap<String, (TypeId, TypeId)>,
}

impl TirRefVisitor for RecordStreamReadFinder<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        // Check this node, then recurse via the exhaustive default walk.
        // `cm_name` is carried on both MethodCall and Call `method_info`,
        // mirroring the rewriter's extraction in `rewrite_cm_methods_in_expr`.
        let cm_name = match &expr.kind {
            TirExprKind::MethodCall { func, .. } | TirExprKind::Call { func, .. } => {
                func.method_info.as_ref().and_then(|m| m.cm_name.clone())
            }
            _ => None,
        };
        if cm_name.as_deref() == Some("stream-read") && !is_u8_array_type(expr.type_id, self.tt) {
            // Extract element type from List<T>
            if let Some(type_args) = self.tt.generic_type_args(expr.type_id)
                && let Some(&elem_type_id) = type_args.first()
                // Value-payload elements are handled by `synthesize_stream_reads`;
                // only true WASI records belong to this path.
                && !matches!(
                    crate::component_model::classify_stream_payload(self.tt, elem_type_id),
                    CmStreamPayload::Value(_)
                )
            {
                let elem_name = self.tt.base_type_name(elem_type_id);
                self.results
                    .entry(elem_name)
                    .or_insert((elem_type_id, expr.type_id));
            }
        }
        self.walk_expr(expr);
    }
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
    let _tuple_type_id = type_table
        .borrow_mut()
        .make_tuple(vec![TypeTable::I32, TypeTable::I32]);

    let mut next_local: u32 = 0;
    let mut locals: Vec<TirLocal> = Vec::new();
    let mut stmts: Vec<TirStmt> = Vec::new();

    // Params: handle (i32), max (i32)
    let handle_idx = next_local;
    next_local += 1;
    locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
    let max_idx = next_local;
    next_local += 1;
    locals.push(TirLocal::synth(next_local, TypeTable::I32, false));

    // let byte_count = max * elem_size
    let byte_count_idx = next_local;
    next_local += 1;
    locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
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
    let ptr_idx = next_local;
    next_local += 1;
    locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
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
    let result_idx = next_local;
    next_local += 1;
    locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
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

    // if result == -1 { result = wait_for_blocked(handle); }
    let blocked_check = binary(
        TirBinaryOp::Eq,
        local_ref(result_idx, "result", TypeTable::I32),
        i32_const(-1),
        TypeTable::BOOL,
    );
    let wait_call = internal_call(
        "wait_for_blocked",
        vec![local_ref(handle_idx, "handle", TypeTable::I32)],
        TypeTable::I32,
    );
    stmts.push(if_stmt(
        blocked_check,
        TirBlock {
            stmts: vec![expr_stmt(assign(
                local_ref(result_idx, "result", TypeTable::I32),
                wait_call,
            ))],
            span: synth_span(),
        },
        None,
    ));

    // let count = result >> 4
    let count_idx = next_local;
    next_local += 1;
    locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
    let count_expr = binary(
        TirBinaryOp::Shr,
        local_ref(result_idx, "result", TypeTable::I32),
        i32_const(4),
        TypeTable::I32,
    );
    stmts.push(let_stmt("count", count_idx, TypeTable::I32, count_expr));

    // let mut arr = List::<T>::with_capacity(count)
    // Use internal_from_raw with a new GC array
    // Actually, build the array by appending elements one by one
    let arr_idx = next_local;
    next_local += 1;
    locals.push(TirLocal::synth(next_local, array_type_id, false));

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
                    base_struct_name: list_struct_name.clone(),
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
    let i_idx = next_local;
    next_local += 1;
    locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
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
    let addr_idx = next_local;
    next_local += 1;
    locals.push(TirLocal::synth(next_local, TypeTable::I32, false));
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

    // Push to array - use List::push method pattern
    // arr.push(elem) → internal call
    let elem_idx = next_local;
    next_local += 1;
    locals.push(TirLocal::synth(next_local, elem_type_id, false));
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
                    base_struct_name: list_struct_name,
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
    stmts.push(let_stmt("__freed", next_local, TypeTable::I32, free_call));
    next_local += 1;
    locals.push(TirLocal::synth(next_local, TypeTable::I32, false));

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
fn synthesize_stream_read_value_func(
    elem_type_id: TypeId,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &RefCell<TypeTable>,
    interner: &RefCell<ModuleSourceInterner>,
) -> TirFunction {
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
        interner,
    )
}

/// Determine the internal binding function name for a CM resource method.
/// Returns `Some(("internal" | "builtin", function_name))` or `None` if not handled.
/// Maps CM method names to their adapter dispatch.
/// - `"raw"`: direct `CmRawCall` to canonical Wasm import (for simple void operations)
/// - `"internal"`: call to internal.wado binding function (for complex operations)
fn cm_binding_function(cm_name: &str) -> Option<(&'static str, &'static str)> {
    match cm_name {
        // Simple drops → direct CmRawCall (non-parameterized)
        "stream-drop-readable" => Some(("raw", "stream-drop-readable")),
        "stream-drop-writable" => Some(("raw", "stream-drop-writable")),
        "waitable-set-drop" => Some(("raw", "waitable-set-drop")),
        "subtask-drop" => Some(("raw", "subtask-drop")),
        "error-context-drop" => Some(("raw", "error-context-drop")),

        // Simple cancel → direct CmRawCall (non-parameterized)
        "stream-cancel-read" => Some(("raw", "stream-cancel-read")),
        "stream-cancel-write" => Some(("raw", "stream-cancel-write")),
        "subtask-cancel" => Some(("raw", "subtask-cancel")),

        // Future drops/cancels are parameterized by payload type — leave for WIR translate

        // waitable-join: void canonical, returns the handle as Waitable
        "waitable-join" => Some(("internal", "cm_waitable_join")),

        // Simple constructors → direct CmRawCall (returns i32 handle)
        "waitable-set-new" => Some(("raw", "waitable-set-new")),

        // Complex operations → internal binding functions
        "stream-read" => Some(("internal", "cm_stream_read_u8")),
        "stream-write" => Some(("internal", "cm_stream_write_u8")),
        "stream-write-raw" => Some(("internal", "cm_stream_write_raw_u8")),
        "error-context-new" => Some(("internal", "cm_error_context_new")),
        "error-context-debug-message" => Some(("internal", "cm_error_context_debug_message")),
        "waitable-set-wait" => Some(("internal", "cm_waitable_set_wait")),
        "waitable-set-poll" => Some(("internal", "cm_waitable_set_poll")),

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
        // Value future writes (scalar + aggregate) call a generated per-payload
        // binding function; only the transmission shapes (`Result<(), E>`,
        // `Result<Option<resource>, E>`) fall through to the WIR-build path.
        if cm_name == "future-write"
            && let Some(payload_type_id) = future_write_value_payload(self.tt, expr)
        {
            let func_name = future_write_func_name(self.tt, payload_type_id);
            rewrite_cm_instance_method(expr, "entry", &func_name, self.entry_source);
            return;
        }
        // Scalar / structural stream writes call a generated per-element binding;
        // `u8` and record streams fall through to the existing paths.
        if cm_name == "stream-write"
            && let Some(elem_type_id) = stream_write_value_element(self.tt, expr)
        {
            let func_name = stream_write_func_name(self.tt, elem_type_id);
            rewrite_cm_instance_method(expr, "entry", &func_name, self.entry_source);
            return;
        }
        // Future reads call a generated per-payload binding function.
        if cm_name == "future-read" {
            if let Some(type_args) = self.tt.generic_type_args(expr.type_id)
                && let Some(&payload_type_id) = type_args.first()
            {
                let func_name = future_read_func_name(self.tt, payload_type_id);
                rewrite_cm_instance_method(expr, "entry", &func_name, self.entry_source);
            }
            return;
        }
        // Value-payload stream reads call a generated per-element binding;
        // WASI-record reads fall through to the `__cm_stream_read_<record>` path.
        if let Some(elem_type_id) = stream_read_value_element(self.tt, expr) {
            let func_name = stream_read_value_func_name(self.tt, elem_type_id);
            rewrite_cm_instance_method(expr, "entry", &func_name, self.entry_source);
            return;
        }
        // Non-u8 (record) stream reads call a generated binding function.
        if cm_name == "stream-read" && !is_u8_array_type(expr.type_id, self.tt) {
            if let Some(type_args) = self.tt.generic_type_args(expr.type_id)
                && let Some(&elem_type_id) = type_args.first()
            {
                let elem_name = self.tt.base_type_name(elem_type_id);
                let func_name = format!("__cm_stream_read_{elem_name}");
                rewrite_cm_instance_method(expr, "entry", &func_name, self.entry_source);
            }
            return;
        }
        // Stream ops on non-u8 types: parameterize the canonical name and
        // rewrite as a CmRawCall directly (the name is dynamic).
        if is_stream_cm_method(&cm_name) {
            let parameterized =
                parameterize_stream_cm_name(&cm_name, expr, self.tt, self.cm_interface_registry);
            if parameterized != cm_name {
                rewrite_cm_instance_method(expr, "raw", &parameterized, self.entry_source);
                return;
            }
        }
        // Future drop / cancel: parameterize the canonical name by the future's
        // payload and rewrite as a raw CmRawCall (mirrors the stream path above).
        if let Some(parameterized) = parameterize_future_cm_name(&cm_name, expr, self.tt) {
            rewrite_cm_instance_method(expr, "raw", &parameterized, self.entry_source);
            return;
        }
        // Look up the binding function for everything else.
        let Some((kind, func_name)) = cm_binding_function(&cm_name) else {
            // Not handled by synthesis yet — falls through to WIR translate.
            return;
        };
        match &mut expr.kind {
            TirExprKind::MethodCall { .. } => {
                rewrite_cm_instance_method(expr, kind, func_name, self.entry_source);
            }
            TirExprKind::Call { .. } => {
                rewrite_cm_static_method(expr, kind, func_name, self.entry_source);
            }
            _ => {}
        }
    }
}

/// Rewrite a CM instance method call (receiver.method(args)) to a builtin/internal call.
/// The receiver is cast to i32 (resource handle) and passed as the first argument.
fn rewrite_cm_instance_method(
    expr: &mut TirExpr,
    kind: &str,
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
    let new_expr = match kind {
        "raw" => cm_raw_call(func_name, all_args, expr.type_id),
        "internal" => internal_call(func_name, all_args, expr.type_id),
        // "entry": call to a synthesized function in the entry module
        "entry" => entry_call(func_name, all_args, expr.type_id, entry_source.clone()),
        _ => unreachable!(),
    };

    *expr = new_expr;
}

/// Rewrite a CM static method call (`Type::method(args)`) to a raw/internal call.
fn rewrite_cm_static_method(
    expr: &mut TirExpr,
    kind: &str,
    func_name: &str,
    entry_source: &ModuleSource,
) {
    let TirExprKind::Call { args, .. } = &mut expr.kind else {
        return;
    };

    let taken_args: Vec<TirExpr> = std::mem::take(args).into_iter().map(|a| a.expr).collect();

    let new_expr = match kind {
        "raw" => cm_raw_call(func_name, taken_args, expr.type_id),
        "internal" => internal_call(func_name, taken_args, expr.type_id),
        "entry" => entry_call(func_name, taken_args, expr.type_id, entry_source.clone()),
        _ => unreachable!(),
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

/// Check if a `TypeId` represents `List<u8>`.
fn is_u8_array_type(type_id: TypeId, tt: &TypeTable) -> bool {
    let name = tt.type_name(type_id);
    name == "List<u8>"
}
