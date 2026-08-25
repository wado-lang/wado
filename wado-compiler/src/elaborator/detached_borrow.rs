//! A `&mut` to a field or element of a replace-on-assign type is a fresh box,
//! not the place's storage, so a whole-value write through it never lands. See
//! `docs/wep-2026-06-13-reference-representation.md`.

use crate::ast::{AstId, AstVisitor, Block, Expr, Stmt, UnaryOp, walk_expr, walk_stmt};
use crate::compiler_host::CompilerHost;
use crate::tir::TypeId;
use crate::token::Span;

use super::Elaborator;
use super::types::TypeError;

/// Only a `variant` reaches this rule; [`Elaborator::is_replace_on_assign_place_type`]
/// refuses the borrow outright for the other replace-on-assign types.
const DETACHED_BORROW: &str = "cannot store a mutable reference to a field or element of a variant: it is \
     a detached copy, not the place's storage, so a whole-value write through it \
     would be lost; use it where it is taken, or assign the field itself";

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Report every detached borrow put somewhere that outlives the expression
    /// taking it. A borrow consumed in place — a call argument, a `match` or
    /// `if let` scrutinee — is left alone: it cannot outlive its use, which is
    /// what the WEP's temp + write-back carve-out rests on.
    pub(super) fn check_detached_borrows(&mut self, body: &Block) {
        let stored = {
            let mut walker = DetachedBorrowWalker::default();
            walker.visit_block(body);
            walker.stored
        };
        for (place, span) in stored {
            let Some(place_type) = self.sem.types.expression_types.get(&place).copied() else {
                continue;
            };
            if self.borrow_detaches_from(place_type) {
                let _ = self.emit(TypeError::CannotAssign {
                    message: DETACHED_BORROW.to_string(),
                    span,
                });
            }
        }
    }

    /// Whether a `&mut` to a place of this type is a box rather than the place
    /// itself: it is for a referent that is replaced on assign (`Ref` but not
    /// `RefMut`) — see the WEP's classification table.
    fn borrow_detaches_from(&self, place_type: TypeId) -> bool {
        let resolved = self.tysys.type_table.borrow().get(place_type).clone();
        self.tysys.is_ref_identity(&resolved)
            && !self
                .tysys
                .is_ref_mut_identity(&self.type_lookup(), &resolved)
    }
}

/// Collects `&mut <field-or-element>` borrows put into storage that outlives
/// them, as `(place expression, borrow span)`. Types are left to
/// [`Elaborator::check_detached_borrows`], so this walk borrows nothing.
#[derive(Default)]
struct DetachedBorrowWalker {
    stored: Vec<(AstId, Span)>,
}

impl AstVisitor for DetachedBorrowWalker {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(let_stmt) => self.record(let_stmt.value.as_ref()),
            Stmt::Return(ret) => self.record(ret.value.as_ref()),
            Stmt::TaskReturn(ret) => self.record(Some(&ret.value)),
            _ => {}
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::StructLiteral(lit) => {
                for field in &lit.fields {
                    self.record(Some(&field.value));
                }
            }
            Expr::TupleLiteral(lit) => {
                for element in &lit.elements {
                    self.record(Some(element));
                }
            }
            Expr::Assign(assign) => self.record(Some(&assign.value)),
            _ => {}
        }
        walk_expr(self, expr);
    }
}

impl DetachedBorrowWalker {
    /// Record `expr` if it is a `&mut` of a field or element. What that place's
    /// type is decides whether it detaches, and this walk cannot see types.
    fn record(&mut self, expr: Option<&Expr>) {
        let Some(Expr::Unary(unary)) = expr else {
            return;
        };
        if unary.op == UnaryOp::MutRef
            && matches!(&unary.expr, Expr::FieldAccess(_) | Expr::Index(_))
        {
            self.stored.push((unary.expr.id(), unary.span));
        }
    }
}
