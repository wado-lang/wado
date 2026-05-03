// AST definitions for Wado

use crate::hashmap::{IndexMap, IndexSet};
use crate::token::Span;

/// Module-local, parse-stable identifier for AST nodes that bear semantic
/// significance (items, named members, parameters).
///
/// The parser assigns ids in DFS order starting from `0`; ids for a given
/// module are densely packed in `0..Module::ast_id_count()`. Every AST node
/// observed by the compiler has a real id within its module — there is no
/// sentinel value. Builtin modules (`lib/core/*.wado`) are parsed like any
/// other module and receive their own dense id range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AstId(pub u32);

impl AstId {
    /// Allocate a fresh `AstId` for a transient AST node that is never owned
    /// by a parsed [`Module`].
    ///
    /// Use cases:
    /// - Synthesized `Type::Named` / `Type::Generic` operands passed to type
    ///   query functions (`cm_size_with_registry`, `synthesize_lift_*`), which
    ///   only read the type's `name` and structural fields.
    /// - Unit test fixtures that fabricate AST nodes to exercise registries.
    ///
    /// The returned id is globally unique within a process. It is *not* a
    /// sentinel: there is no reserved "synthetic" value. Because transient
    /// nodes never enter a `Module.items` tree and never become
    /// [`SymbolKey`](crate::symbol::SymbolKey) lookup targets, they cannot
    /// collide with parser-allocated ids — those live in per-module dense
    /// ranges indexed by `(ModuleSource, AstId)`.
    #[must_use]
    pub fn fresh() -> Self {
        use core::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Discriminator for the kind of AST node an [`AstPtr`] points to.
///
/// Mirrors the shape of `Item` and the named members reachable from items
/// (struct fields, enum/variant cases, params, etc.). Used to disambiguate
/// nodes that share a span (e.g. a single-field struct and its sole field
/// at the same opening brace position).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AstNodeKind {
    Function,
    Struct,
    Enum,
    Variant,
    Flags,
    Trait,
    Newtype,
    Effect,
    Global,
    Resource,
    World,
    Impl,
    Use,
    Test,
    TupleType,
    StructField,
    EnumCase,
    VariantCase,
    FlagsVariant,
    InterfaceMethod,
    AssocTypeDecl,
    AssocConst,
    AssocTypeBinding,
    GenericParam,
    Param,
}

/// Position-resolvable pointer to an AST node.
///
/// Pairs an [`AstNodeKind`] with the source [`Span`] of the node. Two pointers
/// compare equal iff both halves match, which is sufficient as a stable
/// identity within a single module version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AstPtr {
    pub kind: AstNodeKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Module {
    pub items: Vec<Item>,
    /// Inner attributes like `#![no_prelude]`
    pub inner_attributes: Vec<InnerAttribute>,
    /// Shebang line, if present (e.g., "#!/usr/bin/env wado").
    shebang: Option<String>,
    /// Content of the __DATA__ section, if present in the source file.
    /// This is available after parsing for tooling (test harnesses, IDEs).
    data_section: Option<String>,
    /// Paths referenced by `#include_str` and `#include_bytes` literals, collected during parsing.
    include_paths: IndexSet<String>,
    /// Total number of [`AstId`]s allocated for this module during parsing.
    /// Ids occupy the range `0..ast_id_count`.
    ast_id_count: u32,
}

/// Inner attribute like `#![no_prelude]`, `#![wasm_module("mem")]`, or
/// `#![generated(by = "tool", sources = ["path"])]`.
///
/// Arguments use the same `AttrArg` representation as outer attributes so that
/// key-value and key-array metadata (e.g. `by = "tool"`,
/// `sources = ["a.wit", "b.wit"]`) survive unparse/reformat.
#[derive(Debug, Clone)]
pub struct InnerAttribute {
    pub name: String,
    pub args: Vec<AttrArg>,
    pub span: Span,
}

impl InnerAttribute {
    /// Find the first key = "value" argument with the given key.
    ///
    /// For `#![generated(by = "gale")]`, `attr.kv_value("by")` returns `Some("gale")`.
    /// Key = \[...\] array arguments are ignored — use [`Self::kv_array`] for those.
    pub fn kv_value(&self, key: &str) -> Option<&str> {
        self.args.iter().find_map(|arg| {
            if let AttrArg::KeyValue(k, v) = arg {
                if k == key { Some(v.as_str()) } else { None }
            } else {
                None
            }
        })
    }

    /// Find the first key = \["value", ...\] argument with the given key and
    /// return its values.
    ///
    /// For `#![generated(sources = ["a.wit", "b.wit"])]`,
    /// `attr.kv_array("sources")` returns the slice `["a.wit", "b.wit"]`.
    pub fn kv_array(&self, key: &str) -> Option<&[String]> {
        self.args.iter().find_map(|arg| {
            if let AttrArg::KeyArray(k, vs) = arg {
                if k == key { Some(vs.as_slice()) } else { None }
            } else {
                None
            }
        })
    }
}

impl Module {
    /// Creates a new module with the given items and no data section.
    pub fn new(items: Vec<Item>) -> Self {
        Self {
            items,
            inner_attributes: Vec::new(),
            shebang: None,
            data_section: None,
            include_paths: IndexSet::default(),
            ast_id_count: 0,
        }
    }

    /// Creates a new module with the given items, shebang, and data section.
    pub fn with_metadata(
        items: Vec<Item>,
        inner_attributes: Vec<InnerAttribute>,
        shebang: Option<String>,
        data_section: Option<String>,
        include_paths: IndexSet<String>,
        ast_id_count: u32,
    ) -> Self {
        Self {
            items,
            inner_attributes,
            shebang,
            data_section,
            include_paths,
            ast_id_count,
        }
    }

    /// Returns the total number of [`AstId`]s allocated for this module.
    /// Ids occupy `0..ast_id_count()`.
    pub fn ast_id_count(&self) -> u32 {
        self.ast_id_count
    }

    /// Allocate a fresh dense [`AstId`] at the end of this module's
    /// per-module range.  Use this when post-parse synthesis injects a
    /// new AST node into [`Self::items`] and the node needs a
    /// [`SymbolKey`](crate::symbol::SymbolKey)-compatible id (for
    /// example an `Item::Impl` that the resolver walks).  The returned
    /// id is guaranteed not to collide with any parser-allocated id.
    ///
    /// Prefer this over [`AstId::fresh`] for nodes that enter a module
    /// tree — `AstId::fresh` is globally unique but lives outside the
    /// `(ModuleSource, AstId)` dense range, so symbol-lookup machinery
    /// keyed on that range cannot find them.
    #[must_use]
    pub fn alloc_ast_id(&mut self) -> AstId {
        let id = AstId(self.ast_id_count);
        self.ast_id_count += 1;
        id
    }

    /// Return the [`Span`] of the AST node bearing the given [`AstId`].
    /// Returns `None` if no id-bearing node in this module has that id.
    pub fn span_of_ast_id(&self, target: AstId) -> Option<Span> {
        struct SpanFinder {
            target: AstId,
            result: Option<Span>,
        }
        impl AstVisitor for SpanFinder {
            fn visit_id(&mut self, id: AstId, span: Span) {
                if self.result.is_none() && id == self.target {
                    self.result = Some(span);
                }
            }
        }
        let mut finder = SpanFinder {
            target,
            result: None,
        };
        for item in &self.items {
            finder.visit_item(item);
            if finder.result.is_some() {
                break;
            }
        }
        finder.result
    }

    /// Find the [`AstId`] of the smallest id-bearing AST node whose span
    /// contains the given 1-based `(line, column)` position. Returns `None`
    /// if no id-bearing node contains the position.
    pub fn ast_id_at(&self, line: usize, column: usize) -> Option<AstId> {
        struct Finder {
            line: usize,
            column: usize,
            best: Option<(AstId, Span)>,
        }
        impl AstVisitor for Finder {
            fn visit_id(&mut self, id: AstId, span: Span) {
                if !span_contains(span, self.line, self.column) {
                    return;
                }
                match self.best {
                    None => self.best = Some((id, span)),
                    Some((_, current)) if span_byte_len(span) <= span_byte_len(current) => {
                        self.best = Some((id, span));
                    }
                    _ => {}
                }
            }
        }
        let mut finder = Finder {
            line,
            column,
            best: None,
        };
        for item in &self.items {
            finder.visit_item(item);
        }
        finder.best.map(|(id, _)| id)
    }
}

/// Returns true if `(line, column)` lies in `[span.line:column, span.end_line:end_column)`.
fn span_contains(span: Span, line: usize, column: usize) -> bool {
    if line < span.line || line > span.end_line {
        return false;
    }
    if line == span.line && column < span.column {
        return false;
    }
    if line == span.end_line && column >= span.end_column {
        return false;
    }
    true
}

fn span_byte_len(span: Span) -> usize {
    span.end.saturating_sub(span.start)
}

/// Structural AST visitor used for id-based queries (LSP position lookup,
/// density checks) and scope-aware analyses (reference collection).
///
/// Every `visit_*` method defaults to its corresponding free `walk_*`
/// function, which recurses into children through the visitor's own
/// methods. Implementers override only the nodes where their behavior
/// differs from plain structural traversal.
///
/// `visit_id` receives every [`AstId`] / [`Span`] pair encountered during a
/// walk — including container ids (items, statements, expressions, blocks)
/// and leaf id-bearing nodes that have no dedicated `visit_*` method (struct
/// fields, enum cases, generic/closure parameters, etc.).
pub trait AstVisitor: Sized {
    /// Invoked for every [`AstId`] emitted during traversal.
    ///
    /// Default is a no-op; an id-emitter overrides this to collect all ids,
    /// a scope-aware visitor leaves it as no-op and overrides the relevant
    /// `visit_*` methods instead.
    fn visit_id(&mut self, _id: AstId, _span: Span) {}

    fn visit_item(&mut self, item: &Item) {
        walk_item(self, item);
    }

    fn visit_function(&mut self, func: &Function) {
        walk_function(self, func);
    }

    fn visit_interface_method(&mut self, method: &InterfaceMethod) {
        walk_interface_method(self, method);
    }

    fn visit_block(&mut self, block: &Block) {
        walk_block(self, block);
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_condition(&mut self, cond: &Condition) {
        walk_condition(self, cond);
    }

    fn visit_match_expr(&mut self, m: &MatchExpr) {
        walk_match_expr(self, m);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        walk_expr(self, expr);
    }

    fn visit_pattern(&mut self, pat: &Pattern) {
        walk_pattern(self, pat);
    }

    fn visit_type(&mut self, ty: &Type) {
        walk_type(self, ty);
    }
}

pub fn walk_item<V: AstVisitor>(v: &mut V, item: &Item) {
    match item {
        Item::Use(u) => {
            v.visit_id(u.id, u.span);
            v.visit_id(u.source_id, u.source_span);
            for it in &u.items {
                if let UseItem::Simple { id, name_span, .. } = it {
                    v.visit_id(*id, *name_span);
                }
            }
        }
        Item::Function(func) => v.visit_function(func),
        Item::Interface(e) => {
            v.visit_id(e.id, e.span);
            for m in &e.methods {
                v.visit_interface_method(m);
            }
        }
        Item::Struct(s) => {
            v.visit_id(s.id, s.span);
            for p in &s.type_params {
                v.visit_id(p.id, p.span);
            }
            for field in &s.fields {
                v.visit_id(field.id, field.span);
                v.visit_type(&field.ty);
            }
        }
        Item::Enum(e) => {
            v.visit_id(e.id, e.span);
            for p in &e.type_params {
                v.visit_id(p.id, p.span);
            }
            for case in &e.cases {
                v.visit_id(case.id, case.span);
            }
        }
        Item::Variant(vr) => {
            v.visit_id(vr.id, vr.span);
            for p in &vr.type_params {
                v.visit_id(p.id, p.span);
            }
            for case in &vr.cases {
                v.visit_id(case.id, case.span);
                if let Some(payload) = &case.payload {
                    v.visit_type(payload);
                }
            }
        }
        Item::Flags(fl) => {
            v.visit_id(fl.id, fl.span);
            for va in &fl.flags {
                v.visit_id(va.id, va.span);
            }
        }
        Item::Newtype(n) => {
            v.visit_id(n.id, n.span);
            for p in &n.type_params {
                v.visit_id(p.id, p.span);
            }
            v.visit_type(&n.ty);
        }
        Item::TupleTypeDecl(t) => v.visit_id(t.id, t.span),
        Item::Impl(i) => {
            v.visit_id(i.id, i.span);
            for p in &i.type_params {
                v.visit_id(p.id, p.span);
            }
            if let Some(trait_ty) = &i.trait_type {
                v.visit_type(trait_ty);
            }
            v.visit_type(&i.ty);
            for binding in &i.associated_types {
                v.visit_id(binding.id, binding.span);
                v.visit_type(&binding.ty);
            }
            for c in &i.constants {
                v.visit_id(c.id, c.span);
                v.visit_type(&c.ty);
                v.visit_expr(&c.value);
            }
            for m in &i.methods {
                v.visit_function(m);
            }
        }
        Item::Trait(t) => {
            v.visit_id(t.id, t.span);
            for p in &t.type_params {
                v.visit_id(p.id, p.span);
            }
            for assoc in &t.associated_types {
                v.visit_id(assoc.id, assoc.span);
            }
            for m in &t.methods {
                v.visit_function(m);
            }
        }
        Item::Resource(r) => {
            v.visit_id(r.id, r.span);
            for p in &r.type_params {
                v.visit_id(p.id, p.span);
            }
            for m in &r.methods {
                v.visit_interface_method(m);
            }
        }
        Item::World(w) => v.visit_id(w.id, w.span),
        Item::Test(t) => {
            v.visit_id(t.id, t.span);
            v.visit_block(&t.body);
        }
        Item::Global(g) => {
            v.visit_id(g.id, g.span);
            v.visit_type(&g.ty);
            v.visit_expr(&g.initializer);
        }
    }
}

pub fn walk_function<V: AstVisitor>(v: &mut V, func: &Function) {
    v.visit_id(func.id, func.span);
    for p in &func.type_params {
        v.visit_id(p.id, p.span);
    }
    for param in &func.params {
        v.visit_id(param.id, param.span);
        v.visit_type(&param.ty);
    }
    if let Some(ret) = &func.return_type {
        v.visit_type(ret);
    }
    for (id, span) in &func.effect_ids {
        v.visit_id(*id, *span);
    }
    if let Some(body) = &func.body {
        v.visit_block(body);
    }
}

pub fn walk_interface_method<V: AstVisitor>(v: &mut V, method: &InterfaceMethod) {
    v.visit_id(method.id, method.span);
    for param in &method.params {
        v.visit_id(param.id, param.span);
        v.visit_type(&param.ty);
    }
    if let Some(ret) = &method.return_type {
        v.visit_type(ret);
    }
}

pub fn walk_block<V: AstVisitor>(v: &mut V, block: &Block) {
    v.visit_id(block.id, block.span);
    for stmt in &block.stmts {
        v.visit_stmt(stmt);
    }
}

pub fn walk_stmt<V: AstVisitor>(v: &mut V, stmt: &Stmt) {
    v.visit_id(stmt.id(), stmt.span());
    match stmt {
        Stmt::Let(s) => {
            v.visit_pattern(&s.pattern);
            if let Some(ty) = &s.ty {
                v.visit_type(ty);
            }
            if let Some(val) = &s.value {
                v.visit_expr(val);
            }
        }
        Stmt::Expr(s) => v.visit_expr(&s.expr),
        Stmt::Return(s) => {
            if let Some(val) = &s.value {
                v.visit_expr(val);
            }
        }
        Stmt::TaskReturn(s) => v.visit_expr(&s.value),
        Stmt::If(s) => {
            v.visit_condition(&s.condition);
            v.visit_block(&s.then_block);
            if let Some(eb) = &s.else_block {
                v.visit_block(eb);
            }
        }
        Stmt::While(s) => {
            v.visit_condition(&s.condition);
            v.visit_block(&s.body);
        }
        Stmt::For(s) => {
            if let Some(init) = &s.init {
                v.visit_stmt(init);
            }
            if let Some(cond) = &s.condition {
                v.visit_condition(cond);
            }
            if let Some(update) = &s.update {
                v.visit_expr(update);
            }
            v.visit_block(&s.body);
        }
        Stmt::ForOf(s) => {
            v.visit_pattern(&s.binding);
            v.visit_expr(&s.iterable);
            v.visit_block(&s.body);
        }
        Stmt::Loop(s) => v.visit_block(&s.body),
        Stmt::Match(m) => v.visit_match_expr(m),
        Stmt::Break(s) => {
            if let Some(val) = &s.value {
                v.visit_expr(val);
            }
        }
        Stmt::Continue(_) => {}
        Stmt::Assert(s) => {
            v.visit_expr(&s.condition);
            if let Some(msg) = &s.message {
                v.visit_expr(msg);
            }
        }
        Stmt::LabeledBlock(s) => v.visit_block(&s.block),
    }
}

pub fn walk_condition<V: AstVisitor>(v: &mut V, cond: &Condition) {
    match cond {
        Condition::Expr(e) => v.visit_expr(e),
        Condition::LetChain { elements, .. } => {
            for el in elements {
                match el {
                    ConditionElement::Let { pattern, expr, .. } => {
                        v.visit_pattern(pattern);
                        v.visit_expr(expr);
                    }
                    ConditionElement::Expr(e) => v.visit_expr(e),
                }
            }
        }
    }
}

pub fn walk_match_expr<V: AstVisitor>(v: &mut V, m: &MatchExpr) {
    // NOTE: `m.id` is emitted by the caller (`walk_expr` for `Expr::Match` or
    // `walk_stmt` for `Stmt::Match`), since both share the same `MatchExpr::id`.
    v.visit_expr(&m.expr);
    for arm in &m.arms {
        v.visit_pattern(&arm.pattern);
        if let Some(guard) = &arm.guard {
            v.visit_expr(guard);
        }
        v.visit_expr(&arm.body);
    }
}

pub fn walk_expr<V: AstVisitor>(v: &mut V, expr: &Expr) {
    v.visit_id(expr.id(), expr.span());
    match expr {
        Expr::Ident(i) => {
            for segment in &i.segments {
                v.visit_id(segment.id, segment.span);
            }
        }
        Expr::Literal(_) => {}
        Expr::Binary(e) => {
            v.visit_expr(&e.left);
            v.visit_expr(&e.right);
        }
        Expr::Unary(e) => v.visit_expr(&e.expr),
        Expr::Assign(e) => {
            v.visit_expr(&e.target);
            v.visit_expr(&e.value);
        }
        Expr::CompoundAssign(e) => {
            v.visit_expr(&e.target);
            v.visit_expr(&e.value);
        }
        Expr::ComparisonChain(e) => {
            v.visit_expr(&e.first);
            for c in &e.comparisons {
                v.visit_expr(&c.right);
            }
        }
        Expr::Call(e) => {
            v.visit_expr(&e.callee);
            for ty in &e.type_args {
                v.visit_type(ty);
            }
            for a in &e.args {
                v.visit_expr(a);
            }
        }
        Expr::MethodCall(e) => {
            v.visit_expr(&e.receiver);
            v.visit_id(e.method_id, e.method_span);
            for ty in &e.type_args {
                v.visit_type(ty);
            }
            for a in &e.args {
                v.visit_expr(a);
            }
        }
        Expr::StaticMethodCall(e) => {
            v.visit_type(&e.target_type);
            v.visit_id(e.method_id, e.method_span);
            for ty in &e.type_args {
                v.visit_type(ty);
            }
            for a in &e.args {
                v.visit_expr(a);
            }
        }
        Expr::FieldAccess(e) => {
            v.visit_expr(&e.expr);
            v.visit_id(e.field_id, e.field_span);
        }
        Expr::Index(e) => {
            v.visit_expr(&e.expr);
            v.visit_expr(&e.index);
        }
        Expr::Block(b) => v.visit_block(b),
        Expr::If(e) => {
            v.visit_condition(&e.condition);
            v.visit_block(&e.then_block);
            if let Some(eb) = &e.else_block {
                v.visit_block(eb);
            }
        }
        Expr::Match(m) => v.visit_match_expr(m),
        Expr::Matches(m) => {
            v.visit_expr(&m.expr);
            v.visit_pattern(&m.pattern);
            if let Some(guard) = &m.guard {
                v.visit_expr(guard);
            }
        }
        Expr::Closure(c) => {
            for p in &c.params {
                v.visit_id(p.id, p.name_span);
                if let Some(ty) = &p.ty {
                    v.visit_type(ty);
                }
            }
            v.visit_expr(&c.body);
        }
        Expr::TemplateString(t) => {
            for part in &t.parts {
                if let TemplatePart::Interpolation { expr, .. } = part {
                    v.visit_expr(expr);
                }
            }
        }
        Expr::Cast(c) => {
            v.visit_expr(&c.expr);
            v.visit_type(&c.target_type);
        }
        Expr::StructLiteral(s) => {
            if let (Some(name_id), Some(name_span)) = (s.name_id, s.name_span) {
                v.visit_id(name_id, name_span);
            }
            for field in &s.fields {
                v.visit_id(field.name_id, field.name_span);
                v.visit_expr(&field.value);
            }
        }
        Expr::TupleLiteral(t) => {
            for el in &t.elements {
                v.visit_expr(el);
            }
        }
        Expr::LabeledBlock(lb) => v.visit_block(&lb.block),
        Expr::TryOp(t) => v.visit_expr(&t.expr),
        Expr::Spread(inner, _) => v.visit_expr(inner),
        Expr::Range(r) => {
            v.visit_expr(&r.start);
            v.visit_expr(&r.end);
        }
        Expr::WithHandler(w) => {
            for binding in &w.handlers {
                if let Some(effect) = &binding.effect {
                    v.visit_id(binding.id, effect.span());
                    v.visit_type(effect);
                }
                v.visit_expr(&binding.handler);
            }
            v.visit_block(&w.body);
        }
        Expr::Resume(r) => v.visit_expr(&r.value),
    }
}

pub fn walk_pattern<V: AstVisitor>(v: &mut V, pat: &Pattern) {
    match pat {
        Pattern::Ident { id, span, .. } | Pattern::MutIdent { id, span, .. } => {
            v.visit_id(*id, *span);
        }
        Pattern::Tuple(ps, _) | Pattern::Or(ps) => {
            for p in ps {
                v.visit_pattern(p);
            }
        }
        Pattern::Struct { fields, .. } => {
            for field in fields {
                v.visit_pattern(&field.pattern);
            }
        }
        Pattern::Variant {
            name_id,
            name_span,
            bindings,
            ..
        } => {
            if let Some(id) = name_id {
                v.visit_id(*id, *name_span);
            }
            for p in bindings {
                v.visit_pattern(p);
            }
        }
        Pattern::Literal(_) | Pattern::Wildcard | Pattern::Range { .. } => {}
    }
}

pub fn walk_type<V: AstVisitor>(v: &mut V, ty: &Type) {
    match ty {
        Type::Named(t) => v.visit_id(t.id, t.span),
        Type::Generic(t) => {
            v.visit_id(t.id, t.span);
            for a in &t.args {
                v.visit_type(a);
            }
        }
        Type::NamespacedGeneric(t) => {
            v.visit_id(t.id, t.span);
            for a in &t.args {
                v.visit_type(a);
            }
        }
        Type::Function(ft) => {
            for p in &ft.params {
                v.visit_type(p);
            }
            v.visit_type(&ft.return_type);
            for (id, span) in &ft.effect_ids {
                v.visit_id(*id, *span);
            }
        }
        Type::Tuple(ts) => {
            for t in ts {
                v.visit_type(t);
            }
        }
        Type::Reference(t) | Type::MutReference(t) => v.visit_type(t),
        Type::TypePackSpread(_, _) => {}
    }
}

impl Module {
    /// Returns the inner attributes.
    pub fn inner_attributes(&self) -> &[InnerAttribute] {
        &self.inner_attributes
    }

    /// Returns true if the module has the `#![no_prelude]` attribute.
    pub fn has_no_prelude(&self) -> bool {
        self.inner_attributes.iter().any(|a| a.name == "no_prelude")
    }

    /// Returns true if the module has the `#![TODO]` attribute.
    /// All tests in a TODO module must fail; passing tests become failures.
    pub fn has_todo(&self) -> bool {
        self.inner_attributes.iter().any(|a| a.name == "TODO")
    }

    /// Returns true if the module has the `#![generated]` attribute.
    /// Indicates machine-generated code (e.g. wado-from-idl, gale).
    pub fn has_generated(&self) -> bool {
        self.inner_attributes.iter().any(|a| a.name == "generated")
    }

    /// Returns the `wasm_module` name if `#![wasm_module("name")]` is present.
    pub fn wasm_module(&self) -> Option<&str> {
        self.inner_attributes
            .iter()
            .find(|a| a.name == "wasm_module")
            .and_then(|a| a.args.first())
            .map(AttrArg::as_str)
    }

    /// Returns the value of a scalar `key = "value"` argument on any
    /// `#![generated(...)]` inner attribute.
    ///
    /// Scans all `#![generated(...)]` attributes and returns the first matching
    /// `key = "value"` pair. For example, given
    /// `#![generated(by = "wado-from-idl")]`,
    /// `module.generated_meta("by")` returns `Some("wado-from-idl")`.
    pub fn generated_meta(&self, key: &str) -> Option<&str> {
        self.inner_attributes
            .iter()
            .filter(|a| a.name == "generated")
            .find_map(|a| a.kv_value(key))
    }

    /// Returns the values of a `key = ["v1", "v2", ...]` array argument on any
    /// `#![generated(...)]` inner attribute.
    ///
    /// For `#![generated(sources = ["a.wit", "b.wit"])]`,
    /// `module.generated_meta_array("sources")` returns `["a.wit", "b.wit"]`.
    pub fn generated_meta_array(&self, key: &str) -> Option<&[String]> {
        self.inner_attributes
            .iter()
            .filter(|a| a.name == "generated")
            .find_map(|a| a.kv_array(key))
    }

    /// Returns the shebang line, if present.
    pub fn shebang(&self) -> Option<&str> {
        self.shebang.as_deref()
    }

    /// Returns the content of the __DATA__ section, if present.
    pub fn data_section(&self) -> Option<&str> {
        self.data_section.as_deref()
    }

    pub fn include_paths(&self) -> &IndexSet<String> {
        &self.include_paths
    }
}

#[derive(Debug, Clone)]
pub enum Item {
    Use(UseDecl),
    Function(Function),
    Interface(InterfaceDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    Variant(VariantDecl),
    Flags(FlagsDecl),
    Newtype(Newtype),
    TupleTypeDecl(TupleTypeDecl),
    Impl(ImplBlock),
    Trait(TraitDecl),
    Resource(ResourceDecl),
    World(WorldDecl),
    Test(TestDecl),
    Global(GlobalDecl),
}

/// Test declaration: `test "name" { ... }` or `test { ... }`
#[derive(Debug, Clone)]
pub struct TestDecl {
    pub id: AstId,
    /// Attributes applied to this test (e.g., `#[expect_trap]`).
    pub attributes: Vec<Attribute>,
    /// Optional test name (string literal). If None, identified by <file:line>.
    pub name: Option<String>,
    pub body: Block,
    pub span: Span,
}

/// Global variable declaration: `global name: Type = expr;`
/// or `pub global mut name: Type = expr;`
#[derive(Debug, Clone)]
pub struct GlobalDecl {
    pub id: AstId,
    pub name: String,
    /// Span of the identifier token alone (for LSP name-targeted queries).
    pub name_span: Span,
    pub ty: Type,
    pub initializer: Expr,
    pub mutable: bool,
    pub is_pub: bool,
    pub attributes: Vec<Attribute>,
    pub span: Span,
}

/// A single argument in an attribute.
///
/// Distinguishes between string literals, bare identifiers, and key=value pairs so that
/// `unparse` can reconstruct the original syntax faithfully.
///
/// Examples:
/// - `#[cm("wasi:cli/stdout")]`          → `[Str("wasi:cli/stdout")]`
/// - `#[inline(always)]`                           → `[Ident("always")]`
/// - `#[serde(rename = "type")]`                   → `[KeyValue("rename", "type")]`
/// - `#[canonical("wasi", "stream-new")]`          → `[Str("wasi"), Str("stream-new")]`
/// - `#[timeout_ms(120000)]`                       → `[Number("120000")]`
/// - `#![generated(sources = ["a.wit", "b.wit"])]` → `[KeyArray("sources", ["a.wit", "b.wit"])]`
#[derive(Debug, Clone)]
pub enum AttrArg {
    /// A quoted string literal, e.g. `"value"`.
    Str(String),
    /// A bare identifier, e.g. `always` or `default`.
    Ident(String),
    /// A key = "value" pair, e.g. `rename = "type"`.
    KeyValue(String, String),
    /// A key = \["value", ...\] pair whose value is a string array literal,
    /// e.g. `sources = ["a.wit", "b.wit"]`.
    KeyArray(String, Vec<String>),
    /// A numeric literal, e.g. `120000`.
    Number(String),
}

impl AttrArg {
    /// Returns the string value: for `Str`/`Ident`/`Number` the contained string;
    /// for `KeyValue` the value side; for `KeyArray` the first element (empty if none).
    pub fn as_str(&self) -> &str {
        match self {
            Self::Str(s) | Self::Ident(s) | Self::Number(s) => s,
            Self::KeyValue(_, v) => v,
            Self::KeyArray(_, vs) => vs.first().map(String::as_str).unwrap_or(""),
        }
    }
}

/// Attribute like #[cm("...")]
#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    /// Arguments passed to the attribute.
    ///
    /// - `#[canonical("wasi", "stream-new")]` → `[Str("wasi"), Str("stream-new")]`
    /// - `#[serde(rename = "type")]`           → `[KeyValue("rename", "type")]`
    /// - `#[serde(default)]`                   → `[Ident("default")]`
    pub args: Vec<AttrArg>,
    pub cm_import: Option<CmImport>,
    pub span: Span,
}

impl Attribute {
    /// Find the value of a key-value argument by key name.
    ///
    /// For `#[serde(rename = "type")]`, `attr.kv_value("rename")` returns `Some("type")`.
    pub fn kv_value(&self, key: &str) -> Option<&str> {
        self.args.iter().find_map(|arg| {
            if let AttrArg::KeyValue(k, v) = arg {
                if k == key { Some(v.as_str()) } else { None }
            } else {
                None
            }
        })
    }

    /// Return true if any arg matches the given name as an identifier, string, or key in a key-value pair.
    ///
    /// For `#[serde(default)]`, `attr.has_arg("default")` returns `true`.
    pub fn has_arg(&self, name: &str) -> bool {
        self.args.iter().any(|arg| match arg {
            AttrArg::Str(s) | AttrArg::Ident(s) | AttrArg::Number(s) => s == name,
            AttrArg::KeyValue(k, _) | AttrArg::KeyArray(k, _) => k == name,
        })
    }
}

/// Parsed Component Model import path
/// e.g., "wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream"
#[derive(Debug, Clone)]
pub struct CmImport {
    /// Namespace (e.g., "wasi")
    pub namespace: String,
    /// Package (e.g., "cli")
    pub package: String,
    /// Interface (e.g., "stdout")
    pub interface: String,
    /// Version (e.g., "0.3.0-rc-2025-09-16")
    pub version: Option<String>,
    /// Function name (e.g., "write-via-stream")
    pub function: Option<String>,
}

impl CmImport {
    /// Parse a CM import path string
    /// Format: "namespace:package/interface@version#function"
    /// Examples:
    ///   "wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream"
    ///   "wasi:cli/terminal-input@0.3.0-rc-2025-09-16"
    pub fn parse(s: &str) -> Option<CmImport> {
        // Split by '#' first to extract function name
        let (path, function) = if let Some(pos) = s.rfind('#') {
            (&s[..pos], Some(s[pos + 1..].to_string()))
        } else {
            (s, None)
        };

        // Split by '@' to extract version
        let (path, version) = if let Some(pos) = path.rfind('@') {
            (&path[..pos], Some(path[pos + 1..].to_string()))
        } else {
            (path, None)
        };

        // Split by ':' to extract namespace
        let (namespace, rest) = path.split_once(':')?;

        // Split by '/' to extract package and interface
        let (package, interface) = rest.split_once('/')?;

        Some(CmImport {
            namespace: namespace.to_string(),
            package: package.to_string(),
            interface: interface.to_string(),
            version,
            function,
        })
    }

    /// Get the full interface path (e.g., "wasi:cli/stdout@0.3.0-rc-2025-09-16")
    pub fn interface_path(&self) -> String {
        let mut path = format!("{}:{}/{}", self.namespace, self.package, self.interface);
        if let Some(ref ver) = self.version {
            path.push('@');
            path.push_str(ver);
        }
        path
    }
}

/// Resource declaration like `resource Foo;` or `resource Foo<T> { fn method(...); }`
#[derive(Debug, Clone)]
pub struct ResourceDecl {
    pub id: AstId,
    pub name: String,
    pub is_pub: bool,
    /// Generic type parameters: `resource Future<T> { ... }`
    pub type_params: Vec<GenericParam>,
    pub attrs: Vec<Attribute>,
    /// Methods declared within the resource block (reuses `InterfaceMethod` structure)
    pub methods: Vec<InterfaceMethod>,
    pub span: Span,
}

/// World declaration
/// ```wado
/// world CliCommand {
///     import Stdout {
///         write_via_stream,
///     }
///     export async fn run() -> Result<(), ()>;
/// }
/// ```
#[derive(Debug, Clone)]
pub struct WorldDecl {
    pub id: AstId,
    pub name: String,
    pub is_pub: bool,
    pub attrs: Vec<Attribute>,
    pub imports: Vec<WorldImport>,
    pub exports: Vec<WorldExport>,
    pub span: Span,
}

/// A world import group
/// ```wado
/// import EffectName {
///     function_name_1,
///     function_name_2,
/// }
/// ```
#[derive(Debug, Clone)]
pub struct WorldImport {
    pub interface_name: String,
    pub functions: Vec<String>,
    pub span: Span,
}

/// A world export declaration
/// ```wado
/// export async fn run() -> Result<(), ()>;
/// export fn get_version() -> string;
/// ```
#[derive(Debug, Clone)]
pub struct WorldExport {
    pub name: String,
    pub is_async: bool,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub span: Span,
}

/// Use declaration item with optional renaming
/// Supports simple imports, effect function imports, wildcard imports, and namespace imports:
/// - Simple: `name` or `name as alias`
/// - Effect functions: `Effect::{func1, func2}`
/// - Wildcard: `_` (load module without binding names)
/// - Namespace: `use name from "..."` (import entire module as namespace)
#[derive(Debug, Clone)]
pub enum UseItem {
    /// Simple import: `name` or `name as alias`
    Simple {
        /// `AstId` for the name identifier — used by LSP to record a use→def
        /// edge from the specifier name back to the imported symbol.
        id: AstId,
        name: String,
        /// Span of just the `name` identifier (narrower than the whole item).
        name_span: Span,
        alias: Option<String>,
    },
    /// Effect with functions: `Effect::{func1, func2}`
    InterfaceFunctions {
        interface_name: String,
        functions: Vec<UseItemSimple>,
    },
    /// Wildcard import: `use _ from "..."` (load module for side effects only)
    Wildcard,
    /// Namespace import: `use name from "..."` (import entire module as namespace)
    Namespace { name: String },
}

/// Simple use item (used within effect function imports)
#[derive(Debug, Clone)]
pub struct UseItemSimple {
    pub name: String,
    pub alias: Option<String>,
}

/// Generic attribute-value tree produced by `with { ... }` clauses.
///
/// Scalars and containers only — no identifier references or expressions,
/// per WEP 2026-04-12 (Kiln) §M5. Deterministic insertion order via
/// [`IndexMap`] keeps round-tripping and cache-key encoding stable.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Array(Vec<AttrValue>),
    Object(IndexMap<String, AttrValue>),
}

impl AttrValue {
    /// Borrow the inner string, if this is a [`AttrValue::String`].
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            AttrValue::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Borrow the inner object, if this is a [`AttrValue::Object`].
    #[must_use]
    pub fn as_object(&self) -> Option<&IndexMap<String, AttrValue>> {
        match self {
            AttrValue::Object(o) => Some(o),
            _ => None,
        }
    }

    /// Borrow the inner array, if this is a [`AttrValue::Array`].
    #[must_use]
    pub fn as_array(&self) -> Option<&[AttrValue]> {
        match self {
            AttrValue::Array(a) => Some(a.as_slice()),
            _ => None,
        }
    }
}

/// Import attributes for `with { ... }` clause.
///
/// Stores every key/value pair as a generic [`AttrValue`] tree. The three
/// historical keys (`version`, `integrity`, `type_hint`) are still accessed
/// via thin accessor methods so older call sites compile unchanged. New
/// consumers (e.g. Kiln inline generators) read raw entries via
/// [`ImportAttributes::get`] and [`ImportAttributes::generator`].
#[derive(Debug, Clone, Default)]
pub struct ImportAttributes {
    /// Top-level key/value entries, in parse order.
    pub entries: IndexMap<String, AttrValue>,
}

impl ImportAttributes {
    fn get_str(&self, key: &str) -> Option<String> {
        self.entries
            .get(key)
            .and_then(AttrValue::as_str)
            .map(str::to_string)
    }

    #[must_use]
    pub fn version(&self) -> Option<String> {
        self.get_str("version")
    }

    #[must_use]
    pub fn integrity(&self) -> Option<String> {
        self.get_str("integrity")
    }

    #[must_use]
    pub fn type_hint(&self) -> Option<String> {
        self.get_str("type")
    }

    /// Inline Kiln generator configuration (`with { generator: { ... } }`).
    #[must_use]
    pub fn generator(&self) -> Option<&IndexMap<String, AttrValue>> {
        self.entries.get("generator").and_then(AttrValue::as_object)
    }

    /// Raw lookup for any top-level attribute.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&AttrValue> {
        self.entries.get(key)
    }
}

/// Use declaration with ESM-like syntax:
/// `use {items} from "source"`
/// `use {items} from "source" with { version: "1.0" }`
/// `pub use {items} from "source"` (re-export)
#[derive(Debug, Clone)]
pub struct UseDecl {
    pub id: AstId,
    /// Whether this is a public re-export
    pub is_pub: bool,
    /// Import source (e.g., "core:cli", "wasi:filesystem", "./utils.wado")
    pub source: String,
    /// Span of the source string literal (without surrounding quotes in the
    /// start/end columns; used by LSP for path jumps).
    pub source_span: Span,
    /// `AstId` for the source string literal — use→def target when a cursor
    /// lands inside the `"./path"` portion of a use declaration.
    pub source_id: AstId,
    /// Items being imported
    pub items: Vec<UseItem>,
    /// Optional import attributes
    pub attributes: Option<ImportAttributes>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub id: AstId,
    pub name: String,
    /// Span of the function name identifier alone (for LSP name-targeted queries).
    pub name_span: Span,
    pub is_pub: bool,
    /// Whether this function is exported at the Component Model boundary (world export)
    pub is_export: bool,
    /// Whether this is an async function (`export async fn`).
    /// Async functions use `task return` instead of `return` to deliver results
    /// without terminating the function.
    pub is_async: bool,
    /// Generic type parameters: `fn swap<T>(a: T, b: T) -> T`
    pub type_params: Vec<GenericParam>,
    pub attrs: Vec<Attribute>,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub effects: Vec<String>,
    /// Parallel to `effects`: `(AstId, Span)` of each effect-name identifier as
    /// it appeared in the `with` clause. Used by the resolver to record
    /// use->def references for LSP jump-to-def.
    pub effect_ids: Vec<(AstId, Span)>,
    /// Parameters declared in `stores[param1, param2]` — the function may store these references.
    pub stores: Vec<String>,
    /// Function body. None indicates a compiler built-in (bodyless declaration like `pub fn foo();`)
    pub body: Option<Block>,
    pub span: Span,
}

/// Self parameter kind: &self or &mut self
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfKind {
    None,
    Ref,
    MutRef,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub id: AstId,
    pub name: String,
    /// Span of the parameter name identifier (or `self` token for self params).
    pub name_span: Span,
    pub ty: Type,
    pub self_kind: SelfKind,
    pub is_mut: bool,
    /// Optional default value expression: `fn foo(x: i32 = 0)`.
    /// Must be a pure expression (no effects). Only trailing parameters may have defaults.
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub id: AstId,
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Let(LetStmt),
    Expr(ExprStmt),
    Return(ReturnStmt),
    /// `task return expr;` — delivers the async task result without terminating the function.
    /// Only valid inside `export async fn` bodies.
    TaskReturn(TaskReturnStmt),
    If(IfStmt),
    While(WhileStmt),
    For(ForStmt),
    ForOf(ForOfStmt),
    Loop(LoopStmt),
    Match(Box<MatchExpr>),
    Break(BreakStmt),
    Continue(ContinueStmt),
    Assert(AssertStmt),
    LabeledBlock(LabeledBlockStmt),
}

/// Labeled block statement: `LABEL: { ... }`
/// Creates a new scope with local bindings. The label is required to reduce syntactic ambiguity.
#[derive(Debug, Clone)]
pub struct LabeledBlockStmt {
    pub id: AstId,
    pub label: String,
    pub block: Block,
    pub span: Span,
}

/// Assert statement: `assert expr;` or `assert expr, "message";`
/// If the expression is false, prints a power-assert style error message and calls unreachable
#[derive(Debug, Clone)]
pub struct AssertStmt {
    pub id: AstId,
    pub condition: Expr,
    /// Optional message expression (typically a String literal or template string)
    pub message: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct LetStmt {
    pub id: AstId,
    pub pattern: Pattern,
    /// Span of the pattern's leading token: exactly the identifier for
    /// `Ident`/`MutIdent` patterns, or the opening delimiter/constructor for
    /// destructuring patterns. For LSP name queries this is sufficient for the
    /// simple cases; richer pattern-internal positions can be derived from
    /// `pattern` itself.
    pub name_span: Span,
    pub is_mut: bool,
    pub is_reactive: bool,
    pub ty: Option<Type>,
    /// Initializer expression, or `None` for uninitialized declarations (`let x: i32;`).
    pub value: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ExprStmt {
    pub id: AstId,
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ReturnStmt {
    pub id: AstId,
    pub value: Option<Expr>,
    pub span: Span,
}

/// `task return expr;` — delivers the async task result without terminating the function.
/// Only valid inside `export async fn` bodies.
#[derive(Debug, Clone)]
pub struct TaskReturnStmt {
    pub id: AstId,
    pub value: Expr,
    pub span: Span,
}

/// A single element in a let-chain condition
#[derive(Debug, Clone)]
pub enum ConditionElement {
    /// `let PAT = EXPR` — pattern match element
    Let {
        pattern: Pattern,
        expr: Expr,
        span: Span,
    },
    /// `BOOL_EXPR` — boolean expression element
    Expr(Expr),
}

/// Condition in control flow statements: either a regular expression or a let chain.
/// Used in `if`, `while`, and `for` statements.
#[derive(Debug, Clone)]
pub enum Condition {
    /// Regular boolean expression: `if x > 0 { ... }` or `while x > 0 { ... }`
    Expr(Expr),
    /// Let chain: `if let PAT = EXPR && BOOL && let PAT2 = EXPR2 { ... }`
    /// Also handles simple `if let PAT = EXPR { ... }` (single Let element, no guards).
    LetChain {
        elements: Vec<ConditionElement>,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct IfStmt {
    pub id: AstId,
    pub condition: Condition,
    pub then_block: Block,
    pub else_block: Option<Block>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct WhileStmt {
    pub id: AstId,
    pub condition: Condition,
    pub body: Block,
    pub span: Span,
}

/// C-style for loop: `for (init; condition; update) { body }`
/// Also supports pattern conditions: `for init; let Some(x) = iter.next(); update { body }`
#[derive(Debug, Clone)]
pub struct ForStmt {
    pub id: AstId,
    /// Initialization statement (e.g., `let i = 0`)
    pub init: Option<Box<Stmt>>,
    /// Loop condition (e.g., `i < 10` or `let Some(x) = iter.next()`)
    pub condition: Option<Condition>,
    /// Update expression (e.g., `i = i + 1`)
    pub update: Option<Expr>,
    pub body: Block,
    pub span: Span,
}

/// For-of loop: `for let item of array { body }`
/// Iterates over elements of an Array<T>
#[derive(Debug, Clone)]
pub struct ForOfStmt {
    pub id: AstId,
    /// Pattern to bind each element (Ident for simple, Struct/Tuple for destructuring)
    pub binding: Pattern,
    /// Whether the binding is mutable
    pub is_mut: bool,
    /// The array expression to iterate over
    pub iterable: Expr,
    pub body: Block,
    pub span: Span,
}

/// Infinite loop: `loop { body }`
#[derive(Debug, Clone)]
pub struct LoopStmt {
    pub id: AstId,
    pub body: Block,
    pub span: Span,
}

/// Break statement: `break;`, `break label;`, or `break label: expr;`
#[derive(Debug, Clone)]
pub struct BreakStmt {
    pub id: AstId,
    /// Optional label to break to (for labeled blocks)
    pub label: Option<String>,
    /// Optional value to return from the labeled block
    pub value: Option<Box<Expr>>,
    pub span: Span,
}

/// Continue statement: `continue;`
#[derive(Debug, Clone)]
pub struct ContinueStmt {
    pub id: AstId,
    pub span: Span,
}

impl Stmt {
    /// Returns the parse-stable [`AstId`] for this statement.
    pub fn id(&self) -> AstId {
        match self {
            Stmt::Let(s) => s.id,
            Stmt::Expr(s) => s.id,
            Stmt::Return(s) => s.id,
            Stmt::TaskReturn(s) => s.id,
            Stmt::If(s) => s.id,
            Stmt::While(s) => s.id,
            Stmt::For(s) => s.id,
            Stmt::ForOf(s) => s.id,
            Stmt::Loop(s) => s.id,
            Stmt::Match(s) => s.id,
            Stmt::Break(s) => s.id,
            Stmt::Continue(s) => s.id,
            Stmt::Assert(s) => s.id,
            Stmt::LabeledBlock(s) => s.id,
        }
    }

    /// Returns the source [`Span`] for this statement.
    pub fn span(&self) -> Span {
        match self {
            Stmt::Let(s) => s.span,
            Stmt::Expr(s) => s.span,
            Stmt::Return(s) => s.span,
            Stmt::TaskReturn(s) => s.span,
            Stmt::If(s) => s.span,
            Stmt::While(s) => s.span,
            Stmt::For(s) => s.span,
            Stmt::ForOf(s) => s.span,
            Stmt::Loop(s) => s.span,
            Stmt::Match(s) => s.span,
            Stmt::Break(s) => s.span,
            Stmt::Continue(s) => s.span,
            Stmt::Assert(s) => s.span,
            Stmt::LabeledBlock(s) => s.span,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Expr {
    Ident(IdentExpr),
    Literal(LiteralExpr),
    Binary(Box<BinaryExpr>),
    Unary(Box<UnaryExpr>),
    Assign(Box<AssignExpr>),
    CompoundAssign(Box<CompoundAssignExpr>),
    ComparisonChain(Box<ComparisonChainExpr>),
    Call(Box<CallExpr>),
    MethodCall(Box<MethodCallExpr>),
    StaticMethodCall(Box<StaticMethodCallExpr>),
    FieldAccess(Box<FieldAccessExpr>),
    Index(Box<IndexExpr>),
    Block(Box<Block>),
    If(Box<IfExpr>),
    Match(Box<MatchExpr>),
    Matches(Box<MatchesExpr>),
    Closure(Box<ClosureExpr>),
    TemplateString(Box<TemplateStringExpr>),
    Cast(Box<CastExpr>),
    StructLiteral(Box<StructLiteralExpr>),
    TupleLiteral(Box<TupleLiteralExpr>),
    /// Labeled block expression: `label: { ... }` where the last expression is the value
    LabeledBlock(Box<LabeledBlockExpr>),
    /// Postfix `?` operator for error propagation on `Result<T, E>` and `Option<T>`.
    TryOp(Box<TryOpExpr>),
    /// Spread expression inside a tuple literal: `[..expr]`
    /// At compile time, splices the tuple's elements into the enclosing tuple.
    Spread(Box<Expr>, Span),
    /// Range expression: `a..<b` (exclusive) or `a..=b` (inclusive)
    Range(Box<RangeExpr>),
    /// Effect handler installation block: `with E1 => h1, E2 => h2 do { body }`.
    /// See `docs/wep-2026-04-11-effect-handler.md`.
    WithHandler(Box<WithHandlerExpr>),
    /// `resume value` — control-flow expression valid only inside an effect
    /// handler method. Delivers `value` to the suspended computation; in the
    /// MVP (no post-resume code) it lowers to `return value`.
    Resume(Box<ResumeExpr>),
}

/// `with E1 => h1, E2 => h2 do { body }` — installs effect handlers for the
/// duration of `body`. The block's value is the value of `body`.
#[derive(Debug, Clone)]
pub struct WithHandlerExpr {
    pub id: AstId,
    pub handlers: Vec<EffectHandlerBinding>,
    pub body: Block,
    pub span: Span,
}

/// A single `Effect => handler` binding inside `with ... do`.
///
/// `effect` is `None` for handler bundling: `with &mut value do { ... }`
/// installs the value as a handler for every effect it implements.
///
/// The effect is stored as a full `Type`, which lets the parser accept
/// generic effect names (e.g., `Stream<u8>`) without losing information;
/// later compiler phases decide which forms they actually support.
#[derive(Debug, Clone)]
pub struct EffectHandlerBinding {
    pub id: AstId,
    /// Effect type on the LHS of `=>` (e.g., `Stdout`, `Stream<u8>`). `None`
    /// for bundled handlers (`with &mut value do { ... }`).
    pub effect: Option<Type>,
    /// Handler expression, e.g., `&mut mock`.
    pub handler: Expr,
    pub span: Span,
}

/// `resume value` — see `WithHandlerExpr` and the WEP for semantics.
#[derive(Debug, Clone)]
pub struct ResumeExpr {
    pub id: AstId,
    pub value: Expr,
    pub span: Span,
}

/// Labeled block expression: `label: { ... }` that produces a value
/// The value is the last expression in the block (if any).
#[derive(Debug, Clone)]
pub struct LabeledBlockExpr {
    pub id: AstId,
    pub label: String,
    pub block: Block,
    pub span: Span,
}

/// Postfix `?` operator: `expr?`
#[derive(Debug, Clone)]
pub struct TryOpExpr {
    pub id: AstId,
    pub expr: Expr,
    pub span: Span,
}

/// Range kind: exclusive (..<) or inclusive (..=)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RangeKind {
    Exclusive, // ..<
    Inclusive, // ..=
}

/// Range expression: `start..<end` or `start..=end`
#[derive(Debug, Clone)]
pub struct RangeExpr {
    pub id: AstId,
    pub start: Expr,
    pub end: Expr,
    pub kind: RangeKind,
    pub span: Span,
}

impl Expr {
    /// Returns the parse-stable [`AstId`] for this expression.
    ///
    /// For `Expr::Spread(inner, _)` the id of the inner expression is returned,
    /// since spread is a compile-time splice operation without its own identity.
    pub fn id(&self) -> AstId {
        match self {
            Expr::Ident(e) => e.id,
            Expr::Literal(e) => e.id,
            Expr::Binary(e) => e.id,
            Expr::Unary(e) => e.id,
            Expr::Assign(e) => e.id,
            Expr::CompoundAssign(e) => e.id,
            Expr::ComparisonChain(e) => e.id,
            Expr::Call(e) => e.id,
            Expr::MethodCall(e) => e.id,
            Expr::StaticMethodCall(e) => e.id,
            Expr::FieldAccess(e) => e.id,
            Expr::Index(e) => e.id,
            Expr::Block(e) => e.id,
            Expr::If(e) => e.id,
            Expr::Match(e) => e.id,
            Expr::Matches(e) => e.id,
            Expr::Closure(e) => e.id,
            Expr::TemplateString(e) => e.id,
            Expr::Cast(e) => e.id,
            Expr::StructLiteral(e) => e.id,
            Expr::TupleLiteral(e) => e.id,
            Expr::LabeledBlock(e) => e.id,
            Expr::TryOp(e) => e.id,
            Expr::Spread(inner, _) => inner.id(),
            Expr::Range(e) => e.id,
            Expr::WithHandler(e) => e.id,
            Expr::Resume(e) => e.id,
        }
    }

    /// Get the source span for this expression
    pub fn span(&self) -> Span {
        match self {
            Expr::Ident(e) => e.span,
            Expr::Literal(e) => e.span,
            Expr::Binary(e) => e.span,
            Expr::Unary(e) => e.span,
            Expr::Assign(e) => e.span,
            Expr::CompoundAssign(e) => e.span,
            Expr::ComparisonChain(e) => e.span,
            Expr::Call(e) => e.span,
            Expr::MethodCall(e) => e.span,
            Expr::StaticMethodCall(e) => e.span,
            Expr::FieldAccess(e) => e.span,
            Expr::Index(e) => e.span,
            Expr::Block(e) => e.span,
            Expr::If(e) => e.span,
            Expr::Match(e) => e.span,
            Expr::Matches(e) => e.span,
            Expr::Closure(e) => e.span,
            Expr::TemplateString(e) => e.span,
            Expr::Cast(e) => e.span,
            Expr::StructLiteral(e) => e.span,
            Expr::TupleLiteral(e) => e.span,
            Expr::LabeledBlock(e) => e.span,
            Expr::TryOp(e) => e.span,
            Expr::Spread(_, span) => *span,
            Expr::Range(e) => e.span,
            Expr::WithHandler(e) => e.span,
            Expr::Resume(e) => e.span,
        }
    }

    /// Create a copy of this expression with an updated span.
    /// Used for parenthesized expressions to include the parens in the span.
    pub fn with_span(self, new_span: Span) -> Expr {
        match self {
            Expr::Ident(mut e) => {
                e.span = new_span;
                Expr::Ident(e)
            }
            Expr::Literal(mut e) => {
                e.span = new_span;
                Expr::Literal(e)
            }
            Expr::Binary(mut e) => {
                e.span = new_span;
                Expr::Binary(e)
            }
            Expr::Unary(mut e) => {
                e.span = new_span;
                Expr::Unary(e)
            }
            Expr::Assign(mut e) => {
                e.span = new_span;
                Expr::Assign(e)
            }
            Expr::CompoundAssign(mut e) => {
                e.span = new_span;
                Expr::CompoundAssign(e)
            }
            Expr::ComparisonChain(mut e) => {
                e.span = new_span;
                Expr::ComparisonChain(e)
            }
            Expr::Call(mut e) => {
                e.span = new_span;
                Expr::Call(e)
            }
            Expr::MethodCall(mut e) => {
                e.span = new_span;
                Expr::MethodCall(e)
            }
            Expr::StaticMethodCall(mut e) => {
                e.span = new_span;
                Expr::StaticMethodCall(e)
            }
            Expr::FieldAccess(mut e) => {
                e.span = new_span;
                Expr::FieldAccess(e)
            }
            Expr::Index(mut e) => {
                e.span = new_span;
                Expr::Index(e)
            }
            Expr::Block(mut e) => {
                e.span = new_span;
                Expr::Block(e)
            }
            Expr::If(mut e) => {
                e.span = new_span;
                Expr::If(e)
            }
            Expr::Match(mut e) => {
                e.span = new_span;
                Expr::Match(e)
            }
            Expr::Matches(mut e) => {
                e.span = new_span;
                Expr::Matches(e)
            }
            Expr::Closure(mut e) => {
                e.span = new_span;
                Expr::Closure(e)
            }
            Expr::TemplateString(mut e) => {
                e.span = new_span;
                Expr::TemplateString(e)
            }
            Expr::Cast(mut e) => {
                e.span = new_span;
                Expr::Cast(e)
            }
            Expr::StructLiteral(mut e) => {
                e.span = new_span;
                Expr::StructLiteral(e)
            }
            Expr::TupleLiteral(mut e) => {
                e.span = new_span;
                Expr::TupleLiteral(e)
            }
            Expr::LabeledBlock(mut e) => {
                e.span = new_span;
                Expr::LabeledBlock(e)
            }
            Expr::TryOp(mut e) => {
                e.span = new_span;
                Expr::TryOp(e)
            }
            Expr::Spread(inner, _) => Expr::Spread(inner, new_span),
            Expr::Range(mut e) => {
                e.span = new_span;
                Expr::Range(e)
            }
            Expr::WithHandler(mut e) => {
                e.span = new_span;
                Expr::WithHandler(e)
            }
            Expr::Resume(mut e) => {
                e.span = new_span;
                Expr::Resume(e)
            }
        }
    }

    /// Substitute free occurrences of identifiers inside this expression.
    ///
    /// Walks the expression tree and replaces each [`Expr::Ident`] whose name
    /// appears as a key in `subs` with a clone of the mapped expression.
    /// Used by default-argument expansion to rewrite references to earlier
    /// parameters (e.g. `fn make_rect(w, h = w)`) into the caller's values.
    ///
    /// Only traverses expression forms that can appear inside the simple,
    /// pure defaults permitted by the language; blocks, closures, and
    /// match/if forms are left untouched because they cannot appear in a
    /// pure default.
    pub fn substitute_idents(&mut self, subs: &crate::hashmap::IndexMap<String, Expr>) {
        match self {
            Expr::Ident(ident) => {
                if let Some(replacement) = subs.get(&ident.name) {
                    *self = replacement.clone();
                }
            }
            Expr::Literal(_) => {}
            Expr::Binary(b) => {
                b.left.substitute_idents(subs);
                b.right.substitute_idents(subs);
            }
            Expr::Unary(u) => {
                u.expr.substitute_idents(subs);
            }
            Expr::ComparisonChain(c) => {
                c.first.substitute_idents(subs);
                for cmp in &mut c.comparisons {
                    cmp.right.substitute_idents(subs);
                }
            }
            Expr::Call(c) => {
                c.callee.substitute_idents(subs);
                for a in &mut c.args {
                    a.substitute_idents(subs);
                }
            }
            Expr::MethodCall(m) => {
                m.receiver.substitute_idents(subs);
                for a in &mut m.args {
                    a.substitute_idents(subs);
                }
            }
            Expr::StaticMethodCall(s) => {
                for a in &mut s.args {
                    a.substitute_idents(subs);
                }
            }
            Expr::FieldAccess(f) => {
                f.expr.substitute_idents(subs);
            }
            Expr::Index(i) => {
                i.expr.substitute_idents(subs);
                i.index.substitute_idents(subs);
            }
            Expr::Cast(c) => {
                c.expr.substitute_idents(subs);
            }
            Expr::TemplateString(t) => {
                for part in &mut t.parts {
                    if let TemplatePart::Interpolation { expr, .. } = part {
                        expr.substitute_idents(subs);
                    }
                }
            }
            Expr::TupleLiteral(t) => {
                for e in &mut t.elements {
                    e.substitute_idents(subs);
                }
            }
            Expr::StructLiteral(sl) => {
                for f in &mut sl.fields {
                    f.value.substitute_idents(subs);
                }
            }
            Expr::Range(r) => {
                r.start.substitute_idents(subs);
                r.end.substitute_idents(subs);
            }
            Expr::Spread(inner, _) => {
                inner.substitute_idents(subs);
            }
            Expr::TryOp(t) => {
                t.expr.substitute_idents(subs);
            }
            Expr::Assign(_)
            | Expr::CompoundAssign(_)
            | Expr::Block(_)
            | Expr::If(_)
            | Expr::Match(_)
            | Expr::Matches(_)
            | Expr::Closure(_)
            | Expr::LabeledBlock(_)
            | Expr::WithHandler(_)
            | Expr::Resume(_) => {
                // Not expected inside pure default expressions; leave as-is.
            }
        }
    }
}

/// Type cast expression: `expr as Type`
#[derive(Debug, Clone)]
pub struct CastExpr {
    pub id: AstId,
    pub expr: Expr,
    pub target_type: Type,
    pub span: Span,
}

/// Struct literal expression: `Point { x: 10, y: 20 }` or implicit `{ x: 10, y: 20 }`
#[derive(Debug, Clone)]
pub struct StructLiteralExpr {
    pub id: AstId,
    /// The struct type name. None for implicit struct literals like `{ x: 1, y: 2 }`
    /// which require type context (e.g., `let p: Point = { x: 1, y: 2 }`).
    pub name: Option<String>,
    /// `AstId` of just the type name, for cursor-based navigation (jump-to-def).
    /// Always `Some` iff `name` is `Some`.
    pub name_id: Option<AstId>,
    /// Span of just the type name token.
    /// Always `Some` iff `name` is `Some`.
    pub name_span: Option<Span>,
    pub fields: Vec<StructLiteralField>,
    /// Whether the original source had a trailing comma (for formatting purposes).
    /// Multiline formatting is used when this is true.
    pub has_trailing_comma: bool,
    pub span: Span,
}

/// A field in a struct literal: `x: 10` or `x` (shorthand)
#[derive(Debug, Clone)]
pub struct StructLiteralField {
    pub name: String,
    /// `AstId` of the field name, for cursor-based navigation (jump-to-def).
    pub name_id: AstId,
    /// Span of just the field name token.
    pub name_span: Span,
    pub value: Expr,
    /// Whether this field uses shorthand syntax `{ x }` instead of `{ x: x }`
    pub is_shorthand: bool,
    pub span: Span,
}

/// Tuple literal expression: `[1, 2, 3]` or `[1, "hello", true]`
/// This uses bracket syntax following TypeScript conventions.
/// Can be coerced to Array<T> when all elements have the same type.
#[derive(Debug, Clone)]
pub struct TupleLiteralExpr {
    pub id: AstId,
    pub elements: Vec<Expr>,
    pub span: Span,
}

/// Assignment expression: `x = value` or `x.field = value`
#[derive(Debug, Clone)]
pub struct AssignExpr {
    pub id: AstId,
    pub target: Expr,
    pub value: Expr,
    pub span: Span,
}

/// Compound assignment operators
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompoundAssignOp {
    Add,    // +=
    Sub,    // -=
    Mul,    // *=
    Div,    // /=
    Mod,    // %=
    BitAnd, // &=
    BitOr,  // |=
    BitXor, // ^=
    Shl,    // <<=
    Shr,    // >>=
}

/// Compound assignment expression: `x += value`
#[derive(Debug, Clone)]
pub struct CompoundAssignExpr {
    pub id: AstId,
    pub target: Expr,
    pub op: CompoundAssignOp,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IdentExpr {
    pub id: AstId,
    pub name: String,
    pub span: Span,
    /// When the identifier is a qualified path like `Color::Red`, each segment
    /// carries its own [`AstId`] and span so LSP navigation can pinpoint which
    /// segment the cursor is on. Empty for simple identifiers. For a qualified
    /// path with N segments, this has N entries in left-to-right order; the
    /// last entry's name matches the suffix of `name` after the final `::`.
    pub segments: Vec<PathSegment>,
}

#[derive(Debug, Clone)]
pub struct PathSegment {
    pub id: AstId,
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct LiteralExpr {
    pub id: AstId,
    pub value: Literal,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Literal {
    /// Numeric literal: raw source text (e.g. `"42"`, `"0xFF"`, `"3.14"`). Type resolved later.
    Number(String),
    /// String literal: raw source text between quotes (escape sequences not interpreted).
    String(String),
    /// Char literal: raw source text between quotes (escape sequences not interpreted).
    Char(String),
    Bool(bool),
    Null,
    Unit,
    /// Compile-time location literal: `#file`
    LocationFile,
    /// Compile-time location literal: `#line`
    LocationLine,
    /// Compile-time location literal: `#function`
    LocationFunction,
    /// Compile-time data section literal: `#data`
    DataSection,
    /// Compile-time file include as string: `#include_str("path")`
    IncludeStr(String),
    /// Compile-time file include as bytes: `#include_bytes("path")`
    IncludeBytes(String),
}

#[derive(Debug, Clone)]
pub struct BinaryExpr {
    pub id: AstId,
    pub left: Expr,
    pub op: BinaryOp,
    pub right: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

/// A comparison in a chain (e.g., the `< b` part of `a < b < c`)
#[derive(Debug, Clone)]
pub struct ChainedComparison {
    pub op: BinaryOp,
    pub right: Expr,
    pub op_span: Span,
}

/// Comparison chain expression: `a < b < c` or `0 <= x <= 100`
#[derive(Debug, Clone)]
pub struct ComparisonChainExpr {
    pub id: AstId,
    pub first: Expr,
    pub comparisons: Vec<ChainedComparison>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct UnaryExpr {
    pub id: AstId,
    pub op: UnaryOp,
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
    BitNot,
    Ref,
    MutRef,
    Deref,
}

#[derive(Debug, Clone)]
pub struct CallExpr {
    pub id: AstId,
    pub callee: Expr,
    /// Explicit type arguments for generic functions: `foo::<i32>(x)`
    pub type_args: Vec<Type>,
    pub args: Vec<Expr>,
    /// Whether the original source had a trailing comma (for formatting purposes).
    pub has_trailing_comma: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MethodCallExpr {
    pub id: AstId,
    pub receiver: Expr,
    pub method: String,
    /// `AstId` of the method name token, for cursor-based navigation (jump-to-def).
    pub method_id: AstId,
    /// Span of just the method name token.
    pub method_span: Span,
    /// Explicit type arguments for generic methods: `obj.foo::<i32>(x)`
    pub type_args: Vec<Type>,
    pub args: Vec<Expr>,
    /// Whether the original source had a trailing comma (for formatting purposes).
    pub has_trailing_comma: bool,
    pub span: Span,
}

/// Static method call expression: `Array::<i32>::with_capacity(100)` or `Point::origin()`
#[derive(Debug, Clone)]
pub struct StaticMethodCallExpr {
    pub id: AstId,
    /// The target type (e.g., `Array<i32>` or `Point`)
    pub target_type: Type,
    /// The method name (e.g., `with_capacity` or `origin`)
    pub method: String,
    /// `AstId` of the method name token, for cursor-based navigation (jump-to-def).
    pub method_id: AstId,
    /// Span of just the method name token.
    pub method_span: Span,
    /// Explicit type arguments for generic methods: `Box::<i32>::wrap_other::<String>(x)`
    pub type_args: Vec<Type>,
    /// Arguments to the method
    pub args: Vec<Expr>,
    /// Whether the original source had a trailing comma (for formatting purposes).
    pub has_trailing_comma: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FieldAccessExpr {
    pub id: AstId,
    pub expr: Expr,
    pub field: String,
    /// `AstId` of the field name token, for cursor-based navigation (jump-to-def).
    pub field_id: AstId,
    /// Span of just the field name token.
    pub field_span: Span,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IndexExpr {
    pub id: AstId,
    pub expr: Expr,
    pub index: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IfExpr {
    pub id: AstId,
    pub condition: Condition,
    pub then_block: Block,
    pub else_block: Option<Block>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MatchExpr {
    pub id: AstId,
    pub expr: Expr,
    pub arms: Vec<MatchArm>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    /// Optional guard expression (the condition after `&&`)
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

/// Matches expression: `expr matches { pattern && guard }`
/// Returns true if the pattern matches and the optional guard is true.
#[derive(Debug, Clone)]
pub struct MatchesExpr {
    pub id: AstId,
    pub expr: Expr,
    pub pattern: Pattern,
    /// Optional guard expression (the condition after `&&`)
    pub guard: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    /// Plain identifier binding: `x`.
    Ident {
        id: AstId,
        name: String,
        span: Span,
    },
    /// Mutable identifier binding: `mut x`.
    MutIdent {
        id: AstId,
        name: String,
        span: Span,
    },
    Literal(Literal),
    Wildcard,
    Tuple(Vec<Pattern>, /* has_rest */ bool),
    /// Variant pattern: `Some(x)` or `None`
    Variant {
        variant_name: String,
        /// `AstId` of the variant-name identifier in the pattern. Used to
        /// record use→def references for LSP navigation (cursor on `Some`
        /// inside a match arm jumps to the case's declaration site).
        /// `None` for resolver-synthesized patterns that do not originate in
        /// source (e.g., None-coercion from `null`).
        name_id: Option<AstId>,
        /// Span of the variant-name identifier (not the whole pattern).
        name_span: Span,
        bindings: Vec<Pattern>,
        span: Span,
    },
    /// Struct destructuring pattern: `{ x, y }` or `Point { x, y }`
    Struct {
        type_name: Option<String>,
        fields: Vec<StructPatternField>,
        has_rest: bool,
        span: Span,
    },
    /// Or pattern: `Red | Blue` or `Some(x) | Other(x)`
    Or(Vec<Pattern>),
    /// Range pattern: `0..<10` or `'a'..='z'`
    Range {
        start: Box<Pattern>,
        end: Box<Pattern>,
        kind: RangeKind,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct StructPatternField {
    pub field_name: String,
    pub pattern: Pattern,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ClosureExpr {
    pub id: AstId,
    pub params: Vec<ClosureParam>,
    pub body: Expr,
    /// Pre-desugar source text, set by the desugar phase before transforming the body.
    pub source_text: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ClosureParam {
    pub id: AstId,
    pub name: String,
    /// Span of the closure parameter name identifier.
    pub name_span: Span,
    pub ty: Option<Type>,
    pub is_mut: bool,
    pub default: Option<Expr>,
}

/// Template string expression: `Hello, {name}!`
#[derive(Debug, Clone)]
pub struct TemplateStringExpr {
    pub id: AstId,
    pub parts: Vec<TemplatePart>,
    pub span: Span,
}

/// A part of a template string - either a literal string or an interpolation
#[derive(Debug, Clone)]
pub enum TemplatePart {
    /// A literal string part
    String(String),
    /// An interpolated expression with optional format specifier
    Interpolation {
        expr: Box<Expr>,
        format: Option<FormatSpec>,
    },
}

/// Format specifier for template string interpolation
/// Examples: ".2f", "0.3f", "10", "d"
#[derive(Debug, Clone)]
pub struct FormatSpec {
    pub spec: String,
}

#[derive(Debug, Clone)]
pub enum Type {
    Named(NamedType),
    Generic(GenericType),
    /// Namespaced generic type like `builtin::array<T>`
    NamespacedGeneric(NamespacedGenericType),
    Function(Box<FunctionType>),
    Tuple(Vec<Type>),
    Reference(Box<Type>),
    MutReference(Box<Type>),
    /// Type pack spread inside a tuple: `..T` in `[i32, ..T, bool]`
    TypePackSpread(String, Span),
}

impl Type {
    /// Returns the [`AstId`] for types that carry one (named types and
    /// generics). Structural types (tuple, reference, function) aggregate
    /// children that each carry their own ids.
    pub fn id(&self) -> Option<AstId> {
        match self {
            Type::Named(t) => Some(t.id),
            Type::Generic(t) => Some(t.id),
            Type::NamespacedGeneric(t) => Some(t.id),
            Type::Function(_)
            | Type::Tuple(_)
            | Type::Reference(_)
            | Type::MutReference(_)
            | Type::TypePackSpread(_, _) => None,
        }
    }

    /// Returns the source [`Span`] covering this type expression.
    ///
    /// `Function` and empty `Tuple` types have no top-level span field;
    /// they fall back to the span of their first child (or a default
    /// span for empty tuples). Callers that need precise spans for these
    /// shapes should walk the children themselves.
    pub fn span(&self) -> Span {
        match self {
            Type::Named(t) => t.span,
            Type::Generic(t) => t.span,
            Type::NamespacedGeneric(t) => t.span,
            Type::Function(t) => t
                .params
                .first()
                .map_or_else(|| t.return_type.span(), Type::span),
            Type::Tuple(elems) => elems.first().map(Type::span).unwrap_or_default(),
            Type::Reference(inner) | Type::MutReference(inner) => inner.span(),
            Type::TypePackSpread(_, span) => *span,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NamedType {
    pub id: AstId,
    pub name: String,
    pub span: Span,
    /// The defining source interface for this name reference, populated
    /// during stdlib bootstrap and user-code resolution so that registry
    /// lookups are unambiguous. `None` means the reference has not been
    /// resolved yet (e.g. freshly parsed user code before the resolver
    /// runs, or a bare primitive/generic parameter). Format matches the
    /// `#[cm("...")]` prefix, e.g. `"wasi:filesystem/types@0.3.0-rc-..."`
    /// or `"core:kiln/types@0.1.0"`.
    pub source_interface: Option<String>,
}

impl NamedType {
    /// Construct a `NamedType` with no resolved source interface. The source
    /// is populated later during stdlib registration (for stdlib types) or by
    /// the resolver (for user-code references into imported symbols).
    pub fn new(id: AstId, name: String, span: Span) -> Self {
        Self {
            id,
            name,
            span,
            source_interface: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GenericType {
    pub id: AstId,
    pub name: String,
    pub args: Vec<Type>,
    pub span: Span,
}

/// Namespaced generic type like `builtin::array<T>`
#[derive(Debug, Clone)]
pub struct NamespacedGenericType {
    pub id: AstId,
    /// Namespace (e.g., "builtin")
    pub namespace: String,
    /// Type name (e.g., "array")
    pub name: String,
    /// Generic arguments
    pub args: Vec<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FunctionType {
    pub params: Vec<Type>,
    pub return_type: Type,
    pub effects: Vec<String>,
    /// Parallel to `effects`: `(AstId, Span)` of each effect-name identifier as
    /// it appeared in source. Used by the resolver to record use->def
    /// references for LSP jump-to-def. Empty when constructed by the compiler
    /// (synthesized function types from monomorphization, etc.).
    pub effect_ids: Vec<(AstId, Span)>,
    /// Positional indices of parameters the function may store (e.g., `stores[0]`).
    pub stores: Vec<StoresEntry>,
}

/// A stores entry: either a parameter name (in function declarations) or a
/// positional index (in function type expressions).
#[derive(Debug, Clone)]
pub enum StoresEntry {
    Name(String),
    Index(u32),
}

impl std::fmt::Display for StoresEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoresEntry::Name(name) => write!(f, "{name}"),
            StoresEntry::Index(idx) => write!(f, "{idx}"),
        }
    }
}

// Placeholder types for future implementation
#[derive(Debug, Clone)]
pub struct InterfaceDecl {
    pub id: AstId,
    pub name: String,
    /// Span of the interface name identifier.
    pub name_span: Span,
    pub is_pub: bool,
    pub attrs: Vec<Attribute>,
    pub methods: Vec<InterfaceMethod>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct InterfaceMethod {
    pub id: AstId,
    pub name: String,
    /// Span of the method name identifier.
    pub name_span: Span,
    pub is_async: bool,
    pub attrs: Vec<Attribute>,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub span: Span,
}

/// An associated type binding inside a trait bound, e.g., `Output = T` in `Builder<Output = T>`.
#[derive(Debug, Clone)]
pub struct AssocTypeBound {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

/// A single trait bound on a generic parameter.
/// Simple: `Ord`; with associated type bindings: `Builder<Output = T>`.
#[derive(Debug, Clone)]
pub struct TraitBound {
    pub name: String,
    pub assoc_types: Vec<AssocTypeBound>,
    pub span: Span,
}

/// Generic type parameter declaration: `<T>`, `<T, U>`, `<T: Ord>`, `<T: Builder<Output = T>>`
/// Effect parameter declaration: `<effect E>` — represents a set of effects
#[derive(Debug, Clone)]
pub struct GenericParam {
    pub id: AstId,
    pub name: String,
    /// Span of the type parameter name identifier.
    pub name_span: Span,
    /// Whether this is an effect parameter (`effect E`)
    pub is_effect: bool,
    /// Whether this is a type pack parameter (`..T`)
    pub is_pack: bool,
    /// Trait bounds (e.g., `Ord`, `Builder<Output = T>`)
    pub bounds: Vec<TraitBound>,
    /// Default type (e.g., `T = []` or `Effects = []`)
    pub default: Option<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructDecl {
    pub id: AstId,
    pub name: String,
    /// Span of the struct name identifier.
    pub name_span: Span,
    pub is_pub: bool,
    /// Generic type parameters: `struct Pair<T, U> { ... }`
    pub type_params: Vec<GenericParam>,
    pub fields: Vec<StructField>,
    /// Attributes like `#[cm("...")]`
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructField {
    pub id: AstId,
    pub name: String,
    /// Span of the field name identifier.
    pub name_span: Span,
    pub is_pub: bool,
    pub ty: Type,
    /// Attributes like `#[cm("...")]` for CM name override
    pub attrs: Vec<Attribute>,
    /// Optional default value expression: `struct S { x: i32 = 0 }`.
    /// Must be a pure expression (no effects).
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumDecl {
    pub id: AstId,
    pub name: String,
    /// Span of the enum name identifier.
    pub name_span: Span,
    pub is_pub: bool,
    /// Generic type parameters: `enum Option<T> { Some(T), None }`
    pub type_params: Vec<GenericParam>,
    pub cases: Vec<EnumCase>,
    /// Attributes like #[cm("...")]
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// A case in an enum declaration.
/// Unlike `VariantCase`, enum cases have no payload.
#[derive(Debug, Clone)]
pub struct EnumCase {
    pub id: AstId,
    pub name: String,
    /// Span of the case name identifier.
    pub name_span: Span,
    /// Attributes like `#[cm("wit-kebab-name")]` for CM name override
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// Flags declaration (bitflags type from WASI)
/// ```wado
/// flags DescriptorFlags {
///     Read,
///     Write,
///     FileIntegritySync,
/// }
/// ```
#[derive(Debug, Clone)]
pub struct FlagsDecl {
    pub id: AstId,
    pub name: String,
    /// Span of the flags name identifier.
    pub name_span: Span,
    pub is_pub: bool,
    pub attributes: Option<Vec<Attribute>>,
    pub flags: Vec<FlagsVariant>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FlagsVariant {
    pub id: AstId,
    pub name: String,
    /// Span of the flag member name identifier.
    pub name_span: Span,
    /// Attributes like `#[cm("wit-kebab-name")]` for CM name override
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// Variant declaration (tagged union with payloads)
/// ```wado
/// variant Option<T> {
///     Some(T),
///     None,
/// }
/// ```
#[derive(Debug, Clone)]
pub struct VariantDecl {
    pub id: AstId,
    pub name: String,
    /// Span of the variant name identifier.
    pub name_span: Span,
    pub is_pub: bool,
    /// Generic type parameters: `variant Option<T> { Some(T), None }`
    pub type_params: Vec<GenericParam>,
    pub cases: Vec<VariantCase>,
    /// Attributes like `#[cm("...")]`
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// A case in a variant declaration: `Some(T)` or `None`
///
/// Each variant case has exactly one payload type:
/// - Unit variants: `None` → payload is `None` (parser level), becomes `()` in TIR
/// - Scalar payloads: `Some(T)` → payload is `Some(T)`
/// - Tuple payloads: `Rectangle([f64, f64])` → payload is `Some([f64, f64])`
/// - Struct payloads: `Named({ w: f64 })` → payload is `Some({ w: f64 })`
#[derive(Debug, Clone)]
pub struct VariantCase {
    pub id: AstId,
    pub name: String,
    /// Span of the case name identifier.
    pub name_span: Span,
    /// Payload type for this case. None for unit variants like `None`.
    /// At TIR level, unit variants are normalized to have `()` payload.
    pub payload: Option<Type>,
    /// Attributes like `#[cm("wit-kebab-name")]` for CM name override
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Newtype {
    pub id: AstId,
    pub name: String,
    /// Span of the newtype name identifier.
    pub name_span: Span,
    pub is_pub: bool,
    pub type_params: Vec<GenericParam>,
    pub ty: Type,
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// Tuple type family declaration: `pub type [..T];`
///
/// Declares the current module as the owner of the tuple type family.
/// This is a type-system anchor that generates no code.
#[derive(Debug, Clone)]
pub struct TupleTypeDecl {
    pub id: AstId,
    pub is_pub: bool,
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// Associated type declaration in a trait: `type Output;` or `type Output: Trait1 + Trait2;`
#[derive(Debug, Clone)]
pub struct AssociatedTypeDecl {
    pub id: AstId,
    pub name: String,
    /// Trait bounds on this associated type (e.g., `SerializeSeq` in `type SeqSerializer: SerializeSeq;`
    /// or `Iterator<Item = Self::Item>` in `type Iter: Iterator<Item = Self::Item>;`)
    pub bounds: Vec<TraitBound>,
    pub span: Span,
}

/// Associated type binding in an impl block: `type Output = T;`
#[derive(Debug, Clone)]
pub struct AssociatedTypeBinding {
    pub id: AstId,
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

/// Associated constant in an impl block: `pub const PI: f64 = 3.14159;`
/// These are compile-time constants that are inlined at every use site.
#[derive(Debug, Clone)]
pub struct AssociatedConst {
    pub id: AstId,
    pub name: String,
    pub is_pub: bool,
    pub ty: Type,
    pub value: Expr,
    pub span: Span,
}

/// Trait declaration: `trait Foo { type Output; fn method(&self) -> Self::Output; }`
#[derive(Debug, Clone)]
pub struct TraitDecl {
    pub id: AstId,
    pub name: String,
    /// Span of the trait name identifier.
    pub name_span: Span,
    pub is_pub: bool,
    pub type_params: Vec<GenericParam>,
    /// Associated type declarations: `type Output;`
    pub associated_types: Vec<AssociatedTypeDecl>,
    /// Trait methods. Body is None for required methods.
    pub methods: Vec<Function>,
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ImplBlock {
    pub id: AstId,
    /// Generic type parameters: `impl<T> Box<T> { ... }`
    pub type_params: Vec<GenericParam>,
    /// The trait being implemented, if any: `impl Trait for Type`
    /// None for inherent impl blocks: `impl Type`
    pub trait_type: Option<Type>,
    pub ty: Type,
    /// Associated type bindings: `type Output = T;`
    pub associated_types: Vec<AssociatedTypeBinding>,
    /// Associated constants: `pub const PI: f64 = 3.14159;`
    pub constants: Vec<AssociatedConst>,
    pub methods: Vec<Function>,
    /// `impl Trait for Type;` — synthesis request (compiler generates the body)
    pub is_synthesize_request: bool,
    /// `..` rest pattern at the end of an effect handler `impl` block.
    /// Indicates that any operation of the trait (effect) not implemented in
    /// `methods` should trap when dispatched. Only meaningful for effect
    /// handler impls; ignored for ordinary trait impls.
    pub has_rest: bool,
    pub span: Span,
}

#[cfg(test)]
mod ast_id_tests {
    use super::*;
    use crate::hashmap::IndexSet;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> Module {
        let tokens = Lexer::new(source).tokenize().expect("lex");
        let mut parser = Parser::new(tokens);
        parser.parse().expect("parse")
    }

    /// Collect every `(AstId, Span)` emitted while walking `items` using the
    /// default [`AstVisitor`] traversal.
    fn collect_ids(items: &[Item]) -> Vec<(AstId, Span)> {
        struct Collector(Vec<(AstId, Span)>);
        impl AstVisitor for Collector {
            fn visit_id(&mut self, id: AstId, span: Span) {
                self.0.push((id, span));
            }
        }
        let mut c = Collector(Vec::new());
        for item in items {
            c.visit_item(item);
        }
        c.0
    }

    const SAMPLE: &str = r#"
use { println } from "core:cli";

global COUNT: i32 = 0;

struct Point {
    x: i32,
    y: i32,
}

enum Color { Red, Green, Blue }

variant Shape {
    Circle(f64),
    Square,
}

flags Perms { Read, Write }

trait Greet {
    fn hello(&self) -> String;
}

impl Point {
    fn origin() -> Point {
        return Point { x: 0, y: 0 };
    }
}

fn add(a: i32, b: i32) -> i32 {
    return a + b;
}

test "addition" {
    assert add(1, 2) == 3;
}
"#;

    #[test]
    fn parse_assigns_stable_ids() {
        let m1 = parse(SAMPLE);
        let m2 = parse(SAMPLE);
        assert_eq!(m1.ast_id_count(), m2.ast_id_count());
        assert!(m1.ast_id_count() > 0);

        let ids_1: Vec<AstId> = collect_ids(&m1.items)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let ids_2: Vec<AstId> = collect_ids(&m2.items)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids_1, ids_2);
    }

    #[test]
    fn ids_are_dense_and_unique() {
        let m = parse(SAMPLE);
        let ids: Vec<AstId> = collect_ids(&m.items)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(!ids.is_empty());

        let mut seen: IndexSet<AstId> = IndexSet::default();
        for id in &ids {
            assert!(seen.insert(*id), "duplicate id: {id:?}");
            assert!(
                id.0 < m.ast_id_count(),
                "id {} out of range (count={})",
                id.0,
                m.ast_id_count()
            );
        }
        for i in 0..m.ast_id_count() {
            assert!(seen.contains(&AstId(i)), "missing id {i} in dense range");
        }
    }

    #[test]
    fn ast_id_at_returns_innermost_node() {
        let src = "struct Point {\n    x: i32,\n    y: i32,\n}\n";
        let m = parse(src);
        let struct_id = m.items.iter().find_map(|it| match it {
            Item::Struct(s) => Some(s.id),
            _ => None,
        });
        let field_ids: Vec<AstId> = m
            .items
            .iter()
            .flat_map(|it| match it {
                Item::Struct(s) => s.fields.iter().map(|f| f.id).collect::<Vec<_>>(),
                _ => vec![],
            })
            .collect();
        assert_eq!(field_ids.len(), 2);

        // Position inside the `x: i32` field span returns the field id, not the struct.
        let at_field = m.ast_id_at(2, 5);
        assert_eq!(at_field, Some(field_ids[0]));

        // Position inside the struct but outside any field (e.g. the struct name).
        let at_struct_name = m.ast_id_at(1, 9);
        assert_eq!(at_struct_name, struct_id);
    }

    #[test]
    fn ast_id_at_outside_returns_none() {
        let m = parse("fn add(a: i32, b: i32) -> i32 { return a + b; }\n");
        assert_eq!(m.ast_id_at(42, 1), None);
    }

    const SAMPLE_WITH_BODIES: &str = r"
fn add(a: i32, b: i32) -> i32 {
    let c = a + b;
    let mut d: i32 = c;
    if let Some(v) = Option::<i32>::Some(d) {
        return v;
    }
    for let i of 0..<c {
        d = d + i;
    }
    let { x, y } = Point { x: 1, y: 2 };
    return match d {
        0 => x,
        n => n + y,
    };
}

struct Point { x: i32, y: i32 }
";

    #[test]
    fn ids_dense_with_function_bodies_and_patterns() {
        let m = parse(SAMPLE_WITH_BODIES);
        let ids: Vec<(AstId, Span)> = collect_ids(&m.items);
        assert!(
            ids.len() > 20,
            "expected rich id coverage, got {}",
            ids.len()
        );

        let mut seen: IndexSet<AstId> = IndexSet::default();
        for (id, sp) in &ids {
            assert!(seen.insert(*id), "duplicate id: {id:?} at span {sp:?}");
            assert!(
                id.0 < m.ast_id_count(),
                "id {} out of range (count={})",
                id.0,
                m.ast_id_count()
            );
        }
        for i in 0..m.ast_id_count() {
            assert!(seen.contains(&AstId(i)), "missing id {i} in dense range");
        }
    }

    #[test]
    fn pattern_leaf_ids_resolve_at_position() {
        let src = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x;\n}\n";
        let m = parse(src);
        let pat_id = m
            .items
            .iter()
            .find_map(|it| match it {
                Item::Function(f) => f.body.as_ref().and_then(|b| {
                    b.stmts.iter().find_map(|s| match s {
                        Stmt::Let(l) => match &l.pattern {
                            Pattern::Ident { id, .. } => Some(*id),
                            _ => None,
                        },
                        _ => None,
                    })
                }),
                _ => None,
            })
            .expect("let x pattern id");
        let at_pat = m.ast_id_at(2, 9);
        assert_eq!(at_pat, Some(pat_id));
    }
}
