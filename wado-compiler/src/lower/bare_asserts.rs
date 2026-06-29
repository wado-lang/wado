//! Lower the `assert_failed` marker reify emits for assertion failures.
//!
//! `assert cond[, msg]` reifies to
//! `if !cond { cold_path(); assert_failed(<template>) }`, where `<template>`
//! formats the asserted operands through the whole `Formatter` / `Inspect` /
//! `String` stack (see `elaborator::reify::reify_assert`). The reify-emitted
//! `core:rt::assert_failed` is a distinct callee — a marker — so this
//! lowering can treat assertion failures differently from explicit
//! `panic(...)` calls:
//!
//! - default: rewrite the call back to `core:rt::panic`, so the marker
//!   never reaches codegen and the output is identical to a direct `panic` —
//!   no `assert_failed` wrapper frame, no codegen drift.
//! - `-f bare-asserts` (on by default at `-Os`): replace the whole cold block
//!   with a bare `unreachable()`. The assertion still checks and traps, but the
//!   message-building statements and diagnostic literals are gone; the
//!   now-unreachable formatting functions then fall out at the next DCE.
//!
//! It runs in `lower` *before* string-literal planning, so the dropped template
//! literals are never collected into the data section, and before NIR
//! conversion, so the discarded formatting is never lowered.

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
