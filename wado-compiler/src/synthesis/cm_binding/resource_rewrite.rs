//! Two passes ahead of adapter synthesis: [`synthesize_record_stream_reads`]
//! generates a `$cm_stream_read_<T>` binding per WASI record
//! `Stream<T>::read()` mentions, and [`rewrite_cm_resource_methods`] then turns
//! every `#[cm("…")]` resource method call into its raw / internal /
//! entry-module form, before a downstream phase meets a `cm_name`-tagged call.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::{AstId, NamedType, Type};
use crate::call_args::CallArgs;
use crate::compiler_item::CompilerItem;
use crate::component_model::{CmInterfaceRegistry, CmTypeKind};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::{CmNamespace, ModuleSource, ModuleSourceInterner};
use crate::name::{
    LocalMethodName, cm_future_read_func_name, cm_future_write_func_name, cm_stream_lift_func_name,
    cm_stream_lift_value_func_name, cm_stream_read_func_name, cm_stream_read_value_func_name,
    cm_stream_write_func_name, minted_name, to_cm_name,
};
use crate::package::Package;
use crate::tir::{
    CallArg, FrameLocals, FunctionKind, FunctionRef, InlineHint, MonomorphInfo, ResolvedType,
    TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal, TirParam, TirStmt,
    TirStmtKind, TypeId,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};
use crate::type_table::TypeTable;

use crate::canonical::{CanonicalIntrinsic, CmDecl, CmFuturePayload, CmStreamPayload};
use crate::synthesis::common::{
    alloc_named_local, assign, binary, block, break_stmt, builtin_call, cast, cm_canonical_call,
    entry_call, expr_stmt, handle_scalar, i32_const, i64_const, if_stmt,
    internal_call, let_mut_stmt, let_stmt, local_ref, loop_stmt, option_none, option_some,
    return_stmt, struct_literal, synth_span,
};

use super::synthesize_lift;
use super::types::{
    CmStdlibNames, LiftContext, LowerContext, binary_add, cm_layout_i32, type_id_to_ast_type,
};
use crate::ast::{GenericType, Visibility};
use crate::component_model::{
    classify_future_payload, classify_stream_payload, cm_payload_type_from_type_id,
    is_u8_stream_element, peel_newtypes,
};
use crate::flat_package::FlatPackage;
use crate::synthesis::cm_binding::lower::{
    buffer_bytes, synthesize_lower_list_to_buffer, synthesize_lower_wasi_type_to_memory,
};
use crate::synthesis::cm_binding::{PayloadsValidated, future_stream_payload_site};
use crate::{ast, tir};

/// CM async built-ins (`stream-read`, `stream-write`, `future-read`, …)
/// pack their result as `(count << 4) | status`, with `-1` meaning BLOCKED.
const CM_PACKED_STATUS_MASK: i32 = 0xF;
const CM_BLOCKED: i32 = -1;
/// The `CopyResult` status of a copy whose reader or writer dropped first.
const CM_DROPPED: i32 = 1;

/// `cm_copy_count(count)`: the part of `count` one CM copy may move.
fn copy_count(count: TirExpr) -> TirExpr {
    internal_call("cm_copy_count", vec![count], TypeTable::I32)
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

/// `cm_future_write_result(packed)`: how a future write ended, a BLOCKED one
/// pending.
fn future_write_result_of(packed: TirExpr, type_table: &RefCell<TypeTable>) -> TirExpr {
    let result = type_table
        .borrow_mut()
        .make_compiler_enum(CompilerItem::FutureWriteResult);
    internal_call(
        CompilerItem::CmFutureWriteResult.attr_name(),
        vec![packed],
        result,
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
    future_stream_payload_site(tt, expr).is_none_or(|(payload, _)| tt.is_concrete(payload))
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

    fn from_flat(flat: &'a FlatPackage) -> Self {
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
    synthesize: impl Fn(K, &SynthCtx) -> Vec<TirFunction>,
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
        .flat_map(|(_, key)| synthesize(key, &ctx))
        .map(|func| Rc::new(RefCell::new(func)))
        .collect()
}

/// Generate binding functions for Stream<T>.`read()` where T is a non-u8 WASI record type.
///
/// For each unique stream element type T found in stream-read calls, generates a
/// TIR function `$cm_stream_read_<T>` that:
/// 1. Allocates a `max * elem_size` buffer and issues the element-parameterized
///    `stream-read` canonical, awaiting BLOCKED
/// 2. Loops through the buffer, lifting each record from linear memory
/// 3. Returns them as a `StreamChunk<T>` with the copy's result
fn synthesize_record_stream_reads(sites: &BindingSites<'_>) -> Vec<Rc<RefCell<TirFunction>>> {
    synthesize_bindings(
        sites,
        |tt, expr| {
            let elem = record_stream_read_element(tt, expr)?;
            Some((cm_stream_read_func_name(&tt.base_type_name(elem)), elem))
        },
        synthesize_record_stream_read_func,
    )
}

fn synthesize_record_stream_read_func(elem_type_id: TypeId, ctx: &SynthCtx) -> Vec<TirFunction> {
    let registry = ctx.cm_interface_registry;
    let elem_name = ctx.type_table.borrow().base_type_name(elem_type_id);
    let source = registry
        .find_binding_source(CmTypeKind::Struct, &elem_name)
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
    let (_, elem_pkg) = CmNamespace::split_package(&source)
        .expect("`find_binding_source` yields only bundled-namespace sources");
    let (elem_size, elem_align) = cm_layout_i32(&ast_type, registry);
    // The same fallback the other builder of this declaration uses: one
    // declaration renders to one CM name, whichever site reaches it first.
    let cm_record_name = registry
        .get_struct_cm_name_by_source(&source, &elem_name)
        .map_or_else(|| to_cm_name(&elem_name), str::to_string);
    let record = stream_element_decl(&ctx.type_table.borrow(), elem_type_id, &cm_record_name);
    synthesize_stream_read_funcs(
        StreamReadNames {
            begin: cm_stream_read_func_name(&elem_name),
            lift: cm_stream_lift_func_name(&elem_name),
        },
        CanonicalIntrinsic::StreamRead(CmStreamPayload::Record(record)),
        elem_type_id,
        elem_size,
        elem_align,
        &ast_type,
        elem_pkg,
        ctx,
    )
}

/// Generate the per-payload `Future<T>::read()` binding functions.
///
/// For each distinct future payload `T` consumed by `future-read`, synthesizes
/// `$cm_future_read_<mangle(T)>(handle: i32) -> Option<T>` which allocates a
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
            vec![synthesize_future_read_func(payload_type_id, option_type_id, ctx)]
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
        |payload, ctx| vec![synthesize_future_write_func(payload, ctx)],
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

/// The `$cm_future_write_*` helper name for a payload type.
fn future_write_func_name(tt: &TypeTable, payload_type_id: TypeId) -> String {
    cm_future_write_func_name(&tt.mangle_type_arg_for_generic(payload_type_id))
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
        |elem, ctx| vec![synthesize_stream_write_func(elem, ctx)],
    )
}

/// The AST type a payload lays out as: a newtype has no representation of its
/// own, so its base's, at every level.
fn payload_ast_type(payload: TypeId, tt: &TypeTable, registry: &CmInterfaceRegistry) -> ast::Type {
    let id = peel_newtypes(tt, payload);
    // Peeled off the TypeId, not the produced AST: a lib-local alias's
    // synthesized `NamedType` carries no source interface to look it up by.
    if let ResolvedType::GenericInstance { def, type_args } = tt.get(id) {
        let name = tt.def_name(*def).to_string();
        let args: Vec<ast::Type> = type_args
            .iter()
            .map(|&a| payload_ast_type(a, tt, registry))
            .collect();
        return if tt.is_tuple_def(*def) {
            ast::Type::Tuple(args)
        } else {
            ast::Type::Generic(GenericType {
                id: AstId::fresh(),
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
    !is_u8_stream_element(tt, elem) && cm_payload_type_from_type_id(tt, elem).is_some()
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
    cm_stream_write_func_name(&tt.mangle_type_arg_for_generic(elem_type_id))
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
pub(super) fn rewrite_async_primitives(project: &mut Package, _validated: PayloadsValidated) {
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
    flat: &mut FlatPackage,
    _validated: PayloadsValidated,
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

/// The `$cm_stream_read_val_*` helper name for an element type.
fn stream_read_value_func_name(tt: &TypeTable, elem_type_id: TypeId) -> String {
    cm_stream_read_value_func_name(&tt.mangle_type_arg_for_generic(elem_type_id))
}

fn synthesize_stream_write_func(elem_type_id: TypeId, ctx: &SynthCtx) -> TirFunction {
    let cm_interface_registry = ctx.cm_interface_registry;
    let type_table = ctx.type_table;
    let list_type_id = type_table.borrow_mut().make_list(elem_type_id);
    let (func_name, write_name, elem_ast, elem_size, elem_align) = {
        let tt = type_table.borrow();
        let func_name = stream_write_func_name(&tt, elem_type_id);
        let payload = classify_stream_payload(&tt, elem_type_id);
        let write_name = CanonicalIntrinsic::StreamWrite(payload);
        let elem_ast = payload_ast_type(elem_type_id, &tt, cm_interface_registry);
        let (size, align) = cm_layout_i32(&elem_ast, cm_interface_registry);
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
        true,
    );

    // Lower the list into a CM element buffer: (ptr, element count).
    let lower_ctx = LowerContext {
        cm_interface_registry,
        type_table,
        wasi_package: "cli",
        names: CmStdlibNames::from_type_table(&type_table.borrow()),
    };
    let (lower_stmts, ptr_local, count_local) = synthesize_lower_list_to_buffer(
        &elem_ast,
        local_ref(data_idx, "data", list_type_id),
        &mut next_local,
        &mut locals,
        &lower_ctx,
    );
    stmts.extend(lower_stmts);

    // One copy, started. A reader may take only a prefix, and
    // `StartedWrite::settle` returns the rest of `data`.
    let started_type_id = type_table.borrow_mut().make_started_write(elem_type_id);
    let count = || local_ref(count_local, "$list_len", TypeTable::I32);
    let packed = cm_canonical_call(
        write_name,
        vec![
            local_ref(handle_idx, "handle", TypeTable::I32),
            local_ref(ptr_local, "$list_base", TypeTable::I32),
            copy_count(count()),
        ],
        TypeTable::I32,
    );
    let byte_count = binary(TirBinaryOp::Mul, count(), i32_const(elem_size), TypeTable::I32);
    stmts.push(return_stmt(Some(started_copy_literal(
        started_type_id,
        CompilerItem::StartedWrite,
        vec![
            (
                "buffer",
                result_buffer_literal(
                    local_ref(ptr_local, "$list_base", TypeTable::I32),
                    byte_count,
                    elem_align,
                    type_table,
                ),
            ),
            ("packed", packed),
            ("data", local_ref(data_idx, "data", list_type_id)),
        ],
        type_table,
    ))));

    binding_function(
        func_name,
        vec![
            binding_param("handle", handle_idx, TypeTable::I32),
            binding_param("data", data_idx, list_type_id),
        ],
        started_type_id,
        stmts,
        next_local,
        locals,
    )
}

/// `cm_await_blocked(handle, <copy>)`: the packed result of `copy`, awaited
/// first if it answered BLOCKED.
fn settled(copy: TirExpr, handle_idx: u32) -> TirExpr {
    internal_call(
        CompilerItem::CmAwaitBlocked.attr_name(),
        vec![local_ref(handle_idx, "handle", TypeTable::I32), copy],
        TypeTable::I32,
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
        Some("$freed".to_string()),
        TypeTable::I32,
        false,
    );
    let_stmt(
        "$freed",
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
    let (func_name, write_name, drop_name, cm_package, payload_ast, size, align, awaits_reader) = {
        let tt = type_table.borrow();
        let func_name = future_write_func_name(&tt, payload_type_id);
        let payload = classify_future_payload(&tt, payload_type_id);
        let write_name = CanonicalIntrinsic::FutureWrite(payload.clone());
        let drop_name = CanonicalIntrinsic::FutureDropWritable(payload.clone());
        let cm_package = future_payload_package(&payload);
        let payload_ast = payload_ast_type(payload_type_id, &tt, cm_interface_registry);
        let (size, align) = cm_layout_i32(&payload_ast, cm_interface_registry);
        let awaits_reader = payload.write_awaits_reader();
        (
            func_name,
            write_name,
            drop_name,
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
    stmts.extend(synthesize_lower_wasi_type_to_memory(
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
        Some("$written".to_string()),
        TypeTable::I32,
        false,
    );
    let written = || local_ref(written_idx, "$written", TypeTable::I32);
    let write = cm_canonical_call(
        write_name,
        vec![
            local_ref(handle_idx, "handle", TypeTable::I32),
            local_ref(ptr_idx, "ptr", TypeTable::I32),
        ],
        TypeTable::I32,
    );
    stmts.push(let_stmt(
        "$written",
        written_idx,
        TypeTable::I32,
        if awaits_reader {
            settled(write, handle_idx)
        } else {
            write
        },
    ));

    // The write consumes the end (WEP: Affine Types).
    let drop_end = expr_stmt(cm_canonical_call(
        drop_name,
        vec![local_ref(handle_idx, "handle", TypeTable::I32)],
        TypeTable::UNIT,
    ));
    if awaits_reader {
        stmts.push(free_cm_buffer(
            ptr_idx,
            size,
            align,
            &mut next_local,
            &mut locals,
        ));
        stmts.push(drop_end);
    } else {
        // An end whose copy is pending cannot be dropped, and waiting for the
        // reader would deadlock one in this instance, so the end is left to the
        // host, which delivers the value from the buffer and reports the write
        // `Pending` (WEP: Affine Types, known gaps). A write that finished
        // frees the buffer.
        let free = free_cm_buffer(ptr_idx, size, align, &mut next_local, &mut locals);
        stmts.push(if_stmt(
            is_blocked(written()),
            block(vec![]),
            Some(block(vec![free, drop_end])),
        ));
    }

    // The value is the reader's unless the reader dropped first. A binding is
    // synthesized after drops are elaborated, so a delivered `value` is never
    // dropped here.
    let (write_type_id, option_type_id) = {
        let mut tt = type_table.borrow_mut();
        (
            tt.make_future_write(payload_type_id),
            tt.make_option(payload_type_id),
        )
    };
    let rest_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("$rest".to_string()),
        option_type_id,
        true,
    );
    let rest = || local_ref(rest_idx, "$rest", option_type_id);
    let (none, some) = {
        let tt = type_table.borrow();
        let value = local_ref(value_idx, "value", payload_type_id);
        (
            option_none(option_type_id, tt.compiler_items()),
            option_some(value, option_type_id, tt.compiler_items()),
        )
    };
    stmts.push(let_mut_stmt("$rest", rest_idx, option_type_id, none));
    stmts.push(if_stmt(
        binary(
            TirBinaryOp::Eq,
            binary(
                TirBinaryOp::BitAnd,
                written(),
                i32_const(CM_PACKED_STATUS_MASK),
                TypeTable::I32,
            ),
            i32_const(CM_DROPPED),
            TypeTable::BOOL,
        ),
        block(vec![expr_stmt(assign(rest(), some))]),
        None,
    ));
    let write_name = type_table
        .borrow()
        .compiler_struct_name(CompilerItem::FutureWrite)
        .to_string();
    stmts.push(return_stmt(Some(struct_literal(
        write_type_id,
        write_name,
        [
            ("rest", rest()),
            ("result", future_write_result_of(written(), type_table)),
        ],
    ))));

    TirFunction {
        module_source: ModuleSource::default(),
        name: func_name,
        def_id: None,
        visibility: Visibility::Private,
        is_export: false,
        is_async: false,
        type_params: vec![],
        impl_type_params: vec![],
        impl_origin: None,
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
        return_type: write_type_id,
        task_return_type: None,
        effects: vec![],
        immediates: vec![],
        declared: None,
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
        return_abi: tir::ReturnAbi::default(),
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

/// The `$cm_future_read_*` helper name for a payload type.
fn future_read_func_name(tt: &TypeTable, payload_type_id: TypeId) -> String {
    cm_future_read_func_name(&tt.mangle_type_arg_for_generic(payload_type_id))
}

/// The CM package a future payload's named types resolve in, or `""` where the
/// payload names none and resolution must not be steered into one.
fn future_payload_package(payload: &CmFuturePayload) -> String {
    match payload {
        // Read off the declaration, never guessed: naming a package the payload
        // does not belong to resolves its types to that package's same-named ones.
        CmFuturePayload::Transmission(decl) => decl.cm_package().unwrap_or_default().to_string(),
        CmFuturePayload::Trailers => "http".to_string(),
        CmFuturePayload::Scalar(_) | CmFuturePayload::Value(_) => String::new(),
    }
}

fn synthesize_future_read_func(
    payload_type_id: TypeId,
    option_type_id: TypeId,
    ctx: &SynthCtx,
) -> TirFunction {
    let cm_interface_registry = ctx.cm_interface_registry;
    let type_table = ctx.type_table;
    let (func_name, read_name, drop_name, cm_package, payload_ast, size, align) = {
        let tt = type_table.borrow();
        let func_name = future_read_func_name(&tt, payload_type_id);
        let payload = classify_future_payload(&tt, payload_type_id);
        let read_name = CanonicalIntrinsic::FutureRead(payload.clone());
        let drop_name = CanonicalIntrinsic::FutureDropReadable(payload.clone());
        let cm_package = future_payload_package(&payload);
        let payload_ast = payload_ast_type(payload_type_id, &tt, cm_interface_registry);
        let (size, align) = cm_layout_i32(&payload_ast, cm_interface_registry);
        (
            func_name,
            read_name,
            drop_name,
            cm_package,
            payload_ast,
            size,
            align,
        )
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

    let status_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("status".to_string()),
        TypeTable::I32,
        false,
    );
    stmts.push(let_stmt(
        "status",
        status_idx,
        TypeTable::I32,
        settled(
            cm_canonical_call(
                read_name,
                vec![
                    local_ref(handle_idx, "handle", TypeTable::I32),
                    local_ref(ptr_idx, "ptr", TypeTable::I32),
                ],
                TypeTable::I32,
            ),
            handle_idx,
        ),
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

    // The read consumes the end (WEP: Affine Types); it is done, so it drops.
    stmts.push(expr_stmt(cm_canonical_call(
        drop_name,
        vec![local_ref(handle_idx, "handle", TypeTable::I32)],
        TypeTable::UNIT,
    )));

    stmts.push(return_stmt(Some(local_ref(
        result_idx,
        "result",
        option_type_id,
    ))));

    TirFunction {
        module_source: ModuleSource::default(),
        name: func_name,
        def_id: None,
        visibility: Visibility::Private,
        is_export: false,
        is_async: false,
        type_params: vec![],
        impl_type_params: vec![],
        impl_origin: None,
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
        immediates: vec![],
        declared: None,
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
        return_abi: tir::ReturnAbi::default(),
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
    let recv = peel_newtypes(tt, tt.peel_refs(receiver.type_id));
    tt.generic_type_args(recv)?.first().copied()
}

/// The `#[cm(...)]` name a call carries, if any.
fn cm_name_of(expr: &TirExpr) -> Option<&str> {
    let TirExprKind::Call { func, .. } = &expr.kind else {
        return None;
    };
    func.method_info.as_ref()?.cm_name.as_deref()
}

/// The element type of a `stream-read` or `stream-lift` call, the two halves
/// one synthesized pair binds, or `None` for anything else and for the `u8`
/// stream, which `core:rt` binds by hand.
fn stream_read_element(tt: &TypeTable, expr: &TirExpr) -> Option<TypeId> {
    if !matches!(cm_name_of(expr), Some("stream-read" | "stream-lift")) {
        return None;
    }
    let elem = stream_receiver_element(tt, expr)?;
    (!is_u8_stream_element(tt, elem)).then_some(elem)
}

/// The names of the two functions a stream read binds to.
struct StreamReadNames {
    /// Starts the copy: `(handle, max) -> StartedRead<T>`.
    begin: String,
    /// Lifts what it copied: `(ptr, count) -> List<T>`.
    lift: String,
}

/// Shared stream-read generator. `names` / `stream_read_name` / `payload_ast` /
/// `cm_package` are precomputed by the caller so this body serves both the
/// WASI-record path ([`synthesize_record_stream_reads`]) and the value-payload
/// path ([`synthesize_stream_reads`]). The begin function issues the copy into a
/// buffer and returns it unsettled, with the lift function `StartedRead::settle`
/// calls once it has.
fn synthesize_stream_read_funcs(
    names: StreamReadNames,
    stream_read_name: CanonicalIntrinsic,
    elem_type_id: TypeId,
    elem_size: i32,
    elem_align: i32,
    payload_ast: &Type,
    cm_package: &str,
    ctx: &SynthCtx,
) -> Vec<TirFunction> {
    let type_table = ctx.type_table;
    let started_type_id = type_table.borrow_mut().make_started_read(elem_type_id);

    let mut next_local: u32 = 0;
    let mut locals: Vec<TirLocal> = Vec::new();
    let mut stmts: Vec<TirStmt> = Vec::new();
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
    let byte_count_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("byte_count".to_string()),
        TypeTable::I32,
        false,
    );
    stmts.push(let_stmt(
        "byte_count",
        byte_count_idx,
        TypeTable::I32,
        buffer_bytes(
            copy_count(local_ref(max_idx, "max", TypeTable::I32)),
            elem_size,
        ),
    ));
    let byte_count = || local_ref(byte_count_idx, "byte_count", TypeTable::I32);
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
            vec![i32_const(0), i32_const(0), i32_const(elem_align), byte_count()],
            TypeTable::I32,
        ),
    ));
    let ptr = || local_ref(ptr_idx, "ptr", TypeTable::I32);
    let packed = cm_canonical_call(
        stream_read_name,
        vec![
            local_ref(handle_idx, "handle", TypeTable::I32),
            ptr(),
            copy_count(local_ref(max_idx, "max", TypeTable::I32)),
        ],
        TypeTable::I32,
    );
    let started = started_copy_literal(
        started_type_id,
        CompilerItem::StartedRead,
        vec![
            (
                "buffer",
                result_buffer_literal(ptr(), byte_count(), elem_align, type_table),
            ),
            ("packed", packed),
            (
                "items",
                list_with_capacity(elem_type_id, i32_const(0), type_table),
            ),
        ],
        type_table,
    );
    stmts.push(return_stmt(Some(started)));
    let begin = binding_function(
        names.begin,
        vec![
            binding_param("handle", handle_idx, TypeTable::I32),
            binding_param("max", max_idx, TypeTable::I32),
        ],
        started_type_id,
        stmts,
        next_local,
        locals,
    );
    let lift = synthesize_stream_lift_func(
        names.lift,
        elem_type_id,
        elem_size,
        payload_ast,
        cm_package,
        ctx,
    );
    vec![begin, lift]
}

/// `$cm_stream_lift_*(handle, ptr, count) -> List<T>`: the `count` elements a
/// read on `handle` copied to `ptr`, lifted one by one.
fn synthesize_stream_lift_func(
    name: String,
    elem_type_id: TypeId,
    elem_size: i32,
    payload_ast: &Type,
    cm_package: &str,
    ctx: &SynthCtx,
) -> TirFunction {
    let type_table = ctx.type_table;
    let array_type_id = type_table.borrow_mut().make_list(elem_type_id);
    let list_fq = CmStdlibNames::from_type_table(&type_table.borrow()).array_fq;
    let elem_fq = type_table.borrow().fq_type_name(elem_type_id);
    let list_template = |item| type_table.borrow().compiler_items().require_template(item);
    let list_method = |method: &str| {
        LocalMethodName::new(list_fq.clone(), None, method.to_string())
            .with_struct_type_args(std::slice::from_ref(&elem_fq))
    };

    let mut next_local: u32 = 0;
    let mut locals: Vec<TirLocal> = Vec::new();
    let mut stmts: Vec<TirStmt> = Vec::new();
    let handle_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("handle".to_string()),
        TypeTable::I32,
        false,
    );
    let ptr_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("ptr".to_string()),
        TypeTable::I32,
        false,
    );
    let count_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("count".to_string()),
        TypeTable::I32,
        false,
    );

    let arr_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("arr".to_string()),
        array_type_id,
        false,
    );
    let empty_arr = list_with_capacity(
        elem_type_id,
        local_ref(count_idx, "count", TypeTable::I32),
        type_table,
    );
    stmts.push(let_mut_stmt("arr", arr_idx, array_type_id, empty_arr));

    let i_idx = alloc_named_local(
        &mut next_local,
        &mut locals,
        Some("i".to_string()),
        TypeTable::I32,
        false,
    );
    stmts.push(let_mut_stmt("i", i_idx, TypeTable::I32, i32_const(0)));

    let mut loop_body_stmts = Vec::new();
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

    let lift_ctx = LiftContext {
        cm_interface_registry: ctx.cm_interface_registry,
        type_table,
        cm_package,
        interner: ctx.interner,
    };
    let lifted_elem = synthesize_lift(
        payload_ast,
        local_ref(addr_idx, "addr", TypeTable::I32),
        &mut next_local,
        &mut loop_body_stmts,
        &mut locals,
        &lift_ctx,
    );
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
                template: Some(list_template(CompilerItem::ListPush)),
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
    loop_body_stmts.push(expr_stmt(assign(
        local_ref(i_idx, "i", TypeTable::I32),
        binary_add(local_ref(i_idx, "i", TypeTable::I32), i32_const(1)),
    )));
    stmts.push(loop_stmt(TirBlock {
        stmts: loop_body_stmts,
        span: synth_span(),
    }));
    stmts.push(return_stmt(Some(local_ref(arr_idx, "arr", array_type_id))));

    binding_function(
        name,
        vec![
            binding_param("handle", handle_idx, TypeTable::I32),
            binding_param("ptr", ptr_idx, TypeTable::I32),
            binding_param("count", count_idx, TypeTable::I32),
        ],
        array_type_id,
        stmts,
        next_local,
        locals,
    )
}

/// `List::<Elem>::with_capacity(capacity)`. `List` is a declaration, so the
/// receiver its methods are named after carries it rather than the spelling
/// `List` happens to have.
fn list_with_capacity(
    elem_type_id: TypeId,
    capacity: TirExpr,
    type_table: &RefCell<TypeTable>,
) -> TirExpr {
    let array_type_id = type_table.borrow_mut().make_list(elem_type_id);
    let list_fq = CmStdlibNames::from_type_table(&type_table.borrow()).array_fq;
    let elem_fq = type_table.borrow().fq_type_name(elem_type_id);
    let with_capacity = LocalMethodName::new(list_fq.clone(), None, "with_capacity".to_string())
        .with_struct_type_args(std::slice::from_ref(&elem_fq));
    let template = type_table
        .borrow()
        .compiler_items()
        .require_template(CompilerItem::ListWithCapacity);
    TirExpr::new(
        TirExprKind::Call {
            func: Box::new(FunctionRef {
                module_source: ModuleSource::list(),
                name: with_capacity.to_mangled_name(),
                template: Some(template),
                monomorph_info: Some(MonomorphInfo {
                    generic_name: format!("{list_fq}::with_capacity"),
                    impl_type_args: vec![elem_type_id],
                    method_type_args: vec![],
                    is_blanket: false,
                }),
                method_info: Some(with_capacity),
            }),
            type_args: vec![],
            args: CallArgs::free(vec![CallArg::new(capacity, false)]),
        },
        array_type_id,
        synth_span(),
    )
}

/// `ResultBuffer { ptr, size, align }`: the linear memory a started copy holds
/// until it settles, freed as the buffer drops.
fn result_buffer_literal(
    ptr: TirExpr,
    size: TirExpr,
    align: i32,
    type_table: &RefCell<TypeTable>,
) -> TirExpr {
    let buffer_type = type_table.borrow_mut().make_result_buffer();
    struct_literal(
        buffer_type,
        "ResultBuffer",
        vec![("ptr", ptr), ("size", size), ("align", i32_const(align))],
    )
}

/// `StartedRead { buffer, packed, items }` / `StartedWrite { buffer, packed, data }`
/// — a copy the canonical answered `packed` for, not yet settled. Field order
/// follows the declaration.
fn started_copy_literal(
    type_id: TypeId,
    item: CompilerItem,
    fields: Vec<(&str, TirExpr)>,
    type_table: &RefCell<TypeTable>,
) -> TirExpr {
    let struct_name = type_table.borrow().compiler_struct_name(item).to_string();
    struct_literal(type_id, struct_name, fields)
}

fn binding_param(name: &str, local_index: u32, type_id: TypeId) -> TirParam {
    TirParam {
        name: name.to_string(),
        local_index,
        type_id,
        is_mut: false,
        is_mut_ref: false,
        span: synth_span(),
    }
}

/// A synthesized binding function of the entry module.
fn binding_function(
    name: String,
    params: Vec<TirParam>,
    return_type: TypeId,
    stmts: Vec<TirStmt>,
    local_count: u32,
    locals: Vec<TirLocal>,
) -> TirFunction {
    TirFunction {
        module_source: ModuleSource::default(),
        name,
        def_id: None,
        visibility: Visibility::Private,
        is_export: false,
        is_async: false,
        type_params: vec![],
        impl_type_params: vec![],
        impl_origin: None,
        monomorph_info: None,
        method_info: None,
        params,
        return_type,
        task_return_type: None,
        effects: vec![],
        immediates: vec![],
        declared: None,
        body: Some(TirBlock {
            stmts,
            span: synth_span(),
        }),
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
        return_abi: tir::ReturnAbi::default(),
    }
}

/// Synthesize `$cm_stream_read_val_<mangle>(handle, max) -> List<T>` for a
/// value-payload element type, delegating the read loop to the shared
/// [`synthesize_stream_read_func`]. The canonical name matches the `val-`
/// naming the codegen builds for `CmStreamPayload::Value`.
fn synthesize_stream_read_value_func(elem_type_id: TypeId, ctx: &SynthCtx) -> Vec<TirFunction> {
    let cm_interface_registry = ctx.cm_interface_registry;
    let type_table = ctx.type_table;
    let (names, read_name, payload_ast, elem_size, elem_align) = {
        let tt = type_table.borrow();
        let names = StreamReadNames {
            begin: stream_read_value_func_name(&tt, elem_type_id),
            lift: cm_stream_lift_value_func_name(&tt.mangle_type_arg_for_generic(elem_type_id)),
        };
        let payload = classify_stream_payload(&tt, elem_type_id);
        let read_name = CanonicalIntrinsic::StreamRead(payload);
        let payload_ast = payload_ast_type(elem_type_id, &tt, cm_interface_registry);
        let (size, align) = cm_layout_i32(&payload_ast, cm_interface_registry);
        (names, read_name, payload_ast, size, align)
    };

    synthesize_stream_read_funcs(
        names,
        read_name,
        elem_type_id,
        elem_size,
        elem_align,
        &payload_ast,
        "cli",
        ctx,
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
        "stream-lift" => CompilerItem::CmStreamLiftU8,
        "stream-write" => CompilerItem::CmStreamWriteU8,
        "stream-write-raw-all" => CompilerItem::CmStreamWriteRawAllU8,
        "error-context-new" => CompilerItem::CmErrorContextNew,
        "error-context-debug-message" => CompilerItem::CmErrorContextDebugMessage,
        "waitable-set-wait" => CompilerItem::CmWaitableSetWait,
        "waitable-set-poll" => CompilerItem::CmWaitableSetPoll,
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
    // A constructor's call carries the payload its name needs, so
    // `rewrite_cm_new` binds it.
    (!intrinsic.creates_pair()).then_some(BindingTarget::Canonical(intrinsic))
}

/// Rewrite all #[cm("...")] resource method calls in the project.
fn rewrite_cm_resource_methods(sites: &BindingSites<'_>) {
    let type_table = sites.type_table.borrow();
    for func_rc in &sites.functions {
        let mut func = func_rc.borrow_mut();
        let func = &mut *func;
        if let Some(body) = &mut func.body {
            CmMethodRewriter {
                tt: &type_table,
                entry_source: &sites.entry_module_source,
                cm_interface_registry: sites.cm_interface_registry,
                frame: FrameLocals::Function {
                    locals: &mut func.locals,
                    local_count: &mut func.local_count,
                },
            }
            .visit_block(body);
        }
    }
}

/// Rewrites `#[cm("...")]` resource method calls into the appropriate
/// raw / internal / entry-module call. Detection runs post-order — after the
/// exhaustive `TirMutVisitor` walk has rewritten any nested calls — matching
/// the previous hand-written walker's "recurse first, then rewrite" order.
struct CmMethodRewriter<'a> {
    tt: &'a TypeTable,
    entry_source: &'a ModuleSource,
    cm_interface_registry: &'a CmInterfaceRegistry,
    /// The enclosing frame's locals, which a constructor's split grows.
    frame: FrameLocals<'a>,
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
            _ => self.walk_stmt(stmt),
        }
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if let TirExprKind::Closure {
            params,
            body,
            body_locals,
            ..
        } = &mut expr.kind
        {
            CmMethodRewriter {
                tt: self.tt,
                entry_source: self.entry_source,
                cm_interface_registry: self.cm_interface_registry,
                frame: FrameLocals::Closure {
                    params: params.as_slice(),
                    body_locals,
                },
            }
            .visit_expr(body);
            return;
        }
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

        if matches!(cm_name.as_str(), "future-new" | "stream-new") {
            self.rewrite_cm_new(expr, cm_name == "future-new");
            return;
        }
        if cm_name == RESOURCE_DROP {
            self.rewrite_resource_drop(expr);
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
        // Value-payload stream reads and lifts call a generated per-element
        // pair; WASI-record ones fall through to the `$cm_stream_read_<record>`
        // path.
        let lifts = cm_name == "stream-lift";
        if let Some(elem_type_id) = stream_read_value_element(self.tt, expr) {
            let func_name = if lifts {
                cm_stream_lift_value_func_name(&self.tt.mangle_type_arg_for_generic(elem_type_id))
            } else {
                stream_read_value_func_name(self.tt, elem_type_id)
            };
            self.rewrite_call(expr, BindingTarget::Entry(func_name));
            return;
        }
        if let Some(elem_type_id) = record_stream_read_element(self.tt, expr) {
            let elem_name = self.tt.base_type_name(elem_type_id);
            let func_name = if lifts {
                cm_stream_lift_func_name(&elem_name)
            } else {
                cm_stream_read_func_name(&elem_name)
            };
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
                .is_some_and(|e| is_u8_stream_element(self.tt, e))
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
    /// `r.drop()` on a resource: the canonical `resource.drop` of its
    /// declaration, handed the box itself, which the drop zeroes.
    fn rewrite_resource_drop(&self, expr: &mut TirExpr) {
        let TirExprKind::Call { args, .. } = &mut expr.kind else {
            unreachable!("a `cm_name` is read off a call");
        };
        let receiver = std::mem::take(args)
            .into_vec()
            .into_iter()
            .next()
            .expect("a drop takes its resource")
            .expr;
        *expr = cm_canonical_call(
            CanonicalIntrinsic::ResourceDrop(resource_decl(
                receiver.type_id,
                self.tt,
                self.cm_interface_registry,
            )),
            vec![receiver],
            TypeTable::UNIT,
        );
    }

    /// Rewrite a CM call — `recv.method(args)` or `Type::method(args)` — to a
    /// builtin/internal call, casting each handle it passes to the scalar the
    /// canonical import takes.
    fn rewrite_call(&self, expr: &mut TirExpr, target: BindingTarget) {
        let TirExprKind::Call { args, .. } = &mut expr.kind else {
            return;
        };
        let taken_args: Vec<TirExpr> = std::mem::take(args)
            .into_vec()
            .into_iter()
            .map(|a| match handle_scalar(self.tt, a.expr.type_id) {
                Some(scalar) => cast(a.expr, scalar),
                None => a.expr,
            })
            .collect();

        *expr = match target {
            BindingTarget::Canonical(intrinsic) => {
                cm_canonical_call(intrinsic, taken_args, expr.type_id)
            }
            // A `core:rt` binding returns a resource as its bare handle.
            BindingTarget::Internal(item) => match handle_scalar(self.tt, expr.type_id) {
                Some(scalar) => cast(
                    internal_call(item.attr_name(), taken_args, scalar),
                    expr.type_id,
                ),
                None => internal_call(item.attr_name(), taken_args, expr.type_id),
            },
            BindingTarget::Entry(name) => {
                entry_call(&name, taken_args, expr.type_id, self.entry_source.clone())
            }
        };
    }
}

impl CmMethodRewriter<'_> {
    /// Rewrite a `Future::<T>::new()` / `Stream::<T>::new()` static call into
    /// the canonical that creates the pair, parameterized by the call's
    /// payload. The canonical returns both ends packed in an `i64`
    /// (`readable | writable << 32`), and each half is lifted to its end:
    /// `{ let packed = new(); [packed as i32 as R, (packed >> 32) as i32 as W] }`.
    fn rewrite_cm_new(&mut self, expr: &mut TirExpr, is_future: bool) {
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
        let canonical = if is_future {
            CanonicalIntrinsic::FutureNew(classify_future_payload(self.tt, payload_tid))
        } else {
            CanonicalIntrinsic::StreamNew(classify_stream_payload(self.tt, payload_tid))
        };
        let pair_type = expr.type_id;
        let Some(&[readable, writable]) = self.tt.as_tuple(pair_type).as_deref() else {
            unreachable!("a stream or future constructor returns its two ends");
        };
        let name = minted_name("cm_pair", self.frame.next_index());
        let packed = self.frame.alloc(name.clone(), TypeTable::I64);
        let half = |shift: i64, end: TypeId| {
            let packed = local_ref(packed, &name, TypeTable::I64);
            let bits = if shift == 0 {
                packed
            } else {
                binary(TirBinaryOp::Shr, packed, i64_const(shift), TypeTable::I64)
            };
            cast(cast(bits, TypeTable::I32), end)
        };
        let ends = TirExpr::new(
            TirExprKind::TupleLiteral {
                elements: vec![half(0, readable), half(32, writable)],
            },
            pair_type,
            synth_span(),
        );
        let stmts = vec![
            let_stmt(
                &name,
                packed,
                TypeTable::I64,
                cm_canonical_call(canonical, vec![], TypeTable::I64),
            ),
            expr_stmt(ends),
        ];
        *expr = TirExpr::new(TirExprKind::Block(block(stmts)), pair_type, synth_span());
    }
}

/// The `cm_name` of a resource's drop operation, which wado-from-idl declares
/// on every affine resource.
pub(crate) const RESOURCE_DROP: &str = "resource-drop";

/// The Component Model declaration of the resource `resource_type`.
fn resource_decl(
    resource_type: TypeId,
    tt: &TypeTable,
    cm_interface_registry: &CmInterfaceRegistry,
) -> CmDecl {
    let def = tt
        .nominal_def(tt.representation_head(resource_type))
        .expect("a resource is declared");
    let cm = cm_interface_registry
        .get_resource_cm_name_by_module(&tt.def_module(def).to_string(), tt.def_name(def))
        .expect("a dropped resource has a Component Model name");
    CmDecl::new(tt.defs(), def, cm)
}

/// `resource.drop()` on a resource of type `resource_type`: the call a user
/// writes, which the dispatch rewrite routes to an installed handler and this
/// pass binds otherwise. A scope-exit drop is one (WEP: Affine Types).
pub(crate) fn resource_drop_call(
    resource: TirExpr,
    resource_type: TypeId,
    tt: &TypeTable,
) -> TirExpr {
    let head = tt.representation_head(resource_type);
    let def = tt.nominal_def(head).expect("a resource is declared");
    let mut info = LocalMethodName::new(tt.fq_base_type_name(head), None, "drop".to_string());
    info.cm_name = Some(RESOURCE_DROP.to_string());
    let func = FunctionRef {
        module_source: tt.def_module(def).clone(),
        name: info.to_mangled_name(),
        template: None,
        monomorph_info: None,
        method_info: Some(info),
    };
    TirExpr::new(
        TirExprKind::method_call(Box::new(resource), func, vec![], vec![]),
        TypeTable::UNIT,
        synth_span(),
    )
}

/// `end.drop()` on a stream or future end of type `end_type`: the call a user
/// writes, which this pass binds once the end's payload is known. A scope-exit
/// drop is one (WEP: Affine Types).
pub(crate) fn end_drop_call(end: TirExpr, end_type: TypeId, tt: &TypeTable) -> TirExpr {
    let head = peel_newtypes(tt, end_type);
    let is = |item| tt.is_compiler_item_type(head, item);
    let cm_name = if is(CompilerItem::Stream) {
        "stream-drop-readable"
    } else if is(CompilerItem::StreamWritable) {
        "stream-drop-writable"
    } else if is(CompilerItem::Future) {
        "future-drop-readable"
    } else {
        assert!(
            is(CompilerItem::FutureWritable),
            "a generic resource is a stream or future end"
        );
        "future-drop-writable"
    };
    let def = tt.nominal_def(head).expect("an end is a declared resource");
    let type_args: Vec<_> = tt
        .generic_type_args(head)
        .expect("an end names its payload")
        .iter()
        .map(|arg| tt.fq_type_name(*arg))
        .collect();
    let mut info = LocalMethodName::new(tt.fq_base_type_name(head), None, "drop".to_string())
        .with_struct_type_args(&type_args);
    info.cm_name = Some(cm_name.to_string());
    let func = FunctionRef {
        module_source: tt.def_module(def).clone(),
        name: info.to_mangled_name(),
        template: None,
        monomorph_info: None,
        method_info: Some(info),
    };
    TirExpr::new(
        TirExprKind::method_call(Box::new(end), func, vec![], vec![]),
        TypeTable::UNIT,
        synth_span(),
    )
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
    let payload = classify_future_payload(tt, payload_tid);
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
    if is_u8_stream_element(tt, elem) {
        return None;
    }
    let payload = cm_payload_type_from_type_id(tt, elem).map_or_else(
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
                .unwrap_or_else(|| to_cm_name(&elem_name));
            CmStreamPayload::Record(stream_element_decl(tt, elem, &cm_elem))
        },
        CmStreamPayload::Value,
    );
    CanonicalIntrinsic::stream_op(cm_name, payload)
}

/// The declaration a `stream<T>` element names, under the CM name `cm_name`.
fn stream_element_decl(tt: &TypeTable, elem: TypeId, cm_name: &str) -> CmDecl {
    let def = tt
        .nominal_def(tt.representation_head(elem))
        .unwrap_or_else(|| {
            panic!(
                "`stream<{}>` element names no declaration",
                tt.type_name(elem)
            )
        });
    CmDecl::new(tt.defs(), def, cm_name)
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

#[cfg(test)]
mod cm_binding_tests {
    use super::*;
    use crate::ast::{Attribute, Item};
    use crate::parse;
    use crate::stdlib::all_core_modules;

    /// What each Component Model primitive binds to. The table is the
    /// specification; [`cm_binding_function`] derives it rather than listing it.
    /// `None` is left to a caller above it: the constructors to `rewrite_cm_new`,
    /// and the payload-carrying futures to the parameterized paths.
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
        ("stream-lift", Some("internal cm_stream_lift_u8")),
        ("stream-write", Some("internal cm_stream_write_u8")),
        (
            "stream-write-raw-all",
            Some("internal cm_stream_write_raw_all_u8"),
        ),
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
        ("subtask-drop", Some("canonical subtask-drop")),
        ("subtask-cancel", Some("canonical subtask-cancel")),
        ("error-context-new", Some("internal cm_error_context_new")),
        (
            "error-context-debug-message",
            Some("internal cm_error_context_debug_message"),
        ),
        ("error-context-drop", Some("canonical error-context-drop")),
    ];

    fn binding_of(cm_name: &str) -> Option<String> {
        Some(match cm_binding_function(cm_name)? {
            BindingTarget::Canonical(i) => format!("canonical {}", i.import_name()),
            BindingTarget::Internal(n) => format!("internal {n}"),
            BindingTarget::Entry(n) => format!("entry {n}"),
        })
    }

    /// Every `#[cm("…")]` the prelude declares on a method — the same
    /// attribute reader the elaborator records `cm_name` from.
    fn declared_primitives() -> Vec<String> {
        let source = all_core_modules()
            .iter()
            .find(|(import, _)| *import == "core:prelude/types.wado")
            .expect("the prelude declares the Component Model primitives")
            .1;
        parse(source)
            .ast
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Resource(decl) => Some(&decl.methods),
                Item::Impl(block) => Some(&block.methods),
                _ => None,
            })
            .flatten()
            .filter_map(|method| method.attrs.iter().find_map(Attribute::cm_identifier))
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
