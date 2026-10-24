//! Where `unchecked` may stand: before a call whose callee a `contract` clause
//! binds, by its own clauses or by its trait's.

use crate::ast::{self, AstVisitor, Expr, Item, Module, UnaryOp};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::defs::DefId;
use crate::elaborator::sem::ModuleSemantics;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::resolve::Resolutions;
use crate::token::Span;

const NOT_A_CALL: &str = "`unchecked` marks a call to a function with a `contract` clause";
const NO_CONTRACT: &str = "the callee has no `contract` clause for `unchecked` to vouch for";
const TRAIT_NO_CONTRACT: &str = "this trait method declares no `contract` clause, and `unchecked` \
                                 on a call to it vouches for the trait's alone";
const ASSIGNED: &str = "an index assignment takes no `unchecked`: the marker vouches for a read";
const INNER_CALL: &str = "postfix operators bind tighter than `unchecked`, so it marks the \
                          whole chain; write `(unchecked …)` around the call it vouches for";

/// An error for each `unchecked` in `module` that marks no call a `contract`
/// clause binds. `sem` holds the facts the body walk recorded for `module`.
pub(super) fn misplaced_unchecked(
    module: &Module,
    sem: &ModuleSemantics,
    resolutions: &Resolutions,
    modules: &IndexMap<ModuleSource, Module>,
) -> Vec<Diagnostic> {
    let mut finder = Finder {
        sem,
        contracts: Contracts {
            resolutions,
            modules,
        },
        found: Vec::new(),
    };
    for item in &module.items {
        finder.visit_item(item);
    }
    finder.found
}

struct Finder<'a> {
    sem: &'a ModuleSemantics,
    contracts: Contracts<'a, 'a>,
    found: Vec<Diagnostic>,
}

impl Finder<'_> {
    /// The declarations the call `expr` reaches, one per walk that reached it,
    /// or `None` where `expr` is no call. A call reaching none (a closure, a
    /// dispatched interface operation) is `Some` of nothing.
    fn callees(&self, expr: &Expr) -> Option<Vec<DefId>> {
        let types = &self.sem.types;
        let defs = match expr {
            Expr::MethodCall(call) => types
                .all(|facts| &facts.method_dispatch, call.id)
                .filter_map(|dispatch| dispatch.method_def)
                .collect(),
            Expr::StaticMethodCall(call) => types
                .all(|facts| &facts.static_method_dispatch, call.id)
                .filter_map(|dispatch| dispatch.method_def)
                .collect(),
            Expr::Index(index) => types
                .all(|facts| &facts.operator_dispatch, index.id)
                .filter_map(|dispatch| dispatch.method_def)
                .collect(),
            Expr::Call(call) => {
                let statics: Vec<DefId> = types
                    .all(|facts| &facts.static_method_dispatch, call.id)
                    .filter_map(|dispatch| dispatch.method_def)
                    .collect();
                match &call.callee {
                    Expr::Ident(ident) if statics.is_empty() => self
                        .sem
                        .bindings
                        .references
                        .get(&ident.id)
                        .and_then(|&declared| self.contracts.resolutions.defs().of_ast_id(declared))
                        .into_iter()
                        .collect(),
                    _ => statics,
                }
            }
            _ => return None,
        };
        Some(defs)
    }

    /// Whether `expr` reaches a call a `contract` clause binds.
    fn binds(&self, expr: &Expr) -> bool {
        self.callees(expr)
            .is_some_and(|callees| every(&callees, |def| self.contracts.binds(def)))
    }

    /// Whether a postfix operand inside `expr`'s chain is a call a `contract`
    /// clause binds: what `unchecked` meant to mark in `unchecked f(x)?`.
    fn binds_inside_chain(&self, expr: &Expr) -> bool {
        let operand = match expr {
            Expr::TryOp(op) => &op.expr,
            Expr::MethodCall(call) => &call.receiver,
            Expr::FieldAccess(access) => &access.expr,
            Expr::Index(index) => &index.expr,
            _ => return false,
        };
        self.binds(operand) || self.binds_inside_chain(operand)
    }

    /// Why `unchecked` cannot mark `operand`, or `None` where it can.
    fn misplaced(&self, operand: &Expr) -> Option<&'static str> {
        let callees = self.callees(operand);
        if let Some(callees) = &callees
            && every(callees, |def| self.contracts.binds(def))
        {
            return None;
        }
        Some(if self.binds_inside_chain(operand) {
            INNER_CALL
        } else {
            match callees {
                None => NOT_A_CALL,
                Some(callees)
                    if every(&callees, |def| self.contracts.unbound_trait_method(def)) =>
                {
                    TRAIT_NO_CONTRACT
                }
                Some(_) => NO_CONTRACT,
            }
        })
    }
}

impl AstVisitor<'_> for Finder<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        let assignment = match expr {
            Expr::Assign(assign) => Some((&assign.target, &assign.value)),
            Expr::CompoundAssign(compound) => Some((&compound.target, &compound.value)),
            _ => None,
        };
        if let Some((Expr::Index(index), value)) = assignment
            && let Some(span) = index.unchecked
        {
            self.report(ASSIGNED, span);
            self.visit_expr(&index.expr);
            self.visit_expr(&index.index);
            self.visit_expr(value);
            return;
        }
        let verdict = match expr {
            // The parser folds `unchecked` into a subscript it marks, so a
            // subscript under one was marked already.
            Expr::Unary(unary) if unary.op == UnaryOp::Unchecked => Some((
                match &unary.expr {
                    Expr::Index(_) => Some(NOT_A_CALL),
                    operand => self.misplaced(operand),
                },
                unary.span,
            )),
            Expr::Index(index) => index.unchecked.map(|span| (self.misplaced(expr), span)),
            _ => None,
        };
        if let Some((Some(message), span)) = verdict {
            self.report(message, span);
        }
        ast::walk_expr(self, expr);
    }
}

impl Finder<'_> {
    fn report(&mut self, message: &str, span: Span) {
        self.found.push(Diagnostic {
            severity: Severity::Error,
            code: Code::UncheckedInvalid,
            message: message.to_string(),
            span: Some(DiagnosticSpan::from_span(&span, None)),
        });
    }
}

/// Whether `callees` names a declaration and `holds` of each.
fn every(callees: &[DefId], holds: impl Fn(DefId) -> bool) -> bool {
    !callees.is_empty() && callees.iter().all(|&def| holds(def))
}

/// Which declarations a `contract` clause binds, read off the AST.
pub(super) struct Contracts<'r, 'a> {
    pub(super) resolutions: &'r Resolutions,
    pub(super) modules: &'a IndexMap<ModuleSource, Module>,
}

impl<'a> Contracts<'_, 'a> {
    /// Whether the function `def` declares is bound by a `contract` clause:
    /// one it writes, or one its trait declares for it.
    pub(super) fn binds(&self, def: DefId) -> bool {
        let target = self.resolutions.defs().ast_id(def);
        self.items_declaring(def).iter().any(|item| match item {
            Item::Function(func) => func.id == target && has_contract(func),
            Item::Impl(block) => block.methods.iter().any(|method| {
                method.id == target
                    && (has_contract(method)
                        || self
                            .implemented_trait(block)
                            .is_some_and(|decl| trait_binds(decl, method)))
            }),
            Item::Trait(decl) => decl
                .methods
                .iter()
                .any(|method| method.id == target && trait_binds(decl, method)),
            _ => false,
        })
    }

    /// Whether `def` declares a trait method its trait gives no clause: a call
    /// through a bound, or to a default body, sees no impl's.
    fn unbound_trait_method(&self, def: DefId) -> bool {
        let target = self.resolutions.defs().ast_id(def);
        self.items_declaring(def).iter().any(|item| match item {
            Item::Trait(decl) => decl
                .methods
                .iter()
                .any(|method| method.id == target && !trait_binds(decl, method)),
            _ => false,
        })
    }

    /// The trait `block` implements, if it implements one.
    fn implemented_trait(&self, block: &ast::ImplBlock) -> Option<&'a ast::TraitDecl> {
        block
            .trait_type
            .as_ref()
            .and_then(|ty| self.resolutions.head_decl(ty))
            .and_then(|decl| self.trait_decl(decl))
    }

    /// The trait `def` declares.
    pub(super) fn trait_decl(&self, def: DefId) -> Option<&'a ast::TraitDecl> {
        let target = self.resolutions.defs().ast_id(def);
        self.items_declaring(def)
            .iter()
            .find_map(|item| match item {
                Item::Trait(decl) if decl.id == target => Some(decl),
                _ => None,
            })
    }

    /// The items of the module `def` is declared in.
    fn items_declaring(&self, def: DefId) -> &'a [Item] {
        let module = self.resolutions.defs().module(def);
        self.modules.get(module).map_or(&[], |m| m.items.as_slice())
    }
}

fn has_contract(func: &ast::Function) -> bool {
    func.contracts().next().is_some()
}

fn trait_binds(decl: &ast::TraitDecl, method: &ast::Function) -> bool {
    decl.contract_holder(&method.name).is_some()
}
