//! Where `unchecked` may stand: before a call whose callee a `contract` clause
//! binds, by its own clauses or by its trait's. An impl states clauses of its
//! own only on a method its trait leaves unbound, so a call through a bound
//! sees the trait's clauses or none.

use crate::ast::{self, AstVisitor, Expr, Item, Module, UnaryOp};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::defs::DefId;
use crate::elaborator::sem::ModuleSemantics;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::resolve::Resolutions;

const NOT_A_CALL: &str = "`unchecked` marks a call to a function with a `contract` clause";
const NO_CONTRACT: &str = "the callee has no `contract` clause for `unchecked` to vouch for";
const BOUND_NO_CONTRACT: &str = "through a bound, `unchecked` vouches for the trait's clauses, \
                                 and this trait method declares none";
const IMPL_CLAUSE: &str = "an impl writes no `contract` on a method its trait's clauses bind";
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
        if let Item::Impl(block) = item {
            finder.impl_clauses_under_trait_clauses(block);
        }
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
    /// An error for each clause `block` writes on a method its trait binds:
    /// a caller through a bound vouches for the trait's clauses, never an
    /// impl's.
    fn impl_clauses_under_trait_clauses(&mut self, block: &ast::ImplBlock) {
        let Some(decl) = self.contracts.implemented_trait(block) else {
            return;
        };
        for method in &block.methods {
            if let Some(clause) = method.contracts().next()
                && trait_binds(decl, method)
            {
                self.found.push(Diagnostic {
                    severity: Severity::Error,
                    code: Code::TraitDeclInvalid,
                    message: IMPL_CLAUSE.to_string(),
                    span: Some(DiagnosticSpan::from_span(&clause.span, None)),
                });
            }
        }
    }
    /// The declarations the call `expr` reaches, one per walk that reached it.
    fn callees(&self, expr: &Expr) -> Vec<DefId> {
        let types = &self.sem.types;
        match expr {
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
                if !statics.is_empty() {
                    return statics;
                }
                let Expr::Ident(ident) = &call.callee else {
                    return Vec::new();
                };
                self.sem
                    .bindings
                    .references
                    .get(&ident.id)
                    .and_then(|&declared| self.contracts.resolutions.defs().of_ast_id(declared))
                    .into_iter()
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// Whether every callee of `expr` is a trait method left without clauses:
    /// a call through a bound, where an impl's own clauses are out of sight.
    fn reaches_unbound_trait_method(&self, expr: &Expr) -> bool {
        let callees = self.callees(expr);
        !callees.is_empty()
            && callees
                .iter()
                .all(|&def| self.contracts.unbound_trait_method(def))
    }

    /// Whether `expr` reaches a call a `contract` clause binds.
    fn binds(&self, expr: &Expr) -> bool {
        let callees = self.callees(expr);
        !callees.is_empty() && callees.iter().all(|&def| self.contracts.binds(def))
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
}

impl AstVisitor for Finder<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if let Expr::Unary(unary) = expr
            && unary.op == UnaryOp::Unchecked
        {
            let message = if self.binds(&unary.expr) {
                None
            } else if self.binds_inside_chain(&unary.expr) {
                Some(INNER_CALL)
            } else if self.callees(&unary.expr).is_empty() {
                Some(NOT_A_CALL)
            } else if self.reaches_unbound_trait_method(&unary.expr) {
                Some(BOUND_NO_CONTRACT)
            } else {
                Some(NO_CONTRACT)
            };
            if let Some(message) = message {
                self.found.push(Diagnostic {
                    severity: Severity::Error,
                    code: Code::UncheckedInvalid,
                    message: message.to_string(),
                    span: Some(DiagnosticSpan::from_span(&unary.span, None)),
                });
            }
        }
        ast::walk_expr(self, expr);
    }
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

    /// Whether `def` declares a trait method its trait gives no clause, which
    /// a call through a bound reaches without seeing any impl's.
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
