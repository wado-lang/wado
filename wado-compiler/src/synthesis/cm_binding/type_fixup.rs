//! Post-synthesis type fixups and call-site rewrites.
//!
//! After import-adapter synthesis the binding bodies use WASI-derived
//! `TypeId`s (e.g. `List<Tuple<String, List<u8>>>`) while the call sites
//! see the user's newtype-aliased types (e.g. `List<Tuple<FieldName,
//! FieldValue>>`). [`rewrite_calls_in_block`] reaches every effect-style
//! call and resource method call, swaps in the binding `FunctionRef`,
//! flattens args to the binding's flat CM parameter shape, and runs
//! [`fixup_wasi_derived_types_in_adapter`] to re-type the binding body.
//! [`collect_effect_calls_in_block`] is the discovery pass that drives
//! the same set of call sites for adapter generation.
//!
//! Design invariant: one binding function is shared by every call site of
//! its import, so all sites must agree on the types the fixup applies. The
//! rewrite records each adapter's applied return type and turns a
//! disagreeing site into an ICE (see
//! `fixup_adapter_return_from_call_site`) instead of silently overwriting
//! an earlier site's typing. The longer-term direction is to synthesize
//! bindings with call-site types up front and retire this pass; until
//! then, the invariant is enforced, not assumed.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::Type;
use crate::component_model::{CmFunctionInfo, CmInterfaceRegistry};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::{
    CallArg, FunctionRef, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal, TirStmt,
    TirStmtKind, TypeId, TypeTable,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};

use crate::synthesis::common::{cast, i32_const, option_none, synth_span};

use super::types::{
    cm_type_to_type_id, flatten_param_type, is_gc_passthrough_param, is_wasm_flat_type,
};

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
    let names =
        super::types::CmStdlibNames::from_compiler_items(type_table.borrow().compiler_items());
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
            drop(tt);
            if old_name == new_name {
                replace_type_in_adapter(adapter, old_type, user_type);
            } else {
                replace_type_in_adapter_with_names(
                    adapter, old_type, user_type, &old_name, &new_name,
                );
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
        Type::Generic(g) if g.name == names.result && g.args.len() == 2 => {
            let tt = type_table.borrow();
            if let Some(new_args) = tt.generic_type_args(user_type)
                && new_args.len() == 2
            {
                let new_ok = new_args[0];
                let new_err = new_args[1];
                drop(tt);
                replace_wasi_derived_type_recursive(
                    adapter,
                    &g.args[0],
                    new_ok,
                    cm_interface_registry,
                    wasi_package,
                    type_table,
                );
                replace_wasi_derived_type_recursive(
                    adapter,
                    &g.args[1],
                    new_err,
                    cm_interface_registry,
                    wasi_package,
                    type_table,
                );
            }
        }
        _ => {}
    }
}

/// Fix up WASI-derived types in the binding body to match the user's types.
///
/// The binding body uses `TypeIds` from `cm_type_to_type_id` (e.g., `List<Tuple<String, List<u8>>>`).
/// The call site uses user types with newtype aliases (e.g., `List<Tuple<FieldName, FieldValue>>`).
/// This function computes the WASI-derived `TypeId` for each param and replaces it in the body.
fn fixup_wasi_derived_types_in_adapter(
    adapter: &mut TirFunction,
    func_info: &CmFunctionInfo,
    call_args: &[TirExpr],
    user_return_type: TypeId,
    type_table: &RefCell<TypeTable>,
    cm_interface_registry: &CmInterfaceRegistry,
    skip_self: bool,
) {
    let wasi_package = func_info.package.as_str();
    let params_iter: Box<dyn Iterator<Item = &(String, String, Type)>> = if skip_self {
        Box::new(func_info.params.iter().skip(1))
    } else {
        Box::new(func_info.params.iter())
    };
    for (i, (_param_name, _, param_type)) in params_iter.enumerate() {
        if i >= call_args.len() {
            break;
        }
        let new_type = call_args[i].type_id;
        // Resolve newtypes (e.g., FieldName → String) before computing WASI-derived TypeId
        let resolved = cm_interface_registry.resolve_type(param_type);
        replace_wasi_derived_type_recursive(
            adapter,
            &resolved,
            new_type,
            cm_interface_registry,
            wasi_package,
            type_table,
        );
    }
    // Also fix up return type's WASI-derived sub-types — but skip for CM bindings
    // that return non-flat types (tuples, variants). Their return TypeId was set
    // precisely by synthesis and should not be modified by the recursive type
    // replacement, which can produce different TypeIds for the same logical type
    // through different cm_type_to_type_id resolution paths.
    if !adapter.is_cm_binding
        && let Some(return_type) = &func_info.return_type
    {
        let resolved = cm_interface_registry.resolve_type(return_type);
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

/// Fix up the return expression's type in the binding body to match the caller's
/// expected return type. The binding was created with placeholder `TypeId`s
/// (e.g., `TypeTable::I32`) that need to be corrected to actual Wado types.
fn fixup_return_type_in_body(adapter: &mut TirFunction, old_type: TypeId, new_type: TypeId) {
    if let Some(body) = &mut adapter.body {
        fixup_types_in_block(body, old_type, new_type, &mut adapter.locals);
    }
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

/// Like `replace_type_in_adapter` but also renames function references that
/// contain the old type name to use the new type name. This is needed when
/// the binding body calls monomorphized functions like `List<T>::with_capacity`
/// where T is a WASI-derived type that differs from the user's newtype alias.
fn replace_type_in_adapter_with_names(
    adapter: &mut TirFunction,
    old_type: TypeId,
    new_type: TypeId,
    old_name: &str,
    new_name: &str,
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
            rename: Some((old_name, new_name)),
        }
        .visit_block(body);
    }
}

/// Replaces every occurrence of `old_type` with `new_type` throughout an
/// adapter body, and — when `rename` is set — rewrites function references
/// whose mangled name embeds `old_name` to use `new_name` (needed for
/// monomorphized helpers like `List<T>::with_capacity` where `T` is a
/// WASI-derived type differing from the user's newtype alias).
///
/// Traversal is exhaustive via `TirMutVisitor`, so the swap reaches every
/// expression position — match arms, nested blocks, closures, index/assign
/// targets — rather than the partial set the previous hand-written walkers
/// covered.
struct TypeReplacer<'a> {
    old_type: TypeId,
    new_type: TypeId,
    rename: Option<(&'a str, &'a str)>,
}

impl TypeReplacer<'_> {
    /// Apply the type swap (and optional rename) to a callee `FunctionRef`'s
    /// name and monomorphization type arguments.
    fn fix_func_ref(&self, func: &mut FunctionRef) {
        if let Some((old_name, new_name)) = self.rename {
            func.name = crate::name::replace_type_name_in_mangled(&func.name, old_name, new_name);
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
            TirExprKind::Call { func, .. } | TirExprKind::MethodCall { func, .. } => {
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

/// Recursively fix types in a block — replaces `old_type` with `new_type`
/// in return statements, let bindings, and expressions.
/// Also replaces `TypeTable::I32` placeholders with `new_type` (for cases
/// where the binding used I32 as a placeholder).
fn fixup_types_in_block(
    block: &mut TirBlock,
    old_type: TypeId,
    new_type: TypeId,
    locals: &mut Vec<TirLocal>,
) {
    for stmt in &mut block.stmts {
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
                fixup_types_in_block(then_block, old_type, new_type, locals);
                if let Some(blk) = else_block {
                    fixup_types_in_block(blk, old_type, new_type, locals);
                }
            }
            TirStmtKind::Loop { body } => {
                fixup_types_in_block(body, old_type, new_type, locals);
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

/// Fix up an expression in a Let statement that might hold adapter intermediate values.
fn fixup_adapter_let(
    expr: &mut TirExpr,
    local_index: u32,
    old_type: TypeId,
    new_type: TypeId,
    let_type_id: &mut TypeId,
    locals: &mut [TirLocal],
) {
    let should_fix = expr.type_id == TypeTable::I32 || expr.type_id == old_type;
    match &mut expr.kind {
        TirExprKind::Call { func, .. } if func.method_info.is_some() => {
            if should_fix {
                expr.type_id = new_type;
                *let_type_id = new_type;
                if (local_index as usize) < locals.len() {
                    locals[local_index as usize].type_id = new_type;
                }
            }
        }
        TirExprKind::Null => {
            if should_fix {
                expr.type_id = new_type;
                *let_type_id = new_type;
                if (local_index as usize) < locals.len() {
                    locals[local_index as usize].type_id = new_type;
                }
            }
        }
        _ => {}
    }
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

/// Flatten a Wado-level arg into flat CM ABI args at the call site.
///
/// For multi-flat types like `Option<T>`, the Wado-level arg (e.g., `null`)
/// is expanded into multiple i32 args (discriminant + payload).
fn flatten_arg_for_call_site(
    arg: &TirExpr,
    flat_tys: &[TypeId],
    flat_args: &mut Vec<TirExpr>,
    names: &super::types::CmStdlibNames,
) {
    // Unwrap Cast nodes transparently
    let inner = match &arg.kind {
        TirExprKind::Cast { expr, .. } => expr.as_ref(),
        _ => arg,
    };
    match &inner.kind {
        // null literal → discriminant=0, payload=0 for each flat type
        TirExprKind::Null => {
            for _ in flat_tys {
                flat_args.push(i32_const(0));
            }
        }
        // VariantConstruct None → discriminant=0, payload=0 for each flat type
        TirExprKind::VariantConstruct {
            case_name,
            payload: None,
            ..
        } if case_name == &names.none_name => {
            for _ in flat_tys {
                flat_args.push(i32_const(0));
            }
        }
        // VariantConstruct Some(value) → discriminant=1, then flatten inner value
        TirExprKind::VariantConstruct {
            case_name,
            payload: Some(value),
            ..
        } if case_name == &names.some_name => {
            flat_args.push(i32_const(1));
            let remaining = &flat_tys[1..];
            if remaining.len() == 1 {
                // Single-value payload: pass through (e.g., enum discriminant)
                flatten_arg_for_call_site(value, remaining, flat_args, names);
            } else {
                // Multi-value payload (e.g., String → ptr+len): pass through as-is
                // The binding will lower it internally
                flat_args.push((**value).clone());
                for _ in 2..flat_tys.len() {
                    flat_args.push(i32_const(0));
                }
            }
        }
        // For any other expression, this is an arbitrary Option<T> value.
        // Currently not supported — would need runtime null-check logic.
        _ => {
            panic!(
                "StaticCall adapter: cannot flatten arg of kind {:?} into {} flat types at call site; \
                 only null and VariantConstruct literals are supported",
                inner.kind,
                flat_tys.len()
            );
        }
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
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &Rc<RefCell<TypeTable>>,
    applied_returns: &mut IndexMap<usize, TypeId>,
) {
    CallRewriteWalker {
        adapters,
        entry_source,
        cm_interface_registry,
        type_table,
        applied_returns,
    }
    .visit_block(block);
}

/// Drives the exhaustive block / statement / generic-expression traversal for
/// [`Self::rewrite_expr`]. The per-node rewrite logic (effect calls,
/// resource methods, resource statics) lives in `rewrite_expr`, which
/// this walker re-enters via `visit_expr`; the walker supplies the shared
/// traversal plus the `Let` type-sync, the defensive `TaskReturn` descent,
/// and `applied_returns` — the per-run record of which call-site return type
/// each shared adapter was retyped to, so a second call site that disagrees
/// is an ICE instead of a silent last-write-wins overwrite.
struct CallRewriteWalker<'a> {
    adapters: &'a IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &'a ModuleSource,
    cm_interface_registry: &'a CmInterfaceRegistry,
    type_table: &'a Rc<RefCell<TypeTable>>,
    applied_returns: &'a mut IndexMap<usize, TypeId>,
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
        rewrite_calls_in_expr(
            expr,
            self.adapters,
            self.entry_source,
            self.cm_interface_registry,
            self.type_table,
            self.applied_returns,
        );
    }
}

/// Retype a shared adapter's return from a call site. Streaming adapters keep
/// their WIR-level i32 return (the caller-visible `Future` type stays on the
/// call expression), so they are left untouched.
///
/// The adapter is shared by every call site of the import, so all sites must
/// agree on its return type; `applied_returns` records what each adapter was
/// typed to, and a disagreeing site is an ICE rather than a silent
/// last-write-wins overwrite of the earlier site's typing. The map is keyed by
/// the adapter's `Rc` identity (`adapter_key`) — not its name, which is a
/// non-injective `interface_method` join — so two distinct adapters can never
/// collide.
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
    if local_idx < adapter.locals.len() {
        adapter.locals[local_idx].type_id = arg_type;
    }
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
            let target = adapter.params[idx].type_id;
            let original = std::mem::replace(
                &mut arg.expr,
                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
            );
            arg.expr = cast(original, target);
        }
    }
}

/// Flatten call-site args to the binding's flat CM param shape. GC
/// passthrough params (String / List / Option-of-GC) pass the GC ref through
/// — a bare `null` becomes a typed `None` — while multi-flat aggregates
/// expand via [`flatten_arg_for_call_site`]. `skip_self` offsets into the
/// WASI param list for method calls, whose `args` exclude the receiver.
fn flatten_call_site_args(
    func_info: &CmFunctionInfo,
    args: &[TirExpr],
    skip_self: bool,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &RefCell<TypeTable>,
    names: &super::types::CmStdlibNames,
) -> Vec<TirExpr> {
    let param_offset = usize::from(skip_self);
    let mut flat = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        let Some((_, _, param_type)) = func_info.params.get(i + param_offset) else {
            flat.push(arg.clone());
            continue;
        };
        let flat_tys = flatten_param_type(param_type, cm_interface_registry, names);
        if flat_tys.is_empty() {
            continue;
        }
        if is_gc_passthrough_param(param_type, cm_interface_registry, names) {
            if matches!(arg.kind, TirExprKind::Null) {
                // Use the binding's param type (from the WASI registry) to
                // type the `None`; the source null's inner type may be unknown.
                let none_expr = {
                    let mut tt = type_table.borrow_mut();
                    let option_type_id = cm_type_to_type_id(
                        param_type,
                        &mut tt,
                        cm_interface_registry,
                        &func_info.package,
                    );
                    option_none(option_type_id, tt.compiler_items())
                };
                flat.push(none_expr);
            } else {
                flat.push(arg.clone());
            }
        } else if flat_tys.len() == 1 {
            flat.push(arg.clone());
        } else {
            flatten_arg_for_call_site(arg, &flat_tys, &mut flat, names);
        }
    }
    flat
}

fn rewrite_calls_in_expr(
    expr: &mut TirExpr,
    adapters: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    entry_source: &ModuleSource,
    cm_interface_registry: &CmInterfaceRegistry,
    type_table: &Rc<RefCell<TypeTable>>,
    applied_returns: &mut IndexMap<usize, TypeId>,
) {
    let names =
        super::types::CmStdlibNames::from_compiler_items(type_table.borrow().compiler_items());
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
        let qualified = format!("{interface_name}::{method_name}");

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
                            && is_gc_passthrough_param(
                                &f.params[i].2,
                                cm_interface_registry,
                                &names,
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

            // Rewrite to call the binding function
            *func = FunctionRef::from_resolved(&adapter_rc.borrow(), entry_source.clone());
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
                );
            }
            return;
        }
    }

    // Check if this is a resource MethodCall that should be rewritten to target a binding
    if let TirExprKind::MethodCall { func, .. } = &expr.kind
        && let Some(method_info) = func.method_info.clone()
    {
        let mut qualified = format!(
            "{}::{}",
            method_info.base_struct_name, method_info.method_name
        );
        // Resolve through type aliases (e.g., Headers -> Fields). Scoped to
        // `wasi:` — the method resolution path is WASI-only.
        if !adapters.contains_key(&qualified)
            && let Some(source) =
                cm_interface_registry.find_wasi_newtype_source(&method_info.base_struct_name)
            && let Some(Type::Named(resolved)) =
                cm_interface_registry.get_newtype_by_source(source, &method_info.base_struct_name)
        {
            let aliased = format!("{}::{}", resolved.name, method_info.method_name);
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
            let (taken_receiver, mut taken_args) =
                if let TirExprKind::MethodCall { receiver, args, .. } = &mut expr.kind {
                    (
                        std::mem::replace(
                            receiver.as_mut(),
                            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span()),
                        ),
                        std::mem::take(args),
                    )
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
                                &names,
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
                    &names,
                ),
                None => taken_exprs,
            };

            // Replace MethodCall with Call targeting the binding
            // Prepend receiver to args
            let mut all_args = vec![taken_receiver];
            all_args.extend(flat_taken_args);

            expr.kind = TirExprKind::Call {
                func: FunctionRef::from_resolved(&adapter_rc.borrow(), entry_source.clone()),
                args: all_args
                    .into_iter()
                    .map(|e| CallArg::new(e, false))
                    .collect(),
                type_args: vec![],
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
                    );
                }
            }
            return;
        }
    }

    // Check if this is a resource static Call (with method_info) that should be rewritten to target a binding
    if let TirExprKind::Call { func, .. } = &expr.kind
        && func.method_info.is_some()
    {
        let func_name = func.name.clone();
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
                // Fix up adapter param types for GC pass-through params (String,
                // List<T>) where the binding receives the GC value directly.
                // Do NOT fix up params that get flattened (Option, resource handles)
                // because taken_args indices don't match flat adapter param indices.
                if let Some(func_info) = &wasi_func_info {
                    let mut flat_idx = 0;
                    for (i, (_name, _, param_type)) in func_info.params.iter().enumerate() {
                        let is_gc_passthrough = matches!(
                            param_type,
                            Type::Named(n) if n.name == names.string
                        ) || matches!(
                            param_type,
                            Type::Generic(g) if g.name == names.array && g.args.len() == 1
                        );
                        if is_gc_passthrough {
                            if let Some(arg) = taken_args.get(i) {
                                fixup_adapter_param_from_call_site(
                                    &mut adapter,
                                    flat_idx,
                                    arg.type_id,
                                    true,
                                    false,
                                );
                            }
                            flat_idx += 1;
                        } else {
                            let flat_tys =
                                flatten_param_type(param_type, cm_interface_registry, &names);
                            flat_idx += flat_tys.len().max(1);
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
                    &names,
                )
            } else {
                // No WASI function info: pass args as-is (fallback)
                taken_args
            };

            // Replace static Call with Call targeting the binding
            expr.kind = TirExprKind::Call {
                func: FunctionRef::from_resolved(&adapter_rc.borrow(), entry_source.clone()),
                args: flat_call_args
                    .into_iter()
                    .map(|e| CallArg::new(e, false))
                    .collect(),
                type_args: vec![],
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
    }
    .walk_expr(expr);
}

pub(super) fn collect_effect_calls_in_block(
    block: &TirBlock,
    effects: &mut IndexSet<String>,
    cm_interface_registry: &CmInterfaceRegistry,
) {
    EffectCallCollector {
        effects,
        cm_interface_registry,
    }
    .visit_block(block);
}

/// Discovery pass that records every used WASI effect call and resource
/// method call so [`super::generate_adapters`] synthesizes a binding for
/// each. Detection fires on `Call` / `MethodCall`; the exhaustive
/// `TirRefVisitor` walk reaches every other position (closures, `with`
/// handler bodies, match arms, template interpolations, …) so an effect
/// call nested anywhere still triggers adapter generation.
struct EffectCallCollector<'a> {
    effects: &'a mut IndexSet<String>,
    cm_interface_registry: &'a CmInterfaceRegistry,
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
        match &expr.kind {
            TirExprKind::Call { func, .. } => {
                // Sync WASI effect calls (e.g. `Environment::get_arguments`).
                if func.module_source.clone().is_effect_like()
                    && let Some(interface_name) = func.module_source.clone().interface_name()
                {
                    let qualified = format!("{interface_name}::{}", func.name);
                    if self
                        .cm_interface_registry
                        .get_function(&qualified)
                        .is_some()
                    {
                        self.effects.insert(qualified);
                    }
                }
                // WASI resource static method calls (e.g. `Response::new`).
                if func.method_info.is_some()
                    && self
                        .cm_interface_registry
                        .get_function(&func.name)
                        .is_some()
                {
                    self.effects.insert(func.name.clone());
                }
            }
            TirExprKind::MethodCall { func, .. } => {
                if let Some(method_info) = func.method_info.clone() {
                    let qualified = format!(
                        "{}::{}",
                        method_info.base_struct_name, method_info.method_name
                    );
                    if self
                        .cm_interface_registry
                        .get_function(&qualified)
                        .is_some()
                    {
                        self.effects.insert(qualified);
                    } else if let Some(source) = self
                        .cm_interface_registry
                        .find_wasi_newtype_source(&method_info.base_struct_name)
                        && let Some(Type::Named(resolved)) = self
                            .cm_interface_registry
                            .get_newtype_by_source(source, &method_info.base_struct_name)
                    {
                        // Resolve through type aliases (e.g. Headers -> Fields).
                        let aliased = format!("{}::{}", resolved.name, method_info.method_name);
                        if self.cm_interface_registry.get_function(&aliased).is_some() {
                            self.effects.insert(aliased);
                        }
                    }
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}
