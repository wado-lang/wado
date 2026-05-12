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
use crate::component_model::WasiRegistry;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name::LocalMethodName;
use crate::package::Package;
use crate::tir::{
    CallArg, FunctionKind, FunctionRef, InlineHint, MonomorphInfo, ResolvedType, TirBinaryOp,
    TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal, TirParam, TirStmt, TirStmtKind,
    TirTemplatePart, TypeId, TypeTable,
};

use crate::synthesis::common::{
    assign, binary, break_stmt, builtin_call, cast, cm_raw_call, entry_call, expr_stmt, i32_const,
    if_stmt, internal_call, let_mut_stmt, let_stmt, local_ref, loop_stmt, return_stmt, synth_span,
};

use super::synthesize_lift;
use super::types::{LiftContext, binary_add};

/// Generate binding functions for Stream<T>.`read()` where T is a non-u8 WASI record type.
///
/// For each unique stream element type T found in stream-read calls, generates a
/// TIR function `__cm_stream_read_<T>` that:
/// 1. Calls `cm_stream_read_raw(handle, max, elem_size, elem_align)` to get raw buffer
/// 2. Loops through the buffer, lifting each record from linear memory
/// 3. Constructs `Array<T>` and returns it
///
/// The generated functions are added to the entry module so they can be called
/// by the CM resource method rewriter.
pub(super) fn synthesize_record_stream_reads(project: &mut Package) {
    let wasi_registry = project.wasi_registry;
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
        let Some(source) = wasi_registry.find_wasi_struct_source(elem_name) else {
            continue;
        };
        let source = source.to_string();
        let Some(fields) = wasi_registry.get_struct_fields_by_source(&source, elem_name) else {
            continue;
        };
        let ast_type = Type::Named(NamedType {
            id: AstId::fresh(),
            name: elem_name.clone(),
            span: synth_span(),
            source_interface: Some(source.clone()),
        });
        let elem_size =
            crate::component_model::cm_size_with_registry(&ast_type, wasi_registry) as i32;
        let elem_align =
            crate::component_model::cm_align_with_registry(&ast_type, wasi_registry) as i32;

        let func = synthesize_stream_read_func(
            elem_name,
            *elem_type_id,
            *array_type_id,
            fields,
            elem_size,
            elem_align,
            wasi_registry,
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

/// Find all stream-read method calls that return Array<T> where T is not u8.
fn find_record_stream_reads(
    block: &TirBlock,
    tt: &TypeTable,
    results: &mut IndexMap<String, (TypeId, TypeId)>,
) {
    for stmt in &block.stmts {
        find_record_stream_reads_in_stmt(stmt, tt, results);
    }
}

fn find_record_stream_reads_in_stmt(
    stmt: &TirStmt,
    tt: &TypeTable,
    results: &mut IndexMap<String, (TypeId, TypeId)>,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => find_record_stream_reads_in_expr(value, tt, results),
        TirStmtKind::Expr(value) => find_record_stream_reads_in_expr(value, tt, results),
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                find_record_stream_reads_in_expr(v, tt, results);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            find_record_stream_reads_in_expr(condition, tt, results);
            find_record_stream_reads(then_block, tt, results);
            if let Some(blk) = else_block {
                find_record_stream_reads(blk, tt, results);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            find_record_stream_reads(body, tt, results);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            find_record_stream_reads_in_expr(scrutinee, tt, results);
            find_record_stream_reads(then_block, tt, results);
            if let Some(blk) = else_block {
                find_record_stream_reads(blk, tt, results);
            }
        }
        _ => {}
    }
}

fn find_record_stream_reads_in_expr(
    expr: &TirExpr,
    tt: &TypeTable,
    results: &mut IndexMap<String, (TypeId, TypeId)>,
) {
    // Recurse into sub-expressions
    match &expr.kind {
        TirExprKind::MethodCall { receiver, args, .. } => {
            find_record_stream_reads_in_expr(receiver, tt, results);
            for arg in args {
                find_record_stream_reads_in_expr(&arg.expr, tt, results);
            }
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                find_record_stream_reads_in_expr(&arg.expr, tt, results);
            }
        }
        _ => {}
    }

    // Check if this is a stream-read call with non-u8 element type
    let cm_name = match &expr.kind {
        TirExprKind::MethodCall { func, .. } => {
            func.method_info.as_ref().and_then(|m| m.cm_name.clone())
        }
        _ => None,
    };
    if cm_name.as_deref() == Some("stream-read") && !is_u8_array_type(expr.type_id, tt) {
        // Extract element type from Array<T>
        if let Some(type_args) = tt.generic_type_args(expr.type_id)
            && let Some(&elem_type_id) = type_args.first()
        {
            let elem_name = tt.base_type_name(elem_type_id);
            results
                .entry(elem_name)
                .or_insert((elem_type_id, expr.type_id));
        }
    }
}

/// Generate a TIR function for reading records from a stream.
///
/// Generates `__cm_stream_read_<T>(handle: i32, max: i32) -> Array<T>`:
/// 1. Call `cm_stream_read_raw` to get raw buffer [ptr, count]
/// 2. Loop: lift each record from buffer at ptr + i * `elem_size`
/// 3. Append to result array
/// 4. Free buffer
/// 5. Return array
fn synthesize_stream_read_func(
    elem_name: &str,
    elem_type_id: TypeId,
    array_type_id: TypeId,
    _fields: &[(String, Type)],
    elem_size: i32,
    elem_align: i32,
    wasi_registry: &WasiRegistry,
    type_table: &RefCell<TypeTable>,
    interner: &RefCell<ModuleSourceInterner>,
) -> TirFunction {
    let func_name = format!("__cm_stream_read_{elem_name}");
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

    // Use the CM kebab-case name for the stream-read intrinsic
    let cm_record_name = wasi_registry
        .get_struct_cm_name(elem_name)
        .unwrap_or(elem_name)
        .to_string();
    let stream_read_name = format!("stream-read:{cm_record_name}");

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

    // let mut arr = Array::<T>::with_capacity(count)
    // Use internal_from_raw with a new GC array
    // Actually, build the array by appending elements one by one
    let arr_idx = next_local;
    next_local += 1;
    locals.push(TirLocal::synth(next_local, array_type_id, false));

    // Create empty array via Array<T>::with_capacity(count)
    let empty_arr = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::array(),
                name: format!("Array<{elem_name}>::with_capacity"),
                monomorph_info: Some(MonomorphInfo {
                    generic_name: "Array::with_capacity".to_string(),
                    impl_type_args: vec![elem_type_id],
                    method_type_args: vec![],
                    is_blanket: false,
                }),
                method_info: Some(LocalMethodName {
                    struct_name: format!("Array<{elem_name}>"),
                    base_struct_name: "Array".to_string(),
                    trait_name: None,
                    base_trait_name: None,
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

    // Lift each field from linear memory at addr + field_offset
    let lift_ctx = LiftContext {
        wasi_registry,
        type_table,
        cm_package: "filesystem",
        interner,
    };
    let ast_type = Type::Named(NamedType {
        id: AstId::fresh(),
        name: elem_name.to_string(),
        span: synth_span(),
        source_interface: None,
    });
    let lifted_elem = synthesize_lift(
        &ast_type,
        local_ref(addr_idx, "addr", TypeTable::I32),
        &mut next_local,
        &mut loop_body_stmts,
        &mut locals,
        &lift_ctx,
    );

    // Push to array - use Array::push method pattern
    // arr.push(elem) → internal call
    let elem_idx = next_local;
    next_local += 1;
    locals.push(TirLocal::synth(next_local, elem_type_id, false));
    loop_body_stmts.push(let_stmt("elem", elem_idx, elem_type_id, lifted_elem));

    let push_call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(local_ref(arr_idx, "arr", array_type_id)),
            FunctionRef {
                module_source: ModuleSource::array(),
                name: format!("Array<{elem_name}>::push"),
                monomorph_info: Some(MonomorphInfo {
                    generic_name: "Array::push".to_string(),
                    impl_type_args: vec![elem_type_id],
                    method_type_args: vec![],
                    is_blanket: false,
                }),
                method_info: Some(LocalMethodName {
                    struct_name: format!("Array<{elem_name}>"),
                    base_struct_name: "Array".to_string(),
                    trait_name: None,
                    base_trait_name: None,
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
        is_pub: false,
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
                span: synth_span(),
                default_expr: None,
            },
            TirParam {
                name: "max".to_string(),
                local_index: max_idx,
                type_id: TypeTable::I32,
                is_mut: false,
                span: synth_span(),
                default_expr: None,
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
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
    }
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
    let wasi_registry = project.wasi_registry;
    for module in project.tir_modules.values() {
        let type_table = module.type_table.clone();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                rewrite_cm_methods_in_block(
                    body,
                    &type_table.borrow(),
                    &entry_source,
                    wasi_registry,
                );
            }
        }
    }
}

fn rewrite_cm_methods_in_block(
    block: &mut TirBlock,
    tt: &TypeTable,
    entry_source: &ModuleSource,
    wasi_registry: &WasiRegistry,
) {
    for stmt in &mut block.stmts {
        rewrite_cm_methods_in_stmt(stmt, tt, entry_source, wasi_registry);
    }
}

fn rewrite_cm_methods_in_stmt(
    stmt: &mut TirStmt,
    tt: &TypeTable,
    entry_source: &ModuleSource,
    wasi_registry: &WasiRegistry,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, type_id, .. } => {
            let old_type = value.type_id;
            rewrite_cm_methods_in_expr(value, tt, entry_source, wasi_registry);
            if value.type_id != old_type {
                *type_id = value.type_id;
            }
        }
        TirStmtKind::Expr(value) => {
            rewrite_cm_methods_in_expr(value, tt, entry_source, wasi_registry);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                rewrite_cm_methods_in_expr(v, tt, entry_source, wasi_registry);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            rewrite_cm_methods_in_expr(condition, tt, entry_source, wasi_registry);
            rewrite_cm_methods_in_block(then_block, tt, entry_source, wasi_registry);
            if let Some(blk) = else_block {
                rewrite_cm_methods_in_block(blk, tt, entry_source, wasi_registry);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_cm_methods_in_block(body, tt, entry_source, wasi_registry);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            rewrite_cm_methods_in_expr(scrutinee, tt, entry_source, wasi_registry);
            rewrite_cm_methods_in_block(then_block, tt, entry_source, wasi_registry);
            if let Some(blk) = else_block {
                rewrite_cm_methods_in_block(blk, tt, entry_source, wasi_registry);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_cm_methods_in_expr(v, tt, entry_source, wasi_registry);
            }
        }
        TirStmtKind::LetDestructure { value, .. } => {
            rewrite_cm_methods_in_expr(value, tt, entry_source, wasi_registry);
        }
        TirStmtKind::Continue => {}
        TirStmtKind::TaskReturn { value } => {
            rewrite_cm_methods_in_expr(value, tt, entry_source, wasi_registry);
        }
        TirStmtKind::VariadicForOf { .. } => {}
    }
}

fn rewrite_cm_methods_in_expr(
    expr: &mut TirExpr,
    tt: &TypeTable,
    entry_source: &ModuleSource,
    wasi_registry: &WasiRegistry,
) {
    // First, recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::Call { args, .. } => {
            for arg in args.iter_mut() {
                rewrite_cm_methods_in_expr(&mut arg.expr, tt, entry_source, wasi_registry);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            rewrite_cm_methods_in_expr(receiver, tt, entry_source, wasi_registry);
            for arg in args.iter_mut() {
                rewrite_cm_methods_in_expr(&mut arg.expr, tt, entry_source, wasi_registry);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            rewrite_cm_methods_in_expr(left, tt, entry_source, wasi_registry);
            rewrite_cm_methods_in_expr(right, tt, entry_source, wasi_registry);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            rewrite_cm_methods_in_expr(inner, tt, entry_source, wasi_registry);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            rewrite_cm_methods_in_expr(inner, tt, entry_source, wasi_registry);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_cm_methods_in_expr(condition, tt, entry_source, wasi_registry);
            rewrite_cm_methods_in_block(then_branch, tt, entry_source, wasi_registry);
            if let Some(blk) = else_branch {
                rewrite_cm_methods_in_block(blk, tt, entry_source, wasi_registry);
            }
        }
        TirExprKind::Match { expr, arms } => {
            rewrite_cm_methods_in_expr(expr, tt, entry_source, wasi_registry);
            for arm in arms {
                rewrite_cm_methods_in_expr(&mut arm.body, tt, entry_source, wasi_registry);
                if let Some(guard) = &mut arm.guard {
                    rewrite_cm_methods_in_expr(guard, tt, entry_source, wasi_registry);
                }
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                rewrite_cm_methods_in_expr(&mut f.value, tt, entry_source, wasi_registry);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for e in elements {
                rewrite_cm_methods_in_expr(e, tt, entry_source, wasi_registry);
            }
        }
        TirExprKind::FieldAccess { expr, .. } => {
            rewrite_cm_methods_in_expr(expr, tt, entry_source, wasi_registry);
        }
        TirExprKind::Index { expr, index, .. } => {
            rewrite_cm_methods_in_expr(expr, tt, entry_source, wasi_registry);
            rewrite_cm_methods_in_expr(index, tt, entry_source, wasi_registry);
        }
        TirExprKind::Assign { target, value } => {
            rewrite_cm_methods_in_expr(target, tt, entry_source, wasi_registry);
            rewrite_cm_methods_in_expr(value, tt, entry_source, wasi_registry);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                rewrite_cm_methods_in_expr(p, tt, entry_source, wasi_registry);
            }
        }
        TirExprKind::Block(block) => {
            rewrite_cm_methods_in_block(block, tt, entry_source, wasi_registry);
        }
        // CM resource method calls inside `with E => h do { ... }` and
        // its handler binding expressions need the same rewriting as
        // calls in plain bodies — otherwise gap-2/3 dispatch wrappers
        // never see a `__cm_*` shape and the un-rewritten MethodCall
        // (with `cm_name` still set) reaches WIR build, which panics
        // in `try_translate_canonical_method` for resource ops the
        // canonical translator no longer handles.
        TirExprKind::WithHandler { bindings, body, .. } => {
            for binding in bindings {
                rewrite_cm_methods_in_expr(&mut binding.handler, tt, entry_source, wasi_registry);
            }
            rewrite_cm_methods_in_block(body, tt, entry_source, wasi_registry);
        }
        TirExprKind::Resume { value } => {
            rewrite_cm_methods_in_expr(value, tt, entry_source, wasi_registry);
        }
        TirExprKind::Closure { body, .. } => {
            rewrite_cm_methods_in_expr(body, tt, entry_source, wasi_registry);
        }
        TirExprKind::IndirectCall { callee, args } => {
            rewrite_cm_methods_in_expr(callee, tt, entry_source, wasi_registry);
            for arg in args.iter_mut() {
                rewrite_cm_methods_in_expr(arg, tt, entry_source, wasi_registry);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args.iter_mut() {
                rewrite_cm_methods_in_expr(arg, tt, entry_source, wasi_registry);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            rewrite_cm_methods_in_expr(value, tt, entry_source, wasi_registry);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            rewrite_cm_methods_in_block(block, tt, entry_source, wasi_registry);
        }
        TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            rewrite_cm_methods_in_expr(inner, tt, entry_source, wasi_registry);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            rewrite_cm_methods_in_expr(scrutinee, tt, entry_source, wasi_registry);
            for arm in arms.iter_mut() {
                rewrite_cm_methods_in_block(arm, tt, entry_source, wasi_registry);
            }
            rewrite_cm_methods_in_block(default, tt, entry_source, wasi_registry);
        }
        TirExprKind::TemplateString { parts } => {
            for part in parts.iter_mut() {
                if let TirTemplatePart::Interpolation { expr, .. } = part {
                    rewrite_cm_methods_in_expr(expr, tt, entry_source, wasi_registry);
                }
            }
        }
        // Leaf-like: no nested expressions to recurse into.
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }

    // Now check if this expression is a CM resource method call
    let cm_name = match &expr.kind {
        TirExprKind::MethodCall { func, .. } => {
            func.method_info.as_ref().and_then(|m| m.cm_name.clone())
        }
        TirExprKind::Call { func, .. } => func.method_info.as_ref().and_then(|m| m.cm_name.clone()),
        _ => None,
    };

    let Some(cm_name) = cm_name else {
        return;
    };

    // stream-new / future-new remain handled by WIR translate for now,
    // because they require i64→tuple splitting with proper GC type casting.
    // stream-read for non-u8 element types also stays for WIR translate,
    // which generates proper record lifting from linear memory.
    if matches!(cm_name.as_str(), "stream-new" | "future-new") {
        return;
    }
    if cm_name == "stream-read" && !is_u8_array_type(expr.type_id, tt) {
        // Non-u8 stream reads use a generated binding function
        if let Some(type_args) = tt.generic_type_args(expr.type_id)
            && let Some(&elem_type_id) = type_args.first()
        {
            let elem_name = tt.base_type_name(elem_type_id);
            let func_name = format!("__cm_stream_read_{elem_name}");
            rewrite_cm_instance_method(expr, "entry", &func_name, entry_source);
            return;
        }
        return;
    }

    // For stream operations on non-u8 types, parameterize the canonical name
    // and rewrite as CmRawCall directly (since the name is dynamic).
    if is_stream_cm_method(&cm_name) {
        let parameterized = parameterize_stream_cm_name(&cm_name, expr, tt, wasi_registry);
        if parameterized != cm_name {
            rewrite_cm_instance_method(expr, "raw", &parameterized, entry_source);
            return;
        }
    }

    // Look up the binding function
    let Some((kind, func_name)) = cm_binding_function(&cm_name) else {
        // Not handled by synthesis yet — will fall through to WIR translate
        return;
    };

    match &mut expr.kind {
        TirExprKind::MethodCall { .. } => {
            rewrite_cm_instance_method(expr, kind, func_name, entry_source);
        }
        TirExprKind::Call { .. } => {
            rewrite_cm_static_method(expr, kind, func_name, entry_source);
        }
        _ => {}
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
    wasi_registry: &WasiRegistry,
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
            let cm_elem = registered_cm_name(&elem_name, wasi_registry)
                .unwrap_or_else(|| pascal_to_kebab(&elem_name));
            return format!("{cm_name}:{cm_elem}");
        }
    }
    cm_name.to_string()
}

/// Look up the canonical `#[cm("…")]` CM name for a Wado type name across
/// the registry's stream-eligible categories (struct, resource, variant,
/// enum, flags). Returns `None` for ambiguous lookups or unregistered
/// names.
fn registered_cm_name(name: &str, registry: &WasiRegistry) -> Option<String> {
    registry
        .get_struct_cm_name(name)
        .or_else(|| registry.get_resource_cm_name(name))
        .or_else(|| registry.get_variant_cm_name(name))
        .or_else(|| registry.get_enum_cm_name(name))
        .or_else(|| registry.get_flags_cm_name(name))
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

/// Check if a `TypeId` represents `Array<u8>`.
fn is_u8_array_type(type_id: TypeId, tt: &TypeTable) -> bool {
    let name = tt.type_name(type_id);
    name == "Array<u8>"
}
