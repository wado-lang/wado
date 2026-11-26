//! Post-monomorphization rewrites: function call rewriting to monomorphized names.

use std::rc::Rc;

use crate::module_source::ModuleSource;
use crate::tir::{
    CallArg, FunctionRef, InstantiationKey, MonomorphInfo, ResolvedType, TirBlock, TirExpr,
    TirExprKind, TirFunction, TirLocal, TirModule, TirStmt, TirStmtKind, TirUnaryOp, TypeId,
    TypeParamId,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};
use crate::type_table::TypeTable;

use super::Unrewritten;
use super::state::{Monomorphizer, SharedMutLeak};

impl Monomorphizer {
    /// Rewrite the function calls in what is `unrewritten` to use monomorphized
    /// names.
    pub(super) fn rewrite_function_calls_in_module(
        &self,
        module: &mut TirModule,
        unrewritten: Unrewritten,
    ) {
        let type_table = Rc::clone(&module.type_table);
        let mut type_table = type_table.borrow_mut();
        let mut rewriter = CallRewriter {
            mono: self,
            type_table: &mut type_table,
            judged: None,
        };

        for func_rc in unrewritten.functions(module) {
            let mut func = func_rc.borrow_mut();
            // An instance's calls answer to the call that instantiated it, which
            // was judged where it was written.
            rewriter.judged = func
                .monomorph_info
                .is_none()
                .then(|| func.module_source.clone());
            if let Some(mut body) = func.body.take() {
                rewriter.visit_block(&mut body);
                Self::sync_local_types_from_lets(&body, &mut func.locals);
                Self::update_local_expr_types(&mut body, &func.locals);
                func.body = Some(body);
            }
        }

        rewriter.judged = None;
        for global in unrewritten.globals_mut(module) {
            rewriter.visit_expr(global.init.slot_expr_mut());
        }
    }

    /// Sync `locals` types with the let statements they were derived from, at
    /// statement level only: a closure has its own local scope.
    fn sync_local_types_from_lets(block: &TirBlock, locals: &mut [TirLocal]) {
        struct SyncVisitor<'a> {
            locals: &'a mut [TirLocal],
        }
        impl TirRefVisitor for SyncVisitor<'_> {
            fn visit_stmt(&mut self, stmt: &TirStmt) {
                if let TirStmtKind::Let {
                    local_index,
                    type_id,
                    ..
                } = &stmt.kind
                    && let Some(local) = self.locals.get_mut(*local_index as usize)
                {
                    local.type_id = *type_id;
                }
                self.walk_stmt(stmt);
            }
            fn visit_expr(&mut self, _expr: &TirExpr) {}
        }
        SyncVisitor { locals }.visit_block(block);
    }

    /// Update all Local expression types based on the function's `locals`.
    fn update_local_expr_types(block: &mut TirBlock, locals: &[TirLocal]) {
        struct LocalTypeUpdater<'a> {
            locals: &'a [TirLocal],
        }
        impl TirMutVisitor for LocalTypeUpdater<'_> {
            fn visit_expr(&mut self, expr: &mut TirExpr) {
                if let TirExprKind::Local { index, .. } = &expr.kind
                    && let Some(local) = self.locals.get(*index as usize)
                {
                    expr.type_id = local.type_id;
                }
                // Closures have their own local scope, don't update with parent's locals
                if matches!(expr.kind, TirExprKind::Closure { .. }) {
                    return;
                }
                self.walk_expr(expr);
            }
        }
        LocalTypeUpdater { locals }.visit_block(block);
    }

    /// Point a call or function reference at the instance its template reaches,
    /// the same answer collection queued it under.
    fn rewrite_to_instance(
        &self,
        expr: &mut TirExpr,
        type_table: &mut TypeTable,
        judged: Option<&ModuleSource>,
    ) {
        let templates = &self.functions.templates;
        let Some(key) = self.call_instance(expr, templates, type_table) else {
            return;
        };
        if let Some(module_source) = judged
            && let TirExprKind::Call { args, .. } = &expr.kind
            && let Some(template) = key.template.as_ref().and_then(|t| templates.get(t))
            && let Some((param, arg)) =
                mut_leaked_through_shared(&template.borrow(), &key, args, expr.type_id, type_table)
        {
            self.shared_mut_leaks.borrow_mut().push(SharedMutLeak {
                module_source: module_source.clone(),
                span: expr.span,
                callee: template.borrow().name.clone(),
                param,
                arg,
            });
        }
        let Some(mangled) = self.lookup_function_instantiation(&key).cloned() else {
            return;
        };
        match &mut expr.kind {
            TirExprKind::Call {
                func, type_args, ..
            } => {
                apply_instantiation(func, &key, mangled);
                type_args.clear();
            }
            TirExprKind::FuncRef {
                module_source,
                name,
                type_args,
                template,
            } => {
                module_source.clone_from(&key.module_source);
                *name = mangled;
                type_args.clear();
                *template = None;
            }
            _ => unreachable!("only a call or a function reference reaches an instance"),
        }
        if let ResolvedType::TypeParam { id, .. } = *type_table.get(expr.type_id)
            && let Some(template) = key.template.as_ref().and_then(|t| templates.get(t))
            && let Some(concrete) = instance_arg(&template.borrow(), &key, id)
        {
            expr.type_id = concrete;
        }
    }
}

/// What `key` gives `template`'s parameter `param`: an impl parameter the
/// argument at its slot, a method's own the one at its position.
fn instance_arg(
    template: &TirFunction,
    key: &InstantiationKey,
    param: TypeParamId,
) -> Option<TypeId> {
    if template.impl_type_params.iter().any(|p| p.id == param) {
        return key.impl_type_args.get(param.index as usize).copied();
    }
    let at = template.type_params.iter().position(|p| p.id == param)?;
    key.method_type_args.get(at).copied()
}

/// The type parameter of `template`, and the `&mut` `key` instantiates it to,
/// that a `&` parameter reads where the caller holds that argument shared, and
/// that a return, a `&mut` parameter or a function parameter can hand back.
fn mut_leaked_through_shared(
    template: &TirFunction,
    key: &InstantiationKey,
    args: &[CallArg],
    returns: TypeId,
    tt: &TypeTable,
) -> Option<(String, TypeId)> {
    let head = |ty: TypeId| tt.get(tt.representation_head(ty));
    let mentions = |ty: TypeId, param: TypeParamId| tt.type_params_in(ty).contains(&param);
    template
        .impl_type_params
        .iter()
        .chain(&template.type_params)
        .filter(|param| !param.is_pack)
        .find_map(|param| {
            let arg = instance_arg(template, key, param.id)?;
            if !matches!(head(arg), ResolvedType::MutRef(_)) {
                return None;
            }
            let read_shared = template.params.iter().zip(args).any(|(p, a)| {
                matches!(head(p.type_id), ResolvedType::Ref(_))
                    && mentions(p.type_id, param.id)
                    && reaches_through_shared(&a.expr, tt)
            });
            // A subscript the elaborator read through a `&` already reads its
            // element, the template's bare `T`, as the `&` it is.
            let narrowed = matches!(tt.get(template.return_type), ResolvedType::TypeParam { id, .. } if *id == param.id)
                && matches!(head(returns), ResolvedType::Ref(_));
            let handed_back = (mentions(template.return_type, param.id) && !narrowed)
                || template.params.iter().any(|p| {
                    matches!(
                        head(p.type_id),
                        ResolvedType::MutRef(_) | ResolvedType::Function { .. }
                    ) && mentions(p.type_id, param.id)
                });
            (read_shared && handed_back).then(|| (param.name.clone(), arg))
        })
}

/// Whether the argument `expr` lends a `&` its caller holds: a `&` value, or a
/// place one is on the way to.
fn reaches_through_shared(expr: &TirExpr, tt: &TypeTable) -> bool {
    let is_shared = |ty: TypeId| matches!(tt.get(tt.representation_head(ty)), ResolvedType::Ref(_));
    match &expr.kind {
        TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: place,
        } => place_crosses_shared(place, tt),
        _ => is_shared(expr.type_id) || place_crosses_shared(expr, tt),
    }
}

/// Whether the place `expr` names is reached through a `&`.
fn place_crosses_shared(expr: &TirExpr, tt: &TypeTable) -> bool {
    match &expr.kind {
        TirExprKind::FieldAccess { expr: base, .. }
        | TirExprKind::Index { expr: base, .. }
        | TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: base,
        } => {
            matches!(
                tt.get(tt.representation_head(base.type_id)),
                ResolvedType::Ref(_)
            ) || place_crosses_shared(base, tt)
        }
        _ => false,
    }
}

/// Point `func` at the instance `key` names, recording in `monomorph_info` the
/// template's name and the arguments the instance was keyed with.
fn apply_instantiation(func: &mut FunctionRef, key: &InstantiationKey, mangled: String) {
    let generic_name = if key.method_info.is_some() {
        key.name.clone()
    } else {
        func.name.clone()
    };
    let is_blanket = func.monomorph_info.as_ref().is_some_and(|m| m.is_blanket);
    func.module_source.clone_from(&key.module_source);
    func.name = mangled;
    func.template = None;
    func.monomorph_info = Some(MonomorphInfo {
        generic_name,
        impl_type_args: key.impl_type_args.clone(),
        method_type_args: key.method_type_args.clone(),
        is_blanket,
    });
}

struct CallRewriter<'a> {
    mono: &'a Monomorphizer,
    type_table: &'a mut TypeTable,
    /// The module of the function whose calls are judged for a `&mut` handed
    /// behind a `&`; `None` for an instance.
    judged: Option<ModuleSource>,
}

impl TirMutVisitor for CallRewriter<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        self.walk_stmt(stmt);
        // Update the Let's type_id if it was a type parameter that got substituted
        if let TirStmtKind::Let {
            value: Some(value),
            type_id,
            ..
        } = &mut stmt.kind
            && self.type_table.contains_type_param(*type_id)
            && !self.type_table.contains_type_param(value.type_id)
        {
            *type_id = value.type_id;
        }
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        self.mono
            .rewrite_to_instance(expr, self.type_table, self.judged.as_ref());
        self.walk_expr(expr);
    }
}
