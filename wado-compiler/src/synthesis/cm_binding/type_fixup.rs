//! The rewrite pointing each call of a CM import at its adapter, and the
//! retyping of that adapter from WASI-derived types to the call site's.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::Type;
use crate::component_model::{CmFunctionInfo, CmInterfaceRegistry, CmTypeKind};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::{DeclName, DeclPath, FqTypeName};
use crate::tir::{
    CallArg, FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal,
    TirStmt, TirStmtKind, TypeId, TypeTable,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};

use crate::synthesis::common::{cast, option_none, synth_span};

use super::import_adapter::is_gc_passthrough_param;
use super::types::{CmStdlibNames, cm_type_to_type_id, flatten_param_type, is_wasm_flat_type};

/// Replace the `TypeId` WASI-derived from `wasi_type` with `user_type` in the
/// binding, then likewise each type argument and tuple element beneath it.
fn replace_wasi_derived_type_recursive(
    adapter: &mut TirFunction,
    wasi_type: &Type,
    user_type: TypeId,
    ctx: &WasiRetype<'_>,
) {
    let old_type = {
        let mut tt = ctx.type_table.borrow_mut();
        cm_type_to_type_id(
            wasi_type,
            &mut tt,
            ctx.cm_interface_registry,
            ctx.wasi_package,
        )
    };
    if old_type != user_type && old_type != TypeTable::I32 && old_type != TypeTable::UNIT {
        let tt = ctx.type_table.borrow();
        let old_name = tt.mangle_type_name(old_type);
        // A newtype introduced into an adapter body breaks monomorphization and
        // WIR type lookup, so a user type that is one over the same base stays out.
        if old_name != tt.mangle_type_name_resolving_newtypes(user_type) {
            let rename = (old_name != tt.mangle_type_name(user_type))
                .then(|| (tt.fq_type_name(old_type), tt.fq_type_name(user_type)));
            drop(tt);
            replace_type_in_adapter(
                adapter,
                old_type,
                user_type,
                rename.as_ref().map(|(old_fq, new_fq)| (old_fq, new_fq)),
            );
        }
    }
    let user_parts = match wasi_type {
        Type::Tuple(elems) => ctx
            .type_table
            .borrow()
            .as_tuple(user_type)
            .map(|user_elems| (elems, user_elems)),
        Type::Generic(g)
            if ((g.name == ctx.names.array || g.name == ctx.names.option) && g.args.len() == 1)
                || (g.name == ctx.names.result && g.args.len() == 2)
                || ctx.names.is_tree_map(g) =>
        {
            ctx.type_table
                .borrow()
                .generic_type_args(user_type)
                .filter(|user_args| user_args.len() == g.args.len())
                .map(|user_args| (&g.args, user_args))
        }
        _ => None,
    };
    if let Some((wasi_parts, user_parts)) = user_parts {
        for (wasi_part, user_part) in wasi_parts.iter().zip(user_parts) {
            replace_wasi_derived_type_recursive(adapter, wasi_part, user_part, ctx);
        }
    }
}

/// What retyping an adapter from one WASI function's types reads.
struct WasiRetype<'a> {
    cm_interface_registry: &'a CmInterfaceRegistry,
    wasi_package: &'a str,
    type_table: &'a RefCell<TypeTable>,
    names: &'a CmStdlibNames,
}

/// Retype the binding body's WASI-derived types (`List<[String, List<u8>]>`) to
/// the call site's (`List<[FieldName, FieldValue]>`). `args` start at WASI param `param_offset`.
fn fixup_wasi_derived_types_in_adapter(
    adapter: &mut TirFunction,
    func_info: &CmFunctionInfo,
    args: &[CallArg],
    param_offset: usize,
    user_return_type: TypeId,
    type_table: &RefCell<TypeTable>,
    cm_interface_registry: &CmInterfaceRegistry,
    names: &CmStdlibNames,
) {
    // Synthesis typed a CM binding exactly; re-deriving can mint a different
    // TypeId for one type, as for `[Stream<T>, Future<Result<_, E>>]`.
    if adapter.is_cm_binding {
        return;
    }
    let ctx = WasiRetype {
        cm_interface_registry,
        wasi_package: &func_info.package,
        type_table,
        names,
    };
    for ((_, _, param_type), arg) in func_info.params[param_offset..].iter().zip(args) {
        let resolved = cm_interface_registry.value_type(param_type);
        replace_wasi_derived_type_recursive(adapter, &resolved, arg.expr.type_id, &ctx);
    }
    if let Some(return_type) = &func_info.return_type {
        let resolved = cm_interface_registry.value_type(return_type);
        replace_wasi_derived_type_recursive(adapter, &resolved, user_return_type, &ctx);
    }
}

/// Give the return value the caller's type, in place of the `TypeTable::I32`
/// the binding was built with.
fn fixup_return_type_in_body(adapter: &mut TirFunction, old_type: TypeId, new_type: TypeId) {
    if let Some(body) = &mut adapter.body {
        let from = raw_call_stmt_index(body);
        fixup_types_in_stmts(
            &mut body.stmts[from..],
            old_type,
            new_type,
            &mut adapter.locals,
        );
    }
}

/// Where the adapter's `CmRawCall` sits, or 0 where it makes none. A statement
/// before it lowers a parameter, so it never holds the result.
fn raw_call_stmt_index(body: &TirBlock) -> usize {
    struct FindRawCall {
        found: bool,
    }
    impl TirRefVisitor for FindRawCall {
        fn visit_expr(&mut self, expr: &TirExpr) {
            if matches!(expr.kind, TirExprKind::CmRawCall { .. }) {
                self.found = true;
            }
            self.walk_expr(expr);
        }
    }
    body.stmts
        .iter()
        .position(|stmt| {
            let mut finder = FindRawCall { found: false };
            finder.visit_stmt(stmt);
            finder.found
        })
        .unwrap_or(0)
}

/// Replace every `old_type` with `new_type` across the adapter's signature,
/// locals and body; `rename` also swaps the pair inside each callee's identity.
fn replace_type_in_adapter(
    adapter: &mut TirFunction,
    old_type: TypeId,
    new_type: TypeId,
    rename: Option<(&FqTypeName, &FqTypeName)>,
) {
    if adapter.return_type == old_type {
        adapter.return_type = new_type;
    }
    for param in &mut adapter.params {
        if param.type_id == old_type {
            param.type_id = new_type;
        }
    }
    for lt in &mut adapter.locals {
        if lt.type_id == old_type {
            lt.type_id = new_type;
        }
    }
    if let Some(body) = &mut adapter.body {
        TypeReplacer {
            old_type,
            new_type,
            rename,
        }
        .visit_block(body);
    }
}

/// Swaps `old_type` for `new_type` through a body. A callee keyed on the old
/// type, as a monomorphized `List<T>::with_capacity` is, takes `rename` too.
struct TypeReplacer<'a> {
    old_type: TypeId,
    new_type: TypeId,
    rename: Option<(&'a FqTypeName, &'a FqTypeName)>,
}

impl TypeReplacer<'_> {
    /// Apply the type swap (and optional rename) to a callee `FunctionRef`'s
    /// name and monomorphization type arguments.
    fn fix_func_ref(&self, func: &mut FunctionRef) {
        if let Some((old_fq, new_fq)) = self.rename
            && let Some(info) = &mut func.method_info
        {
            info.substitute_type(old_fq, new_fq);
        }
        if let Some(mono) = &mut func.monomorph_info {
            for ta in mono
                .impl_type_args
                .iter_mut()
                .chain(mono.method_type_args.iter_mut())
            {
                if *ta == self.old_type {
                    *ta = self.new_type;
                }
            }
        }
    }
}

impl TirMutVisitor for TypeReplacer<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        if let TirStmtKind::Let { type_id, .. } = &mut stmt.kind
            && *type_id == self.old_type
        {
            *type_id = self.new_type;
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if expr.type_id == self.old_type {
            expr.type_id = self.new_type;
        }
        match &mut expr.kind {
            TirExprKind::Call { func, .. } => {
                self.fix_func_ref(func);
            }
            TirExprKind::VariantConstruct { variant_type, .. } => {
                if *variant_type == self.old_type {
                    *variant_type = self.new_type;
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

/// Replace `old_type`, and the `TypeTable::I32` the binding used as its
/// placeholder, with `new_type` across returns, lets, and expressions.
fn fixup_types_in_stmts(
    stmts: &mut [TirStmt],
    old_type: TypeId,
    new_type: TypeId,
    locals: &mut Vec<TirLocal>,
) {
    for stmt in stmts {
        match &mut stmt.kind {
            TirStmtKind::Return {
                value: Some(ret_expr),
            } => {
                fixup_expr_type(ret_expr, old_type, new_type);
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                fixup_types_in_stmts(&mut then_block.stmts, old_type, new_type, locals);
                if let Some(blk) = else_block {
                    fixup_types_in_stmts(&mut blk.stmts, old_type, new_type, locals);
                }
            }
            TirStmtKind::Loop { body } => {
                fixup_types_in_stmts(&mut body.stmts, old_type, new_type, locals);
            }
            TirStmtKind::Let {
                value,
                local_index,
                type_id,
                ..
            } => {
                let idx = *local_index;
                fixup_adapter_let(value, idx, old_type, new_type, type_id, locals);
            }
            TirStmtKind::Expr(expr) => {
                fixup_adapter_expr(expr, old_type, new_type);
            }
            _ => {}
        }
    }
}

/// Retype a Let holding an adapter intermediate: a method-call result, or the
/// `ref.null` or `None` a lifted result starts from.
fn fixup_adapter_let(
    expr: &mut TirExpr,
    local_index: u32,
    old_type: TypeId,
    new_type: TypeId,
    let_type_id: &mut TypeId,
    locals: &mut [TirLocal],
) {
    let should_fix = expr.type_id == TypeTable::I32 || expr.type_id == old_type;
    let holds_intermediate = match &expr.kind {
        TirExprKind::Call { func, .. } => func.method_info.is_some(),
        TirExprKind::Null | TirExprKind::VariantConstruct { .. } => true,
        _ => false,
    };
    if !(should_fix && holds_intermediate) {
        return;
    }
    fixup_variant_construct(expr, old_type, new_type);
    expr.type_id = new_type;
    *let_type_id = new_type;
    locals[local_index as usize].type_id = new_type;
}

/// Fix up an expression statement (e.g., Assign with `VariantConstruct`).
fn fixup_adapter_expr(expr: &mut TirExpr, old_type: TypeId, new_type: TypeId) {
    if let TirExprKind::Assign { target, value } = &mut expr.kind {
        fixup_variant_construct(value, old_type, new_type);
        if target.type_id == TypeTable::I32 || target.type_id == old_type {
            target.type_id = new_type;
        }
    }
}

/// Fix up `VariantConstruct` expressions to use the real type.
fn fixup_variant_construct(expr: &mut TirExpr, old_type: TypeId, new_type: TypeId) {
    if let TirExprKind::VariantConstruct { variant_type, .. } = &mut expr.kind {
        if *variant_type == TypeTable::I32 || *variant_type == old_type {
            *variant_type = new_type;
        }
        if expr.type_id == TypeTable::I32 || expr.type_id == old_type {
            expr.type_id = new_type;
        }
    }
}

/// Retype a returned value, and the variant it constructs.
fn fixup_expr_type(expr: &mut TirExpr, old_type: TypeId, new_type: TypeId) {
    if expr.type_id == old_type || expr.type_id == TypeTable::I32 {
        expr.type_id = new_type;
    }
    fixup_variant_construct(expr, old_type, new_type);
}

/// Each `(local_index, type)` whose `Let` the rewrite retyped away from the
/// `TirLocal` the lower phase recorded.
pub(super) fn collect_local_type_updates(
    block: &TirBlock,
    locals: &[TirLocal],
    updates: &mut Vec<(usize, TypeId)>,
) {
    LocalTypeUpdateCollector { locals, updates }.visit_block(block);
}

/// Collects `(local_index, new_type)` for every `Let` whose binding type
/// drifted from its `TirLocal`.
struct LocalTypeUpdateCollector<'a> {
    locals: &'a [TirLocal],
    updates: &'a mut Vec<(usize, TypeId)>,
}

impl TirRefVisitor for LocalTypeUpdateCollector<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index,
                type_id,
                value,
                ..
            } => {
                let idx = *local_index as usize;
                if idx < self.locals.len() && self.locals[idx].type_id != *type_id {
                    self.updates.push((idx, *type_id));
                }
                self.visit_expr(value);
            }
            _ => self.walk_stmt(stmt),
        }
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        // A closure body owns a separate local-index space; descending into it
        // would collect `Let` indices that belong to the closure and wrongly
        // apply them to the enclosing function's `locals`.
        if matches!(expr.kind, TirExprKind::Closure { .. }) {
            return;
        }
        self.walk_expr(expr);
    }
}

pub(super) fn rewrite_calls_in_block(
    block: &mut TirBlock,
    adapters: &IndexMap<DeclPath, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &Rc<RefCell<TypeTable>>,
    applied_returns: &mut IndexMap<usize, TypeId>,
) {
    let names = CmStdlibNames::from_type_table(&type_table.borrow());
    CallRewriteWalker {
        adapters,
        entry_source,
        cm_interface_registry,
        type_table,
        applied_returns,
        names: &names,
    }
    .visit_block(block);
}

/// Points every call of a CM import at its adapter, retyped to the call site.
/// `applied_returns` records each adapter's retyped return across all sites.
struct CallRewriteWalker<'a> {
    adapters: &'a IndexMap<DeclPath, Rc<RefCell<TirFunction>>>,
    entry_source: &'a ModuleSource,
    cm_interface_registry: &'a CmInterfaceRegistry,
    type_table: &'a Rc<RefCell<TypeTable>>,
    applied_returns: &'a mut IndexMap<usize, TypeId>,
    names: &'a CmStdlibNames,
}

impl TirMutVisitor for CallRewriteWalker<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, type_id, .. } => {
                let old_type = value.type_id;
                self.visit_expr(value);
                // A streaming-binding rewrite can retype the let value (e.g. to
                // i32); keep the binding's recorded type in sync.
                if value.type_id != old_type {
                    *type_id = value.type_id;
                }
            }
            _ => self.walk_stmt(stmt),
        }
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        self.rewrite_expr(expr);
    }
}

/// Retype a shared adapter's return from a call site, leaving a streaming
/// adapter's i32 alone. Call sites that disagree are an ICE.
fn fixup_adapter_return_from_call_site(
    adapter: &mut TirFunction,
    adapter_key: usize,
    call_site_type: TypeId,
    is_streaming: bool,
    type_table: &RefCell<TypeTable>,
    applied_returns: &mut IndexMap<usize, TypeId>,
) {
    if is_streaming {
        return;
    }
    // A newtype over a CM resource (`type Trailers = Fields`) shares the base's
    // canonical binding, so normalize to the underlying resource.
    let call_site_type = {
        let tt = type_table.borrow();
        if matches!(tt.get(call_site_type), ResolvedType::Newtype { .. }) {
            tt.representation_head(call_site_type)
        } else {
            call_site_type
        }
    };
    if let Some(prev) = record_applied_return(applied_returns, adapter_key, call_site_type) {
        let tt = type_table.borrow();
        panic!(
            "call sites disagree on the return type of shared CM binding `{}`: \
             `{}` vs `{}`; one binding cannot satisfy both",
            adapter.name,
            tt.type_name(prev),
            tt.type_name(call_site_type)
        );
    }
    if adapter.return_type == call_site_type {
        return;
    }
    let old_return_type = adapter.return_type;
    adapter.return_type = call_site_type;
    fixup_return_type_in_body(adapter, old_return_type, call_site_type);
}

/// Record `call_site_type` as the applied return for `adapter_key`, or answer
/// the different type already recorded, leaving it in place.
fn record_applied_return(
    applied_returns: &mut IndexMap<usize, TypeId>,
    adapter_key: usize,
    call_site_type: TypeId,
) -> Option<TypeId> {
    match applied_returns.get(&adapter_key) {
        Some(&prev) if prev != call_site_type => Some(prev),
        _ => {
            applied_returns.insert(adapter_key, call_site_type);
            None
        }
    }
}

/// Retype one adapter param and its local from a call-site arg, except a flat
/// one, whose arg is cast instead, and a streaming adapter's i32.
fn fixup_adapter_param_from_call_site(
    adapter: &mut TirFunction,
    param_idx: usize,
    arg_type: TypeId,
    is_gc_passthrough: bool,
    is_streaming: bool,
) {
    let param = &mut adapter.params[param_idx];
    if param.type_id == arg_type {
        return;
    }
    if is_streaming && param.type_id == TypeTable::I32 {
        return;
    }
    if !is_gc_passthrough && is_wasm_flat_type(param.type_id) {
        return;
    }
    let local_idx = param.local_index as usize;
    param.type_id = arg_type;
    adapter.locals[local_idx].type_id = arg_type;
}

/// The args from WASI param `param_offset` on that reach an adapter param, each
/// with its adapter param index and WASI type. A param flattening to nothing has none.
fn adapter_params_of<'f, A>(
    func_info: &'f CmFunctionInfo,
    args: impl ExactSizeIterator<Item = A>,
    param_offset: usize,
    registry: &CmInterfaceRegistry,
    names: &CmStdlibNames,
) -> Vec<(usize, &'f Type, A)> {
    let wasi_params = &func_info.params[param_offset..];
    assert_eq!(
        args.len(),
        wasi_params.len(),
        "a CM import call passes one arg per WASI param"
    );
    let mut next_param = param_offset;
    wasi_params
        .iter()
        .zip(args)
        .filter_map(|((_, _, param_type), arg)| {
            let slots = flatten_param_type(param_type, registry, names).len();
            if slots == 0 {
                return None;
            }
            assert!(
                slots == 1 || is_gc_passthrough_param(param_type, registry, names),
                "a direct CM import param takes one flat slot, got {param_type:?}"
            );
            next_param += 1;
            Some((next_param - 1, param_type, arg))
        })
        .collect()
}

fn cast_in_place(expr: &mut TirExpr, target: TypeId) {
    let original = std::mem::replace(
        expr,
        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
    );
    *expr = cast(original, target);
}

/// Call-site args in the binding's param shape: a GC passthrough param passes
/// through, a bare `null` typed as its `None`. `args` start at WASI param `param_offset`.
fn flatten_call_site_args(
    func_info: &CmFunctionInfo,
    args: Vec<CallArg>,
    param_offset: usize,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &RefCell<TypeTable>,
    names: &CmStdlibNames,
) -> Vec<CallArg> {
    adapter_params_of(
        func_info,
        args.into_iter(),
        param_offset,
        cm_interface_registry,
        names,
    )
    .into_iter()
    .map(|(_, param_type, CallArg { expr: arg, .. })| {
        let arg = if matches!(arg.kind, TirExprKind::Null)
            && is_gc_passthrough_param(param_type, cm_interface_registry, names)
        {
            // The source null's inner type may be unknown; the registry's is not.
            let mut tt = type_table.borrow_mut();
            let option_type_id = cm_type_to_type_id(
                param_type,
                &mut tt,
                cm_interface_registry,
                &func_info.package,
            );
            option_none(option_type_id, tt.compiler_items())
        } else {
            arg
        };
        CallArg::new(arg, false)
    })
    .collect()
}

impl<'a> CallRewriteWalker<'a> {
    fn rewrite_expr(&mut self, expr: &mut TirExpr) {
        let adapters = self.adapters;
        let registry = self.cm_interface_registry;
        let Some((key, kind)) = cm_call_key(expr, self.type_table, registry, |key| {
            adapters.contains_key(key)
        }) else {
            self.walk_expr(expr);
            return;
        };
        let adapter = &adapters[&key];
        let func_info = self.cm_function(&key);
        let mut args = take_call_args(expr);
        let args = match kind {
            CmCallKind::Free => {
                let is_streaming = keeps_flat_i32_shape(func_info);
                self.fixup_adapter_from_call_site(
                    adapter,
                    func_info,
                    expr.type_id,
                    &args,
                    0,
                    is_streaming,
                );
                self.cast_args_to_adapter_params(adapter, func_info, &mut args, 0);
                if is_streaming {
                    for arg in &mut args {
                        if arg.expr.type_id != TypeTable::I32 {
                            cast_in_place(&mut arg.expr, TypeTable::I32);
                        }
                    }
                }
                args
            }
            CmCallKind::Method => {
                let is_streaming = keeps_flat_i32_shape(func_info);
                let receiver = args.remove(0).expr;
                self.fixup_adapter_from_call_site(
                    adapter,
                    func_info,
                    expr.type_id,
                    &args,
                    1,
                    is_streaming,
                );
                {
                    let mut adapter = adapter.borrow_mut();
                    fixup_adapter_param_from_call_site(
                        &mut adapter,
                        0,
                        receiver.type_id,
                        true,
                        false,
                    );
                    fixup_wasi_derived_types_in_adapter(
                        &mut adapter,
                        func_info,
                        &args,
                        1,
                        expr.type_id,
                        self.type_table,
                        registry,
                        self.names,
                    );
                }
                self.cast_args_to_adapter_params(adapter, func_info, &mut args, 1);
                let mut all_args = vec![CallArg::new(receiver, false)];
                all_args.extend(flatten_call_site_args(
                    func_info,
                    args,
                    1,
                    registry,
                    self.type_table,
                    self.names,
                ));
                all_args
            }
            CmCallKind::Static => {
                self.fixup_adapter_from_call_site(
                    adapter,
                    func_info,
                    expr.type_id,
                    &args,
                    0,
                    false,
                );
                fixup_wasi_derived_types_in_adapter(
                    &mut adapter.borrow_mut(),
                    func_info,
                    &args,
                    0,
                    expr.type_id,
                    self.type_table,
                    registry,
                    self.names,
                );
                flatten_call_site_args(func_info, args, 0, registry, self.type_table, self.names)
            }
        };
        self.retarget_to_adapter(expr, adapter, args);
    }

    fn cm_function(&self, key: &DeclPath) -> &'a CmFunctionInfo {
        self.cm_interface_registry
            .get_function(key)
            .expect("an adapter exists only for a registered CM function")
    }

    /// Retype `adapter` to one call site: its return, and each param from the
    /// arg that reaches it. `args` start at WASI param `param_offset`.
    fn fixup_adapter_from_call_site(
        &mut self,
        adapter: &Rc<RefCell<TirFunction>>,
        func_info: &CmFunctionInfo,
        call_site_type: TypeId,
        args: &[CallArg],
        param_offset: usize,
        is_streaming: bool,
    ) {
        // The adapter's name is a non-injective `interface_method` join.
        let adapter_key = Rc::as_ptr(adapter) as usize;
        let mut adapter = adapter.borrow_mut();
        fixup_adapter_return_from_call_site(
            &mut adapter,
            adapter_key,
            call_site_type,
            is_streaming,
            self.type_table,
            self.applied_returns,
        );
        let registry = self.cm_interface_registry;
        for (param_idx, param_type, arg) in
            adapter_params_of(func_info, args.iter(), param_offset, registry, self.names)
        {
            fixup_adapter_param_from_call_site(
                &mut adapter,
                param_idx,
                arg.expr.type_id,
                is_gc_passthrough_param(param_type, registry, self.names),
                is_streaming,
            );
        }
    }

    /// Cast each arg whose adapter param is flat, and so authoritative: an i32
    /// literal where the ABI wants i64. `args` start at WASI param `param_offset`.
    fn cast_args_to_adapter_params(
        &self,
        adapter: &RefCell<TirFunction>,
        func_info: &CmFunctionInfo,
        args: &mut [CallArg],
        param_offset: usize,
    ) {
        let adapter = adapter.borrow();
        for (param_idx, _, arg) in adapter_params_of(
            func_info,
            args.iter_mut(),
            param_offset,
            self.cm_interface_registry,
            self.names,
        ) {
            let param_type = adapter.params[param_idx].type_id;
            if param_type != arg.expr.type_id && is_wasm_flat_type(param_type) {
                cast_in_place(&mut arg.expr, param_type);
            }
        }
    }

    /// Replace `expr` with a call of `adapter` on `args`, then rewrite inside them.
    fn retarget_to_adapter(
        &mut self,
        expr: &mut TirExpr,
        adapter: &RefCell<TirFunction>,
        mut args: Vec<CallArg>,
    ) {
        let func = Box::new(FunctionRef::from_resolved(
            &adapter.borrow(),
            self.entry_source.clone(),
        ));
        for arg in &mut args {
            self.visit_expr(&mut arg.expr);
        }
        expr.kind = TirExprKind::Call {
            func,
            args,
            type_args: vec![],
            has_receiver: false,
        };
    }
}

fn take_call_args(expr: &mut TirExpr) -> Vec<CallArg> {
    let TirExprKind::Call { args, .. } = &mut expr.kind else {
        unreachable!("only a call is retargeted to an adapter");
    };
    std::mem::take(args)
}

/// Whether the adapter keeps its i32 params and return at every call site: a
/// sync import taking a stream or future.
fn keeps_flat_i32_shape(func_info: &CmFunctionInfo) -> bool {
    !func_info.is_async && func_info.has_streaming_param()
}

/// How a call reaches its CM import: as a free function, or a resource method
/// through a receiver or statically.
enum CmCallKind {
    Free,
    Method,
    Static,
}

/// The registry key of the CM import `expr` calls, where `is_bound` accepts it.
fn cm_call_key(
    expr: &TirExpr,
    type_table: &RefCell<TypeTable>,
    registry: &CmInterfaceRegistry,
    is_bound: impl Fn(&DeclPath) -> bool,
) -> Option<(DeclPath, CmCallKind)> {
    let TirExprKind::Call { func, .. } = &expr.kind else {
        return None;
    };
    // The same-source check keeps a same-named local function from being taken
    // for a world import.
    let world_import = (registry.world_import_source(&func.name) == Some(&func.module_source))
        .then(|| DeclPath::from_declared(&func.name));
    let interface_function = func
        .module_source
        .interface_name()
        .map(|interface| DeclPath::method_of(&DeclName::new(interface), &func.name));
    if let Some(key) = world_import
        .into_iter()
        .chain(interface_function)
        .find(|key| is_bound(key))
    {
        return Some((key, CmCallKind::Free));
    }
    if let Some((receiver, func, _)) = expr.kind.as_method_call()
        && let Some(key) = cm_method_call_key(receiver, func, type_table, registry, &is_bound)
    {
        return Some((key, CmCallKind::Method));
    }
    cm_static_method_key(func)
        .filter(|key| is_bound(key))
        .map(|key| (key, CmCallKind::Static))
}

/// The registry key, `Resource::method`, of a `#[cm]` static call.
fn cm_static_method_key(func: &FunctionRef) -> Option<DeclPath> {
    let info = func.method_info.as_ref().filter(|i| i.cm_name.is_some())?;
    Some(DeclPath::method_of(
        &info.receiver_decl_name(),
        &info.method_name,
    ))
}

/// The registry key of a `#[cm]` method call that `is_bound` accepts: the
/// receiver's declared `Resource::method`, or the one its bundled alias names.
fn cm_method_call_key(
    receiver: &TirExpr,
    func: &FunctionRef,
    type_table: &RefCell<TypeTable>,
    registry: &CmInterfaceRegistry,
    is_bound: impl Fn(&DeclPath) -> bool,
) -> Option<DeclPath> {
    let info = func.method_info.as_ref().filter(|i| i.cm_name.is_some())?;
    // A mangled head carries the declaring module the registry never stores.
    let head = {
        let tt = type_table.borrow();
        tt.impl_receiver_key(tt.peel_refs(receiver.type_id))
            .decl_key()
    };
    let qualified = DeclPath::method_of(&head, &info.method_name);
    if is_bound(&qualified) {
        return Some(qualified);
    }
    let source = registry.find_binding_source(CmTypeKind::Newtype, head.as_decl_str())?;
    let Some(Type::Named(resolved)) = registry.get_newtype_by_source(source, &head) else {
        return None;
    };
    let aliased = DeclPath::method_of(&DeclName::new(&resolved.name), &info.method_name);
    is_bound(&aliased).then_some(aliased)
}

pub(super) fn collect_effect_calls_in_block(
    block: &TirBlock,
    effects: &mut IndexSet<DeclPath>,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &RefCell<TypeTable>,
) {
    EffectCallCollector {
        effects,
        cm_interface_registry,
        type_table,
    }
    .visit_block(block);
}

/// Records the registry key of every CM import call, for
/// [`super::generate_adapters`] to synthesize an adapter for each.
struct EffectCallCollector<'a> {
    effects: &'a mut IndexSet<DeclPath>,
    cm_interface_registry: &'a CmInterfaceRegistry,
    type_table: &'a RefCell<TypeTable>,
}

impl TirRefVisitor for EffectCallCollector<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        let registry = self.cm_interface_registry;
        if let Some((key, _)) = cm_call_key(expr, self.type_table, registry, |key| {
            registry.get_function(key).is_some()
        }) {
            self.effects.insert(key);
        }
        self.walk_expr(expr);
    }
}

#[cfg(test)]
mod tests {
    use super::record_applied_return;
    use crate::hashmap::IndexMap;
    use crate::tir::TypeId;

    #[test]
    fn record_applied_return_first_and_matching_are_ok() {
        let mut m: IndexMap<usize, TypeId> = IndexMap::default();
        assert_eq!(record_applied_return(&mut m, 1, TypeId(10)), None);
        // Same key, same type re-records without conflict.
        assert_eq!(record_applied_return(&mut m, 1, TypeId(10)), None);
        // A distinct adapter key is independent.
        assert_eq!(record_applied_return(&mut m, 2, TypeId(20)), None);
    }

    #[test]
    fn record_applied_return_detects_conflict() {
        let mut m: IndexMap<usize, TypeId> = IndexMap::default();
        assert_eq!(record_applied_return(&mut m, 1, TypeId(10)), None);
        // Same key, different type → the earlier type is reported and the map
        // is left unchanged so the caller can ICE with both names.
        assert_eq!(
            record_applied_return(&mut m, 1, TypeId(11)),
            Some(TypeId(10))
        );
        assert_eq!(m.get(&1), Some(&TypeId(10)));
    }
}
