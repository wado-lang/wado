//! Lower the `assert_failed` marker reify emits for an assertion failure, a
//! distinct callee precisely so this can treat it unlike an explicit `panic`. By
//! default it rewrites back to `core:rt::panic`, leaving no wrapper frame; under
//! `-f bare-asserts` (default at `-Os`) the cold block becomes `unreachable()`,
//! so the assertion still traps while its message-building falls out at DCE.

use crate::flat_package::FlatPackage;
use crate::synthesis::common::builtin_call;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeTable};
use crate::tir_visitor::TirMutVisitor;

/// The reify-emitted assert-failure callee (`core:rt::assert_failed`).
pub const ASSERT_FAILED: &str = "assert_failed";

/// The real trap-with-message routine `assert_failed` wraps.
const PANIC: &str = "panic";

/// Lower every `assert_failed(..)` call in `flat`. With `bare_asserts`, each
/// failure cold block becomes a bare trap; otherwise the call is rewritten to a
/// direct `panic`, leaving the diagnostic intact and the codegen unchanged.
pub fn lower(flat: &FlatPackage, bare_asserts: bool) {
    let mut visitor = AssertLowering { bare_asserts };
    for func_rc in &flat.functions {
        if let Some(body) = func_rc.borrow_mut().body.as_mut() {
            visitor.visit_block(body);
        }
    }
}

struct AssertLowering {
    bare_asserts: bool,
}

impl TirMutVisitor for AssertLowering {
    fn visit_block(&mut self, block: &mut TirBlock) {
        if self.bare_asserts && block_calls_assert_failed(block) {
            // `assert_failed` diverges, so every statement here is diagnostic
            // prep (cold_path, message building) or dead code after it. Replace
            // the lot with a bare trap; the WIR trap-based branch-hint inference
            // still marks the enclosing `if` guard cold, so no explicit
            // `cold_path()` marker is needed.
            let span = block.span;
            block.stmts = vec![TirStmt::new(
                TirStmtKind::Expr(builtin_call("unreachable", Vec::new(), TypeTable::NEVER)),
                span,
            )];
            return;
        }
        self.walk_block(block);
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        // Default mode: the marker is transparent — route it straight to
        // `panic` so codegen sees exactly what a direct `panic(<template>)`
        // would produce.
        if !self.bare_asserts
            && let TirExprKind::Call { func, .. } = &mut expr.kind
            && func.name == ASSERT_FAILED
        {
            func.name = PANIC.to_string();
        }
        self.walk_expr(expr);
    }
}

fn block_calls_assert_failed(block: &TirBlock) -> bool {
    block.stmts.iter().any(|s| {
        matches!(&s.kind,
            TirStmtKind::Expr(TirExpr { kind: TirExprKind::Call { func, .. }, .. })
                if func.name == ASSERT_FAILED)
    })
}
