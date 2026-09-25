//! Post-monomorphization rewrites: function call rewriting to monomorphized names.

use crate::tir::{
    FunctionRef, InstantiationKey, MonomorphInfo, ResolvedType, TirBlock, TirExpr, TirExprKind,
    TirLocal, TirModule, TirStmt, TirStmtKind, TypeTable,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};

use super::state::Monomorphizer;

impl Monomorphizer {
    /// Rewrite function calls in all functions to use monomorphized names
    pub(super) fn rewrite_function_calls_in_module(&self, module: &mut TirModule) {
        let mut type_table = module.type_table.borrow_mut();
        let mut rewriter = CallRewriter {
            mono: self,
            type_table: &mut type_table,
        };

        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(mut body) = func.body.take() {
                rewriter.visit_block(&mut body);
                // Sync `locals` types with Let statement types
                Self::sync_local_types_from_lets(&body, &mut func.locals);
                // Update all Local expression types based on `locals`
                Self::update_local_expr_types(&mut body, &func.locals);
                func.body = Some(body);
            }
        }

        // Rewrite function calls in global variable initializers
        for global in &mut module.globals {
            rewriter.visit_expr(global.init.slot_expr_mut());
        }
    }

    /// Sync `locals` types with the let statements they were derived from.
    ///
    /// Only walks into statement-level blocks (If/Loop/LabeledBlock/IfLet), not
    /// into expression blocks, since closures have their own local scope.
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
            fn visit_expr(&mut self, _expr: &TirExpr) {
                // Don't recurse into expressions — only statement-level blocks matter
            }
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
    fn rewrite_to_instance(&self, expr: &mut TirExpr, type_table: &mut TypeTable) {
        let templates = &self.functions.templates;
        let Some((key, _)) = self.call_instance(expr, templates, type_table) else {
            return;
        };
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
        if let ResolvedType::TypeParam { index, .. } = type_table.get(expr.type_id)
            && let Some(&concrete) = key
                .impl_type_args
                .iter()
                .chain(&key.method_type_args)
                .nth(*index as usize)
        {
            expr.type_id = concrete;
        }
    }
}

/// Rebuild `func` to point at the instance `key` names, keeping the call's
/// `method_info`. `monomorph_info` records the template's name and the
/// arguments the instance was keyed with.
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
}

impl TirMutVisitor for CallRewriter<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        self.walk_stmt(stmt);
        // Update the Let's type_id if it was a type parameter that got substituted
        if let TirStmtKind::Let { value, type_id, .. } = &mut stmt.kind
            && self.type_table.contains_type_param(*type_id)
            && !self.type_table.contains_type_param(value.type_id)
        {
            *type_id = value.type_id;
        }
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        self.mono.rewrite_to_instance(expr, self.type_table);
        self.walk_expr(expr);
    }
}
