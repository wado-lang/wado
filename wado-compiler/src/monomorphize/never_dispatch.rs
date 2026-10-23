//! A call dispatched on `!` that no impl answers. `!` holds every trait
//! (spec-types.md § The Never Type), so such a call type-checks, yet no body
//! exists for it: it evaluates its arguments and traps.

use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, NEVER_TYPE_NAME};
use crate::synthesis::common::builtin_call;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirModule, TirStmt, TirStmtKind};
use crate::tir_visitor::TirMutVisitor;
use crate::type_table::TypeTable;

pub(super) fn trap_never_dispatch_in_module(module: &mut TirModule) {
    let mut trapper = NeverDispatch {
        defined: module
            .functions
            .iter()
            .map(|func| {
                let func = func.borrow();
                (func.module_source.clone(), func.name.clone())
            })
            .collect(),
        never: FqTypeName::builtin(NEVER_TYPE_NAME),
    };
    for func_rc in &module.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = func.body.as_mut() {
            trapper.visit_block(body);
        }
    }
    for global in &mut module.globals {
        trapper.visit_expr(global.init.slot_expr_mut());
    }
}

struct NeverDispatch {
    defined: IndexSet<(ModuleSource, String)>,
    never: FqTypeName,
}

impl TirMutVisitor for NeverDispatch {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        self.walk_expr(expr);
        let TirExprKind::Call { func, args, .. } = &mut expr.kind else {
            return;
        };
        let on_never = func
            .method_info
            .as_ref()
            .is_some_and(|info| info.fq_base_struct_name() == self.never);
        if !on_never
            || self
                .defined
                .contains(&(func.module_source.clone(), func.name.clone()))
        {
            return;
        }
        let mut stmts: Vec<TirStmt> = std::mem::take(args)
            .into_vec()
            .into_iter()
            .map(|arg| TirStmt::new(TirStmtKind::Expr(arg.expr), expr.span))
            .collect();
        stmts.push(TirStmt::new(
            TirStmtKind::Expr(builtin_call("unreachable", Vec::new(), TypeTable::NEVER)),
            expr.span,
        ));
        expr.kind = TirExprKind::Block(TirBlock::new(stmts, expr.span));
    }
}
