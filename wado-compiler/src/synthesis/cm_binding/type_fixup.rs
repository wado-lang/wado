//! The rewrite pointing each call of a CM import at its adapter, retyped to
//! the call site.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::Type;
use crate::component_model::{CmFunctionInfo, CmInterfaceRegistry, CmTypeKind};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::{DeclName, DeclPath};
use crate::tir::{
    CallArg, FunctionRef, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal,
    TirStmt, TirStmtKind, TypeId, TypeTable,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};

<<<<<<< HEAD
use crate::synthesis::common::{cast, option_none};
||||||| 014361be8
use crate::synthesis::common::{cast, option_none, synth_span};
=======
use crate::synthesis::common::{cast, internal_call, option_none, synth_span};
>>>>>>> origin/main

use super::callback_export::{Callbacks, erased_callback_type};
use super::import_adapter::is_gc_passthrough_param;
use super::types::{CmStdlibNames, cm_type_to_type_id, flatten_param_type, is_wasm_flat_type};

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

/// Replace `old_type`, and the `TypeTable::I32` the binding used as its
/// placeholder, with `new_type` across returns, lets, and expressions.
fn fixup_types_in_stmts(
    stmts: &mut [TirStmt],
    old_type: TypeId,
    new_type: TypeId,
    locals: &mut [TirLocal],
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

pub(super) fn rewrite_calls_in_block(
    block: &mut TirBlock,
    adapters: &IndexMap<DeclPath, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &Rc<RefCell<TypeTable>>,
    applied_returns: &mut IndexMap<usize, TypeId>,
    callbacks: &mut Callbacks,
) {
<<<<<<< HEAD
||||||| 014361be8
    // The stdlib-name snapshot is invariant across the whole walk; build it
    // once here instead of once per expression node in `rewrite_calls_in_expr`.
=======
    // The stdlib-name snapshot is invariant across the whole walk; build it
    // once here instead of once per expression node in `rewrite_expr`.
>>>>>>> origin/main
    let names = CmStdlibNames::from_type_table(&type_table.borrow());
    CallRewriteWalker {
        adapters,
        entry_source,
        cm_interface_registry,
        type_table,
        applied_returns,
        callbacks,
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
    /// The type of every closure an import takes, which the host calls back.
    callbacks: &'a mut Callbacks,
    names: &'a CmStdlibNames,
}

impl TirMutVisitor for CallRewriteWalker<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        self.rewrite_expr(expr);
    }
}

/// Retype a shared adapter's return from a call site. Call sites that disagree
/// are an ICE.
fn fixup_adapter_return_from_call_site(
    adapter: &mut TirFunction,
    adapter_key: usize,
    call_site_type: TypeId,
    type_table: &RefCell<TypeTable>,
    applied_returns: &mut IndexMap<usize, TypeId>,
) {
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
    let same_type = |a, b| {
        let tt = type_table.borrow();
        tt.type_key(a) == tt.type_key(b)
    };
    if let Some(prev) =
        record_applied_return(applied_returns, adapter_key, call_site_type, same_type)
    {
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
    same_type: impl Fn(TypeId, TypeId) -> bool,
) -> Option<TypeId> {
    match applied_returns.get(&adapter_key) {
        Some(&prev) if !same_type(prev, call_site_type) => Some(prev),
        _ => {
            applied_returns.insert(adapter_key, call_site_type);
            None
        }
    }
}

/// Retype one adapter param and its local from a call-site arg, except a flat
/// one, whose arg is cast instead.
fn fixup_adapter_param_from_call_site(
    adapter: &mut TirFunction,
    param_idx: usize,
    arg_type: TypeId,
    is_gc_passthrough: bool,
) {
    let param = &mut adapter.params[param_idx];
    if param.type_id == arg_type {
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

impl CallRewriteWalker<'_> {
<<<<<<< HEAD
    fn rewrite_expr(&mut self, expr: &mut TirExpr) {
        let adapters = self.adapters;
        let Some((key, kind)) =
            cm_call_key(expr, self.type_table, self.cm_interface_registry, |key| {
                adapters.contains_key(key)
            })
        else {
            self.walk_expr(expr);
            return;
        };
        let adapter = &adapters[&key];
        let func_info = self
            .cm_interface_registry
            .get_function(&key)
            .expect("an adapter exists only for a registered CM function");
        let mut args = take_call_args(expr);
        let receiver = matches!(kind, CmCallKind::Method).then(|| args.remove(0));
        let args = {
            // The adapter's name is a non-injective `interface_method` join.
            let adapter_key = Rc::as_ptr(adapter) as usize;
            let mut adapter = adapter.borrow_mut();
            fixup_adapter_return_from_call_site(
                &mut adapter,
                adapter_key,
                expr.type_id,
                self.type_table,
                self.applied_returns,
            );
            if let Some(receiver) = &receiver {
                fixup_adapter_param_from_call_site(&mut adapter, 0, receiver.expr.type_id, true);
||||||| 014361be8
fn rewrite_calls_in_expr(
    expr: &mut TirExpr,
    adapters: &IndexMap<DeclPath, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &Rc<RefCell<TypeTable>>,
    applied_returns: &mut IndexMap<usize, TypeId>,
    names: &CmStdlibNames,
) {
    // World function (Phase 9): retarget the bare call to its synthesized adapter.
    if let TirExprKind::Call {
        func,
        args,
        type_args,
        ..
    } = &mut expr.kind
        && cm_interface_registry.world_import_source(&func.name) == Some(&func.module_source)
        && let Some(adapter_rc) = adapters.get(&DeclPath::from_declared(&func.name))
    {
        {
            let adapter_key = Rc::as_ptr(adapter_rc) as usize;
            let mut adapter = adapter_rc.borrow_mut();
            fixup_adapter_return_from_call_site(
                &mut adapter,
                adapter_key,
                expr.type_id,
                false,
                type_table,
                applied_returns,
            );
            let wasi_func =
                cm_interface_registry.get_function(&DeclPath::from_declared(&func.name));
            for (i, arg) in args.iter().enumerate() {
                let is_gc_passthrough = wasi_func.is_some_and(|f| {
                    i < f.params.len()
                        && is_gc_passthrough_param(&f.params[i].2, cm_interface_registry, names)
                });
                fixup_adapter_param_from_call_site(
                    &mut adapter,
                    i,
                    arg.expr.type_id,
                    is_gc_passthrough,
                    false,
                );
=======
    /// Replace each closure `func_info` takes with its `u32` key. `args` starts at
    /// parameter `param_offset`.
    fn key_callbacks<'e>(
        &mut self,
        func_info: &CmFunctionInfo,
        args: impl Iterator<Item = &'e mut TirExpr>,
        param_offset: usize,
    ) {
        for (i, arg) in args.enumerate() {
            if func_info.callback_at(i + param_offset).is_none() {
                continue;
>>>>>>> origin/main
            }
<<<<<<< HEAD
            let param_offset = usize::from(receiver.is_some());
            let adapted = self.adapt_args(&mut adapter, func_info, args, param_offset);
            receiver.into_iter().chain(adapted).collect()
        };
        self.retarget_to_adapter(expr, adapter, args);
||||||| 014361be8
        }
        cast_args_to_adapter_params(&adapter_rc.borrow(), args, 0);
        **func = FunctionRef::from_resolved(&adapter_rc.borrow(), entry_source.clone());
        *type_args = vec![];
        for arg in args {
            rewrite_calls_in_expr(
                &mut arg.expr,
                adapters,
                entry_source,
                cm_interface_registry,
                type_table,
                applied_returns,
                names,
            );
        }
        return;
=======
            // A diverging argument never reaches the call.
            let table = self.type_table.borrow();
            if !table.is_never(arg.type_id) {
                self.callbacks
                    .insert(table.type_key(arg.type_id), arg.type_id);
            }
            drop(table);
            let erased = erased_callback_type(&mut self.type_table.borrow_mut());
            let closure = std::mem::replace(
                arg,
                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
            );
            *arg = internal_call(
                "cm_callback_key",
                vec![cast(closure, erased)],
                TypeTable::U32,
            );
        }
>>>>>>> origin/main
    }

<<<<<<< HEAD
    /// Call-site args in the adapter's param shape, from WASI param
    /// `param_offset` on: a GC param retyped to its arg, a flat one's arg cast.
    fn adapt_args(
        &self,
        adapter: &mut TirFunction,
        func_info: &CmFunctionInfo,
        args: Vec<CallArg>,
        param_offset: usize,
    ) -> Vec<CallArg> {
        let registry = self.cm_interface_registry;
        adapter_params_of(
            func_info,
            args.into_iter(),
            param_offset,
            registry,
            self.names,
        )
        .into_iter()
        .map(|(param_idx, param_type, CallArg { expr: arg, .. })| {
            let is_gc_passthrough = is_gc_passthrough_param(param_type, registry, self.names);
            let arg = if is_gc_passthrough && matches!(arg.kind, TirExprKind::Null) {
                // The source null's inner type may be unknown; the registry's is not.
                let mut tt = self.type_table.borrow_mut();
                let option_type_id =
                    cm_type_to_type_id(param_type, &mut tt, registry, &func_info.package);
                option_none(option_type_id, tt.compiler_items())
            } else {
                arg
            };
            fixup_adapter_param_from_call_site(adapter, param_idx, arg.type_id, is_gc_passthrough);
            let param_type = adapter.params[param_idx].type_id;
            let arg = if arg.type_id == param_type {
                arg
            } else {
                cast(arg, param_type)
            };
            CallArg::new(arg, false)
        })
        .collect()
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
||||||| 014361be8
    // Check if this is an effect-like Call that should be rewritten
    let is_effect_call = matches!(&expr.kind, TirExprKind::Call { func, .. }
        if func.module_source.clone().is_effect_like() && func.module_source.clone().interface_name().is_some());
    if is_effect_call
        && let TirExprKind::Call {
=======
    fn rewrite_expr(&mut self, expr: &mut TirExpr) {
        let (adapters, entry_source, cm_interface_registry, type_table, names) = (
            self.adapters,
            self.entry_source,
            self.cm_interface_registry,
            self.type_table,
            self.names,
        );
        // World function (Phase 9): retarget the bare call to its synthesized adapter.
        if let TirExprKind::Call {
>>>>>>> origin/main
            func,
            args,
<<<<<<< HEAD
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

/// Whether a CM import call passes its receiver apart from the args it
/// lowers: a resource method called through one does.
enum CmCallKind {
    Function,
    Method,
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
        return Some((key, CmCallKind::Function));
    }
    if let Some((receiver, func, _)) = expr.kind.as_method_call()
        && let Some(key) = cm_method_call_key(receiver, func, type_table, registry, &is_bound)
    {
        return Some((key, CmCallKind::Method));
||||||| 014361be8
            type_args,
            ..
        } = &mut expr.kind
    {
        let interface_name = func
            .module_source
            .clone()
            .interface_name()
            .unwrap_or_default();
        let method_name = func.name.clone();
        let qualified = DeclPath::from_declared(format!("{interface_name}::{method_name}"));

        if let Some(adapter_rc) = adapters.get(&qualified) {
            // Check if this is a streaming async function
            let is_streaming = cm_interface_registry
                .get_function(&qualified)
                .is_some_and(|f| !f.is_async && f.has_streaming_param());

            // Fix up binding function types from the call site
            {
                let adapter_key = Rc::as_ptr(adapter_rc) as usize;
                let mut adapter = adapter_rc.borrow_mut();
                fixup_adapter_return_from_call_site(
                    &mut adapter,
                    adapter_key,
                    expr.type_id,
                    is_streaming,
                    type_table,
                    applied_returns,
                );
                let wasi_func = cm_interface_registry.get_function(&qualified);
                for (i, arg) in args.iter().enumerate() {
                    let is_gc_passthrough = wasi_func.is_some_and(|f| {
                        i < f.params.len()
                            && is_gc_passthrough_param(&f.params[i].2, cm_interface_registry, names)
                    });
                    fixup_adapter_param_from_call_site(
                        &mut adapter,
                        i,
                        arg.expr.type_id,
                        is_gc_passthrough,
                        is_streaming,
                    );
                }
            }

            // Cast call-site args to match adapter param types when they differ
            // (e.g., i32 literal → i64 for Duration newtype)
            cast_args_to_adapter_params(&adapter_rc.borrow(), args, 0);

            // For streaming adapters, cast GC ref args to i32
            if is_streaming {
                for arg in args.iter_mut() {
                    if arg.expr.type_id != TypeTable::I32 {
                        let original = std::mem::replace(
                            &mut arg.expr,
                            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
                        );
                        arg.expr = cast(original, TypeTable::I32);
                    }
                }
            }

            // Rewrite to call the binding function
            **func = FunctionRef::from_resolved(&adapter_rc.borrow(), entry_source.clone());
            *type_args = vec![];

            // Recurse into args
            for arg in args {
                rewrite_calls_in_expr(
                    &mut arg.expr,
                    adapters,
                    entry_source,
                    cm_interface_registry,
                    type_table,
                    applied_returns,
                    names,
                );
            }
            return;
        }
    }

    // Check if this is a resource method call that should be rewritten to target a binding
    if let Some((receiver, func, _)) = expr.kind.as_method_call()
        && let Some(method_info) = func.method_info.clone()
    {
        // The adapter map is keyed by the declared `Resource::method`, the same
        // key `EffectCallCollector` inserted under.
        let head = {
            let tt = type_table.borrow();
            tt.impl_receiver_key(tt.peel_refs(receiver.type_id))
                .decl_key()
        };
        let mut qualified = DeclPath::method_of(&head, &method_info.method_name);
        // Resolve through type aliases (e.g., Headers -> Fields), scoped to the
        // bundled CM namespaces.
        if !adapters.contains_key(&qualified)
            && let Some(source) =
                cm_interface_registry.find_binding_source(CmTypeKind::Newtype, head.as_decl_str())
            && let Some(Type::Named(resolved)) =
                cm_interface_registry.get_newtype_by_source(source, &head)
        {
            let aliased =
                DeclPath::method_of(&DeclName::new(&resolved.name), &method_info.method_name);
            if adapters.contains_key(&aliased) {
                qualified = aliased;
            }
        }
        if let Some(adapter_rc) = adapters.get(&qualified) {
            // Check if this is a streaming async function
            let is_streaming = cm_interface_registry
                .get_function(&qualified)
                .is_some_and(|f| !f.is_async && f.has_streaming_param());

            // Extract receiver and args before replacing
            let (taken_receiver, mut taken_args) = if let TirExprKind::Call {
                args,
                has_receiver: true,
                ..
            } = &mut expr.kind
            {
                let mut taken = std::mem::take(args);
                let receiver = taken.remove(0).expr;
                (receiver, taken)
            } else {
                unreachable!()
            };

            // Fix up binding function types from the call site
            // The binding params include self as the first param
            {
                let adapter_key = Rc::as_ptr(adapter_rc) as usize;
                let mut adapter = adapter_rc.borrow_mut();
                fixup_adapter_return_from_call_site(
                    &mut adapter,
                    adapter_key,
                    expr.type_id,
                    is_streaming,
                    type_table,
                    applied_returns,
                );
                // Self param (index 0): unconditional retype from the receiver.
                fixup_adapter_param_from_call_site(
                    &mut adapter,
                    0,
                    taken_receiver.type_id,
                    true,
                    false,
                );
                // Remaining params: the i-th arg corresponds to WASI param
                // index i+1 (WASI params include self at index 0).
                let method_func = cm_interface_registry.get_function(&qualified);
                for (i, arg) in taken_args.iter().enumerate() {
                    let wasi_param_idx = i + 1;
                    let is_gc_passthrough = method_func.is_some_and(|f| {
                        wasi_param_idx < f.params.len()
                            && is_gc_passthrough_param(
                                &f.params[wasi_param_idx].2,
                                cm_interface_registry,
                                names,
                            )
                    });
                    fixup_adapter_param_from_call_site(
                        &mut adapter,
                        i + 1,
                        arg.expr.type_id,
                        is_gc_passthrough,
                        is_streaming,
                    );
                }
                // Replace WASI-derived types in the body (including function names).
                // Skip for CM bindings — their types were set precisely by synthesis
                // and the recursive replacement can produce TypeId mismatches for
                // complex return types like [Stream<T>, Future<Result<_, E>>].
                if !adapter.is_cm_binding
                    && let Some(func_info) = cm_interface_registry.get_function(&qualified)
                {
                    let call_args: Vec<TirExpr> =
                        taken_args.iter().map(|a| a.expr.clone()).collect();
                    fixup_wasi_derived_types_in_adapter(
                        &mut adapter,
                        func_info,
                        &call_args,
                        expr.type_id,
                        type_table,
                        cm_interface_registry,
                        true, // skip_self: call_args excludes self
                    );
                }
            }

            // Cast call-site args to match adapter param types when they differ
            cast_args_to_adapter_params(&adapter_rc.borrow(), &mut taken_args, 1);

            // Flatten call site args to match the binding's flat CM params.
            // For method calls, self is the first param; remaining args may need flattening.
            let taken_exprs: Vec<TirExpr> = taken_args.into_iter().map(|a| a.expr).collect();
            let flat_taken_args = match cm_interface_registry.get_function(&qualified) {
                Some(func_info) => flatten_call_site_args(
                    func_info,
                    &taken_exprs,
                    true,
                    cm_interface_registry,
                    type_table,
                    names,
                ),
                None => taken_exprs,
            };

            // Retarget the call targeting the binding
            // Prepend receiver to args
            let mut all_args = vec![taken_receiver];
            all_args.extend(flat_taken_args);

            expr.kind = TirExprKind::Call {
                func: Box::new(FunctionRef::from_resolved(
                    &adapter_rc.borrow(),
                    entry_source.clone(),
                )),
                args: all_args
                    .into_iter()
                    .map(|e| CallArg::new(e, false))
                    .collect(),
                type_args: vec![],
                has_receiver: false,
            };

            // Recurse into args of the new Call
            if let TirExprKind::Call { args, .. } = &mut expr.kind {
                for arg in args {
                    rewrite_calls_in_expr(
                        &mut arg.expr,
                        adapters,
                        entry_source,
                        cm_interface_registry,
                        type_table,
                        applied_returns,
                        names,
                    );
                }
            }
            return;
        }
    }

    // Check if this is a resource static Call (with method_info) that should be rewritten to target a binding
    if let TirExprKind::Call { func, .. } = &expr.kind
        && let Some(info) = func.method_info.as_ref()
    {
        // Keyed as the registry declares it — `Resource::method`, no module.
        let func_name = DeclPath::method_of(&info.receiver_decl_name(), &info.method_name);
        if let Some(adapter_rc) = adapters.get(&func_name) {
            // Look up WASI function info to flatten args at the call site
            let wasi_func_info = cm_interface_registry.get_function(&func_name).cloned();

            // Extract args before replacing
            let taken_args = if let TirExprKind::Call { args, .. } = &mut expr.kind {
                std::mem::take(args)
                    .into_iter()
                    .map(|a| a.expr)
                    .collect::<Vec<_>>()
            } else {
                unreachable!()
            };

            // Fix up adapter return type and param types from the call site
            {
                let adapter_key = Rc::as_ptr(adapter_rc) as usize;
                let mut adapter = adapter_rc.borrow_mut();
                fixup_adapter_return_from_call_site(
                    &mut adapter,
                    adapter_key,
                    expr.type_id,
                    false,
                    type_table,
                    applied_returns,
                );
                if let Some(func_info) = &wasi_func_info {
                    let mut adapter_idx = 0;
                    for (i, (_name, _, param_type)) in func_info.params.iter().enumerate() {
                        if is_gc_passthrough_param(param_type, cm_interface_registry, names) {
                            if let Some(arg) = taken_args.get(i) {
                                fixup_adapter_param_from_call_site(
                                    &mut adapter,
                                    adapter_idx,
                                    arg.type_id,
                                    true,
                                    false,
                                );
                            }
                            adapter_idx += 1;
                        } else {
                            adapter_idx +=
                                flatten_param_type(param_type, cm_interface_registry, names).len();
                        }
                    }
                }
                // Replace WASI-derived types in the body with the user's types.
                // Skip for CM bindings (same reason as above).
                if !adapter.is_cm_binding
                    && let Some(func_info) = &wasi_func_info
                {
                    fixup_wasi_derived_types_in_adapter(
                        &mut adapter,
                        func_info,
                        &taken_args,
                        expr.type_id,
                        type_table,
                        cm_interface_registry,
                        false, // skip_self: static calls have no self
                    );
                }
            }

            // Flatten call site args to match the binding's flat CM params.
            // GC passthrough types (String, List<u8>, Option<T>) are passed
            // through as GC refs — the binding body handles lowering.
            // Other multi-flat types are flattened here into individual i32 args.
            let flat_call_args = if let Some(func_info) = &wasi_func_info {
                flatten_call_site_args(
                    func_info,
                    &taken_args,
                    false,
                    cm_interface_registry,
                    type_table,
                    names,
                )
            } else {
                // No WASI function info: pass args as-is (fallback)
                taken_args
            };

            // Replace static Call with Call targeting the binding
            expr.kind = TirExprKind::Call {
                func: Box::new(FunctionRef::from_resolved(
                    &adapter_rc.borrow(),
                    entry_source.clone(),
                )),
                args: flat_call_args
                    .into_iter()
                    .map(|e| CallArg::new(e, false))
                    .collect(),
                type_args: vec![],
                has_receiver: false,
            };

            // Recurse into args of the new Call
            if let TirExprKind::Call { args, .. } = &mut expr.kind {
                for arg in args {
                    rewrite_calls_in_expr(
                        &mut arg.expr,
                        adapters,
                        entry_source,
                        cm_interface_registry,
                        type_table,
                        applied_returns,
                        names,
                    );
                }
            }
            return;
        }
    }

    // Recurse into sub-expressions through the shared exhaustive walk; each
    // child re-enters `rewrite_calls_in_expr` via the walker's `visit_expr`,
    // so a rewrite-eligible call nested anywhere is still rewritten.
    CallRewriteWalker {
        adapters,
        entry_source,
        cm_interface_registry,
        type_table,
        applied_returns,
        names,
=======
            type_args,
            ..
        } = &mut expr.kind
            && cm_interface_registry.world_import_source(&func.name) == Some(&func.module_source)
            && let Some(adapter_rc) = adapters.get(&DeclPath::from_declared(&func.name))
        {
            let wasi_func =
                cm_interface_registry.get_function(&DeclPath::from_declared(&func.name));
            if let Some(info) = wasi_func {
                self.key_callbacks(info, args.iter_mut().map(|a| &mut a.expr), 0);
            }
            {
                let adapter_key = Rc::as_ptr(adapter_rc) as usize;
                let mut adapter = adapter_rc.borrow_mut();
                fixup_adapter_return_from_call_site(
                    &mut adapter,
                    adapter_key,
                    expr.type_id,
                    false,
                    type_table,
                    self.applied_returns,
                );
                for (i, arg) in args.iter().enumerate() {
                    let is_gc_passthrough = wasi_func.is_some_and(|f| {
                        i < f.params.len()
                            && is_gc_passthrough_param(&f.params[i].2, cm_interface_registry, names)
                    });
                    fixup_adapter_param_from_call_site(
                        &mut adapter,
                        i,
                        arg.expr.type_id,
                        is_gc_passthrough,
                        false,
                    );
                }
            }
            cast_args_to_adapter_params(&adapter_rc.borrow(), args, 0);
            **func = FunctionRef::from_resolved(&adapter_rc.borrow(), entry_source.clone());
            *type_args = vec![];
            for arg in args {
                self.rewrite_expr(&mut arg.expr);
            }
            return;
        }

        // Check if this is an effect-like Call that should be rewritten
        let is_effect_call = matches!(&expr.kind, TirExprKind::Call { func, .. }
            if func.module_source.clone().is_effect_like() && func.module_source.clone().interface_name().is_some());
        if is_effect_call
            && let TirExprKind::Call {
                func,
                args,
                type_args,
                ..
            } = &mut expr.kind
        {
            let interface_name = func
                .module_source
                .clone()
                .interface_name()
                .unwrap_or_default();
            let method_name = func.name.clone();
            let qualified = DeclPath::from_declared(format!("{interface_name}::{method_name}"));

            if let Some(adapter_rc) = adapters.get(&qualified) {
                let wasi_func = cm_interface_registry.get_function(&qualified);
                if let Some(info) = wasi_func {
                    self.key_callbacks(info, args.iter_mut().map(|a| &mut a.expr), 0);
                }
                let is_streaming =
                    wasi_func.is_some_and(|f| !f.is_async && f.has_streaming_param());

                // Fix up binding function types from the call site
                {
                    let adapter_key = Rc::as_ptr(adapter_rc) as usize;
                    let mut adapter = adapter_rc.borrow_mut();
                    fixup_adapter_return_from_call_site(
                        &mut adapter,
                        adapter_key,
                        expr.type_id,
                        is_streaming,
                        type_table,
                        self.applied_returns,
                    );
                    for (i, arg) in args.iter().enumerate() {
                        let is_gc_passthrough = wasi_func.is_some_and(|f| {
                            i < f.params.len()
                                && is_gc_passthrough_param(
                                    &f.params[i].2,
                                    cm_interface_registry,
                                    names,
                                )
                        });
                        fixup_adapter_param_from_call_site(
                            &mut adapter,
                            i,
                            arg.expr.type_id,
                            is_gc_passthrough,
                            is_streaming,
                        );
                    }
                }

                // Cast call-site args to match adapter param types when they differ
                // (e.g., i32 literal → i64 for Duration newtype)
                cast_args_to_adapter_params(&adapter_rc.borrow(), args, 0);

                // For streaming adapters, cast GC ref args to i32
                if is_streaming {
                    for arg in args.iter_mut() {
                        if arg.expr.type_id != TypeTable::I32 {
                            let original = std::mem::replace(
                                &mut arg.expr,
                                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
                            );
                            arg.expr = cast(original, TypeTable::I32);
                        }
                    }
                }

                **func = FunctionRef::from_resolved(&adapter_rc.borrow(), entry_source.clone());
                *type_args = vec![];

                for arg in args {
                    self.rewrite_expr(&mut arg.expr);
                }
                return;
            }
        }

        // Check if this is a resource method call that should be rewritten to target a binding
        if let Some((receiver, func, _)) = expr.kind.as_method_call()
            && let Some(method_info) = func.method_info.clone()
        {
            // The adapter map is keyed by the declared `Resource::method`, the same
            // key `EffectCallCollector` inserted under.
            let head = {
                let tt = type_table.borrow();
                tt.impl_receiver_key(tt.peel_refs(receiver.type_id))
                    .decl_key()
            };
            let mut qualified = DeclPath::method_of(&head, &method_info.method_name);
            // Resolve through type aliases (e.g., Headers -> Fields), scoped to the
            // bundled CM namespaces.
            if !adapters.contains_key(&qualified)
                && let Some(source) = cm_interface_registry
                    .find_binding_source(CmTypeKind::Newtype, head.as_decl_str())
                && let Some(Type::Named(resolved)) =
                    cm_interface_registry.get_newtype_by_source(source, &head)
            {
                let aliased =
                    DeclPath::method_of(&DeclName::new(&resolved.name), &method_info.method_name);
                if adapters.contains_key(&aliased) {
                    qualified = aliased;
                }
            }
            if let Some(adapter_rc) = adapters.get(&qualified) {
                let wasi_func = cm_interface_registry.get_function(&qualified);
                let is_streaming =
                    wasi_func.is_some_and(|f| !f.is_async && f.has_streaming_param());

                let (taken_receiver, mut taken_args) = if let TirExprKind::Call {
                    args,
                    has_receiver: true,
                    ..
                } = &mut expr.kind
                {
                    let mut taken = std::mem::take(args);
                    let receiver = taken.remove(0).expr;
                    (receiver, taken)
                } else {
                    unreachable!()
                };
                if let Some(info) = wasi_func {
                    self.key_callbacks(info, taken_args.iter_mut().map(|a| &mut a.expr), 1);
                }

                // Fix up binding function types from the call site
                // The binding params include self as the first param
                {
                    let adapter_key = Rc::as_ptr(adapter_rc) as usize;
                    let mut adapter = adapter_rc.borrow_mut();
                    fixup_adapter_return_from_call_site(
                        &mut adapter,
                        adapter_key,
                        expr.type_id,
                        is_streaming,
                        type_table,
                        self.applied_returns,
                    );
                    // Self param (index 0): unconditional retype from the receiver.
                    fixup_adapter_param_from_call_site(
                        &mut adapter,
                        0,
                        taken_receiver.type_id,
                        true,
                        false,
                    );
                    // Remaining params: the i-th arg corresponds to WASI param
                    // index i+1 (WASI params include self at index 0).
                    for (i, arg) in taken_args.iter().enumerate() {
                        let wasi_param_idx = i + 1;
                        let is_gc_passthrough = wasi_func.is_some_and(|f| {
                            wasi_param_idx < f.params.len()
                                && is_gc_passthrough_param(
                                    &f.params[wasi_param_idx].2,
                                    cm_interface_registry,
                                    names,
                                )
                        });
                        fixup_adapter_param_from_call_site(
                            &mut adapter,
                            i + 1,
                            arg.expr.type_id,
                            is_gc_passthrough,
                            is_streaming,
                        );
                    }
                    // Replace WASI-derived types in the body (including function names).
                    // Skip for CM bindings — their types were set precisely by synthesis
                    // and the recursive replacement can produce TypeId mismatches for
                    // complex return types like [Stream<T>, Future<Result<_, E>>].
                    if !adapter.is_cm_binding
                        && let Some(func_info) = wasi_func
                    {
                        let call_args: Vec<TirExpr> =
                            taken_args.iter().map(|a| a.expr.clone()).collect();
                        fixup_wasi_derived_types_in_adapter(
                            &mut adapter,
                            func_info,
                            &call_args,
                            expr.type_id,
                            type_table,
                            cm_interface_registry,
                            true, // skip_self: call_args excludes self
                        );
                    }
                }

                // Cast call-site args to match adapter param types when they differ
                cast_args_to_adapter_params(&adapter_rc.borrow(), &mut taken_args, 1);

                // Flatten call site args to match the binding's flat CM params.
                // For method calls, self is the first param; remaining args may need flattening.
                let taken_exprs: Vec<TirExpr> = taken_args.into_iter().map(|a| a.expr).collect();
                let flat_taken_args = match wasi_func {
                    Some(func_info) => flatten_call_site_args(
                        func_info,
                        &taken_exprs,
                        true,
                        cm_interface_registry,
                        type_table,
                        names,
                    ),
                    None => taken_exprs,
                };

                let mut all_args = vec![taken_receiver];
                all_args.extend(flat_taken_args);

                expr.kind = TirExprKind::Call {
                    func: Box::new(FunctionRef::from_resolved(
                        &adapter_rc.borrow(),
                        entry_source.clone(),
                    )),
                    args: all_args
                        .into_iter()
                        .map(|e| CallArg::new(e, false))
                        .collect(),
                    type_args: vec![],
                    has_receiver: false,
                };

                if let TirExprKind::Call { args, .. } = &mut expr.kind {
                    for arg in args {
                        self.rewrite_expr(&mut arg.expr);
                    }
                }
                return;
            }
        }

        // Check if this is a resource static Call (with method_info) that should be rewritten to target a binding
        if let TirExprKind::Call { func, .. } = &expr.kind
            && let Some(info) = func.method_info.as_ref()
        {
            // Keyed as the registry declares it — `Resource::method`, no module.
            let func_name = DeclPath::method_of(&info.receiver_decl_name(), &info.method_name);
            if let Some(adapter_rc) = adapters.get(&func_name) {
                let wasi_func_info = cm_interface_registry.get_function(&func_name).cloned();

                let mut taken_args = if let TirExprKind::Call { args, .. } = &mut expr.kind {
                    std::mem::take(args)
                        .into_iter()
                        .map(|a| a.expr)
                        .collect::<Vec<_>>()
                } else {
                    unreachable!()
                };
                if let Some(info) = &wasi_func_info {
                    self.key_callbacks(info, taken_args.iter_mut(), 0);
                }

                // Fix up adapter return type and param types from the call site
                {
                    let adapter_key = Rc::as_ptr(adapter_rc) as usize;
                    let mut adapter = adapter_rc.borrow_mut();
                    fixup_adapter_return_from_call_site(
                        &mut adapter,
                        adapter_key,
                        expr.type_id,
                        false,
                        type_table,
                        self.applied_returns,
                    );
                    if let Some(func_info) = &wasi_func_info {
                        let mut adapter_idx = 0;
                        for (i, (_name, _, param_type)) in func_info.params.iter().enumerate() {
                            if is_gc_passthrough_param(param_type, cm_interface_registry, names) {
                                if let Some(arg) = taken_args.get(i) {
                                    fixup_adapter_param_from_call_site(
                                        &mut adapter,
                                        adapter_idx,
                                        arg.type_id,
                                        true,
                                        false,
                                    );
                                }
                                adapter_idx += 1;
                            } else {
                                adapter_idx +=
                                    flatten_param_type(param_type, cm_interface_registry, names)
                                        .len();
                            }
                        }
                    }
                    // Replace WASI-derived types in the body with the user's types.
                    // Skip for CM bindings (same reason as above).
                    if !adapter.is_cm_binding
                        && let Some(func_info) = &wasi_func_info
                    {
                        fixup_wasi_derived_types_in_adapter(
                            &mut adapter,
                            func_info,
                            &taken_args,
                            expr.type_id,
                            type_table,
                            cm_interface_registry,
                            false, // skip_self: static calls have no self
                        );
                    }
                }

                // Flatten call site args to match the binding's flat CM params.
                // GC passthrough types (String, List<u8>, Option<T>) are passed
                // through as GC refs — the binding body handles lowering.
                // Other multi-flat types are flattened here into individual i32 args.
                let flat_call_args = if let Some(func_info) = &wasi_func_info {
                    flatten_call_site_args(
                        func_info,
                        &taken_args,
                        false,
                        cm_interface_registry,
                        type_table,
                        names,
                    )
                } else {
                    // No WASI function info: pass args as-is (fallback)
                    taken_args
                };

                // Replace static Call with Call targeting the binding
                expr.kind = TirExprKind::Call {
                    func: Box::new(FunctionRef::from_resolved(
                        &adapter_rc.borrow(),
                        entry_source.clone(),
                    )),
                    args: flat_call_args
                        .into_iter()
                        .map(|e| CallArg::new(e, false))
                        .collect(),
                    type_args: vec![],
                    has_receiver: false,
                };

                if let TirExprKind::Call { args, .. } = &mut expr.kind {
                    for arg in args {
                        self.rewrite_expr(&mut arg.expr);
                    }
                }
                return;
            }
        }

        // Recurse into sub-expressions through the shared exhaustive walk; each
        // child re-enters `rewrite_expr` via the walker's `visit_expr`,
        // so a rewrite-eligible call nested anywhere is still rewritten.
        self.walk_expr(expr);
>>>>>>> origin/main
    }
<<<<<<< HEAD
    cm_static_method_key(func)
        .filter(|key| is_bound(key))
        .map(|key| (key, CmCallKind::Function))
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
||||||| 014361be8
    .walk_expr(expr);
=======
>>>>>>> origin/main
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

    fn record(m: &mut IndexMap<usize, TypeId>, key: usize, ty: u32) -> Option<TypeId> {
        // Slots 10 through 19 hold one type.
        record_applied_return(m, key, TypeId(ty), |a, b| a.0 / 10 == b.0 / 10)
    }

    #[test]
    fn record_applied_return_first_and_matching_are_ok() {
        let mut m: IndexMap<usize, TypeId> = IndexMap::default();
        assert_eq!(record(&mut m, 1, 10), None);
        // Same key, same type re-records without conflict, from any slot.
        assert_eq!(record(&mut m, 1, 10), None);
        assert_eq!(record(&mut m, 1, 12), None);
        // A distinct adapter key is independent.
        assert_eq!(record(&mut m, 2, 20), None);
    }

    #[test]
    fn record_applied_return_detects_conflict() {
        let mut m: IndexMap<usize, TypeId> = IndexMap::default();
        assert_eq!(record(&mut m, 1, 10), None);
        // Same key, different type → the earlier type is reported and the map
        // is left unchanged so the caller can ICE with both names.
        assert_eq!(record(&mut m, 1, 21), Some(TypeId(10)));
        assert_eq!(m.get(&1), Some(&TypeId(10)));
    }
}
