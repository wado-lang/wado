//! Where an expression may change state without performing an effect. The
//! `self_comparison` lint and the `contract` clause check both ask it.

use crate::ast::{AstId, AstVisitor, Expr, SelfKind, UnaryOp, walk_expr};
use crate::semantics::Semantics;
use crate::symbol::SymbolKind;
use crate::tir::TypeId;

/// One way an expression may write.
pub(crate) enum WriteSite<'e> {
    /// The place an assignment, a `&mut`, or a method taking `&mut self`
    /// writes.
    Place(&'e Expr),
    /// An operand handed to a call as a parameter that reaches a `&mut`, which
    /// the callee may write through. A receiver counts only beneath the
    /// reference its method takes, since a `&self` method cannot write the
    /// receiver itself.
    Lent(&'e Expr),
    /// The callee of a call that is not a declared function: a function value,
    /// `fn` or `fn mut`, which may write through a `&mut` it captured.
    Opaque(&'e Expr),
    /// The receiver of a method call with no recorded dispatch, which nothing
    /// can see into.
    Unresolved(&'e Expr),
}

/// Hands `found` every [`WriteSite`] in `expr`, in source order.
pub(crate) fn for_each_write_site(sem: &Semantics, expr: &Expr, found: impl FnMut(WriteSite<'_>)) {
    Sites { sem, found }.visit_expr(expr);
}

struct Sites<'s, F> {
    sem: &'s Semantics,
    found: F,
}

impl<F: FnMut(WriteSite<'_>)> Sites<'_, F> {
    fn reaches_mut_ref(&self, operand: &Expr, beneath_refs: bool) -> bool {
        self.sem.expression_type(operand.id()).is_none_or(|ty| {
            let ty = if beneath_refs {
                self.sem.types.peel_refs(ty)
            } else {
                ty
            };
            self.sem.reaches_mut_ref(ty)
        })
    }

    /// Reports each argument of the call at `id` that hands its callee the
    /// means to write. That is the parameter's type, where the callee is
    /// known: a `&mut T` bound to a `&T` gives it no more than `T` holds.
    fn lend(&mut self, id: AstId, args: &[Expr]) {
        let params: Vec<&[TypeId]> = self
            .sem
            .callee_param_types_at(id)
            .filter(|params| params.len() == args.len())
            .collect();
        for (i, arg) in args.iter().enumerate() {
            let lent = if params.is_empty() {
                self.reaches_mut_ref(arg, false)
            } else {
                params
                    .iter()
                    .any(|params| self.sem.reaches_mut_ref(params[i]))
            };
            if lent {
                (self.found)(WriteSite::Lent(arg));
            }
        }
    }

    fn method_call(&mut self, receiver: &Expr, args: &[Expr], id: AstId) {
        let mut dispatches = self.sem.method_dispatches_at(id).peekable();
        if dispatches.peek().is_none() {
            (self.found)(WriteSite::Unresolved(receiver));
        } else if dispatches.any(|dispatch| dispatch.self_kind == SelfKind::MutRef) {
            (self.found)(WriteSite::Place(receiver));
        } else if self.reaches_mut_ref(receiver, true) {
            (self.found)(WriteSite::Lent(receiver));
        }
        self.lend(id, args);
    }

    fn call(&mut self, callee: &Expr, args: &[Expr], id: AstId) {
        if self.is_function_value(callee) {
            (self.found)(WriteSite::Opaque(callee));
        }
        self.lend(id, args);
    }

    /// Whether `callee` is a value of function type rather than a path to a
    /// declared function or method.
    fn is_function_value(&self, callee: &Expr) -> bool {
        let Expr::Ident(ident) = callee else {
            return true;
        };
        self.sem
            .referenced_symbol_at(ident.id)
            .is_some_and(|symbol| {
                matches!(symbol.kind, SymbolKind::Variable(_) | SymbolKind::Global(_))
            })
    }
}

impl<F: FnMut(WriteSite<'_>)> AstVisitor<'_> for Sites<'_, F> {
    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Assign(assign) => (self.found)(WriteSite::Place(&assign.target)),
            Expr::CompoundAssign(compound) => (self.found)(WriteSite::Place(&compound.target)),
            Expr::Unary(unary) if unary.op == UnaryOp::MutRef => {
                (self.found)(WriteSite::Place(&unary.expr));
            }
            Expr::MethodCall(call) => self.method_call(&call.receiver, &call.args, call.id),
            Expr::Call(call) => self.call(&call.callee, &call.args, call.id),
            Expr::StaticMethodCall(call) => self.lend(call.id, &call.args),
            _ => {}
        }
        walk_expr(self, expr);
    }
}
