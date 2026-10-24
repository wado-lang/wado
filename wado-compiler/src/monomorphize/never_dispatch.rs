//! A call dispatched on `!` or a newtype over it. Either holds every trait and
//! no impl is written for it (spec-types.md § Trait Bounds on `!`), so such a
//! call type-checks, yet no body answers it: it evaluates its arguments and
//! traps.

use crate::hashmap::IndexSet;
use crate::name::FqTypeName;
use crate::synthesis::common::builtin_call;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirModule, TirStmt, TirStmtKind};
use crate::tir_visitor::TirMutVisitor;
use crate::type_table::TypeTable;

pub(super) fn trap_never_dispatch_in_module(module: &mut TirModule) {
    let never_heads = {
        let tt = module.type_table.borrow();
        tt.all_types()
            .filter(|&(id, _)| tt.is_never(id))
            .map(|(id, _)| tt.fq_base_type_name(id))
            .collect()
    };
    let mut trapper = NeverDispatch { never_heads };
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
    never_heads: IndexSet<FqTypeName>,
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
            .is_some_and(|info| self.never_heads.contains(&info.fq_base_struct_name()));
        if !on_never {
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
