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

use crate::synthesis::common::{cast, option_none};

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
            }
            let param_offset = usize::from(receiver.is_some());
            let adapted = self.adapt_args(&mut adapter, func_info, args, param_offset);
            receiver.into_iter().chain(adapted).collect()
        };
        self.retarget_to_adapter(expr, adapter, args);
    }

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
    }
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
