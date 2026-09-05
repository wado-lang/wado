//! Two passes ahead of adapter synthesis: [`synthesize_record_stream_reads`]
//! generates a `__cm_stream_read_<T>` binding per WASI record
//! `Stream<T>::read()` mentions, and [`rewrite_cm_resource_methods`] then turns
//! every `#[cm("…")]` resource method call into its raw / internal /
//! entry-module form, before a downstream phase meets a `cm_name`-tagged call.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::{AstId, NamedType, Type};
use crate::compiler_item::CompilerItem;
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

use crate::canonical::{CanonicalIntrinsic, CmFuturePayload, CmStreamPayload};
use crate::synthesis::common::{
    alloc_named_local, assign, binary, break_stmt, builtin_call, cast, cm_canonical_call,
    entry_call, expr_stmt, i32_const, if_stmt, internal_call, let_mut_stmt, let_stmt, local_ref,
    loop_stmt, option_none, option_some, return_stmt, synth_span,
};

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

/// `cm_copy_result(packed)`: the `CopyResult` a packed copy reports.
fn copy_result_of(packed: TirExpr, type_table: &RefCell<TypeTable>) -> TirExpr {
    let copy_result = type_table
        .borrow_mut()
        .make_compiler_enum(CompilerItem::CopyResult);
    internal_call("cm_copy_result", vec![packed], copy_result)
}

/// `StreamChunk { items, result }` / `StreamWrite { count, result }` — what a
/// copy moved, paired with how it ended. Field order follows the declaration.
fn copy_report_literal(
    type_id: TypeId,
    item: CompilerItem,
    moved_field: &str,
    moved: TirExpr,
    result: TirExpr,
    type_table: &RefCell<TypeTable>,
) -> TirExpr {
    let struct_name = type_table.borrow().compiler_struct_name(item).to_string();
    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: type_id,
            struct_name,
            fields: vec![
                crate::tir::TirStructField {
                    name: moved_field.to_string(),
                    value: moved,
                    field_index: 0,
                },
                crate::tir::TirStructField {
                    name: "result".to_string(),
                    value: result,
                    field_index: 1,
                },
            ],
        },
        type_id,
        synth_span(),
    )
}

fn stream_chunk_literal(
    chunk_type_id: TypeId,
    items: TirExpr,
    result: TirExpr,
    type_table: &RefCell<TypeTable>,
) -> TirExpr {
    copy_report_literal(
        chunk_type_id,
        CompilerItem::StreamChunk,
        "items",
        items,
        result,
        type_table,
    )
}

fn stream_write_literal(
    write_type_id: TypeId,
    count: TirExpr,
    result: TirExpr,
    type_table: &RefCell<TypeTable>,
) -> TirExpr {
    copy_report_literal(
        write_type_id,
        CompilerItem::StreamWrite,
        "count",
        count,
        result,
        type_table,
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

/// Whether a CM async primitive call can be bound where it stands: a call in a
/// generic body names its payload with a type parameter, and the helper it
/// needs is minted per instance, after monomorphize.
pub(super) fn payload_is_bindable(tt: &TypeTable, expr: &TirExpr) -> bool {
    super::future_stream_payload_site(tt, expr).is_none_or(|(payload, _)| tt.is_concrete(payload))
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
        if payload_is_bindable(self.tt, expr)
            && let Some((name, key)) = (self.find)(self.tt, expr)
        {
            self.results.entry(name).or_insert(key);
        }
        self.walk_expr(expr);
    }
}

/// The bodies a binding pass walks and the state it synthesizes against. A
/// `Package` and a `FlatPackage` both present it, so the passes run over either.
pub(super) struct BindingSites<'a> {
    /// Every function whose body may hold an unrewritten `#[cm]` call.
    pub functions: Vec<Rc<RefCell<TirFunction>>>,
    /// The package-wide type table every module shares.
    pub type_table: Rc<RefCell<TypeTable>>,
    pub cm_interface_registry: &'a CmInterfaceRegistry,
    pub interner: &'a RefCell<ModuleSourceInterner>,
    pub entry_module_source: ModuleSource,
    /// The names the entry module — where every helper lands — already holds.
    /// Reaching a payload a second time calls its helper, never mints another.
    existing: IndexSet<String>,
}

impl<'a> BindingSites<'a> {
    fn from_package(project: &'a Package) -> Self {
        let entry = project
            .tir_modules
            .get(&project.entry_module_source)
            .expect("entry module must exist in tir_modules");
        Self {
            existing: entry
                .functions
                .iter()
                .map(|f| f.borrow().name.clone())
                .collect(),
            functions: project
                .tir_modules
                .values()
                .flat_map(|m| m.functions.iter().cloned())
                .collect(),
            type_table: entry.type_table.clone(),
            cm_interface_registry: &project.cm_interface_registry,
            interner: &project.interner,
            entry_module_source: project.entry_module_source.clone(),
        }
    }

    fn from_flat(flat: &'a crate::flat_package::FlatPackage) -> Self {
        Self {
            functions: flat.functions.clone(),
            existing: flat
                .functions
                .iter()
                .filter_map(|f| {
                    let f = f.borrow();
                    (f.module_source == flat.entry_module_source).then(|| f.name.clone())
                })
                .collect(),
            type_table: flat.type_table.clone(),
            cm_interface_registry: &flat.cm_interface_registry,
            interner: &flat.interner,
            entry_module_source: flat.entry_module_source.clone(),
        }
    }
}

/// Shared driver for the `synthesize_*` binding passes: walk every TIR
/// function body with `find` (an exhaustive [`TirRefVisitor`] traversal whose
/// coverage matches the rewriter's, so a call nested in an `if`-expression
/// branch still gets its helper generated), dedupe matches by helper name, and
/// synthesize one function per key. The helpers belong to the entry module,
/// which is where `rewrite_cm_resource_methods` points its `entry_call`s.
fn synthesize_bindings<K>(
    sites: &BindingSites<'_>,
    find: impl Fn(&TypeTable, &TirExpr) -> Option<(String, K)>,
    synthesize: impl Fn(K, &SynthCtx) -> TirFunction,
) -> Vec<Rc<RefCell<TirFunction>>> {
    let mut needed: IndexMap<String, K> = IndexMap::default();
    {
        let tt = sites.type_table.borrow();
        for func_rc in &sites.functions {
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
    needed.retain(|name, _| !sites.existing.contains(name));
    if needed.is_empty() {
        return Vec::new();
    }

    let ctx = SynthCtx {
        cm_interface_registry: sites.cm_interface_registry,
        type_table: &sites.type_table,
        interner: sites.interner,
    };
    needed
        .into_iter()
        .map(|(_, key)| Rc::new(RefCell::new(synthesize(key, &ctx))))
        .collect()
}

/// Generate binding functions for Stream<T>.`read()` where T is a non-u8 WASI record type.
///
/// For each unique stream element type T found in stream-read calls, generates a
/// TIR function `__cm_stream_read_<T>` that:
/// 1. Allocates a `max * elem_size` buffer and issues the element-parameterized
///    `stream-read` canonical, awaiting BLOCKED
/// 2. Loops through the buffer, lifting each record from linear memory
/// 3. Returns them as a `StreamChunk<T>` with the copy's result
fn synthesize_record_stream_reads(sites: &BindingSites<'_>) -> Vec<Rc<RefCell<TirFunction>>> {
    synthesize_bindings(
        sites,
        |tt, expr| {
            let elem = record_stream_read_element(tt, expr)?;
            Some((record_stream_read_func_name(&tt.base_type_name(elem)), elem))
        },
        synthesize_record_stream_read_func,
    )
}

fn synthesize_record_stream_read_func(elem_type_id: TypeId, ctx: &SynthCtx) -> TirFunction {
    let registry = ctx.cm_interface_registry;
    let elem_name = ctx.type_table.borrow().base_type_name(elem_type_id);
    let source = registry
        .find_binding_struct_source(&elem_name)
        .unwrap_or_else(|| {
            panic!(
                "record `{elem_name}` used as a stream-read element has no defining \
                 bundled interface in the CM interface registry; cannot synthesize \
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
    });
    // Scope the layout and the lift alike to the record's own package, so
    // nested field names resolve the same way on both sides.
    let (_, elem_pkg) = super::types::cm_package_from_source(&source)
        .expect("`find_binding_struct_source` yields only bundled-namespace sources");
    let scope = Some(elem_pkg);
    let elem_size =
        crate::component_model::cm_size_with_registry_scoped(&ast_type, registry, scope) as i32;
    let elem_align =
        crate::component_model::cm_align_with_registry_scoped(&ast_type, registry, scope) as i32;
    let cm_record_name = registry
        .get_struct_cm_name_by_source(&source, &elem_name)
        .unwrap_or(&elem_name)
        .to_string();
    synthesize_stream_read_func(
        record_stream_read_func_name(&elem_name),
        CanonicalIntrinsic::StreamRead(CmStreamPayload::Record(cm_record_name)),
        elem_type_id,
        elem_size,
        elem_align,
        &ast_type,
        elem_pkg,
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
/// `future-read` canonical, handles BLOCKED via `cm_await_blocked`, lifts the
/// payload with the shared `synthesize_lift`, and wraps it in `Option`. This
/// replaces the hand-rolled WIR-build lift with its hardcoded CM offsets.
fn synthesize_future_reads(sites: &BindingSites<'_>) -> Vec<Rc<RefCell<TirFunction>>> {
    synthesize_bindings(
        sites,
        |tt, expr| {
            let (payload, option) = future_read_payload(tt, expr)?;
            Some((future_read_func_name(tt, payload), (payload, option)))
        },
        |(payload_type_id, option_type_id), ctx| {
            synthesize_future_read_func(payload_type_id, option_type_id, ctx)
        },
    )
}

/// Generate the per-payload `FutureWritable<T>::write()` binding functions,
/// mirroring [`synthesize_future_reads`].
///
/// On BLOCKED, transmission shapes (write-completion, trailers) await the host
/// reader and free the buffer, but value payloads leave the buffer alive and
/// return: their reader is another task in the same instance, so busy-waiting
/// would deadlock the async executor.
fn synthesize_future_writes(sites: &BindingSites<'_>) -> Vec<Rc<RefCell<TirFunction>>> {
    synthesize_bindings(
        sites,
        |tt, expr| {
            let payload = future_write_payload(tt, expr)?;
            Some((future_write_func_name(tt, payload), payload))
        },
        synthesize_future_write_func,
    )
}

/// The payload type of a `future-write` method call, or `None` if the
/// expression is not a future-write.
fn future_write_payload(tt: &TypeTable, expr: &TirExpr) -> Option<TypeId> {
    let (receiver, func) = expr.kind.call_receiver()?;
    if func.method_info.as_ref().and_then(|m| m.cm_name.as_deref()) != Some("future-write") {
        return None;
    }
    tt.generic_type_args(tt.peel_refs(receiver.type_id))?
        .first()
        .copied()
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
fn synthesize_stream_writes(sites: &BindingSites<'_>) -> Vec<Rc<RefCell<TirFunction>>> {
    synthesize_bindings(
        sites,
        |tt, expr| {
            let elem = stream_write_value_element(tt, expr)?;
            Some((stream_write_func_name(tt, elem), elem))
        },
        synthesize_stream_write_func,
    )
}

/// The AST type a payload lays out as: a newtype has no representation of its
/// own, so its base's, at every level.
fn payload_ast_type(
    payload: TypeId,
    tt: &TypeTable,
    registry: &CmInterfaceRegistry,
) -> crate::ast::Type {
    let id = crate::component_model::peel_newtypes(tt, payload);
    // Peeled off the TypeId, not the produced AST: a lib-local alias's
    // synthesized `NamedType` carries no source interface to look it up by.
    if let ResolvedType::GenericInstance { def, type_args } = tt.get(id) {
        let name = tt.def_name(*def).to_string();
        let args: Vec<crate::ast::Type> = type_args
            .iter()
            .map(|&a| payload_ast_type(a, tt, registry))
            .collect();
        return if TypeTable::is_tuple_type(&name) {
            crate::ast::Type::Tuple(args)
        } else {
            crate::ast::Type::Generic(crate::ast::GenericType {
                id: crate::ast::AstId::fresh(),
                name,
                args,
                span: synth_span(),
            })
        };
    }
    type_id_to_ast_type(id, tt, registry)
}

/// Asked directly rather than through `classify_stream_payload`, which panics
/// instead of answering `false`.
fn has_value_payload(tt: &TypeTable, elem: TypeId) -> bool {
    !crate::component_model::is_u8_stream_element(tt, elem)
        && crate::component_model::cm_payload_type_from_type_id(tt, elem).is_some()
}

/// The stream-write element type for a scalar / structural `stream-write`, or
/// `None` for `u8` and record streams (handled elsewhere).
fn stream_write_value_element(tt: &TypeTable, expr: &TirExpr) -> Option<TypeId> {
    if cm_name_of(expr) != Some("stream-write") {
        return None;
    }
    let elem = stream_receiver_element(tt, expr)?;
    has_value_payload(tt, elem).then_some(elem)
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
fn synthesize_stream_reads(sites: &BindingSites<'_>) -> Vec<Rc<RefCell<TirFunction>>> {
    synthesize_bindings(
        sites,
        |tt, expr| {
            let elem = stream_read_value_element(tt, expr)?;
            Some((stream_read_value_func_name(tt, elem), elem))
        },
        synthesize_stream_read_value_func,
    )
}

/// Generate the payload-parameterized helpers every `#[cm]` async primitive in
/// `sites` needs, rewrite those calls onto them, and return the helpers for the
/// caller to place. Runs twice over a compilation: once before monomorphize for
/// the concrete bodies, once after it for the ones whose payload was still a
/// type parameter.
fn rewrite_async_primitives_at(sites: &BindingSites<'_>) -> Vec<Rc<RefCell<TirFunction>>> {
    let mut generated = synthesize_record_stream_reads(sites);
    generated.extend(synthesize_future_reads(sites));
    generated.extend(synthesize_future_writes(sites));
    generated.extend(synthesize_stream_writes(sites));
    generated.extend(synthesize_stream_reads(sites));
    rewrite_cm_resource_methods(sites);
    generated
}

/// Pre-monomorphize half: every body whose payloads are already concrete.
/// Consumes the witness — these rewrites destroy the shape the scan matches.
pub(super) fn rewrite_async_primitives(
    project: &mut Package,
    _validated: super::PayloadsValidated,
) {
    let generated = rewrite_async_primitives_at(&BindingSites::from_package(project));
    if generated.is_empty() {
        return;
    }
    let entry_source = project.entry_module_source.clone();
    project
        .tir_modules
        .get_mut(&entry_source)
        .expect("entry module must exist in tir_modules")
        .functions
        .extend(generated);
}

/// Post-monomorphize half: the bodies that were generic, where a `#[cm]` call's
/// payload only became concrete when the instance was minted.
pub fn rewrite_async_primitives_monomorphized(
    flat: &mut crate::flat_package::FlatPackage,
    _validated: super::PayloadsValidated,
) {
    let generated = rewrite_async_primitives_at(&BindingSites::from_flat(flat));
    // Link is what stamps a module source on a pre-monomorphize helper, by the
    // module it was placed in. These arrive after it, so they carry the entry
    // module themselves — the module their `entry_call` sites name.
    for func in &generated {
        func.borrow_mut().module_source = flat.entry_module_source.clone();
    }
    flat.functions.extend(generated);
}

/// The stream-read element type for a value-payload `stream-read`, or `None`
/// for `u8` and WASI record streams (handled by their own paths).
fn stream_read_value_element(tt: &TypeTable, expr: &TirExpr) -> Option<TypeId> {
    let elem = stream_read_element(tt, expr)?;
    has_value_payload(tt, elem).then_some(elem)
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
        let write_name = CanonicalIntrinsic::StreamWrite(payload);
        let elem_ast = payload_ast_type(elem_type_id, &tt, cm_interface_registry);
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
        names: CmStdlibNames::from_type_table(&type_table.borrow()),
    };
    let (lower_stmts, ptr_local, count_local) = super::lower::synthesize_lower_list_to_buffer(
        &elem_ast,
        local_ref(data_idx, "data", list_type_id),
        &mut next_local,
        &mut locals,
        &lower_ctx,
    );
    stmts.extend(lower_stmts);

    // One copy. A reader may take only a prefix, which the returned count
    // reports; `StreamWritable::write_all` is the loop that finishes a buffer.
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
        cm_canonical_call(
            write_name,
            vec![
                local_ref(handle_idx, "handle", TypeTable::I32),
                local_ref(ptr_local, "__list_base", TypeTable::I32),
                local_ref(count_local, "__list_len", TypeTable::I32),
            ],
            TypeTable::I32,
        ),
    ));
    stmts.push(await_if_blocked(result_idx, "result", handle_idx));
    let result_ref = || local_ref(result_idx, "result", TypeTable::I32);

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

    let write_type_id = type_table
        .borrow_mut()
        .make_compiler_struct(CompilerItem::StreamWrite);
    stmts.push(return_stmt(Some(stream_write_literal(
        write_type_id,
        packed_count(result_ref()),
        copy_result_of(result_ref(), type_table),
        type_table,
    ))));

    TirFunction {
        module_source: ModuleSource::default(),
        name: func_name,
        def_id: None,
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
        return_type: write_type_id,
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

/// `if <status> == BLOCKED { <status> = cm_await_blocked(handle) }` — re-reads
/// the packed result after the host signals readiness.
fn await_if_blocked(status_idx: u32, status_name: &str, handle_idx: u32) -> TirStmt {
    if_stmt(
        is_blocked(local_ref(status_idx, status_name, TypeTable::I32)),
        TirBlock {
            stmts: vec![expr_stmt(assign(
                local_ref(status_idx, status_name, TypeTable::I32),
                internal_call(
                    "cm_await_blocked",
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
        let write_name = CanonicalIntrinsic::FutureWrite(payload.clone());
        let cm_package = future_payload_package(&payload);
        let payload_ast = payload_ast_type(payload_type_id, &tt, cm_interface_registry);
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
        names: CmStdlibNames::from_type_table(&type_table.borrow()),
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
        cm_canonical_call(
            write_name,
            vec![
                local_ref(handle_idx, "handle", TypeTable::I32),
                local_ref(ptr_idx, "ptr", TypeTable::I32),
            ],
            TypeTable::I32,
        ),
    ));

    if awaits_reader {
        stmts.push(await_if_blocked(written_idx, "__written", handle_idx));
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
        def_id: None,
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
    let TirExprKind::Call { func, .. } = &expr.kind else {
        return None;
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
        let read_name = CanonicalIntrinsic::FutureRead(payload.clone());
        let cm_package = future_payload_package(&payload);
        let payload_ast = payload_ast_type(payload_type_id, &tt, cm_interface_registry);
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
        cm_canonical_call(
            read_name,
            vec![
                local_ref(handle_idx, "handle", TypeTable::I32),
                local_ref(ptr_idx, "ptr", TypeTable::I32),
            ],
            TypeTable::I32,
        ),
    ));

    stmts.push(await_if_blocked(status_idx, "status", handle_idx));

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
        def_id: None,
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
fn record_stream_read_element(tt: &TypeTable, expr: &TirExpr) -> Option<TypeId> {
    let elem_type_id = stream_read_element(tt, expr)?;
    (!has_value_payload(tt, elem_type_id)).then_some(elem_type_id)
}

/// What the receiver of a stream `#[cm]` method streams. Never read off what
/// the call returns: monomorphize replaces a `StreamChunk<T>` with a struct.
fn stream_receiver_element(tt: &TypeTable, expr: &TirExpr) -> Option<TypeId> {
    let (receiver, _) = expr.kind.call_receiver()?;
    // `type MyStream = Stream<u8>` names the same stream; the payload paths peel
    // it too, so a newtype receiver must answer with the element, not nothing.
    let recv = crate::component_model::peel_newtypes(tt, tt.peel_refs(receiver.type_id));
    tt.generic_type_args(recv)?.first().copied()
}

/// The `#[cm(...)]` name a call carries, if any.
fn cm_name_of(expr: &TirExpr) -> Option<&str> {
    let TirExprKind::Call { func, .. } = &expr.kind else {
        return None;
    };
    func.method_info.as_ref()?.cm_name.as_deref()
}

/// The element type of a `stream-read` call, or `None` for anything else and
/// for the `u8` stream, which `core:rt` binds by hand.
fn stream_read_element(tt: &TypeTable, expr: &TirExpr) -> Option<TypeId> {
    if cm_name_of(expr) != Some("stream-read") {
        return None;
    }
    let elem = stream_receiver_element(tt, expr)?;
    (!crate::component_model::is_u8_stream_element(tt, elem)).then_some(elem)
}

/// The `__cm_stream_read_<record>` helper name for a WASI record element.
fn record_stream_read_func_name(elem_name: &str) -> String {
    format!("__cm_stream_read_{elem_name}")
}

/// Shared stream-read loop generator. `func_name` / `stream_read_name` /
/// `payload_ast` / `cm_package` are precomputed by the caller so this body
/// serves both the WASI-record path ([`synthesize_record_stream_reads`]) and
/// the value-payload path ([`synthesize_stream_reads`]).
#[allow(clippy::too_many_arguments)]
fn synthesize_stream_read_func(
    func_name: String,
    stream_read_name: CanonicalIntrinsic,
    elem_type_id: TypeId,
    elem_size: i32,
    elem_align: i32,
    payload_ast: &Type,
    cm_package: &str,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &RefCell<TypeTable>,
    interner: &RefCell<ModuleSourceInterner>,
) -> TirFunction {
    let array_type_id = type_table.borrow_mut().make_list(elem_type_id);
    let chunk_type_id = type_table.borrow_mut().make_stream_chunk(elem_type_id);
    // `List` is a declaration, so the receiver its methods are named after
    // carries it rather than the spelling `List` happens to have.
    let list_fq = super::types::CmStdlibNames::from_type_table(&type_table.borrow()).array_fq;
    let elem_fq = type_table.borrow().fq_type_name(elem_type_id);
    // `List<Elem>::<method>` — the instantiated receiver both calls hang off.
    let list_method = |method: &str| {
        LocalMethodName::new(list_fq.clone(), None, method.to_string())
            .with_struct_type_args(std::slice::from_ref(&elem_fq))
    };

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
    let stream_read_call = cm_canonical_call(
        stream_read_name,
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

    stmts.push(await_if_blocked(result_idx, "result", handle_idx));

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
    let with_capacity = list_method("with_capacity");
    let empty_arr = TirExpr::new(
        TirExprKind::Call {
            func: Box::new(FunctionRef {
                module_source: ModuleSource::list(),
                name: with_capacity.to_mangled_name(),
                monomorph_info: Some(MonomorphInfo {
                    generic_name: format!("{list_fq}::with_capacity"),
                    impl_type_args: vec![elem_type_id],
                    method_type_args: vec![],
                    is_blanket: false,
                }),
                method_info: Some(with_capacity),
            }),
            type_args: vec![],
            args: vec![CallArg::new(
                local_ref(count_idx, "count", TypeTable::I32),
                false,
            )],
            has_receiver: false,
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

    let push = list_method("push");
    let push_call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(local_ref(arr_idx, "arr", array_type_id)),
            FunctionRef {
                module_source: ModuleSource::list(),
                name: push.to_mangled_name(),
                monomorph_info: Some(MonomorphInfo {
                    generic_name: format!("{list_fq}::push"),
                    impl_type_args: vec![elem_type_id],
                    method_type_args: vec![],
                    is_blanket: false,
                }),
                method_info: Some(push),
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

    stmts.push(return_stmt(Some(stream_chunk_literal(
        chunk_type_id,
        local_ref(arr_idx, "arr", array_type_id),
        copy_result_of(local_ref(result_idx, "result", TypeTable::I32), type_table),
        type_table,
    ))));

    TirFunction {
        module_source: ModuleSource::default(),
        name: func_name,
        def_id: None,
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
        return_type: chunk_type_id,
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
    let (func_name, read_name, payload_ast, elem_size, elem_align) = {
        let tt = type_table.borrow();
        let func_name = stream_read_value_func_name(&tt, elem_type_id);
        let payload = crate::component_model::classify_stream_payload(&tt, elem_type_id);
        let read_name = CanonicalIntrinsic::StreamRead(payload);
        let payload_ast = payload_ast_type(elem_type_id, &tt, cm_interface_registry);
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
        (func_name, read_name, payload_ast, size, align)
    };

    synthesize_stream_read_func(
        func_name,
        read_name,
        elem_type_id,
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
#[derive(Clone)]
enum BindingTarget {
    /// Direct `CmRawCall` to the canonical Wasm import (simple operations).
    Canonical(CanonicalIntrinsic),
    /// Call to a `core:rt` binding function (complex operations).
    Internal(CompilerItem),
    /// Call to a synthesized binding function in the entry module.
    Entry(String),
}

/// The `core:rt` binding a CM member needs beyond its canonical import: one
/// that moves a payload, or reads back what the canonical only signals.
fn internal_cm_binding(cm_name: &str) -> Option<CompilerItem> {
    Some(match cm_name {
        "stream-read" => CompilerItem::CmStreamReadU8,
        "stream-write" => CompilerItem::CmStreamWriteU8,
        "stream-write-raw" => CompilerItem::CmStreamWriteRawU8,
        "error-context-new" => CompilerItem::CmErrorContextNew,
        "error-context-debug-message" => CompilerItem::CmErrorContextDebugMessage,
        "waitable-set-wait" => CompilerItem::CmWaitableSetWait,
        "waitable-set-poll" => CompilerItem::CmWaitableSetPoll,
        // Void canonical; the binding returns the handle as a `Waitable`.
        "waitable-join" => CompilerItem::CmWaitableJoin,
        _ => return None,
    })
}

/// What a Component Model primitive binds to. `None` falls through to WIR
/// translate, and the caller has already taken the payload-carrying names, so
/// every stream reaching here is a `u8` one.
fn cm_binding_function(cm_name: &str) -> Option<BindingTarget> {
    if let Some(binding) = internal_cm_binding(cm_name) {
        return Some(BindingTarget::Internal(binding));
    }
    let intrinsic = CanonicalIntrinsic::from_import_name(cm_name)?;
    // A constructor hands back both ends packed in an i64, so the canonical
    // alone is not the binding: `rewrite_cm_new` splits the pair.
    let is_pair = matches!(
        intrinsic,
        CanonicalIntrinsic::StreamNew(_) | CanonicalIntrinsic::FutureNew(_)
    );
    (!is_pair).then_some(BindingTarget::Canonical(intrinsic))
}

/// Rewrite all #[cm("...")] resource method calls in the project.
fn rewrite_cm_resource_methods(sites: &BindingSites<'_>) {
    let type_table = sites.type_table.borrow();
    for func_rc in &sites.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = &mut func.body {
            rewrite_cm_methods_in_block(
                body,
                &type_table,
                &sites.entry_module_source,
                sites.cm_interface_registry,
            );
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
            TirExprKind::Call { func, .. } => {
                func.method_info.as_ref().and_then(|m| m.cm_name.clone())
            }
            _ => return,
        };
        let Some(cm_name) = cm_name else {
            return;
        };
        // Leave a generic body's call for the instances: its helper is keyed by
        // the payload, which this site does not have yet.
        if !payload_is_bindable(self.tt, expr) {
            return;
        }

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
            self.rewrite_call(expr, BindingTarget::Entry(func_name));
            return;
        }
        // Scalar / structural stream writes call a generated per-element binding;
        // `u8` and record streams fall through to the existing paths.
        if cm_name == "stream-write"
            && let Some(elem_type_id) = stream_write_value_element(self.tt, expr)
        {
            let func_name = stream_write_func_name(self.tt, elem_type_id);
            self.rewrite_call(expr, BindingTarget::Entry(func_name));
            return;
        }
        // Future reads call a generated per-payload binding function.
        if cm_name == "future-read" {
            if let Some((payload_type_id, _)) = future_read_payload(self.tt, expr) {
                let func_name = future_read_func_name(self.tt, payload_type_id);
                self.rewrite_call(expr, BindingTarget::Entry(func_name));
            }
            return;
        }
        // Value-payload stream reads call a generated per-element binding;
        // WASI-record reads fall through to the `__cm_stream_read_<record>` path.
        if let Some(elem_type_id) = stream_read_value_element(self.tt, expr) {
            let func_name = stream_read_value_func_name(self.tt, elem_type_id);
            self.rewrite_call(expr, BindingTarget::Entry(func_name));
            return;
        }
        // WASI-record stream reads call a generated binding function.
        if cm_name == "stream-read"
            && let Some(elem_type_id) = record_stream_read_element(self.tt, expr)
        {
            let elem_name = self.tt.base_type_name(elem_type_id);
            let func_name = record_stream_read_func_name(&elem_name);
            self.rewrite_call(expr, BindingTarget::Entry(func_name));
            return;
        }
        // Stream ops on a non-u8 element, and future drop / cancel: parameterized
        // by the receiver's payload, so they go straight to a canonical call.
        let parameterized =
            parameterize_stream_cm_name(&cm_name, expr, self.tt, self.cm_interface_registry)
                .or_else(|| parameterize_future_cm_name(&cm_name, expr, self.tt));
        if let Some(intrinsic) = parameterized {
            self.rewrite_call(expr, BindingTarget::Canonical(intrinsic));
            return;
        }
        // Everything above is what parameterizes a wider element, so what is
        // left of the stream surface is the hand-written `u8` path: an element
        // that got here unparameterized stays unbound and is reported.
        if cm_name.starts_with("stream-")
            && !stream_receiver_element(self.tt, expr)
                .is_some_and(|e| crate::component_model::is_u8_stream_element(self.tt, e))
        {
            return;
        }
        // Look up the binding function for everything else.
        let Some(target) = cm_binding_function(&cm_name) else {
            // Not handled by synthesis yet — falls through to WIR translate.
            return;
        };
        self.rewrite_call(expr, target);
    }
}

impl CmMethodRewriter<'_> {
    /// Rewrite a CM call — `recv.method(args)` or `Type::method(args)` — to a
    /// builtin/internal call, casting the handle it operates on to the `i32` the
    /// canonical import takes.
    fn rewrite_call(&self, expr: &mut TirExpr, target: BindingTarget) {
        let TirExprKind::Call { args, .. } = &mut expr.kind else {
            return;
        };
        let mut taken_args: Vec<TirExpr> =
            std::mem::take(args).into_iter().map(|a| a.expr).collect();
        if let Some(handle) = taken_args.first_mut()
            && is_cm_handle(self.tt, handle.type_id)
        {
            let taken = std::mem::replace(
                handle,
                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
            );
            *handle = cast(taken, TypeTable::I32);
        }

        *expr = match target {
            BindingTarget::Canonical(intrinsic) => {
                cm_canonical_call(intrinsic, taken_args, expr.type_id)
            }
            BindingTarget::Internal(item) => {
                internal_call(item.attr_name(), taken_args, expr.type_id)
            }
            BindingTarget::Entry(name) => {
                entry_call(&name, taken_args, expr.type_id, self.entry_source.clone())
            }
        };
    }
}

/// Whether a type is a resource handle. `args[0]` is one exactly where the
/// member declares a receiver: a constructor's first argument is a value
/// (`ErrorContext::new(message)`), which casting would corrupt.
fn is_cm_handle(tt: &TypeTable, type_id: TypeId) -> bool {
    let peeled = crate::component_model::peel_newtypes(tt, tt.peel_refs(type_id));
    matches!(
        tt.get(peeled),
        ResolvedType::Resource { .. } | ResolvedType::GenericResource { .. }
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
    let (canonical, helper) = if is_future {
        let payload = crate::component_model::classify_future_payload(tt, payload_tid);
        (CanonicalIntrinsic::FutureNew(payload), "cm_future_pair")
    } else {
        let payload = crate::component_model::classify_stream_payload(tt, payload_tid);
        (CanonicalIntrinsic::StreamNew(payload), "cm_stream_pair")
    };
    let result_type = expr.type_id;
    let packed = cm_canonical_call(canonical, vec![], TypeTable::I64);
    *expr = TirExpr::new(
        TirExprKind::Call {
            func: Box::new(FunctionRef {
                module_source: ModuleSource::rt(),
                name: helper.to_string(),
                monomorph_info: Some(MonomorphInfo {
                    generic_name: helper.to_string(),
                    impl_type_args: vec![payload_tid],
                    method_type_args: vec![],
                    is_blanket: false,
                }),
                method_info: None,
            }),
            type_args: vec![payload_tid],
            args: vec![CallArg::new(packed, false)],
            has_receiver: false,
        },
        result_type,
        synth_span(),
    );
}

/// Drop and cancel are the canonical import itself, so the parameterized name
/// is the whole binding. Read and write move a payload and need a generated one.
fn is_drop_or_cancel(cm_name: &str) -> bool {
    let (_, op) = cm_name.split_once('-').unwrap_or_default();
    matches!(
        op,
        "drop-readable" | "drop-writable" | "cancel-read" | "cancel-write"
    )
}

/// The future drop / cancel intrinsic for `cm_name`, parameterized by the
/// receiver's payload.
fn parameterize_future_cm_name(
    cm_name: &str,
    expr: &TirExpr,
    tt: &TypeTable,
) -> Option<CanonicalIntrinsic> {
    if !is_drop_or_cancel(cm_name) {
        return None;
    }
    let (receiver, _) = expr.kind.call_receiver()?;
    let payload_tid = *tt
        .generic_type_args(tt.peel_refs(receiver.type_id))?
        .first()?;
    let payload = crate::component_model::classify_future_payload(tt, payload_tid);
    CanonicalIntrinsic::future_op(cm_name, payload)
}

/// The stream drop / cancel intrinsic for `cm_name`, parameterized by the
/// receiver's element. A record element takes its `#[cm("…")]` name, so a
/// non-mechanical mapping (`DNSRecord` → `dns-record`) is not mangled; only a
/// user-authored element outside the registry falls back to `PascalCase` →
/// kebab-case.
fn parameterize_stream_cm_name(
    cm_name: &str,
    expr: &TirExpr,
    tt: &TypeTable,
    cm_interface_registry: &CmInterfaceRegistry,
) -> Option<CanonicalIntrinsic> {
    if !is_drop_or_cancel(cm_name) {
        return None;
    }
    let elem = stream_receiver_element(tt, expr)?;
    // The same predicate the payload classification uses: a `type MyByte = u8`
    // stream must not drop under one canonical and read under another.
    if crate::component_model::is_u8_stream_element(tt, elem) {
        return None;
    }
    let payload = crate::component_model::cm_payload_type_from_type_id(tt, elem).map_or_else(
        || {
            // The element's declaring interface keys the CM-name lookup. Its
            // `module_source` is the loader identity (a `.wado` path); the
            // registry bridges it to the versioned `#[cm(...)]` key.
            let elem_name = tt.base_type_name(elem);
            let elem_source = tt
                .nominal_head(tt.representation_head(elem))
                .map(|(_, m)| m.to_string());
            let cm_elem = elem_source
                .as_deref()
                .and_then(|source| registered_cm_name(&elem_name, source, cm_interface_registry))
                .unwrap_or_else(|| pascal_to_kebab(&elem_name));
            CmStreamPayload::Record(cm_elem)
        },
        CmStreamPayload::Value,
    );
    CanonicalIntrinsic::stream_op(cm_name, payload)
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

#[cfg(test)]
mod cm_binding_tests {
    use super::*;

    /// What each Component Model primitive binds to. The table is the
    /// specification; [`cm_binding_function`] derives it rather than listing it.
    /// `None` is left to a caller above it: the constructors to `rewrite_cm_new`,
    /// the payload-carrying futures to the parameterized paths, and the task
    /// and context builtins to WIR translate.
    const EXPECTED: &[(&str, Option<&str>)] = &[
        ("future-new", None),
        ("future-read", None),
        ("future-write", None),
        ("future-cancel-read", None),
        ("future-cancel-write", None),
        ("future-drop-readable", None),
        ("future-drop-writable", None),
        ("stream-new", None),
        ("stream-read", Some("internal cm_stream_read_u8")),
        ("stream-write", Some("internal cm_stream_write_u8")),
        ("stream-write-raw", Some("internal cm_stream_write_raw_u8")),
        ("stream-cancel-read", Some("canonical stream-cancel-read")),
        ("stream-cancel-write", Some("canonical stream-cancel-write")),
        (
            "stream-drop-readable",
            Some("canonical stream-drop-readable"),
        ),
        (
            "stream-drop-writable",
            Some("canonical stream-drop-writable"),
        ),
        ("waitable-set-new", Some("canonical waitable-set-new")),
        ("waitable-set-wait", Some("internal cm_waitable_set_wait")),
        ("waitable-set-poll", Some("internal cm_waitable_set_poll")),
        ("waitable-set-drop", Some("canonical waitable-set-drop")),
        ("waitable-join", Some("internal cm_waitable_join")),
        ("subtask-drop", Some("canonical subtask-drop")),
        ("subtask-cancel", Some("canonical subtask-cancel")),
        ("error-context-new", Some("internal cm_error_context_new")),
        (
            "error-context-debug-message",
            Some("internal cm_error_context_debug_message"),
        ),
        ("error-context-drop", Some("canonical error-context-drop")),
        ("backpressure-inc", None),
        ("backpressure-dec", None),
        ("context-get", None),
        ("context-set", None),
        ("task-cancel", None),
    ];

    fn binding_of(cm_name: &str) -> Option<String> {
        Some(match cm_binding_function(cm_name)? {
            BindingTarget::Canonical(i) => format!("canonical {}", i.import_name()),
            BindingTarget::Internal(n) => format!("internal {n}"),
            BindingTarget::Entry(n) => format!("entry {n}"),
        })
    }

    /// Every `#[cm("…")]` the prelude declares on a resource method — the same
    /// attribute reader the elaborator records `cm_name` from.
    fn declared_primitives() -> Vec<String> {
        let source = crate::stdlib::all_core_modules()
            .iter()
            .find(|(import, _)| *import == "core:prelude/types.wado")
            .expect("the prelude declares the Component Model primitives")
            .1;
        crate::parse(source)
            .ast
            .items
            .iter()
            .filter_map(|item| match item {
                crate::ast::Item::Resource(decl) => Some(&decl.methods),
                _ => None,
            })
            .flatten()
            .filter_map(|method| {
                method
                    .attrs
                    .iter()
                    .find_map(crate::ast::Attribute::cm_identifier)
            })
            .collect()
    }

    /// Both directions, so a scan that found nothing fails rather than passes.
    #[test]
    fn the_declared_primitives_are_exactly_the_specified_ones() {
        let declared = declared_primitives();
        let specified: Vec<&str> = EXPECTED.iter().map(|(n, _)| *n).collect();
        let undeclared: Vec<&&str> = specified
            .iter()
            .filter(|n| !declared.iter().any(|d| d == *n))
            .collect();
        let unspecified: Vec<&String> = declared
            .iter()
            .filter(|d| !specified.contains(&d.as_str()))
            .collect();
        assert!(
            undeclared.is_empty() && unspecified.is_empty(),
            "`EXPECTED` states a binding for every declared primitive and no other; \
             not declared: {undeclared:?}, binding unstated: {unspecified:?}"
        );
    }

    #[test]
    fn primitives_bind_as_specified() {
        for (name, expected) in EXPECTED {
            assert_eq!(
                binding_of(name).as_deref(),
                *expected,
                "binding of `{name}`"
            );
        }
    }
}
