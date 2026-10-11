//! Where `unchecked` may stand: before a call whose callee a `contract` clause
//! binds, by its own clauses or by its trait's.

use crate::ast::{self, AstVisitor, Expr, Item, Module, UnaryOp};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::defs::DefId;
use crate::elaborator::sem::ModuleSemantics;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::resolve::Resolutions;

const NOT_A_CALL: &str = "`unchecked` marks a call to a function with a `contract` clause";
const NO_CONTRACT: &str = "the callee has no `contract` clause for `unchecked` to vouch for";

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
}

impl AstVisitor for Finder<'_> {
    fn visit_expr(&mut self, expr: &Expr) {
        if let Expr::Unary(unary) = expr
            && unary.op == UnaryOp::Unchecked
        {
            let callees = self.callees(&unary.expr);
            let message = if callees.is_empty() {
                Some(NOT_A_CALL)
            } else if !callees.iter().all(|&def| self.contracts.binds(def)) {
                Some(NO_CONTRACT)
            } else {
                None
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
        let has_contract = |func: &ast::Function| func.contracts().next().is_some();
        let trait_binds = |decl: &ast::TraitDecl, method: &ast::Function| {
            decl.contract_holder(&method.name).is_some()
        };
        let target = self.resolutions.defs().ast_id(def);
        self.items_declaring(def).iter().any(|item| match item {
            Item::Function(func) => func.id == target && has_contract(func),
            Item::Impl(block) => block.methods.iter().any(|method| {
                method.id == target
                    && (has_contract(method)
                        || block
                            .trait_type
                            .as_ref()
                            .and_then(|ty| self.resolutions.head_decl(ty))
                            .and_then(|decl| self.trait_decl(decl))
                            .is_some_and(|decl| trait_binds(decl, method)))
            }),
            Item::Trait(decl) => decl
                .methods
                .iter()
                .any(|method| method.id == target && trait_binds(decl, method)),
            _ => false,
        })
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
