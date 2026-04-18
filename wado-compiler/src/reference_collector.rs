//! Lightweight use-to-definition resolver for LSP queries.
//!
//! Walks each function body (and test body, global initializer) in an AST,
//! maintaining a lexical scope stack of `name → AstId`. Every [`IdentExpr`]
//! whose name binds to a local (`let` / parameter / closure parameter) is
//! recorded in the references map so that LSP can translate
//! `(module, IdentExpr.id)` into `(module, defining AstId)`.
//!
//! This intentionally sidesteps the full resolver: it does not assign types,
//! does not traverse imports, and does not error. It only answers "given this
//! identifier span, which binding introduced this name in the enclosing
//! lexical scope?" Idents whose names do not resolve to a local binding are
//! left alone — LSP falls back to the per-module symbol index for those.

use crate::ast::{
    self, AstId, AstVisitor, Block, Condition, ConditionElement, Expr, Function, MatchExpr, Module,
    Pattern, Stmt, Type,
};
use crate::hashmap::IndexMap;
use crate::name::ModuleSource;
use crate::symbol::{Symbol, SymbolKey, SymbolKind, VariableSymbol};
use crate::token::Span;

/// Collect (`use-site SymbolKey` → `definition SymbolKey`) references for every
/// module in `modules`, and emit [`Symbol`] entries for every local binding so
/// that `SymbolTable::get(&def_key)` returns something useful for LSP hover.
///
/// The returned [`LocalBindings`] carries both the cross-references map and
/// the synthesised local [`Symbol`] table keyed by the defining
/// [`SymbolKey`]. Local symbols are kept in a side table rather than
/// polluting `SymbolTable::modules` (which is the name index used by item
/// lookup).
pub(crate) fn collect_references(modules: &IndexMap<ModuleSource, Module>) -> LocalBindings {
    let mut out = LocalBindings::default();
    for (module_source, module) in modules {
        let mut collector = Collector {
            module: module_source.clone(),
            scopes: Vec::new(),
            pattern_is_mut: false,
            out: &mut out,
        };
        for item in &module.items {
            collector.visit_item(item);
        }
    }
    out
}

#[derive(Debug, Default)]
pub(crate) struct LocalBindings {
    /// `use → def` map. Both keys are `(module, AstId)`.
    pub(crate) references: IndexMap<SymbolKey, SymbolKey>,
    /// Locally-defined [`Symbol`]s (let bindings, parameters, closure
    /// parameters). Keyed by the binding's defining [`SymbolKey`].
    pub(crate) locals: IndexMap<SymbolKey, Symbol>,
}

struct Collector<'a> {
    module: ModuleSource,
    scopes: Vec<IndexMap<String, AstId>>,
    /// Mutability context passed down while descending a binding pattern.
    /// Set by the enclosing `Stmt::Let` / `Stmt::ForOf` before calling
    /// `visit_pattern`; reset afterwards. `Pattern::MutIdent` always binds
    /// mutably regardless of this flag.
    pattern_is_mut: bool,
    out: &'a mut LocalBindings,
}

impl Collector<'_> {
    fn push_scope(&mut self) {
        self.scopes.push(IndexMap::default());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define_local(&mut self, name: &str, ast_id: AstId, span: Span, is_mut: bool) {
        let key = SymbolKey::new(self.module.clone(), ast_id);
        let symbol = Symbol {
            name: name.to_string(),
            kind: SymbolKind::Variable(VariableSymbol {
                is_mut,
                is_reactive: false,
            }),
            defined_at: key.clone(),
            span: Some(span),
        };
        self.out.locals.insert(key, symbol);
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), ast_id);
        }
    }

    fn lookup(&self, name: &str) -> Option<AstId> {
        for scope in self.scopes.iter().rev() {
            if let Some(id) = scope.get(name) {
                return Some(*id);
            }
        }
        None
    }

    /// Bind a pattern with an explicit mutability context, saving and
    /// restoring `pattern_is_mut` around the nested walk.
    fn bind_pattern_with_mut(&mut self, pat: &Pattern, is_mut: bool) {
        let saved = self.pattern_is_mut;
        self.pattern_is_mut = is_mut;
        self.visit_pattern(pat);
        self.pattern_is_mut = saved;
    }
}

impl AstVisitor for Collector<'_> {
    fn visit_function(&mut self, func: &Function) {
        if let Some(body) = &func.body {
            self.push_scope();
            for p in &func.params {
                self.define_local(&p.name, p.id, p.name_span, p.is_mut);
            }
            self.visit_block(body);
            self.pop_scope();
        }
    }

    fn visit_block(&mut self, block: &Block) {
        self.push_scope();
        for stmt in &block.stmts {
            self.visit_stmt(stmt);
        }
        self.pop_scope();
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => {
                if let Some(val) = &s.value {
                    self.visit_expr(val);
                }
                self.bind_pattern_with_mut(&s.pattern, s.is_mut);
            }
            Stmt::If(s) => {
                self.push_scope();
                self.visit_condition(&s.condition);
                self.visit_block(&s.then_block);
                self.pop_scope();
                if let Some(eb) = &s.else_block {
                    self.visit_block(eb);
                }
            }
            Stmt::While(s) => {
                self.push_scope();
                self.visit_condition(&s.condition);
                self.visit_block(&s.body);
                self.pop_scope();
            }
            Stmt::For(s) => {
                self.push_scope();
                if let Some(init) = &s.init {
                    self.visit_stmt(init);
                }
                if let Some(cond) = &s.condition {
                    self.visit_condition(cond);
                }
                if let Some(update) = &s.update {
                    self.visit_expr(update);
                }
                self.visit_block(&s.body);
                self.pop_scope();
            }
            Stmt::ForOf(s) => {
                self.visit_expr(&s.iterable);
                self.push_scope();
                self.bind_pattern_with_mut(&s.binding, s.is_mut);
                self.visit_block(&s.body);
                self.pop_scope();
            }
            _ => ast::walk_stmt(self, stmt),
        }
    }

    fn visit_condition(&mut self, cond: &Condition) {
        match cond {
            Condition::Expr(e) => self.visit_expr(e),
            Condition::LetChain { elements, .. } => {
                for el in elements {
                    match el {
                        ConditionElement::Let { pattern, expr, .. } => {
                            self.visit_expr(expr);
                            // if-let / while-let bindings take mutability from
                            // the pattern's `MutIdent` marker; there is no
                            // enclosing `LetStmt` to carry a `let mut` flag.
                            self.bind_pattern_with_mut(pattern, false);
                        }
                        ConditionElement::Expr(e) => self.visit_expr(e),
                    }
                }
            }
        }
    }

    fn visit_match_expr(&mut self, m: &MatchExpr) {
        self.visit_expr(&m.expr);
        for arm in &m.arms {
            self.push_scope();
            self.bind_pattern_with_mut(&arm.pattern, false);
            if let Some(guard) = &arm.guard {
                self.visit_expr(guard);
            }
            self.visit_expr(&arm.body);
            self.pop_scope();
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Ident(e) => {
                if let Some(def_id) = self.lookup(&e.name) {
                    let use_key = SymbolKey::new(self.module.clone(), e.id);
                    let def_key = SymbolKey::new(self.module.clone(), def_id);
                    self.out.references.insert(use_key, def_key);
                }
            }
            Expr::If(e) => {
                self.push_scope();
                self.visit_condition(&e.condition);
                self.visit_block(&e.then_block);
                self.pop_scope();
                if let Some(eb) = &e.else_block {
                    self.visit_block(eb);
                }
            }
            Expr::Matches(m) => {
                self.visit_expr(&m.expr);
                // Pattern bindings in `matches` do not escape; open a scope
                // for the guard only.
                self.push_scope();
                self.bind_pattern_with_mut(&m.pattern, false);
                if let Some(g) = &m.guard {
                    self.visit_expr(g);
                }
                self.pop_scope();
            }
            Expr::Closure(c) => {
                self.push_scope();
                for p in &c.params {
                    self.define_local(&p.name, p.id, p.name_span, p.is_mut);
                }
                self.visit_expr(&c.body);
                self.pop_scope();
            }
            _ => ast::walk_expr(self, expr),
        }
    }

    fn visit_pattern(&mut self, pat: &Pattern) {
        match pat {
            Pattern::Ident { id, name, span } => {
                self.define_local(name, *id, *span, self.pattern_is_mut);
            }
            Pattern::MutIdent { id, name, span } => {
                self.define_local(name, *id, *span, true);
            }
            Pattern::Or(pats) => {
                // Or-patterns require all alternatives to bind the same names.
                // Walk only the first alternative; its leaf ids become the
                // defining `SymbolKey`s. Subsequent alternatives add no new
                // bindings.
                if let Some(p) = pats.first() {
                    self.visit_pattern(p);
                }
            }
            _ => ast::walk_pattern(self, pat),
        }
    }

    fn visit_type(&mut self, _ty: &Type) {
        // Types contain no value bindings; skip traversal entirely.
    }
}
