//! Post-synthesis type fixups and call-site rewrites. Import-adapter binding
//! bodies carry WASI-derived `TypeId`s while call sites see the user's
//! newtype-aliased ones, so [`rewrite_calls_in_block`] swaps in the binding
//! `FunctionRef`, flattens args to the flat CM shape, and re-types the body.
//! One binding serves every call site, so a disagreeing site is an ICE.

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

use crate::synthesis::common::{cast, option_none, synth_span};

use super::import_adapter::is_gc_passthrough_param;
use super::types::{CmStdlibNames, cm_type_to_type_id, flatten_param_type, is_wasm_flat_type};
use crate::name::FqTypeName;

/// Recursively replace WASI-derived types with user types in the binding.
/// Given a WASI AST `Type` and the user's `TypeId`, compute the WASI-derived `TypeId`
/// and replace it, then recurse into sub-types (List elements, Tuple fields, etc.).
fn replace_wasi_derived_type_recursive(
    adapter: &mut TirFunction,
    wasi_type: &Type,
    user_type: TypeId,
    cm_interface_registry: &CmInterfaceRegistry,
    wasi_package: &str,
    type_table: &RefCell<TypeTable>,
) {
    let names = CmStdlibNames::from_type_table(&type_table.borrow());
    let old_type = {
        let mut tt = type_table.borrow_mut();
        cm_type_to_type_id(wasi_type, &mut tt, cm_interface_registry, wasi_package)
    };
    if old_type != user_type && old_type != TypeTable::I32 && old_type != TypeTable::UNIT {
        // Skip replacement if the user type resolves to the same base type
        // after deep newtype resolution. Introducing newtypes into adapter
        // bodies creates monomorphization and WIR type lookup issues.
        let tt = type_table.borrow();
        let old_name = tt.mangle_type_name(old_type);
        let user_resolved_name = tt.mangle_type_name_resolving_newtypes(user_type);
        drop(tt);
        if old_name == user_resolved_name {
            // Same underlying type after resolving newtypes — no replacement needed
        } else {
            let tt = type_table.borrow();
            let new_name = tt.mangle_type_name(user_type);
            // The two spellings differ, so a callee naming the old type has to
            // be told about the swap — as the pair of identities, which is what
            // its own key is built from.
            let swap = (old_name != new_name)
                .then(|| (tt.fq_type_name(old_type), tt.fq_type_name(user_type)));
            drop(tt);
            match swap {
                None => replace_type_in_adapter(adapter, old_type, user_type),
                Some((old_fq, new_fq)) => replace_type_in_adapter_with_names(
                    adapter, old_type, user_type, &old_fq, &new_fq,
                ),
            }
        }
    }
    match wasi_type {
        Type::Generic(g) if g.name == names.array && g.args.len() == 1 => {
            let tt = type_table.borrow();
            if let Some(new_elem_args) = tt.generic_type_args(user_type)
                && new_elem_args.len() == 1
            {
                let new_elem = new_elem_args[0];
                drop(tt);
                replace_wasi_derived_type_recursive(
                    adapter,
                    &g.args[0],
                    new_elem,
                    cm_interface_registry,
                    wasi_package,
                    type_table,
                );
            }
        }
        Type::Tuple(elems) => {
            // Get the user's tuple field types
            if let Some(user_elems) = type_table.borrow().as_tuple(user_type) {
                for (wasi_elem, &user_elem) in elems.iter().zip(user_elems.iter()) {
                    replace_wasi_derived_type_recursive(
                        adapter,
                        wasi_elem,
                        user_elem,
                        cm_interface_registry,
                        wasi_package,
                        type_table,
                    );
                }
            }
        }
        Type::Generic(g) if g.name == names.option && g.args.len() == 1 => {
            let tt = type_table.borrow();
            if let Some(new_args) = tt.generic_type_args(user_type)
                && new_args.len() == 1
            {
                let new_inner = new_args[0];
                drop(tt);
                replace_wasi_derived_type_recursive(
                    adapter,
                    &g.args[0],
                    new_inner,
                    cm_interface_registry,
                    wasi_package,
                    type_table,
                );
            }
        }
        Type::Generic(g)
            if (g.name == names.result && g.args.len() == 2) || names.is_tree_map(g) =>
        {
            let new_args = type_table.borrow().generic_type_args(user_type);
            if let Some(new_args) = new_args
                && new_args.len() == 2
            {
                for (wasi_arg, &new_arg) in g.args.iter().zip(new_args.iter()) {
                    replace_wasi_derived_type_recursive(
                        adapter,
                        wasi_arg,
                        new_arg,
                        cm_interface_registry,
                        wasi_package,
                        type_table,
                    );
                }
            }
        }
        _ => {}
    }
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
) {
    // Synthesis typed a CM binding exactly; re-deriving can mint a different
    // TypeId for one type, as for `[Stream<T>, Future<Result<_, E>>]`.
    if adapter.is_cm_binding {
        return;
    }
    let wasi_package = func_info.package.as_str();
    for ((_, _, param_type), arg) in func_info.params.iter().skip(param_offset).zip(args) {
        let resolved = cm_interface_registry.value_type(param_type);
        replace_wasi_derived_type_recursive(
            adapter,
            &resolved,
            arg.expr.type_id,
            cm_interface_registry,
            wasi_package,
            type_table,
        );
    }
    if let Some(return_type) = &func_info.return_type {
        let resolved = cm_interface_registry.value_type(return_type);
        replace_wasi_derived_type_recursive(
            adapter,
            &resolved,
            user_return_type,
            cm_interface_registry,
            wasi_package,
            type_table,
        );
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

/// Where the adapter's `CmRawCall` sits, or 0 where it makes none. Parameter
/// lowering precedes it, so a statement before it holds a parameter's
/// intermediate and never the result.
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

/// Replace ALL occurrences of `old_type` with `new_type` throughout the binding's
/// body, locals, and params. Used when a param or return type is fixed up from
/// WASI-derived types to the user code's newtype aliases.
fn replace_type_in_adapter(adapter: &mut TirFunction, old_type: TypeId, new_type: TypeId) {
    if old_type == new_type {
        return;
    }
    // Don't replace the return type of CM binding adapters.
    // The return type was set by synthesis with precise TypeIds from the entry
    // module's TypeTable. Replacing it with a TypeId computed by cm_type_to_type_id
    // (which may produce different TypeIds for Stream/Future/Result composition)
    // corrupts the type and causes WIR build failures.
    if !adapter.is_cm_binding && adapter.return_type == old_type {
        adapter.return_type = new_type;
    }
    // Fix params
    for param in &mut adapter.params {
        if param.type_id == old_type {
            param.type_id = new_type;
        }
    }
    // Fix locals
    for lt in &mut adapter.locals {
        if lt.type_id == old_type {
            lt.type_id = new_type;
        }
    }
    // Fix body
    if let Some(body) = &mut adapter.body {
        TypeReplacer {
            old_type,
            new_type,
            rename: None,
        }
        .visit_block(body);
    }
}

/// Like `replace_type_in_adapter` but also substitutes the old type inside a
/// callee's own identity. This is needed when the binding body calls
/// monomorphized functions like `List<T>::with_capacity` where `T` is a
/// WASI-derived type that differs from the user's newtype alias: the call is
/// keyed on the receiver its `method_info` names, so a swap that stops at the
/// body's types leaves the call naming the type the adapter no longer uses.
fn replace_type_in_adapter_with_names(
    adapter: &mut TirFunction,
    old_type: TypeId,
    new_type: TypeId,
    old_fq: &FqTypeName,
    new_fq: &FqTypeName,
) {
    if old_type == new_type {
        return;
    }
    // Don't replace return type of CM binding adapters (same as replace_type_in_adapter)
    if !adapter.is_cm_binding && adapter.return_type == old_type {
        adapter.return_type = new_type;
    }
    // Fix params
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
            rename: Some((old_fq, new_fq)),
        }
        .visit_block(body);
    }
}

/// Replaces every `old_type` with `new_type` throughout an adapter body, and
/// with `rename` set also substitutes the same pair inside each callee's own
/// identity — needed for a monomorphized `List<T>::with_capacity` whose `T` is
/// WASI-derived. Traversal rides `TirMutVisitor`, so the swap reaches every
/// expression position.
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
/// starting value of a lifted result — `ref.null`, or the `None` an option lift
/// seeds its accumulator with. The assignment in the `Some` arm and the `return`
/// are retyped by their own arms, so a starting value left behind puts the two
/// arms of one option in different GC type families.
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

/// Recursively fix the `type_id` of an expression and its leaf nodes.
fn fixup_expr_type(expr: &mut TirExpr, old_type: TypeId, new_type: TypeId) {
    if expr.type_id == old_type || expr.type_id == TypeTable::I32 {
        expr.type_id = new_type;
    }
    match &mut expr.kind {
        TirExprKind::TupleLiteral { .. } | TirExprKind::Call { .. } | TirExprKind::Local { .. } => {
        }
        TirExprKind::VariantConstruct { variant_type, .. } => {
            if *variant_type == TypeTable::I32 || *variant_type == old_type {
                *variant_type = new_type;
            }
        }
        _ => {}
    }
}

/// Collect local type updates from Let stmts that were modified by the rewrite.
/// This is needed because the lower phase pre-populates `locals`, and the streaming
/// adapter rewrite changes Let binding types from Result<..> to i32.
pub(super) fn collect_local_type_updates(
    block: &TirBlock,
    locals: &[TirLocal],
    updates: &mut Vec<(usize, TypeId)>,
) {
    LocalTypeUpdateCollector { locals, updates }.visit_block(block);
}

/// Collects `(local_index, new_type)` for every `Let` whose recorded binding
/// type drifted from the slot's `TirLocal` (e.g. a streaming binding call
/// retyped the let from `Result<…>` to `i32`). Exhaustive traversal reaches
/// lets nested in match arms / block expressions that the previous walker
/// skipped.
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
            // `TaskReturn` is still present this early (stripped in a later
            // step); descend into its value without the default walk's
            // `unreachable!` guard.
            TirStmtKind::TaskReturn { value } => self.visit_expr(value),
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
            // `TaskReturn` is still present this early (stripped in a later
            // step); descend into its value without the default walk's guard.
            TirStmtKind::TaskReturn { value } => self.visit_expr(value),
            _ => self.walk_stmt(stmt),
        }
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        self.rewrite_expr(expr);
    }
}

/// Retype a shared adapter's return from a call site, leaving a streaming
/// adapter's WIR-level i32 return alone. Every call site of the import shares
/// the adapter, so `applied_returns` records what each was typed to and a
/// disagreeing site is an ICE. Keyed by the adapter's `Rc` identity, its name
/// being a non-injective `interface_method` join.
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

/// Record `call_site_type` as the applied return for `adapter_key`. Returns
/// `Some(prev)` — leaving the map unchanged — when a different type was already
/// recorded (a shared-adapter conflict the caller turns into an ICE); returns
/// `None` on the first record or a matching re-record.
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

/// Retype one adapter param (and its local slot) from a call-site arg.
/// Skipped when the param is a flat wasm type whose adapter-side type is
/// authoritative — the arg is cast at the call site instead — and for
/// streaming adapters, which keep their i32 params. Pass
/// `is_gc_passthrough: true, is_streaming: false` for an unconditional
/// retype (the `self` receiver param).
fn fixup_adapter_param_from_call_site(
    adapter: &mut TirFunction,
    param_idx: usize,
    arg_type: TypeId,
    is_gc_passthrough: bool,
    is_streaming: bool,
) {
    let Some(param) = adapter.params.get_mut(param_idx) else {
        return;
    };
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

/// Cast call-site args whose flat-typed adapter param is authoritative
/// (e.g. an i32 literal passed where the ABI wants i64). `param_offset`
/// skips the `self` param for method calls.
fn cast_args_to_adapter_params(adapter: &TirFunction, args: &mut [CallArg], param_offset: usize) {
    for (i, arg) in args.iter_mut().enumerate() {
        let idx = i + param_offset;
        if idx < adapter.params.len()
            && adapter.params[idx].type_id != arg.expr.type_id
            && is_wasm_flat_type(adapter.params[idx].type_id)
        {
            cast_in_place(&mut arg.expr, adapter.params[idx].type_id);
        }
    }
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
    let mut flat = Vec::new();
    for (i, CallArg { expr: arg, .. }) in args.into_iter().enumerate() {
        let Some((_, _, param_type)) = func_info.params.get(i + param_offset) else {
            flat.push(CallArg::new(arg, false));
            continue;
        };
        let flat_tys = flatten_param_type(param_type, cm_interface_registry, names);
        if flat_tys.is_empty() {
            continue;
        }
        let arg = if !is_gc_passthrough_param(param_type, cm_interface_registry, names) {
            assert!(
                flat_tys.len() == 1,
                "a direct CM import param takes one flat slot, got {param_type:?}"
            );
            arg
        } else if matches!(arg.kind, TirExprKind::Null) {
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
        flat.push(CallArg::new(arg, false));
    }
    flat
}

impl<'a> CallRewriteWalker<'a> {
    fn rewrite_expr(&mut self, expr: &mut TirExpr) {
        let adapters = self.adapters;
        let registry = self.cm_interface_registry;

        if let TirExprKind::Call { func, .. } = &expr.kind
            && registry.world_import_source(&func.name) == Some(&func.module_source)
            && let Some((key, adapter)) =
                adapters.get_key_value(&DeclPath::from_declared(&func.name))
        {
            let func_info = self.cm_function(key);
            let mut args = take_call_args(expr);
            self.fixup_adapter_from_call_site(adapter, func_info, expr.type_id, &args, 0, false);
            cast_args_to_adapter_params(&adapter.borrow(), &mut args, 0);
            self.retarget_to_adapter(expr, adapter, args);
            return;
        }

        if let TirExprKind::Call { func, .. } = &expr.kind
            && let Some(interface_name) = func.module_source.interface_name()
            && let Some((key, adapter)) = adapters.get_key_value(&DeclPath::from_declared(format!(
                "{interface_name}::{}",
                func.name
            )))
        {
            let func_info = self.cm_function(key);
            let is_streaming = keeps_flat_i32_shape(func_info);
            let mut args = take_call_args(expr);
            self.fixup_adapter_from_call_site(
                adapter,
                func_info,
                expr.type_id,
                &args,
                0,
                is_streaming,
            );
            cast_args_to_adapter_params(&adapter.borrow(), &mut args, 0);
            if is_streaming {
                for arg in &mut args {
                    if arg.expr.type_id != TypeTable::I32 {
                        cast_in_place(&mut arg.expr, TypeTable::I32);
                    }
                }
            }
            self.retarget_to_adapter(expr, adapter, args);
            return;
        }

        if let Some((receiver, func, _)) = expr.kind.as_method_call()
            && let Some(key) =
                cm_method_call_key(receiver, func, self.type_table, registry, |key| {
                    adapters.contains_key(key)
                })
        {
            let adapter = &adapters[&key];
            let func_info = self.cm_function(&key);
            let is_streaming = keeps_flat_i32_shape(func_info);
            let mut args = take_call_args(expr);
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
                fixup_adapter_param_from_call_site(&mut adapter, 0, receiver.type_id, true, false);
                fixup_wasi_derived_types_in_adapter(
                    &mut adapter,
                    func_info,
                    &args,
                    1,
                    expr.type_id,
                    self.type_table,
                    registry,
                );
            }
            cast_args_to_adapter_params(&adapter.borrow(), &mut args, 1);
            let mut all_args = vec![CallArg::new(receiver, false)];
            all_args.extend(flatten_call_site_args(
                func_info,
                args,
                1,
                registry,
                self.type_table,
                self.names,
            ));
            self.retarget_to_adapter(expr, adapter, all_args);
            return;
        }

        if let TirExprKind::Call { func, .. } = &expr.kind
            && let Some(key) = cm_static_method_key(func)
            && let Some(adapter) = adapters.get(&key)
        {
            let func_info = self.cm_function(&key);
            let args = take_call_args(expr);
            self.fixup_adapter_from_call_site(adapter, func_info, expr.type_id, &args, 0, false);
            fixup_wasi_derived_types_in_adapter(
                &mut adapter.borrow_mut(),
                func_info,
                &args,
                0,
                expr.type_id,
                self.type_table,
                registry,
            );
            let args =
                flatten_call_site_args(func_info, args, 0, registry, self.type_table, self.names);
            self.retarget_to_adapter(expr, adapter, args);
            return;
        }

        self.walk_expr(expr);
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
        for (i, arg) in args.iter().enumerate() {
            let param_idx = i + param_offset;
            let is_gc_passthrough = func_info.params.get(param_idx).is_some_and(|(_, _, ty)| {
                is_gc_passthrough_param(ty, self.cm_interface_registry, self.names)
            });
            fixup_adapter_param_from_call_site(
                &mut adapter,
                param_idx,
                arg.expr.type_id,
                is_gc_passthrough,
                is_streaming,
            );
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

/// Discovery pass that records every used WASI effect call and resource
/// method call so [`super::generate_adapters`] synthesizes a binding for
/// each. Detection fires on a call; the exhaustive
/// `TirRefVisitor` walk reaches every other position (closures, `with`
/// handler bodies, match arms, template interpolations, …) so an effect
/// call nested anywhere still triggers adapter generation.
struct EffectCallCollector<'a> {
    effects: &'a mut IndexSet<DeclPath>,
    cm_interface_registry: &'a CmInterfaceRegistry,
    type_table: &'a RefCell<TypeTable>,
}

impl TirRefVisitor for EffectCallCollector<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        // `TaskReturn` is still present this early (stripped in a later
        // step); descend into its value without the default walk's guard.
        if let TirStmtKind::TaskReturn { value } = &stmt.kind {
            self.visit_expr(value);
        } else {
            self.walk_stmt(stmt);
        }
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        let registry = self.cm_interface_registry;
        let is_registered = |key: &DeclPath| registry.get_function(key).is_some();
        if let TirExprKind::Call { func, .. } = &expr.kind {
            // Sync WASI effect calls (e.g. `Environment::get_arguments`).
            if let Some(interface_name) = func.module_source.interface_name() {
                let qualified = DeclPath::from_declared(format!("{interface_name}::{}", func.name));
                if is_registered(&qualified) {
                    self.effects.insert(qualified);
                }
            }
            // WASI resource static method calls (e.g. `Response::new`).
            // The registry keys on the declared `Resource::method`; the
            // `#[cm]` the callee declares is what makes it that method.
            if let Some(qualified) = cm_static_method_key(func)
                && is_registered(&qualified)
            {
                self.effects.insert(qualified);
            }
            // World function (Phase 9): the same-source check keeps a
            // same-named local function from being taken for the import.
            if registry.world_import_source(&func.name) == Some(&func.module_source) {
                self.effects
                    .insert(DeclPath::from_declared(func.name.clone()));
            }
        }
        if let Some((receiver, func, _)) = expr.kind.as_method_call()
            && let Some(qualified) =
                cm_method_call_key(receiver, func, self.type_table, registry, is_registered)
        {
            self.effects.insert(qualified);
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
