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
    let cm_interface_registry = project.cm_interface_registry;
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

        let func = synthesize_stream_read_func(
            elem_name,
            *elem_type_id,
            *array_type_id,
            fields,
            elem_size,
            elem_align,
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

/// Find all stream-read method calls that return Array<T> where T is not u8.
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
            // Extract element type from Array<T>
            if let Some(type_args) = self.tt.generic_type_args(expr.type_id)
                && let Some(&elem_type_id) = type_args.first()
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
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &RefCell<TypeTable>,
    interner: &RefCell<ModuleSourceInterner>,
) -> TirFunction {
    let array_struct_name =
        super::types::CmStdlibNames::from_compiler_items(type_table.borrow().compiler_items())
            .array;
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
    let cm_record_name = cm_interface_registry
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
                module_source: ModuleSource::list(),
                name: format!("{array_struct_name}<{elem_name}>::with_capacity"),
                monomorph_info: Some(MonomorphInfo {
                    generic_name: "List::with_capacity".to_string(),
                    impl_type_args: vec![elem_type_id],
                    method_type_args: vec![],
                    is_blanket: false,
                }),
                method_info: Some(LocalMethodName {
                    struct_name: format!("{array_struct_name}<{elem_name}>"),
                    base_struct_name: array_struct_name.clone(),
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

    // Lift each field from linear memory at addr + field_offset
    let lift_ctx = LiftContext {
        cm_interface_registry,
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
                module_source: ModuleSource::list(),
                name: format!("{array_struct_name}<{elem_name}>::push"),
                monomorph_info: Some(MonomorphInfo {
                    generic_name: "List::push".to_string(),
                    impl_type_args: vec![elem_type_id],
                    method_type_args: vec![],
                    is_blanket: false,
                }),
                method_info: Some(LocalMethodName {
                    struct_name: format!("{array_struct_name}<{elem_name}>"),
                    base_struct_name: array_struct_name,
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
        compiler_item: None,
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
    let cm_interface_registry = project.cm_interface_registry;
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

        // stream-new / future-new remain handled by WIR translate (they need
        // i64→tuple splitting with proper GC type casting).
        if matches!(cm_name.as_str(), "stream-new" | "future-new") {
            return;
        }
        // Non-u8 stream reads call a generated binding function.
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
            let cm_elem = registered_cm_name(&elem_name, cm_interface_registry)
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
fn registered_cm_name(name: &str, registry: &CmInterfaceRegistry) -> Option<String> {
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
    name == "List<u8>"
}
