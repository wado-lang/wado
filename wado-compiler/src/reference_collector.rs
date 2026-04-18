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
    AstId, Block, Condition, ConditionElement, Expr, Item, MatchExpr, Module, Pattern, Stmt,
    StructPatternField, TemplatePart,
};
use crate::hashmap::IndexMap;
use crate::name::ModuleSource;
use crate::symbol::{SymbolKey, Symbol, SymbolKind, VariableSymbol};
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
pub(crate) fn collect_references(
    modules: &IndexMap<ModuleSource, Module>,
) -> LocalBindings {
    let mut out = LocalBindings::default();
    for (module_source, module) in modules {
        let mut collector = Collector {
            module: module_source.clone(),
            scopes: Vec::new(),
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
    out: &'a mut LocalBindings,
}

impl Collector<'_> {
    fn push_scope(&mut self) {
        self.scopes.push(IndexMap::default());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define_local(&mut self, name: &str, ast_id: AstId, span: Span) {
        let key = SymbolKey::new(self.module.clone(), ast_id);
        let symbol = Symbol {
            name: name.to_string(),
            kind: SymbolKind::Variable(VariableSymbol {
                is_mut: false,
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

    fn visit_item(&mut self, item: &Item) {
        match item {
            Item::Function(f) => {
                if let Some(body) = &f.body {
                    self.push_scope();
                    for p in &f.params {
                        self.define_local(&p.name, p.id, p.name_span);
                    }
                    self.visit_block(body);
                    self.pop_scope();
                }
            }
            Item::Impl(imp) => {
                for m in &imp.methods {
                    if let Some(body) = &m.body {
                        self.push_scope();
                        for p in &m.params {
                            self.define_local(&p.name, p.id, p.name_span);
                        }
                        self.visit_block(body);
                        self.pop_scope();
                    }
                }
            }
            Item::Trait(t) => {
                for m in &t.methods {
                    if let Some(body) = &m.body {
                        self.push_scope();
                        for p in &m.params {
                            self.define_local(&p.name, p.id, p.name_span);
                        }
                        self.visit_block(body);
                        self.pop_scope();
                    }
                }
            }
            Item::Test(t) => {
                self.push_scope();
                self.visit_block(&t.body);
                self.pop_scope();
            }
            Item::Global(g) => {
                self.push_scope();
                self.visit_expr(&g.initializer);
                self.pop_scope();
            }
            _ => {}
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
                if let Some(v) = &s.value {
                    self.visit_expr(v);
                }
                self.bind_pattern(&s.pattern, s.id, s.name_span);
            }
            Stmt::Expr(s) => self.visit_expr(&s.expr),
            Stmt::Return(s) => {
                if let Some(v) = &s.value {
                    self.visit_expr(v);
                }
            }
            Stmt::TaskReturn(s) => self.visit_expr(&s.value),
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
                self.bind_pattern(&s.binding, s.id, s.span);
                self.visit_block(&s.body);
                self.pop_scope();
            }
            Stmt::Loop(s) => self.visit_block(&s.body),
            Stmt::Match(m) => self.visit_match_expr(m),
            Stmt::Break(s) => {
                if let Some(v) = &s.value {
                    self.visit_expr(v);
                }
            }
            Stmt::Continue(_) => {}
            Stmt::Assert(s) => {
                self.visit_expr(&s.condition);
                if let Some(msg) = &s.message {
                    self.visit_expr(msg);
                }
            }
            Stmt::LabeledBlock(s) => self.visit_block(&s.block),
        }
    }

    fn visit_condition(&mut self, cond: &Condition) {
        match cond {
            Condition::Expr(e) => self.visit_expr(e),
            Condition::LetChain { elements, .. } => {
                for el in elements {
                    match el {
                        ConditionElement::Let { pattern, expr, span } => {
                            self.visit_expr(expr);
                            // Bindings from let-chain patterns use the pattern span; the AST does
                            // not carry a dedicated id for the ConditionElement, so we use the
                            // `span.start` as a stable anchor via the pattern's textual name.
                            self.bind_pattern_anchor(pattern, *span);
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
            self.bind_match_pattern(&arm.pattern, arm.span);
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
            Expr::Literal(_) => {}
            Expr::Binary(e) => {
                self.visit_expr(&e.left);
                self.visit_expr(&e.right);
            }
            Expr::Unary(e) => self.visit_expr(&e.expr),
            Expr::Assign(e) => {
                self.visit_expr(&e.target);
                self.visit_expr(&e.value);
            }
            Expr::CompoundAssign(e) => {
                self.visit_expr(&e.target);
                self.visit_expr(&e.value);
            }
            Expr::ComparisonChain(e) => {
                self.visit_expr(&e.first);
                for c in &e.comparisons {
                    self.visit_expr(&c.right);
                }
            }
            Expr::Call(e) => {
                self.visit_expr(&e.callee);
                for a in &e.args {
                    self.visit_expr(a);
                }
            }
            Expr::MethodCall(e) => {
                self.visit_expr(&e.receiver);
                for a in &e.args {
                    self.visit_expr(a);
                }
            }
            Expr::StaticMethodCall(e) => {
                for a in &e.args {
                    self.visit_expr(a);
                }
            }
            Expr::FieldAccess(e) => self.visit_expr(&e.expr),
            Expr::Index(e) => {
                self.visit_expr(&e.expr);
                self.visit_expr(&e.index);
            }
            Expr::Block(b) => self.visit_block(b),
            Expr::If(e) => {
                self.push_scope();
                self.visit_condition(&e.condition);
                self.visit_block(&e.then_block);
                self.pop_scope();
                if let Some(eb) = &e.else_block {
                    self.visit_block(eb);
                }
            }
            Expr::Match(m) => self.visit_match_expr(m),
            Expr::Matches(m) => {
                self.visit_expr(&m.expr);
                // pattern bindings in `matches` do not escape; open a scope for the guard only.
                self.push_scope();
                self.bind_match_pattern(&m.pattern, m.span);
                if let Some(g) = &m.guard {
                    self.visit_expr(g);
                }
                self.pop_scope();
            }
            Expr::Closure(c) => {
                self.push_scope();
                for p in &c.params {
                    self.define_local(&p.name, p.id, p.name_span);
                }
                self.visit_expr(&c.body);
                self.pop_scope();
            }
            Expr::TemplateString(t) => {
                for part in &t.parts {
                    if let TemplatePart::Interpolation { expr, .. } = part {
                        self.visit_expr(expr);
                    }
                }
            }
            Expr::Cast(c) => self.visit_expr(&c.expr),
            Expr::StructLiteral(s) => {
                for field in &s.fields {
                    self.visit_expr(&field.value);
                }
            }
            Expr::TupleLiteral(t) => {
                for el in &t.elements {
                    self.visit_expr(el);
                }
            }
            Expr::LabeledBlock(lb) => self.visit_block(&lb.block),
            Expr::TryOp(t) => self.visit_expr(&t.expr),
            Expr::Spread(inner, _) => self.visit_expr(inner),
            Expr::Range(r) => {
                self.visit_expr(&r.start);
                self.visit_expr(&r.end);
            }
        }
    }

    /// Bind the names introduced by a `let` pattern using the `let` statement's
    /// own id as the defining [`AstId`]. For simple `let x = ...` this is
    /// exactly what LSP needs; for destructuring patterns this gives
    /// statement-level granularity rather than per-binding, which is good
    /// enough for MVP.
    fn bind_pattern(&mut self, pattern: &Pattern, let_id: AstId, name_span: Span) {
        match pattern {
            Pattern::Ident(name) | Pattern::MutIdent(name) => {
                self.define_local(name, let_id, name_span);
            }
            Pattern::Tuple(patterns, _) => {
                for p in patterns {
                    self.bind_pattern(p, let_id, name_span);
                }
            }
            Pattern::Struct { fields, .. } => {
                for StructPatternField { field_name, pattern, span } in fields {
                    match pattern {
                        Pattern::Ident(_) | Pattern::MutIdent(_) => {
                            self.define_local(field_name, let_id, *span);
                        }
                        _ => self.bind_pattern(pattern, let_id, *span),
                    }
                }
            }
            Pattern::Variant { bindings, .. } => {
                for p in bindings {
                    self.bind_pattern(p, let_id, name_span);
                }
            }
            Pattern::Or(pats) => {
                if let Some(p) = pats.first() {
                    self.bind_pattern(p, let_id, name_span);
                }
            }
            Pattern::Literal(_) | Pattern::Wildcard | Pattern::Range { .. } => {}
        }
    }

    /// Bind let-chain pattern using the pattern's span start as a synthetic
    /// anchor (no dedicated [`AstId`] exists for [`ConditionElement`]). Falls
    /// back to simple Ident bindings only; this is MVP behavior for if-let
    /// patterns.
    fn bind_pattern_anchor(&mut self, pattern: &Pattern, span: Span) {
        // let-chain patterns do not have their own AstId; we cannot surface them
        // to LSP as definitions. Still bind into the scope so nested uses
        // resolve lexically even if the binding AstId is a placeholder.
        match pattern {
            Pattern::Ident(name) | Pattern::MutIdent(name) => {
                if let Some(scope) = self.scopes.last_mut() {
                    scope.insert(name.clone(), AstId(0));
                }
                let _ = span; // unused
            }
            Pattern::Tuple(patterns, _) => {
                for p in patterns {
                    self.bind_pattern_anchor(p, span);
                }
            }
            Pattern::Struct { fields, .. } => {
                for StructPatternField { field_name, pattern, span: fspan } in fields {
                    match pattern {
                        Pattern::Ident(_) | Pattern::MutIdent(_) => {
                            if let Some(scope) = self.scopes.last_mut() {
                                scope.insert(field_name.clone(), AstId(0));
                            }
                            let _ = fspan;
                        }
                        _ => self.bind_pattern_anchor(pattern, *fspan),
                    }
                }
            }
            Pattern::Variant { bindings, .. } => {
                for p in bindings {
                    self.bind_pattern_anchor(p, span);
                }
            }
            Pattern::Or(pats) => {
                if let Some(p) = pats.first() {
                    self.bind_pattern_anchor(p, span);
                }
            }
            Pattern::Literal(_) | Pattern::Wildcard | Pattern::Range { .. } => {}
        }
    }

    fn bind_match_pattern(&mut self, pattern: &Pattern, span: Span) {
        // Match arm bindings — same placeholder AstId approach as let-chain.
        self.bind_pattern_anchor(pattern, span);
    }
}

