// AST definitions for Wado

use crate::attribute::{
    ALLOW, CM, EXPECT_TRAP, GENERATED, NO_PRELUDE, STDLIB, SYNOPSIS, TIMEOUT_MS, TODO, UNAVAILABLE,
    WASM_MODULE, WIRE,
};
use std::borrow::Cow;

use crate::defs::DefId;
use crate::hashmap::{IndexMap, IndexSet};
use crate::token::Span;

/// Identity of one `AstId` allocation space — one per parse. Each top-level
/// [`crate::parser::Parser`] draws a fresh one from a process-global counter and
/// stamps it into every id, so a full [`AstId`] is globally unique while
/// [`AstId::local`] stays dense per module; sub-parsers continue an existing
/// space. Ids identify a *parse* and never survive a re-parse.
/// [`crate::token::Span`] carries one too, which is what lets a span say which
/// text its offsets index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AstIdSpace(u32);

/// [`AstIdSpace::FRESH`], never the first allocated space: a default-constructed
/// [`crate::token::Span`] belongs to no parse.
impl Default for AstIdSpace {
    fn default() -> Self {
        Self::FRESH
    }
}

impl AstIdSpace {
    /// Space reserved for [`AstId::fresh`] transient ids, and for a
    /// [`crate::token::Span`] no parse produced. Never returned by
    /// [`Self::next`].
    pub const FRESH: Self = Self(u32::MAX);

    /// Allocate the next allocation space from the process-global counter.
    #[must_use]
    pub fn next() -> Self {
        use core::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let v = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(v != u32::MAX, "AstIdSpace counter exhausted");
        Self(v)
    }
}

/// Globally-unique identifier for a semantically-significant AST node: an
/// [`AstIdSpace`] plus a module-local dense index assigned in DFS order. Spaces
/// differ per module, so a per-node fact map keys by bare `AstId` without
/// collision. Ordering is `(space, local)` — ids within a module order by
/// allocation, which parser rollback and [`crate::comment::TriviaMap`] rely on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AstId {
    space: AstIdSpace,
    local: u32,
}

impl std::fmt::Debug for AstId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AstId({}:{})", self.space.0, self.local)
    }
}

impl AstId {
    /// Build an id in `space` with the given dense `local` index. Callers are
    /// the parser, [`Module::alloc_ast_id`], and tests; everything else
    /// should treat ids as opaque.
    #[must_use]
    pub fn new(space: AstIdSpace, local: u32) -> Self {
        Self { space, local }
    }

    /// The module-local dense index (`0..Module::ast_id_count()` for nodes in
    /// a module tree).
    #[must_use]
    pub fn local(self) -> u32 {
        self.local
    }

    /// The allocation space this id was drawn from.
    #[must_use]
    pub fn space(self) -> AstIdSpace {
        self.space
    }

    /// Whether this id was minted by [`Self::fresh`] — a synthesized node that
    /// no module owns and no source position wrote. The resolution table is
    /// built from the modules, so it never answers for one, and a consumer
    /// asserting "every reference site is resolved" must exempt them.
    #[must_use]
    pub fn is_synthetic(self) -> bool {
        self.space == AstIdSpace::FRESH
    }

    /// A globally-unique `AstId` for a transient node never owned by a
    /// [`Module`] — synthesized `Type::Named` / `Type::Generic` operands for
    /// type-query functions, and test fixtures. It lives in the reserved
    /// `AstIdSpace::FRESH` space, so it never collides with a
    /// parser-allocated id (and must never become a fact / symbol key).
    #[must_use]
    pub fn fresh() -> Self {
        use core::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        Self::new(AstIdSpace::FRESH, NEXT.fetch_add(1, Ordering::Relaxed))
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
    /// The [`AstIdSpace`] this module's ids were allocated from (the parsing
    /// `Parser`'s space). [`Self::alloc_ast_id`] continues it.
    ast_id_space: AstIdSpace,
    /// Total number of [`AstId`]s allocated for this module during parsing.
    /// Id locals occupy the range `0..ast_id_count`.
    ast_id_count: u32,
    /// True if error recovery ran during parsing (any syntax error, including
    /// block-internal ones that leave no `Item::Error`). Travels with the AST
    /// so `Semantics::is_complete` can refuse to treat a partial parse as
    /// complete.
    has_syntax_errors: bool,
    /// Span of every token the parse read against its lexical class, in source
    /// order: an identifier read as a keyword (`test "…" { }`, `..trap`), or a
    /// keyword read as a name (`let type = 1`, `x.match`). Only the parse tells
    /// those two readings apart, so it records its own, and every other token's
    /// lexical class is its role. Read by `wado_lsp::semantic_tokens`.
    contextual_keywords: Vec<Span>,
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
            ast_id_space: AstIdSpace::next(),
            ast_id_count: 0,
            has_syntax_errors: false,
            contextual_keywords: Vec::new(),
        }
    }

    /// Creates a new module with the given items, shebang, and data section.
    pub fn with_metadata(
        items: Vec<Item>,
        inner_attributes: Vec<InnerAttribute>,
        shebang: Option<String>,
        data_section: Option<String>,
        include_paths: IndexSet<String>,
        ast_id_space: AstIdSpace,
        ast_id_count: u32,
        has_syntax_errors: bool,
        contextual_keywords: Vec<Span>,
    ) -> Self {
        Self {
            items,
            inner_attributes,
            shebang,
            data_section,
            include_paths,
            ast_id_space,
            ast_id_count,
            has_syntax_errors,
            contextual_keywords,
        }
    }

    /// Span of every token this module's parse read against its lexical class:
    /// an identifier read as a keyword, or a keyword read as a name.
    pub fn contextual_keywords(&self) -> &[Span] {
        &self.contextual_keywords
    }

    /// True if parsing recovered from one or more syntax errors.
    pub fn has_syntax_errors(&self) -> bool {
        self.has_syntax_errors
    }

    /// The [`AstIdSpace`] this module's ids live in.
    pub fn ast_id_space(&self) -> AstIdSpace {
        self.ast_id_space
    }

    /// Returns the total number of [`AstId`]s allocated for this module.
    /// Id locals occupy `0..ast_id_count()`.
    pub fn ast_id_count(&self) -> u32 {
        self.ast_id_count
    }

    /// Allocate a fresh dense [`AstId`] at the end of this module's range, for
    /// post-parse synthesis injecting a node into [`Self::items`]. Prefer it
    /// over [`AstId::fresh`] for anything entering a module tree: that mints
    /// into the reserved transient space, outside this dense range, where
    /// machinery keyed on the range cannot find it.
    #[must_use]
    pub fn alloc_ast_id(&mut self) -> AstId {
        let id = AstId::new(self.ast_id_space, self.ast_id_count);
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

    /// Find the top-level [`Item`] whose own `AstId` (i.e. [`Item::id`])
    /// equals `id`.
    ///
    /// Used to resolve an `AstId` recorded by an index (e.g.
    /// `TraitEnv::impl_headers`) back to its declaration, instead of a
    /// positional index into `items` that a reload could reorder. `id` must
    /// name a top-level item, not a nested node (field, method, case, …) —
    /// those have no entry here.
    pub fn item_by_id(&self, id: AstId) -> Option<&Item> {
        self.items.iter().find(|item| item.id() == id)
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

/// Structural AST visitor, for id-based queries (LSP position lookup, density
/// checks) and scope-aware analyses. Every `visit_*` defaults to its free
/// `walk_*`, which recurses through the visitor's own methods, so an implementer
/// overrides only where its behaviour differs from plain traversal. `visit_id`
/// sees every [`AstId`] / [`Span`] pair, containers and leaves alike.
pub trait AstVisitor: Sized {
    /// Invoked for every [`AstId`] emitted during traversal.
    ///
    /// Default is a no-op; an id-emitter overrides this to collect all ids,
    /// a scope-aware visitor leaves it as no-op and overrides the relevant
    /// `visit_*` methods instead.
    fn visit_id(&mut self, _id: AstId, _span: Span) {}

    /// A `with` clause's reference site, kept apart from [`Self::visit_id`]
    /// because it carries the effect name the site spells.
    fn visit_effect_name(&mut self, effect: &EffectName) {
        self.visit_id(effect.id, effect.span);
    }

    fn visit_item(&mut self, item: &Item) {
        walk_item(self, item);
    }

    fn visit_function(&mut self, func: &Function) {
        walk_function(self, func);
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

    /// A declaration's type parameters. Overridable because a bound is a
    /// reference site and a visitor resolving one needs the bound itself, not
    /// just the id [`walk_generic_params`] emits.
    fn visit_generic_params(&mut self, params: &[GenericParam]) {
        walk_generic_params(self, params);
    }

    /// Every list of trait bounds reaches a visitor here — a parameter's
    /// `<T: Trait>`, a trait's supertraits, an associated type's `type A:
    /// Trait`. They are the same kind of reference, so a visitor that answers
    /// for one answers for all three.
    fn visit_trait_bounds(&mut self, bounds: &[TraitBound]) {
        walk_trait_bounds(self, bounds);
    }

    fn visit_type(&mut self, ty: &Type) {
        walk_type(self, ty);
    }
}

/// The head name a type spells, or `None` for a shape with no nameable head.
#[must_use]
pub fn type_head_name(ty: &Type) -> Option<&str> {
    match ty {
        Type::Named(n) => Some(n.name.as_str()),
        Type::Generic(g) => Some(g.name.as_str()),
        Type::NamespacedGeneric(g) => Some(g.name.as_str()),
        _ => None,
    }
}

/// Where a module declares a function. What a declaration there may leave out,
/// and what qualifies its name, follow from this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionSite<'a> {
    /// A module-level `fn`.
    Free,
    /// An `impl` method, under its type head where the head has a name.
    Impl(Option<&'a str>),
    /// A trait method.
    Trait(&'a str),
    /// An `interface` operation.
    Interface(&'a str),
    /// A `resource` method.
    Resource(&'a str),
}

impl<'a> FunctionSite<'a> {
    /// The name qualifying a declaration written here.
    #[must_use]
    pub fn owner(self) -> Option<&'a str> {
        match self {
            FunctionSite::Free => None,
            FunctionSite::Impl(owner) => owner,
            FunctionSite::Trait(owner)
            | FunctionSite::Interface(owner)
            | FunctionSite::Resource(owner) => Some(owner),
        }
    }

    /// Whether a declaration written here may carry `#[unavailable]`.
    /// See [WEP: Declared Absence](../../docs/wep-2026-09-13-declared-absence.md).
    #[must_use]
    pub fn allows_unavailable(self) -> bool {
        matches!(
            self,
            FunctionSite::Free | FunctionSite::Impl(_) | FunctionSite::Trait(_)
        )
    }

    /// Whether a declaration written here needs a body of its own. An
    /// operation of a trait, interface, or resource is a signature.
    #[must_use]
    pub fn needs_body(self) -> bool {
        matches!(self, FunctionSite::Free | FunctionSite::Impl(_))
    }
}

/// Call `f` for every function `module` declares, with the site declaring it.
pub fn for_each_function<'a>(
    module: &'a Module,
    mut f: impl FnMut(FunctionSite<'a>, &'a Function),
) {
    for item in &module.items {
        match item {
            Item::Function(func) => f(FunctionSite::Free, func),
            Item::Impl(block) => {
                let site = FunctionSite::Impl(type_head_name(&block.ty));
                for method in &block.methods {
                    f(site, method);
                }
            }
            Item::Trait(decl) => {
                for method in &decl.methods {
                    f(FunctionSite::Trait(&decl.name), method);
                }
            }
            Item::Interface(decl) => {
                for method in &decl.methods {
                    f(FunctionSite::Interface(&decl.name), method);
                }
            }
            Item::Resource(decl) => {
                for method in &decl.methods {
                    f(FunctionSite::Resource(&decl.name), method);
                }
            }
            Item::Use(_)
            | Item::Struct(_)
            | Item::Enum(_)
            | Item::Variant(_)
            | Item::Flags(_)
            | Item::Newtype(_)
            | Item::TupleTypeDecl(_)
            | Item::BuiltinTypeDecl(_)
            | Item::World(_)
            | Item::Test(_)
            | Item::Global(_)
            | Item::Error(_) => {}
        }
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
                v.visit_function(m);
            }
        }
        Item::Struct(s) => {
            v.visit_id(s.id, s.span);
            v.visit_generic_params(&s.type_params);
            for field in &s.fields {
                v.visit_id(field.id, field.span);
                v.visit_type(&field.ty);
                // A field default is an expression written here; see
                // `walk_function`'s parameter defaults.
                if let Some(default) = &field.default {
                    v.visit_expr(default);
                }
            }
        }
        Item::Enum(e) => {
            v.visit_id(e.id, e.span);
            v.visit_generic_params(&e.type_params);
            for case in &e.cases {
                v.visit_id(case.id, case.span);
            }
        }
        Item::Variant(vr) => {
            v.visit_id(vr.id, vr.span);
            v.visit_generic_params(&vr.type_params);
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
            v.visit_generic_params(&n.type_params);
            v.visit_type(&n.ty);
        }
        Item::TupleTypeDecl(t) => {
            v.visit_id(t.id, t.span);
            v.visit_type(&t.head);
        }
        Item::BuiltinTypeDecl(d) => {
            v.visit_id(d.id, d.span);
            v.visit_generic_params(&d.type_params);
        }
        Item::Impl(i) => {
            v.visit_id(i.id, i.span);
            v.visit_generic_params(&i.type_params);
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
            v.visit_generic_params(&t.type_params);
            v.visit_trait_bounds(&t.supertraits);
            if let TraitHead::Fixed { effects, .. } = &t.head {
                walk_effect_names(v, effects);
            }
            for assoc in &t.associated_types {
                v.visit_id(assoc.id, assoc.span);
                v.visit_trait_bounds(&assoc.bounds);
            }
            for m in &t.methods {
                v.visit_function(m);
            }
        }
        Item::Resource(r) => {
            v.visit_id(r.id, r.span);
            v.visit_generic_params(&r.type_params);
            if let Some(parent) = &r.parent {
                v.visit_type(parent);
            }
            for m in &r.methods {
                v.visit_function(m);
            }
        }
        Item::World(w) => {
            v.visit_id(w.id, w.span);
            // An exported signature declares parameters like any other.
            for export in &w.exports {
                if let WorldExport::Function(f) = export {
                    walk_params(v, &f.params);
                    if let Some(return_type) = &f.return_type {
                        v.visit_type(return_type);
                    }
                }
            }
        }
        Item::Test(t) => {
            v.visit_id(t.id, t.span);
            v.visit_block(&t.body);
        }
        Item::Global(g) => {
            v.visit_id(g.id, g.span);
            v.visit_type(&g.ty);
            v.visit_expr(&g.initializer);
        }
        Item::Error(e) => v.visit_id(e.id, e.span),
    }
}

/// A parameter list wherever one is declared: a function's, a world export's.
fn walk_params<V: AstVisitor>(v: &mut V, params: &[Param]) {
    for param in params {
        v.visit_id(param.id, param.span);
        v.visit_type(&param.ty);
        // A default argument is an expression written in the declaring
        // module, so its names are reference sites like any other and the
        // walk answers for them from that vantage (WEP 2026-08-12 §3).
        if let Some(default) = &param.default {
            v.visit_expr(default);
        }
    }
}

pub fn walk_function<V: AstVisitor>(v: &mut V, func: &Function) {
    v.visit_id(func.id, func.span);
    v.visit_generic_params(&func.type_params);
    walk_params(v, &func.params);
    if let Some(ret) = &func.return_type {
        v.visit_type(ret);
    }
    walk_effect_names(v, &func.effects);
    if let Some(body) = &func.body {
        v.visit_block(body);
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
            if let Some(ty) = &s.ty {
                v.visit_type(ty);
            }
            if let Some(val) = &s.value {
                v.visit_expr(val);
            }
            // The `else` block runs where the pattern did not match, so it
            // comes before the binding it never sees.
            if let Some(eb) = &s.else_block {
                v.visit_block(eb);
            }
            v.visit_pattern(&s.pattern);
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
            v.visit_expr(&s.iterable);
            v.visit_pattern(&s.binding);
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
        Stmt::Item(item) => v.visit_item(item),
        Stmt::Error(_) => {}
    }
}

pub fn walk_condition<V: AstVisitor>(v: &mut V, cond: &Condition) {
    match cond {
        Condition::Expr(e) => v.visit_expr(e),
        Condition::LetChain { elements, .. } => {
            for el in elements {
                match el {
                    // A let chain binds from its scrutinee, so the scrutinee
                    // comes first: the names the pattern binds reach nothing
                    // written inside it.
                    ConditionElement::Let { pattern, expr, .. } => {
                        v.visit_expr(expr);
                        v.visit_pattern(pattern);
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
        v.visit_id(arm.id, arm.span);
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
            // `Opt::<V>::None` pins its turbofish here rather than on a call.
            for ty in &i.type_args {
                v.visit_type(ty);
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
            if let Some(ty) = &c.return_type {
                v.visit_type(ty);
            }
            v.visit_expr(&c.body);
        }
        Expr::TemplateString(t) => {
            for expr in t.interpolations() {
                v.visit_expr(expr);
            }
        }
        Expr::TaggedTemplate(t) => {
            v.visit_expr(&t.tag);
            for expr in t.template.interpolations() {
                v.visit_expr(expr);
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
            for ty in &s.type_args {
                v.visit_type(ty);
            }
            for field in &s.fields {
                v.visit_id(field.name_id, field.name_span);
                v.visit_expr(&field.value);
            }
            for spread in &s.spreads {
                v.visit_expr(&spread.expr);
            }
        }
        Expr::TupleLiteral(t) => {
            for el in &t.elements {
                v.visit_expr(el);
            }
        }
        Expr::TupleComprehension(c) => {
            v.visit_expr(&c.iterable);
            v.visit_pattern(&c.binding);
            v.visit_expr(&c.body);
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
                // A bundled binding (`with &mut h do`) writes no effect type,
                // so it answers with its own span.
                match &binding.effect {
                    Some(effect) => {
                        v.visit_id(binding.id, effect.span());
                        v.visit_type(effect);
                    }
                    None => v.visit_id(binding.id, binding.span),
                }
                v.visit_expr(&binding.handler);
            }
            v.visit_block(&w.body);
        }
        Expr::Resume(r) => v.visit_expr(&r.value),
        Expr::Error(_) => {}
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
                v.visit_id(field.id, field.span);
                v.visit_pattern(&field.pattern);
            }
        }
        Pattern::Variant {
            name_id,
            name_span,
            variant_qualifier,
            bindings,
            ..
        } => {
            if let Some(id) = name_id {
                v.visit_id(*id, *name_span);
            }
            // `Type::CONST` / `Type::Case` in pattern position: the qualifier
            // is a reference site like any written type, so the walk answers
            // for it rather than leaving the consumer to split the spelling
            // (WEP 2026-08-12 §3).
            if let Some(qualifier) = variant_qualifier {
                v.visit_type(qualifier);
            }
            for p in bindings {
                v.visit_pattern(p);
            }
        }
        Pattern::Typed { pattern, ty, .. } => {
            v.visit_pattern(pattern);
            v.visit_type(ty);
        }
        Pattern::Range { start, end, .. } => {
            v.visit_pattern(start);
            v.visit_pattern(end);
        }
        Pattern::Literal(_) | Pattern::Wildcard | Pattern::Error(_) => {}
    }
}

/// Every name a pattern binds, however deeply nested, in source order.
pub fn for_each_pattern_binding(pat: &Pattern, f: &mut impl FnMut(AstId)) {
    struct Bindings<'a, F>(&'a mut F);

    impl<F: FnMut(AstId)> AstVisitor for Bindings<'_, F> {
        fn visit_pattern(&mut self, pat: &Pattern) {
            if let Pattern::Ident { id, .. } | Pattern::MutIdent { id, .. } = pat {
                (self.0)(*id);
            }
            walk_pattern(self, pat);
        }
    }

    Bindings(f).visit_pattern(pat);
}

/// Every name a pattern binds, with the span of the identifier that binds it.
pub fn for_each_pattern_name(pat: &Pattern, f: &mut impl FnMut(&str, Span)) {
    struct Names<'a, F>(&'a mut F);

    impl<F: FnMut(&str, Span)> AstVisitor for Names<'_, F> {
        fn visit_pattern(&mut self, pat: &Pattern) {
            if let Pattern::Ident { name, span, .. } | Pattern::MutIdent { name, span, .. } = pat {
                (self.0)(name, *span);
            }
            walk_pattern(self, pat);
        }
    }

    Names(f).visit_pattern(pat);
}

/// Walk a declaration's type parameters: each binder's own id, then the
/// reference sites inside its bounds. A bound names a trait and its associated
/// types, so it is a reference site like any other and must be reachable by an
/// id-collecting walk (WEP 2026-08-12).
pub fn walk_generic_params<V: AstVisitor>(v: &mut V, params: &[GenericParam]) {
    for p in params {
        v.visit_id(p.id, p.span);
        v.visit_trait_bounds(&p.bounds);
        if let Some(default) = &p.default {
            v.visit_type(default);
        }
    }
}

pub fn walk_trait_bounds<V: AstVisitor>(v: &mut V, bounds: &[TraitBound]) {
    for bound in bounds {
        v.visit_id(bound.id, bound.span);
        for arg in &bound.type_args {
            v.visit_type(arg);
        }
        for assoc in &bound.assoc_types {
            v.visit_id(assoc.id, assoc.span);
            v.visit_type(&assoc.ty);
        }
        if let Some(signature) = &bound.fn_signature {
            walk_function_type(v, signature);
        }
    }
}

/// The types and effect names a function signature carries, whether it stands
/// as a [`Type::Function`] or as a trait bound's `fn_signature`.
fn walk_function_type<V: AstVisitor>(v: &mut V, ft: &FunctionType) {
    for p in &ft.params {
        v.visit_type(p);
    }
    v.visit_type(&ft.return_type);
    walk_effect_names(v, &ft.effects);
}

fn walk_effect_names<V: AstVisitor>(v: &mut V, effects: &[EffectName]) {
    for effect in effects {
        v.visit_effect_name(effect);
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
        Type::Function(ft) => walk_function_type(v, ft),
        Type::Tuple(ts) => {
            for t in ts {
                v.visit_type(t);
            }
        }
        Type::Reference(t) | Type::MutReference(t) => v.visit_type(t),
        Type::TypePackSpread(_, _) => {}
        Type::Infer(_) => {}
        Type::Error(_) => {}
    }
}

impl Module {
    /// Returns the inner attributes.
    pub fn inner_attributes(&self) -> &[InnerAttribute] {
        &self.inner_attributes
    }

    /// Returns true if the module has the `#![no_prelude]` attribute.
    pub fn has_no_prelude(&self) -> bool {
        self.inner_attributes.iter().any(|a| a.name == NO_PRELUDE)
    }

    /// Returns true if the module has the `#![TODO]` attribute.
    /// All tests in a TODO module must fail; passing tests become failures.
    pub fn has_todo(&self) -> bool {
        self.inner_attributes.iter().any(|a| a.name == TODO)
    }

    /// Returns true if the module has the `#![generated]` attribute.
    /// Indicates machine-generated code (e.g. wado-from-idl, gale).
    pub fn has_generated(&self) -> bool {
        self.inner_attributes.iter().any(|a| a.name == GENERATED)
    }

    /// Returns the `wasm_module` name if `#![wasm_module("name")]` is present.
    pub fn wasm_module(&self) -> Option<&str> {
        self.wasm_module_attribute()
            .and_then(|a| a.args.first())
            .map(AttrArg::as_str)
    }

    /// The attribute itself, for a diagnostic's span.
    #[must_use]
    pub fn wasm_module_attribute(&self) -> Option<&InnerAttribute> {
        self.inner_attributes.iter().find(|a| a.name == WASM_MODULE)
    }

    /// Returns the canonical bundled-stdlib import path declared by
    /// `#![stdlib("core:...")]` / `#![stdlib("wasi:...")]`, if any.
    ///
    /// This attribute is intended for use only by files inside
    /// `wado-compiler/lib/`. It declares the module's canonical identity
    /// independent of how the file is loaded (entry vs. transitive
    /// import), so the loader can pin the entry to its `Core`/`Wasi`
    /// `ModuleSource` and dedup against the bundled cache when an editor
    /// opens the file directly.
    pub fn stdlib_identity(&self) -> Option<&str> {
        self.stdlib_identity_attribute()
            .and_then(|a| a.args.first())
            .map(AttrArg::as_str)
    }

    /// The attribute itself: a malformed one has no identity, but has a span.
    #[must_use]
    pub fn stdlib_identity_attribute(&self) -> Option<&InnerAttribute> {
        self.inner_attributes.iter().find(|a| a.name == STDLIB)
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
            .filter(|a| a.name == GENERATED)
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
            .filter(|a| a.name == GENERATED)
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
    BuiltinTypeDecl(BuiltinTypeDecl),
    Impl(ImplBlock),
    Trait(TraitDecl),
    Resource(ResourceDecl),
    World(WorldDecl),
    Test(TestDecl),
    Global(GlobalDecl),
    /// Unparsable token run, emitted by error recovery so one syntax error
    /// doesn't discard the rest of the module.
    Error(ErrorItem),
}

impl Item {
    /// The `AstId` of this item's declaration node. Every variant carries
    /// one — there is no id-less item — so this is the canonical way to
    /// find an item by identity (e.g. resolving an index built during
    /// `TraitEnv::build` back to the source item) instead of a positional
    /// index into `Module::items`, which a reload can reorder.
    pub fn id(&self) -> AstId {
        match self {
            Item::Use(d) => d.id,
            Item::Function(d) => d.id,
            Item::Interface(d) => d.id,
            Item::Struct(d) => d.id,
            Item::Enum(d) => d.id,
            Item::Variant(d) => d.id,
            Item::Flags(d) => d.id,
            Item::Newtype(d) => d.id,
            Item::TupleTypeDecl(d) => d.id,
            Item::BuiltinTypeDecl(d) => d.id,
            Item::Impl(d) => d.id,
            Item::Trait(d) => d.id,
            Item::Resource(d) => d.id,
            Item::World(d) => d.id,
            Item::Test(d) => d.id,
            Item::Global(d) => d.id,
            Item::Error(d) => d.id,
        }
    }

    /// The source [`Span`] of this item's declaration.
    pub fn span(&self) -> Span {
        match self {
            Item::Use(d) => d.span,
            Item::Function(d) => d.span,
            Item::Interface(d) => d.span,
            Item::Struct(d) => d.span,
            Item::Enum(d) => d.span,
            Item::Variant(d) => d.span,
            Item::Flags(d) => d.span,
            Item::Newtype(d) => d.span,
            Item::TupleTypeDecl(d) => d.span,
            Item::BuiltinTypeDecl(d) => d.span,
            Item::Impl(d) => d.span,
            Item::Trait(d) => d.span,
            Item::Resource(d) => d.span,
            Item::World(d) => d.span,
            Item::Test(d) => d.span,
            Item::Global(d) => d.span,
            Item::Error(d) => d.span,
        }
    }

    /// Declared visibility, or `None` for items that carry no modifier.
    pub fn visibility(&self) -> Option<Visibility> {
        match self {
            Item::Function(d) => Some(d.visibility),
            Item::Interface(d) => Some(d.visibility),
            Item::Struct(d) => Some(d.visibility),
            Item::Enum(d) => Some(d.visibility),
            Item::Variant(d) => Some(d.visibility),
            Item::Flags(d) => Some(d.visibility),
            Item::Newtype(d) => Some(d.visibility),
            Item::TupleTypeDecl(d) => Some(d.visibility),
            Item::BuiltinTypeDecl(d) => Some(d.visibility),
            Item::Trait(d) => Some(d.visibility),
            Item::Resource(d) => Some(d.visibility),
            Item::Global(d) => Some(d.visibility),
            Item::Use(_) | Item::Impl(_) | Item::World(_) | Item::Test(_) | Item::Error(_) => None,
        }
    }

    /// The span of the item's own name, or its whole span where it writes none.
    pub fn name_span(&self) -> Span {
        match self {
            Item::Function(d) => d.name_span,
            Item::Struct(d) => d.name_span,
            Item::Enum(d) => d.name_span,
            Item::Variant(d) => d.name_span,
            Item::Flags(d) => d.name_span,
            Item::Newtype(d) => d.name_span,
            Item::Trait(d) => d.name_span,
            Item::Interface(d) => d.name_span,
            Item::Global(d) => d.name_span,
            Item::BuiltinTypeDecl(d) => d.name_span,
            Item::Resource(_)
            | Item::TupleTypeDecl(_)
            | Item::Use(_)
            | Item::Impl(_)
            | Item::World(_)
            | Item::Test(_)
            | Item::Error(_) => self.span(),
        }
    }

    /// The attributes written before the item. Exhaustive on purpose: a new
    /// item kind that carries attributes must name them here to be read at all.
    pub fn attrs(&self) -> &[Attribute] {
        match self {
            Item::Function(d) => &d.attrs,
            Item::Struct(d) => &d.attrs,
            Item::Enum(d) => &d.attrs,
            Item::Variant(d) => &d.attrs,
            Item::Newtype(d) => &d.attrs,
            Item::Trait(d) => &d.attrs,
            Item::Interface(d) => &d.attrs,
            Item::Resource(d) => &d.attrs,
            Item::TupleTypeDecl(d) => &d.attrs,
            Item::BuiltinTypeDecl(d) => &d.attrs,
            Item::Use(d) => &d.attrs,
            Item::Impl(d) => &d.attrs,
            Item::World(d) => &d.attrs,
            Item::Test(d) => &d.attributes,
            Item::Global(d) => &d.attributes,
            Item::Flags(d) => d.attributes.as_deref().unwrap_or_default(),
            Item::Error(_) => &[],
        }
    }
}

/// Placeholder for a token run that failed to parse as an item. See [`Item::Error`].
#[derive(Debug, Clone)]
pub struct ErrorItem {
    pub id: AstId,
    pub span: Span,
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

/// Test attributes resolved from a `TestDecl`'s `#[...]` annotations (plus the
/// enclosing module's `#[TODO]`). Shared by the annotate and reify walks so the
/// attribute semantics live in one place.
#[derive(Debug, Clone, Copy)]
pub struct TestMetadata {
    pub expect_trap: bool,
    pub is_todo: bool,
    pub timeout_ms: Option<u64>,
    pub is_synopsis: bool,
}

impl TestDecl {
    /// Resolve this test's `#[expect_trap]` / `#[TODO]` / `#[timeout_ms(..)]` /
    /// `#[synopsis]` attributes. `module_is_todo` folds in a module-level `#[TODO]`.
    pub fn metadata(&self, module_is_todo: bool) -> TestMetadata {
        TestMetadata {
            expect_trap: self.attributes.iter().any(|a| a.name == EXPECT_TRAP),
            is_todo: module_is_todo || self.attributes.iter().any(|a| a.name == TODO),
            is_synopsis: self.attributes.iter().any(|a| a.name == SYNOPSIS),
            timeout_ms: self.attributes.iter().find_map(|a| {
                if a.name == TIMEOUT_MS {
                    a.args
                        .first()
                        .and_then(|arg| arg.as_str().parse::<u64>().ok())
                } else {
                    None
                }
            }),
        }
    }
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
    pub visibility: Visibility,
    pub attributes: Vec<Attribute>,
    pub span: Span,
}

/// A single argument in an attribute, keeping string literals, bare identifiers,
/// numbers, `key = value` pairs and `key = [array]` distinct so `unparse` can
/// reconstruct the original syntax — `#[inline(always)]` is `[Ident("always")]`
/// where `#[cm("wasi:cli/stdout")]` is `[Str(…)]`.
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
    /// A `key = ident` pair, whose value names something in the source rather
    /// than carrying text, e.g. `part_of = arr`.
    KeyIdent(String, String),
    /// A `key = 3` pair, whose value is a number rather than text, e.g.
    /// `#[wire(number = 3)]`.
    KeyNumber(String, String),
}

impl AttrArg {
    /// The value this argument carries, the key side aside. An array answers
    /// with its first element, empty where it holds none.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Str(s) | Self::Ident(s) | Self::Number(s) => s,
            Self::KeyValue(_, v) | Self::KeyIdent(_, v) | Self::KeyNumber(_, v) => v,
            Self::KeyArray(_, vs) => vs.first().map(String::as_str).unwrap_or(""),
        }
    }

    /// The name this argument is written under: the key of a `key = ...` pair,
    /// or the argument itself where it is a bare word.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Str(s) | Self::Ident(s) | Self::Number(s) => s,
            Self::KeyValue(k, _)
            | Self::KeyArray(k, _)
            | Self::KeyIdent(k, _)
            | Self::KeyNumber(k, _) => k,
        }
    }
}

/// The field numbers the wire format admits, from the protobuf specification's
/// "Assigning Field Numbers".
pub const WIRE_NUMBER_MIN: u32 = 1;
pub const WIRE_NUMBER_MAX: u32 = 536_870_911;
pub const WIRE_NUMBER_RESERVED: std::ops::RangeInclusive<u32> = 19_000..=19_999;

/// The `#[wire(number = …)]` text these attributes carry, as written. Every
/// `#[wire]` is read, since a field may spell one adjustment per attribute.
#[must_use]
pub fn wire_number_written(attrs: &[Attribute]) -> Option<&str> {
    attrs.iter().find_map(|a| {
        if a.name == WIRE {
            a.kv_number("number")
        } else {
            None
        }
    })
}

/// The `#[wire(number = N)]` these attributes carry, where it is a number the
/// wire format admits. The diagnostics for one it does not are the
/// elaborator's, which is where a declaration is checked.
#[must_use]
pub fn wire_number_of(attrs: &[Attribute]) -> Option<u32> {
    wire_number_written(attrs)
        .and_then(|written| written.parse::<u32>().ok())
        .filter(|n| (WIRE_NUMBER_MIN..=WIRE_NUMBER_MAX).contains(n))
        .filter(|n| !WIRE_NUMBER_RESERVED.contains(n))
}

/// One entry per field, in declaration order, as `StructInfo` holds them.
#[must_use]
pub fn wire_numbers_of(fields: &[StructField]) -> Vec<Option<u32>> {
    fields.iter().map(|f| wire_number_of(&f.attrs)).collect()
}

/// Attribute like #[cm("...")]
#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    /// Arguments passed to the attribute.
    ///
    /// - `#[canonical("wasi", "stream-new")]` → `[Str("wasi"), Str("stream-new")]`
    /// - `#[wire(name = "type")]`           → `[KeyValue("name", "type")]`
    /// - `#[wire(default)]`                   → `[Ident("default")]`
    pub args: Vec<AttrArg>,
    /// Component Model boundary metadata, populated by the parser when the
    /// attribute is `#[cm(...)]` or `#[canonical(...)]`. A `Some` value means
    /// the carrying item (function, method, effect, resource, variant case,
    /// world, ...) participates in the Component Model boundary in some form.
    pub cm_boundary: Option<CmBoundary>,
    pub span: Span,
}

/// The lint names `#[allow(...)]` takes.
pub mod lint {
    /// A binder taking a name that already reaches something.
    pub const SHADOWED_NAME: &str = "shadowed_name";
    /// An item nothing reaches from the export boundary.
    pub const DEAD_CODE: &str = "dead_code";
    /// A trait head that says nothing about what its impls may do.
    pub const UNDECIDED_EFFECTS: &str = "undecided_effects";
}

/// Whether `#[allow(<lint>)]` sits among `attrs`. The one reading of an allow
/// attribute, so every lint honours it the same way.
#[must_use]
pub fn attrs_allow(attrs: &[Attribute], lint: &str) -> bool {
    attrs
        .iter()
        .any(|attr| attr.name == ALLOW && args_name(&attr.args, lint))
}

/// [`attrs_allow`] for a module's `#![allow(...)]`, which waives the lint for
/// the whole file.
#[must_use]
pub fn inner_attrs_allow(attrs: &[InnerAttribute], lint: &str) -> bool {
    attrs
        .iter()
        .any(|attr| attr.name == ALLOW && args_name(&attr.args, lint))
}

fn args_name(args: &[AttrArg], lint: &str) -> bool {
    args.iter()
        .any(|arg| matches!(arg, AttrArg::Ident(name) if name == lint))
}

impl Attribute {
    /// Find the value of a key-value argument by key name.
    ///
    /// For `#[wire(name = "type")]`, `attr.kv_value("name")` returns `Some("type")`.
    pub fn kv_value(&self, key: &str) -> Option<&str> {
        self.args.iter().find_map(|arg| {
            if let AttrArg::KeyValue(k, v) = arg {
                if k == key { Some(v.as_str()) } else { None }
            } else {
                None
            }
        })
    }

    /// The literal text of a `key = <number>` argument, as written.
    ///
    /// For `#[wire(number = 3)]`, `attr.kv_number("number")` returns `Some("3")`.
    pub fn kv_number(&self, key: &str) -> Option<&str> {
        self.args.iter().find_map(|arg| {
            if let AttrArg::KeyNumber(k, v) = arg {
                if k == key { Some(v.as_str()) } else { None }
            } else {
                None
            }
        })
    }

    /// Return true if any arg matches the given name as an identifier, string, or key in a key-value pair.
    ///
    /// For `#[wire(default)]`, `attr.has_arg("default")` returns `true`.
    pub fn has_arg(&self, name: &str) -> bool {
        self.args.iter().any(|arg| arg.name() == name)
    }

    /// The sentence `#[unavailable(...)]` reports, as its one string argument.
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.args.iter().find_map(|arg| match arg {
            AttrArg::Str(s) => Some(s.as_str()),
            _ => None,
        })
    }

    /// The linearity `#[cm(..., linearity=...)]` declares, if any.
    pub fn cm_resource_linearity(&self) -> Option<CmResourceLinearity> {
        if self.name != CM {
            return None;
        }
        self.kv_value("linearity")
            .and_then(CmResourceLinearity::parse)
    }

    /// Returns the parsed CM interface import (`namespace:package/interface[@v][#fn]`)
    /// carried by this attribute, if any. Returns `None` for `#[canonical(...)]`,
    /// for `#[cm("simple-name")]`, and for non-CM attributes.
    pub fn as_cm_import(&self) -> Option<&CmImport> {
        self.cm_boundary.as_ref().and_then(CmBoundary::as_import)
    }

    /// Returns the CM-side identifier carried by a `#[cm("...")]` attribute,
    /// reconstructed from the parsed `CmBoundary` payload rather than read
    /// from `self.args`. Returns `None` for `#[canonical(...)]` and for
    /// non-CM attributes.
    pub fn cm_identifier(&self) -> Option<String> {
        self.cm_boundary
            .as_ref()
            .and_then(CmBoundary::cm_identifier)
    }
}

/// The CM import among `attrs`, wherever it sits. Reading only the first
/// attribute loses the binding on a declaration that carries an `#[allow(…)]`
/// ahead of it, and the phases then disagree about what is a CM import.
#[must_use]
pub fn cm_import_of(attrs: &[Attribute]) -> Option<&CmImport> {
    attrs.iter().find_map(Attribute::as_cm_import)
}

/// The world-level function import among `attrs`, wherever it sits: the bare
/// function name the dependency's world exports.
#[must_use]
pub fn world_import_of(attrs: &[Attribute]) -> Option<&str> {
    attrs
        .iter()
        .find_map(|a| a.cm_boundary.as_ref()?.as_world_import())
}

/// The CM identifier of `function` in the interface at `interface_path`.
#[must_use]
pub fn cm_function_path(interface_path: &str, function: &str) -> String {
    format!("{interface_path}#{function}")
}

/// Which Component Model boundary a `#[cm(…)]` / `#[canonical(…)]` declaration
/// crosses: `Canonical` lowers to a CM canonical built-in such as
/// `canon.task.return`, `Import` resolves to a real import from
/// `namespace:package/interface[@version][#function]`, and `Name` is a bare
/// CM-side identifier naming a field, case or method.
#[derive(Debug, Clone)]
pub enum CmBoundary {
    Canonical {
        namespace: String,
        name: String,
    },
    Import(CmImport),
    /// A world-level function import: the dependency component exports the
    /// function directly in its world (not under an interface), so its only
    /// CM-side identity is the bare function name. See Phase 9 in
    /// `docs/wep-2026-06-26-wasm-cm-component-import.md`.
    WorldImport(String),
    Name(String),
}

/// `Affine` is move-only with a drop obligation, `Unrestricted` a copyable value.
/// See `docs/wep-2026-04-28-resource-inheritance.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmResourceLinearity {
    Affine,
    Unrestricted,
}

impl CmResourceLinearity {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "affine" => Some(Self::Affine),
            "unrestricted" => Some(Self::Unrestricted),
            _ => None,
        }
    }
}

/// Whether these attributes declare `#[cm(..., linearity = "unrestricted")]`.
pub fn declares_unrestricted(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| a.cm_resource_linearity() == Some(CmResourceLinearity::Unrestricted))
}

impl CmBoundary {
    /// Returns the `CmImport` payload if this boundary is an interface import.
    pub fn as_import(&self) -> Option<&CmImport> {
        match self {
            CmBoundary::Import(cm) => Some(cm),
            CmBoundary::Canonical { .. } | CmBoundary::WorldImport(_) | CmBoundary::Name(_) => None,
        }
    }

    /// Returns the bare function name if this boundary is a world-level
    /// function import (Phase 9).
    pub fn as_world_import(&self) -> Option<&str> {
        match self {
            CmBoundary::WorldImport(func) => Some(func),
            CmBoundary::Canonical { .. } | CmBoundary::Import(_) | CmBoundary::Name(_) => None,
        }
    }

    /// Returns the CM-side identifier carried by this boundary.
    ///
    /// - `Canonical` returns `None` (canonical built-ins are addressed by a
    ///   `(namespace, name)` pair, not by a single string identifier).
    /// - `Import` reconstructs the full path
    ///   `"namespace:package/interface[@version][#function]"` from the parsed
    ///   components.
    /// - `Name` returns the bare CM-side name.
    pub fn cm_identifier(&self) -> Option<String> {
        match self {
            CmBoundary::Canonical { .. } => None,
            CmBoundary::Import(cm) => Some(cm.full_path()),
            CmBoundary::WorldImport(func) => Some(func.clone()),
            CmBoundary::Name(s) => Some(s.clone()),
        }
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

    /// Get the bare interface path without version or function
    /// (e.g., "wasi:cli/stdout").
    pub fn bare_path(&self) -> String {
        format!("{}:{}/{}", self.namespace, self.package, self.interface)
    }

    /// Get the full path including the function fragment when present
    /// (e.g., "wasi:cli/stdout@0.3.0-rc-2025-09-16#write-via-stream").
    /// Reconstructs the canonical form parsed by `CmImport::parse`.
    pub fn full_path(&self) -> String {
        let path = self.interface_path();
        match &self.function {
            Some(f) => cm_function_path(&path, f),
            None => path,
        }
    }
}

/// Resource declaration like `resource Foo;` or `resource Foo<T> { fn method(...); }`
#[derive(Debug, Clone)]
pub struct ResourceDecl {
    pub id: AstId,
    pub name: String,
    pub visibility: Visibility,
    /// Generic type parameters: `resource Future<T> { ... }`
    pub type_params: Vec<GenericParam>,
    /// The parent named by `resource Child extends Parent`, unresolved.
    /// See `docs/wep-2026-04-28-resource-inheritance.md`.
    pub parent: Option<Type>,
    pub attrs: Vec<Attribute>,
    /// Methods declared within the resource block. Each is a signature only:
    /// a resource operation is backed by a CM import, so it has no body to
    /// declare.
    pub methods: Vec<Function>,
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
    pub visibility: Visibility,
    pub attrs: Vec<Attribute>,
    pub imports: Vec<WorldImport>,
    pub exports: Vec<WorldExport>,
    pub span: Span,
}

/// A world import declaration — a bare `import Stdout;`, WIT-faithful. Wado has
/// no `from "<package>"` or `as` qualification: every importable interface is a
/// `pub interface Foo` whose `#[cm("…")]` is the source of truth for its CM FQ
/// name, and resource methods and types reach call sites through ordinary `use`.
#[derive(Debug, Clone)]
pub struct WorldImport {
    pub interface_name: String,
    pub span: Span,
}

/// A world export declaration. Two shapes:
/// - Interface export (`export Foo;`) — references a `pub interface Foo`
///   declaration; the CM-side instance export is materialized by codegen
///   from the interface's functions and `#[cm("...")]`.
/// - Function export (`export [async] fn name(...) -> ret;`) — a direct
///   freestanding-function export. Used by synthetic worlds (test world)
///   and for WIT worlds whose export is a bare freestanding function.
#[derive(Debug, Clone)]
pub enum WorldExport {
    Interface(WorldExportInterface),
    Function(WorldExportFn),
}

#[derive(Debug, Clone)]
pub struct WorldExportInterface {
    pub interface_name: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct WorldExportFn {
    pub name: String,
    pub is_async: bool,
    pub params: Vec<Param>,
    /// `(` through `)` of the parameter list — see [`Function::params_span`].
    pub params_span: Span,
    pub return_type: Option<Type>,
    pub span: Span,
}

impl WorldExport {
    pub fn span(&self) -> Span {
        match self {
            WorldExport::Interface(i) => i.span,
            WorldExport::Function(f) => f.span,
        }
    }
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
        /// Span of the interface name identifier — where the item starts.
        name_span: Span,
        functions: Vec<UseItemSimple>,
    },
    /// Wildcard import: `use _ from "..."` (load module for side effects only)
    Wildcard,
    /// Namespace import: `use name from "..."` (import entire module as namespace)
    Namespace { name: String },
}

impl UseItem {
    /// Where the item starts in source. `None` for the forms that *are* the
    /// whole import list (`use _`, `use name`), which have no gap to sit in.
    pub fn start(&self) -> Option<usize> {
        match self {
            UseItem::Simple { name_span, .. } => Some(name_span.start),
            UseItem::InterfaceFunctions { name_span, .. } => Some(name_span.start),
            UseItem::Wildcard | UseItem::Namespace { .. } => None,
        }
    }
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
    Object(AttrObject),
}

/// An attribute object's entries, in parse order, each keyed by its name.
pub type AttrObject = IndexMap<String, AttrEntry>;

/// The value at `key`, dropping the key span stored beside it.
#[must_use]
pub fn attr_value<'a>(object: &'a AttrObject, key: &str) -> Option<&'a AttrValue> {
    object.get(key).map(|entry| &entry.value)
}

/// One `key: value` entry of an attribute object. `key_span` is what lets a
/// reader point at the key rather than at the whole `with { … }`.
#[derive(Debug, Clone, PartialEq)]
pub struct AttrEntry {
    pub key_span: Span,
    pub value: AttrValue,
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
    pub fn as_object(&self) -> Option<&AttrObject> {
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
    pub entries: AttrObject,
}

impl ImportAttributes {
    fn get_str(&self, key: &str) -> Option<String> {
        self.get(key)
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

    /// Inline registry override for a single-file dependency source
    /// (`with { registry: "oci://…" }`).
    #[must_use]
    pub fn registry(&self) -> Option<String> {
        self.get_str("registry")
    }

    /// Real coordinate a `lib:` alias resolves to, for a single-file dependency
    /// source (`with { package: "ns:pkg" }`).
    #[must_use]
    pub fn package(&self) -> Option<String> {
        self.get_str("package")
    }

    /// Inline git repository URL for a single-file source dependency
    /// (`with { git: "https://…" }`).
    #[must_use]
    pub fn git(&self) -> Option<String> {
        self.get_str("git")
    }

    /// Inline git ref (tag, branch, or SHA) paired with [`ImportAttributes::git`]
    /// (`with { git: "…", ref: "main" }`).
    #[must_use]
    pub fn git_ref(&self) -> Option<String> {
        self.get_str("ref")
    }

    /// Subdirectory of a git repository holding the package (monorepo)
    /// (`with { git: "…", directory: "packages/foo" }`).
    #[must_use]
    pub fn directory(&self) -> Option<String> {
        self.get_str("directory")
    }

    #[must_use]
    pub fn type_hint(&self) -> Option<String> {
        self.get_str("type")
    }

    /// Inline Kiln generator configuration (`with { generator: { ... } }`).
    #[must_use]
    pub fn generator(&self) -> Option<&AttrObject> {
        self.get("generator").and_then(AttrValue::as_object)
    }

    /// Inline provider source (`with { provider: "./ext.wado" }`): a Wado file
    /// compiled on-demand into a component that satisfies the imported
    /// dependency's guest-effect imports (research-cm-boundary-callbacks.md).
    #[must_use]
    pub fn provider_path(&self) -> Option<&str> {
        self.get("provider").and_then(AttrValue::as_str)
    }

    /// Raw lookup for any top-level attribute.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&AttrValue> {
        attr_value(&self.entries, key)
    }
}

/// Symbol visibility ladder, orthogonal to `is_export` (the Component Model
/// surface flag). See docs/wep-2026-06-25-visibility-internal-pub-export.md.
///
/// Ordered by reach; `visibility_order_is_the_ladder` pins it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Visibility {
    /// No modifier: visible only within the defining file.
    #[default]
    Private,
    /// `internal`: visible to other files in the same package.
    Internal,
    /// `pub`: visible to other Wado packages (the library API).
    Public,
}

impl Visibility {
    pub fn is_public(self) -> bool {
        matches!(self, Visibility::Public)
    }

    pub fn is_internal(self) -> bool {
        matches!(self, Visibility::Internal)
    }

    pub fn reaches_beyond_file(self) -> bool {
        !matches!(self, Visibility::Private)
    }

    /// Importable from a module in (`same_package`) or outside the package.
    pub fn reachable_from(self, same_package: bool) -> bool {
        match self {
            Visibility::Public => true,
            Visibility::Internal => same_package,
            Visibility::Private => false,
        }
    }

    /// Whether `self` reaches no further than `other`.
    pub fn reaches_no_further_than(self, other: Self) -> bool {
        self <= other
    }

    /// The narrower reach of the two.
    pub fn narrower(self, other: Self) -> Self {
        self.min(other)
    }

    /// Source keyword with a trailing space (`""` for file-private).
    pub fn keyword(self) -> &'static str {
        match self {
            Visibility::Private => "",
            Visibility::Internal => "internal ",
            Visibility::Public => "pub ",
        }
    }
}

/// Use declaration with ESM-like syntax:
/// `use {items} from "source"`
/// `use {items} from "source" with { version: "1.0" }`
/// `pub use {items} from "source"` (re-export)
#[derive(Debug, Clone)]
pub struct UseDecl {
    pub id: AstId,
    /// Leading `#[…]` attributes, distinct from the `with { … }` clause the
    /// `attributes` field carries.
    pub attrs: Vec<Attribute>,
    /// Re-export visibility; `Private` is a local import, not re-exported.
    pub visibility: Visibility,
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
    /// `{` through `}` of the item list, `None` for the wildcard and namespace
    /// forms, which have no braces. The formatter needs the delimiters, not
    /// just the items, to know what a comment sits inside.
    pub items_span: Option<Span>,
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
    pub visibility: Visibility,
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
    /// `(` through `)` of the parameter list. The formatter needs the
    /// delimiters, not just the parameters, to know what a comment sits inside.
    pub params_span: Span,
    pub return_type: Option<Type>,
    pub effects: Vec<EffectName>,
    /// Whether `effects` came from the enclosing trait's head rather than from
    /// a `with` clause here. The formatter prints what the source wrote.
    pub effects_inherited: bool,
    /// Function body. None indicates a compiler built-in (bodyless declaration like `pub fn foo();`)
    pub body: Option<Block>,
    pub span: Span,
}

impl Function {
    /// The effects the source wrote here. Empty when the enclosing trait's
    /// head supplied them.
    pub fn written_effects(&self) -> &[EffectName] {
        if self.effects_inherited {
            return &[];
        }
        &self.effects
    }

    /// Whether the Component Model supplies this declaration's body.
    pub fn is_cm_import(&self) -> bool {
        self.body.is_none() && self.attrs.iter().any(|a| a.cm_boundary.is_some())
    }

    /// The `#[unavailable]` attribute this declaration carries, if any. Present
    /// even when malformed, so the check that rejects it sees it.
    pub fn unavailable_attr(&self) -> Option<&Attribute> {
        self.attrs.iter().find(|a| a.name == UNAVAILABLE)
    }

    /// Why a site naming this declaration cannot call it. `None` where there is
    /// no `#[unavailable]`, and where `analyze` rejects one as saying nothing.
    pub fn unavailable(&self) -> Option<&str> {
        self.unavailable_attr()?
            .unavailable_reason()
            .filter(|reason| !reason.is_empty())
    }
}

/// Self parameter kind: &self or &mut self
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfKind {
    None,
    /// By-value `self` receiver (`fn drop(self)`) — transfers ownership. Legal
    /// only on a resource; a use of the binding after such a call is a move
    /// error (WEP 2026-05-21).
    Value,
    Ref,
    MutRef,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub id: AstId,
    /// `#[allow(...)]` and anything else written before the parameter.
    pub attrs: Vec<Attribute>,
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
    /// A type/impl declaration local to the enclosing block: `struct`,
    /// `enum`, `variant`, `flags`, `type` (newtype), `impl`, or `trait`.
    /// Scoped to the block that writes it and in scope for the whole of it, so
    /// a use may precede the declaration. `Parser::at_local_item_start`
    /// decides which keywords start one; `Parser::visibility_prefixed_local_item_starts_at`
    /// gives a dedicated error for a `pub`/`internal`/`export` prefix, since a
    /// local item is always private.
    Item(Box<Item>),
    /// Placeholder for a statement that failed to parse, emitted by error
    /// recovery so a broken statement inside a block leaves a node (with a
    /// stable span/id) instead of vanishing. Inert in every later phase; the
    /// batch path is fail-fast and never sees it.
    Error(ErrorStmt),
}

/// Placeholder for a token run that failed to parse as a statement. See
/// [`Stmt::Error`].
#[derive(Debug, Clone)]
pub struct ErrorStmt {
    pub id: AstId,
    pub span: Span,
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
    /// `#[allow(...)]` and anything else written before the `let`.
    pub attrs: Vec<Attribute>,
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
    /// Diverging `else` block of a `let PAT = EXPR else { ... };`. When present
    /// the pattern may be refutable: on a match its bindings enter the enclosing
    /// scope, otherwise this block runs and must diverge.
    pub else_block: Option<Block>,
    pub span: Span,
}

impl LetStmt {
    /// The pattern a `let … else` tests: an annotation is the type pattern it
    /// ascribes, keyed by this statement's id.
    pub fn else_pattern(&self) -> Cow<'_, Pattern> {
        match &self.ty {
            Some(ty) => Cow::Owned(Pattern::Typed {
                id: self.id,
                pattern: Box::new(self.pattern.clone()),
                ty: ty.clone(),
                span: self.name_span.merge(&ty.span()),
            }),
            None => Cow::Borrowed(&self.pattern),
        }
    }
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
    /// `let PAT = EXPR` — pattern match element. `pattern` is boxed to keep
    /// the variant compact (without it, the enum is dominated by the
    /// 300+ byte `Pattern` and clippy fires `large_enum_variant`).
    Let {
        pattern: Box<Pattern>,
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
/// Iterates over elements of a `List<T>`
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
    /// Returns the [`AstId`] for this statement.
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
            Stmt::Item(item) => item.id(),
            Stmt::Error(s) => s.id,
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
            Stmt::Item(item) => item.span(),
            Stmt::Error(s) => s.span,
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
    /// A template literal a path tags: `` sql`… ${x} …` ``. A call of the
    /// tag on the template's holes, not a string
    /// (`docs/wep-2026-01-10-tagged-template-literals.md`).
    TaggedTemplate(Box<TaggedTemplateExpr>),
    Cast(Box<CastExpr>),
    StructLiteral(Box<StructLiteralExpr>),
    TupleLiteral(Box<TupleLiteralExpr>),
    /// Tuple comprehension: `[for let v of tuple { expr }]`, one result element
    /// per source element. See `docs/wep-2026-03-14-variadic-type-parameters.md`.
    TupleComprehension(Box<TupleComprehensionExpr>),
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
    /// Placeholder for an expression that failed to parse, emitted by error
    /// recovery so one malformed expression doesn't discard its enclosing
    /// statement or list (e.g. a broken argument in a call). Resolves to
    /// `ResolvedType::Error` in the elaborator; the batch path never sees it
    /// because it is fail-fast on the first syntax error.
    Error(ErrorExpr),
}

/// Placeholder for a token run that failed to parse as an expression. See
/// [`Expr::Error`].
#[derive(Debug, Clone)]
pub struct ErrorExpr {
    pub id: AstId,
    pub span: Span,
}

/// `with E1 => h1, E2 => h2 do { body }` — installs effect handlers for the
/// duration of `body`. The block's value is the value of `body`.
#[derive(Debug, Clone)]
pub struct WithHandlerExpr {
    pub id: AstId,
    pub handlers: Vec<EffectHandlerBinding>,
    pub body: Block,
    /// Span of the contextual `do` keyword. It lexes as an identifier, so this
    /// is the only way to tell it from a variable.
    pub do_span: Span,
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
    /// Returns the [`AstId`] for this expression.
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
            Expr::TaggedTemplate(e) => e.id,
            Expr::Cast(e) => e.id,
            Expr::StructLiteral(e) => e.id,
            Expr::TupleLiteral(e) => e.id,
            Expr::TupleComprehension(e) => e.id,
            Expr::LabeledBlock(e) => e.id,
            Expr::TryOp(e) => e.id,
            Expr::Spread(inner, _) => inner.id(),
            Expr::Range(e) => e.id,
            Expr::WithHandler(e) => e.id,
            Expr::Resume(e) => e.id,
            Expr::Error(e) => e.id,
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
            Expr::TaggedTemplate(e) => e.span,
            Expr::Cast(e) => e.span,
            Expr::StructLiteral(e) => e.span,
            Expr::TupleLiteral(e) => e.span,
            Expr::TupleComprehension(e) => e.span,
            Expr::LabeledBlock(e) => e.span,
            Expr::TryOp(e) => e.span,
            Expr::Spread(_, span) => *span,
            Expr::Range(e) => e.span,
            Expr::WithHandler(e) => e.span,
            Expr::Resume(e) => e.span,
            Expr::Error(e) => e.span,
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
            Expr::TaggedTemplate(mut e) => {
                e.span = new_span;
                Expr::TaggedTemplate(e)
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
            Expr::TupleComprehension(mut e) => {
                e.span = new_span;
                Expr::TupleComprehension(e)
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
            Expr::Error(mut e) => {
                e.span = new_span;
                Expr::Error(e)
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
    /// Turbofish arguments pinning a generic struct's parameters,
    /// `Box::<i32> { value: 1 }`. Empty where the literal writes none.
    pub type_args: Vec<Type>,
    pub fields: Vec<StructLiteralField>,
    /// Spread bases (`{ ..a, field: v, ..b }`) in source order, each supplying
    /// the fields the literal does not list explicitly. See WEP: Literal Spread.
    pub spreads: Vec<StructLiteralSpread>,
    /// Whether the original source had a trailing comma (for formatting purposes).
    /// Multiline formatting is used when this is true.
    pub has_trailing_comma: bool,
    pub span: Span,
}

/// A `..base` spread element inside a struct/key-value literal.
#[derive(Debug, Clone)]
pub struct StructLiteralSpread {
    pub expr: Box<Expr>,
    /// Explicit fields appearing before this spread in source order.
    pub field_pos: usize,
    pub span: Span,
}

/// A struct/key-value literal member in source order, with its index into the
/// owning `StructLiteralExpr`'s `spreads` / `fields` list.
pub enum LiteralMember<'a> {
    Spread(usize, &'a StructLiteralSpread),
    Field(usize, &'a StructLiteralField),
}

impl LiteralMember<'_> {
    pub fn span(&self) -> Span {
        match self {
            LiteralMember::Spread(_, sp) => sp.span,
            LiteralMember::Field(_, f) => f.span,
        }
    }

    /// `AstId` of the member's value expression — the node a same-line trailing
    /// comment attaches to, since it is the member's last-ending node.
    pub fn value_id(&self) -> AstId {
        match self {
            LiteralMember::Spread(_, sp) => sp.expr.id(),
            LiteralMember::Field(_, f) => f.value.id(),
        }
    }
}

impl StructLiteralExpr {
    /// Members in source order (spreads interleaved with fields via `field_pos`).
    /// Every pass that walks members shares this order so last-wins / insert
    /// order stay in lockstep.
    pub fn members(&self) -> Vec<LiteralMember<'_>> {
        let mut out = Vec::with_capacity(self.fields.len() + self.spreads.len());
        let mut si = 0;
        for pos in 0..=self.fields.len() {
            while si < self.spreads.len() && self.spreads[si].field_pos == pos {
                out.push(LiteralMember::Spread(si, &self.spreads[si]));
                si += 1;
            }
            if pos < self.fields.len() {
                out.push(LiteralMember::Field(pos, &self.fields[pos]));
            }
        }
        out
    }
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
/// Can be coerced to `List<T>` when all elements have the same type.
#[derive(Debug, Clone)]
pub struct TupleLiteralExpr {
    pub id: AstId,
    pub elements: Vec<Expr>,
    pub span: Span,
}

/// Tuple comprehension: `[for let v of tuple { expr }]`.
///
/// The source tuple is walked at compile time and `body` evaluated once per
/// element, so the result is a tuple of the same arity. `[for let [i, v] of
/// tuple.enumerate() { expr }]` binds the element index alongside the value.
#[derive(Debug, Clone)]
pub struct TupleComprehensionExpr {
    pub id: AstId,
    /// Element binding: an ident, or `[i, v]` under `.enumerate()`.
    pub binding: Pattern,
    /// The source tuple, as written — `t.enumerate()` included, so the form is
    /// recognised where the statement for-of recognises it.
    pub iterable: Expr,
    /// The per-element expression, written inside braces.
    pub body: Expr,
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
    /// Turbofish type arguments attached to this identifier when it appears
    /// as a bare expression (e.g. `identity::<i32>` in `let f = identity::<i32>;`).
    /// Call-site turbofish (`identity::<i32>(x)`) is recorded on `CallExpr.type_args`
    /// instead, so this is empty for identifiers used directly as a call callee.
    pub type_args: Vec<Type>,
    /// Whether `type_args` were written on the path's *prefix* rather than on
    /// the identifier itself — `Maybe::<i32>::Nothing` (a turbofish-qualified
    /// case) as against `ns::f::<i32>` (a generic function reference). Only the
    /// former admits a `_` slot, which the expected type fills.
    pub type_args_on_prefix: bool,
}

impl IdentExpr {
    /// The segment naming the path's *owner*: `Color` in `Color::Red` and in
    /// `ns::Color::Red` — the one before the member, so a namespace qualifier
    /// ahead of the owner does not stand in for it. `None` for a bare name,
    /// which qualifies nothing.
    pub fn owner_segment(&self) -> Option<&PathSegment> {
        self.segments.get(self.owner_index()?)
    }

    /// Where [`Self::owner_segment`] sits, for a caller that also needs what
    /// qualifies it — the `ns` of `ns::Color::Red`.
    pub fn owner_index(&self) -> Option<usize> {
        self.segments.len().checked_sub(2)
    }
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
    /// Byte-string literal `b"..."`: raw source text between quotes (escape
    /// sequences not interpreted). Lowers to a constant `List<u8>`.
    Bytes(String),
    /// Byte literal `b'x'`: raw source between quotes. Lowers to a `u8`.
    Byte(String),
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

/// Static method call expression: `List::<i32>::with_capacity(100)` or `Point::origin()`
#[derive(Debug, Clone)]
pub struct StaticMethodCallExpr {
    pub id: AstId,
    /// The target type (e.g., `List<i32>` or `Point`)
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
    /// Per-arm [`AstId`]. Allocated at the start of `parse_match_arm`,
    /// so leading comments preceding the arm — which the parser pins to
    /// the first id allocated in that source range — land on the arm
    /// rather than leaking into the pattern's first child id. The
    /// formatter reads `trivia.leading_of(arm.id)` and
    /// `trivia.trailing_of(arm.id)` to render arm-level comments.
    pub id: AstId,
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
        /// Optional qualifier for a qualified case pattern, e.g. `Option` in
        /// `Option::Some(x)`, `shapes::Op` in `shapes::Op::Wildcard`, or
        /// `Result<i32, E>` in `Result<i32, E>::Ok(v)`.
        variant_qualifier: Option<Type>,
        /// `AstId` of the variant-name identifier in the pattern. Used to
        /// record use→def references for LSP navigation (cursor on `Some`
        /// inside a match arm jumps to the case's declaration site).
        /// `None` for elaborator-synthesized patterns that do not originate in
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
        /// The qualifier's own reference site. Naming a type in pattern
        /// position is naming a declaration, so the walk answers for it and
        /// the consumer compares declarations rather than spellings.
        type_name_id: Option<AstId>,
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
    /// Type pattern: `input: HtmlInputElement`. Matches a value of `ty`, which
    /// `pattern` then binds. A `let` keeps a top-level ascription in `LetStmt::ty`.
    Typed {
        id: AstId,
        pattern: Box<Pattern>,
        ty: Type,
        span: Span,
    },
    /// Placeholder for a pattern that failed to parse, emitted by error
    /// recovery (e.g. a broken element in a tuple pattern or match-arm pattern)
    /// so the surrounding pattern list survives. Inert in every later phase;
    /// the batch path is fail-fast and never sees it.
    Error(Span),
}

#[derive(Debug, Clone)]
pub struct StructPatternField {
    /// The node's own id, whose [`AstIdSpace`] names the module that wrote it.
    pub id: AstId,
    pub field_name: String,
    pub pattern: Pattern,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ClosureExpr {
    pub id: AstId,
    pub params: Vec<ClosureParam>,
    /// Explicit return type from `|params| -> Type body`, if written.
    pub return_type: Option<Type>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ClosureParam {
    pub id: AstId,
    /// `#[allow(...)]` and anything else written before the parameter.
    pub attrs: Vec<Attribute>,
    pub name: String,
    /// Span of the closure parameter name identifier.
    pub name_span: Span,
    pub ty: Option<Type>,
    pub is_mut: bool,
    pub default: Option<Expr>,
}

/// Template string expression: `Hello, ${name}!`
#[derive(Debug, Clone)]
pub struct TemplateStringExpr {
    pub id: AstId,
    pub parts: Vec<TemplatePart>,
    pub span: Span,
}

impl TemplateStringExpr {
    /// The hole expressions, in source order.
    pub fn interpolations(&self) -> impl Iterator<Item = &Expr> {
        self.parts.iter().filter_map(|part| match part {
            TemplatePart::Interpolation { expr, .. } => Some(expr.as_ref()),
            TemplatePart::String(_) => None,
        })
    }

    pub fn interpolations_mut(&mut self) -> impl Iterator<Item = &mut Expr> {
        self.parts.iter_mut().filter_map(|part| match part {
            TemplatePart::Interpolation { expr, .. } => Some(expr.as_mut()),
            TemplatePart::String(_) => None,
        })
    }
}

/// A tagged template: `` sql`… ${x} …` ``. The tag is the path written
/// directly before the backtick; the parser admits only an [`Expr::Ident`].
#[derive(Debug, Clone)]
pub struct TaggedTemplateExpr {
    pub id: AstId,
    pub tag: Expr,
    pub template: TemplateStringExpr,
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
        /// The `${` that opens the interpolation. Bounds what the formatter
        /// may place inside these braces.
        open: Span,
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
    /// Namespaced generic type like `ns::Type<T>` or `T::Assoc`. Boxed, as
    /// `Function` is: two names and two spans make `Type` large enough that
    /// every enum holding one draws `large_enum_variant`.
    NamespacedGeneric(Box<NamespacedGenericType>),
    Function(Box<FunctionType>),
    Tuple(Vec<Type>),
    Reference(Box<Type>),
    MutReference(Box<Type>),
    /// Type pack spread inside a tuple: `..T` in `[i32, ..T, bool]`
    TypePackSpread(String, Span),
    /// Inference placeholder `_` in a type position. Inside a turbofish it
    /// leaves a type-argument slot for inference (`Result<_, MyErr>`);
    /// elsewhere it resolves to the elaborator's unknown type.
    Infer(Span),
    /// Placeholder for a type that failed to parse, emitted by error recovery
    /// (e.g. a broken element in a type-argument or parameter list) so the
    /// surrounding list survives. Resolves to the elaborator's error type; the
    /// batch path is fail-fast and never sees it.
    Error(Span),
}

impl Type {
    /// Every identifier this type mentions, at any depth. A caller matching
    /// them against a declaration's parameter names learns which of those the
    /// type determines — a name that is really a concrete type simply matches
    /// no parameter.
    pub fn mentioned_names(&self, out: &mut Vec<String>) {
        match self {
            Type::Named(n) => out.push(n.name.clone()),
            Type::Generic(g) => {
                out.push(g.name.clone());
                for a in &g.args {
                    a.mentioned_names(out);
                }
            }
            Type::NamespacedGeneric(g) => {
                out.push(g.namespace.clone());
                out.push(g.name.clone());
                for a in &g.args {
                    a.mentioned_names(out);
                }
            }
            Type::Function(f) => {
                for p in &f.params {
                    p.mentioned_names(out);
                }
                f.return_type.mentioned_names(out);
            }
            Type::Tuple(elems) => {
                for e in elems {
                    e.mentioned_names(out);
                }
            }
            Type::Reference(inner) | Type::MutReference(inner) => inner.mentioned_names(out),
            Type::TypePackSpread(name, _) => out.push(name.clone()),
            Type::Infer(_) | Type::Error(_) => {}
        }
    }

    /// Whether `pred` holds of this type or of any within it, outermost first.
    /// The one recursion a type predicate takes; a hand-spelled walk forgets arms.
    #[must_use]
    pub fn any(&self, pred: &mut impl FnMut(&Type) -> bool) -> bool {
        if pred(self) {
            return true;
        }
        match self {
            Type::Generic(g) => g.args.iter().any(|a| a.any(pred)),
            Type::NamespacedGeneric(g) => g.args.iter().any(|a| a.any(pred)),
            Type::Function(f) => f.params.iter().any(|p| p.any(pred)) || f.return_type.any(pred),
            Type::Tuple(elems) => elems.iter().any(|e| e.any(pred)),
            Type::Reference(inner) | Type::MutReference(inner) => inner.any(pred),
            Type::Named(_) | Type::TypePackSpread(..) | Type::Infer(_) | Type::Error(_) => false,
        }
    }

    /// Whether `name` is spelled anywhere in this type.
    #[must_use]
    pub fn mentions(&self, name: &str) -> bool {
        self.any(&mut |ty| match ty {
            Type::Named(n) => n.name == name,
            Type::Generic(g) => g.name == name,
            Type::NamespacedGeneric(g) => g.namespace == name || g.name == name,
            Type::TypePackSpread(spread, _) => spread == name,
            Type::Function(_)
            | Type::Tuple(_)
            | Type::Reference(_)
            | Type::MutReference(_)
            | Type::Infer(_)
            | Type::Error(_) => false,
        })
    }

    /// Every `..X` this type spells, with where it is written. Exhaustive, as
    /// [`Self::mentioned_names`] is.
    #[must_use]
    pub fn pack_spreads(&self) -> Vec<(&str, Span)> {
        let mut out = Vec::new();
        self.collect_pack_spreads(&mut out);
        out
    }

    fn collect_pack_spreads<'a>(&'a self, out: &mut Vec<(&'a str, Span)>) {
        match self {
            Type::Generic(g) => {
                for a in &g.args {
                    a.collect_pack_spreads(out);
                }
            }
            Type::NamespacedGeneric(g) => {
                for a in &g.args {
                    a.collect_pack_spreads(out);
                }
            }
            Type::Function(f) => {
                for p in &f.params {
                    p.collect_pack_spreads(out);
                }
                f.return_type.collect_pack_spreads(out);
            }
            Type::Tuple(elems) => {
                for e in elems {
                    e.collect_pack_spreads(out);
                }
            }
            Type::Reference(inner) | Type::MutReference(inner) => inner.collect_pack_spreads(out),
            Type::TypePackSpread(name, span) => out.push((name, *span)),
            Type::Named(_) | Type::Infer(_) | Type::Error(_) => {}
        }
    }

    /// Whether this is the unit type, spelled `()`.
    #[must_use]
    pub fn is_unit(&self) -> bool {
        matches!(self, Type::Named(n) if n.name == "()")
    }

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
            | Type::TypePackSpread(_, _)
            | Type::Infer(_)
            | Type::Error(_) => None,
        }
    }

    /// Base name of the type's head — the named type it denotes, peeling
    /// references and dropping generic arguments (`&List<T>` → `List`,
    /// `Point` → `Point`). `None` for structural types (tuple, function).
    /// Used to match a symbol-notation receiver against `impl` target types.
    pub fn head_base_name(&self) -> Option<&str> {
        let name = match self {
            Type::Named(t) => t.name.as_str(),
            Type::Generic(t) => t.name.as_str(),
            Type::Reference(inner) | Type::MutReference(inner) => return inner.head_base_name(),
            _ => return None,
        };
        Some(name.split('<').next().unwrap_or(name))
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
            Type::Infer(span) => *span,
            Type::Error(span) => *span,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NamedType {
    pub id: AstId,
    pub name: String,
    pub span: Span,
}

impl NamedType {
    /// A name reference. Which declaration it means is answered once by
    /// `crate::resolve::Resolutions`, keyed by [`Self::id`] — not stored here
    /// as a spelling.
    pub fn new(id: AstId, name: String, span: Span) -> Self {
        Self { id, name, span }
    }
}

#[derive(Debug, Clone)]
pub struct GenericType {
    pub id: AstId,
    pub name: String,
    pub args: Vec<Type>,
    pub span: Span,
}

/// Namespaced generic type like `ns::Type<T>` or `T::Assoc`
#[derive(Debug, Clone)]
pub struct NamespacedGenericType {
    pub id: AstId,
    /// Namespace (e.g., "json" for `json::Value`, or a type parameter `T`)
    pub namespace: String,
    /// Type name (e.g., "Value")
    pub name: String,
    /// Span of just the type name token. `span` covers the whole `ns::Value<T>`,
    /// so only this locates the second segment.
    pub name_span: Span,
    /// Generic arguments
    pub args: Vec<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FunctionType {
    /// `true` for `fn mut(...)` (closure may mutate captures);
    /// `false` for `fn(...)` (read-only captures).
    pub is_mut: bool,
    pub params: Vec<Type>,
    pub return_type: Type,
    pub effects: Vec<EffectName>,
}

/// One effect name in a `with` clause, at the site that writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectName {
    pub name: String,
    pub id: AstId,
    pub span: Span,
}

impl AsRef<str> for EffectName {
    fn as_ref(&self) -> &str {
        &self.name
    }
}

/// An effect declaration: an interface whose operations handlers implement.
///
/// An interface is a trait with a different dispatch story, so its members are
/// [`Function`]s parsed exactly as a trait's are. A method with a body declares
/// the default implementation — what the operation does when no handler is
/// installed; without one, dispatching it with no handler traps.
#[derive(Debug, Clone)]
pub struct InterfaceDecl {
    pub id: AstId,
    pub name: String,
    /// Span of the interface name identifier.
    pub name_span: Span,
    pub visibility: Visibility,
    pub attrs: Vec<Attribute>,
    pub methods: Vec<Function>,
    pub span: Span,
}

/// An associated type binding inside a trait bound, e.g., `Output = T` in `Builder<Output = T>`.
#[derive(Debug, Clone)]
pub struct AssocTypeBound {
    /// This reference's own id — the key its resolution is recorded under.
    pub id: AstId,
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

/// A single trait bound on a generic parameter.
/// Simple: `Ord`; with the trait's own arguments: `Eq<String>`; with
/// associated type bindings: `Builder<Output = T>`.
/// `fn(...)` / `fn mut(...)` closure-type bound: `<F: fn(i32) -> i32>` —
/// surface syntax for the internal `Fn` / `FnMut` traits; the parsed function
/// signature is recorded in `fn_signature` and the bound's `name` is set to
/// `"Fn"` or `"FnMut"` for diagnostic and elaborator routing.
#[derive(Debug, Clone)]
pub struct TraitBound {
    /// This reference's own id — the key its resolution is recorded under.
    /// Distinct from the referenced trait's declaration id: one bound is one
    /// reference site, and two modules' `T: Greet` are two of them.
    pub id: AstId,
    pub name: String,
    /// The trait's own arguments, positionally (`String` in `Eq<String>`). A
    /// position the bound leaves out takes the trait's declared default.
    pub type_args: Vec<Type>,
    pub assoc_types: Vec<AssocTypeBound>,
    pub span: Span,
    /// Set when the bound was written as `fn(...)` / `fn mut(...)` in source.
    /// Carries the parsed closure signature so the elaborator can constrain the
    /// generic parameter to that exact function type at use sites.
    pub fn_signature: Option<Box<FunctionType>>,
    /// The declaration this bound names, when it was synthesised rather than
    /// written. A bound the parser produced leaves this `None` and is answered
    /// at its own site by the resolve pass; a bound the compiler rebuilds
    /// already knows its referent, and recording it here is what keeps that
    /// referent from being re-derived out of `name`.
    pub resolved: Option<DefId>,
}

impl TraitBound {
    /// Whether the bound names a trait to check against. An `fn(…)` bound names
    /// none: it is already realised in the bounded parameter's own type.
    pub fn names_a_trait(&self) -> bool {
        self.fn_signature.is_none()
    }

    /// Whether any type the bound writes is rooted at `Self`, so reading it
    /// needs the frame that wrote it. Every position a bound writes a type in,
    /// an `fn` bound's own signature included.
    pub fn writes_self(&self) -> bool {
        let in_signature = self.fn_signature.as_ref().is_some_and(|sig| {
            sig.return_type.mentions("Self") || sig.params.iter().any(|ty| ty.mentions("Self"))
        });
        in_signature
            || self.type_args.iter().any(|ty| ty.mentions("Self"))
            || self.assoc_types.iter().any(|c| c.ty.mentions("Self"))
    }
}

/// Generic type parameter declaration: `<T>`, `<T, U>`, `<T: Ord>`, `<T: Builder<Output = T>>`
/// Effect parameter declaration: `<effect E>` — represents a set of effects
#[derive(Debug, Clone)]
pub struct GenericParam {
    pub id: AstId,
    /// `#[allow(...)]` and anything else written before the parameter.
    pub attrs: Vec<Attribute>,
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

impl GenericParam {
    /// The trait bounds this param declares. An `fn`-signature bound is
    /// excluded: it is already realised in the parameter's own type, so there
    /// is no trait to check a type argument against.
    pub fn real_bounds(&self) -> Vec<TraitBound> {
        self.bounds
            .iter()
            .filter(|b| b.names_a_trait())
            .cloned()
            .collect()
    }

    /// Whether the source spells this param. The effect parameter `with _`
    /// mints reads as `_` at its use sites and appears in no list.
    pub fn is_written(&self) -> bool {
        !(self.is_effect && self.name == EFFECT_HOLE)
    }

    /// Whether this param carries an `fn`-signature bound (`<F: fn(...)>`).
    /// Such params are erased before codegen, so they occupy no positional
    /// monomorphization slot.
    pub fn has_fn_bound(&self) -> bool {
        self.bounds.iter().any(|b| !b.names_a_trait())
    }

    /// Whether this param occupies a dense, positional slot — the "real" type
    /// params monomorphization substitutes by index. Excludes effect params
    /// (`effect E`) and `fn`-bound params (`<F: fn(...)>`), whose bound already
    /// fixes them to a concrete function type. A *pack* consumes a slot
    /// whatever it is bounded by, which is what registers it: both places that
    /// assign slots resolve a pack's shape before they look at its bounds.
    /// Single source for the projection rule shared by the annotate walk and
    /// reify.
    pub fn is_real_type_param(&self) -> bool {
        !self.is_effect && (self.is_pack || !self.has_fn_bound())
    }

    /// Whether this parameter of an `impl` head takes an argument position.
    /// A `fn`-bound one can still be a target argument — `Holder<F>` puts `F`
    /// at position 0 — so only an effect parameter, which is no type, is out.
    pub fn fills_impl_slot(&self) -> bool {
        !self.is_effect
    }
}

#[derive(Debug, Clone)]
pub struct StructDecl {
    pub id: AstId,
    pub name: String,
    /// Span of the struct name identifier.
    pub name_span: Span,
    pub visibility: Visibility,
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
    pub visibility: Visibility,
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
    pub visibility: Visibility,
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
    pub visibility: Visibility,
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
    pub visibility: Visibility,
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
    pub visibility: Visibility,
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
    pub visibility: Visibility,
    pub attrs: Vec<Attribute>,
    /// The declared head, `[..T]`. Kept so a walk reaches the `T` it binds,
    /// and so the formatter prints the name that was written.
    pub head: Type,
    pub span: Span,
}

/// Named, definition-less type declaration: `pub type Array<T>;`
///
/// Anchors a compiler builtin type (carrying its `#[compiler_item("...")]`)
/// to its owning module, giving it a declaration site and a name that
/// `impl` / trait-impl blocks can attach to. Generates no code; its
/// resolution to a builtin `ResolvedType` is wired in the elaborator.
#[derive(Debug, Clone)]
pub struct BuiltinTypeDecl {
    pub id: AstId,
    pub name: String,
    /// Span of the type name identifier.
    pub name_span: Span,
    pub visibility: Visibility,
    pub type_params: Vec<GenericParam>,
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
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

/// Associated constant in an impl block: `pub const PI: f64 = 3.14159;`
/// These are compile-time constants that are inlined at every use site.
#[derive(Debug, Clone)]
pub struct AssociatedConst {
    pub id: AstId,
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub visibility: Visibility,
    pub ty: Type,
    pub value: Expr,
    pub span: Span,
}

/// What a trait's head says about the effects its impls may declare. A method
/// that writes its own `with` clause overrides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraitHead {
    /// No clause: reads as [`TraitHead::Open`], and reports as undecided.
    Undecided,
    /// `with ()`: every impl is pure.
    Pure { span: Span },
    /// `with A` / `with (A, B)`: every impl gets exactly these.
    Fixed {
        effects: Vec<EffectName>,
        span: Span,
    },
    /// `with _`: the impl brings its own effects.
    Open { span: Span },
}

impl TraitHead {
    /// Whether an impl of this trait chooses its own effects.
    pub fn is_open(&self) -> bool {
        matches!(self, TraitHead::Open { .. } | TraitHead::Undecided)
    }

    /// The effects a method inherits when it declares none of its own, each
    /// where the head writes it. `unwritten` stands in for a head left out.
    pub fn inherited_effects(&self, unwritten: Span) -> Vec<(String, Span)> {
        match self {
            TraitHead::Pure { .. } => Vec::new(),
            TraitHead::Fixed { effects, .. } => effects
                .iter()
                .map(|effect| (effect.name.clone(), effect.span))
                .collect(),
            TraitHead::Open { span } => vec![(EFFECT_HOLE.to_string(), *span)],
            TraitHead::Undecided => vec![(EFFECT_HOLE.to_string(), unwritten)],
        }
    }

    /// Where the clause is written. `None` when the source wrote none.
    pub fn span(&self) -> Option<Span> {
        match self {
            TraitHead::Undecided => None,
            TraitHead::Pure { span } | TraitHead::Open { span } | TraitHead::Fixed { span, .. } => {
                Some(*span)
            }
        }
    }
}

/// The name `with _` carries, both as an effect reference and as the name of
/// the effect parameter it mints.
pub const EFFECT_HOLE: &str = "_";

/// The params the source spells, skipping the one `with _` mints.
pub fn written_params(params: &[GenericParam]) -> impl Iterator<Item = &GenericParam> {
    params.iter().filter(|p| p.is_written())
}

/// Trait declaration: `trait Foo { type Output; fn method(&self) -> Self::Output; }`
#[derive(Debug, Clone)]
pub struct TraitDecl {
    pub id: AstId,
    pub name: String,
    /// Span of the trait name identifier.
    pub name_span: Span,
    pub visibility: Visibility,
    /// The `with` clause on the head: what every impl of this trait may do.
    pub head: TraitHead,
    pub type_params: Vec<GenericParam>,
    /// Supertraits: the `Eq + Display` of `trait Ord: Eq + Display`. Every
    /// implementor of this trait must also implement each of them.
    pub supertraits: Vec<TraitBound>,
    /// Associated type declarations: `type Output;`
    pub associated_types: Vec<AssociatedTypeDecl>,
    /// Trait methods. Body is None for required methods.
    pub methods: Vec<Function>,
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// What an effect handler does with an operation it does not implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestClause {
    /// `..trap` — dispatching the operation traps.
    Trap,
    /// `..forward` — dispatching the operation reaches the outer handler.
    Forward,
}

/// A rest clause as written, positioned so the highlighter can reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestClauseDecl {
    pub kind: RestClause,
    /// The `trap` / `forward` word, not the leading `..`.
    pub keyword_span: Span,
}

#[derive(Debug, Clone)]
pub struct ImplBlock {
    pub id: AstId,
    pub attrs: Vec<Attribute>,
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
    /// Rest clause at the end of an effect handler `impl` block, deciding
    /// what an operation absent from `methods` does when dispatched. Only
    /// meaningful for effect handler impls; ignored for ordinary trait impls.
    pub rest: Option<RestClauseDecl>,
    pub span: Span,
}

#[cfg(test)]
mod visibility_tests {
    use super::Visibility;

    /// The ladder is the ordering: every reach comparison reads from it.
    #[test]
    fn visibility_order_is_the_ladder() {
        assert!(Visibility::Private < Visibility::Internal);
        assert!(Visibility::Internal < Visibility::Public);
        assert_eq!(
            Visibility::Public.narrower(Visibility::Internal),
            Visibility::Internal
        );
        assert!(Visibility::Internal.reaches_no_further_than(Visibility::Public));
        assert!(!Visibility::Public.reaches_no_further_than(Visibility::Internal));
    }
}

#[cfg(test)]
mod ast_id_tests {
    use super::*;
    use crate::hashmap::IndexSet;
    use crate::lexer::lex;
    use crate::parser::Parser;

    fn parse(source: &str) -> Module {
        let r = lex(source);
        assert!(r.errors.is_empty(), "lex error: {:?}", r.errors);
        let mut parser = Parser::new(r.tokens);
        parser.parse_strict().expect("parse")
    }
    /// Each bound is its own reference site, so two `T: Ord`s are two ids —
    /// what lets `Ord` mean a different declaration in each, and what a bound
    /// keyed by its spelling cannot express.
    #[test]
    fn each_trait_bound_is_its_own_reference_site() {
        let m = parse("fn f<T: Ord, U: Ord>(a: T, b: U) {}\n");
        let bounds: Vec<AstId> = m
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Function(f) => Some(f),
                _ => None,
            })
            .flat_map(|f| f.type_params.iter())
            .flat_map(|p| p.bounds.iter())
            .map(|b| b.id)
            .collect();
        assert_eq!(bounds.len(), 2);
        assert_ne!(bounds[0], bounds[1]);

        // And the walk reaches them, so an id-collecting pass sees every site.
        let walked: IndexSet<AstId> = collect_ids(&m.items)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        for id in bounds {
            assert!(
                walked.contains(&id),
                "bound id {id:?} not reached by the walk"
            );
        }
    }

    #[test]
    fn ast_id_spaces_are_unique_per_parse() {
        // Each top-level parse mints a fresh `AstIdSpace`, so the same dense
        // local index in two modules yields globally distinct `AstId`s — the
        // invariant that makes per-node fact maps collision-proof (issue #1342).
        let a = parse("fn f() { let x = 1; }\n");
        let b = parse("fn f() { let x = 1; }\n");
        assert_ne!(a.ast_id_space(), b.ast_id_space());
        let a0 = AstId::new(a.ast_id_space(), 0);
        let b0 = AstId::new(b.ast_id_space(), 0);
        assert_eq!(
            a0.local(),
            b0.local(),
            "locals still restart at 0 per module"
        );
        assert_ne!(a0, b0, "same local in different modules must not collide");
    }

    #[test]
    fn template_interpolations_share_one_module_space() {
        // Interpolations historically restarted ids at 0 and collided on
        // `AstId(0)`; sub-parsers now continue the parent's space + counter,
        // so every id in one module tree shares one space and a unique local.
        let m = parse("fn f(a: i32, b: i32) -> i32 { return `${a}${b}`.len(); }\n");
        let ids = collect_ids(&m.items);
        let space = m.ast_id_space();
        let mut seen: IndexSet<AstId> = IndexSet::default();
        for (id, _) in &ids {
            assert_eq!(id.space(), space, "id {id:?} not in the module's space");
            assert!(
                seen.insert(*id),
                "duplicate id {id:?} (interpolation collision)"
            );
        }
    }

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
    fn parse_assigns_stable_locals_and_fresh_spaces() {
        let m1 = parse(SAMPLE);
        let m2 = parse(SAMPLE);
        assert_eq!(m1.ast_id_count(), m2.ast_id_count());
        assert!(m1.ast_id_count() > 0);

        // The surviving parse-stability contract: re-parsing the same source
        // assigns the same dense *local* sequence...
        let locals_1: Vec<u32> = collect_ids(&m1.items)
            .into_iter()
            .map(|(id, _)| id.local())
            .collect();
        let locals_2: Vec<u32> = collect_ids(&m2.items)
            .into_iter()
            .map(|(id, _)| id.local())
            .collect();
        assert_eq!(locals_1, locals_2);

        // ...while each parse mints its own `AstIdSpace`, so the full ids of
        // two parses never collide (cross-module/global uniqueness).
        assert_ne!(m1.ast_id_space(), m2.ast_id_space());
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
            assert_eq!(id.space(), m.ast_id_space(), "foreign space: {id:?}");
            assert!(
                id.local() < m.ast_id_count(),
                "id {} out of range (count={})",
                id.local(),
                m.ast_id_count()
            );
        }
        for i in 0..m.ast_id_count() {
            assert!(
                seen.contains(&AstId::new(m.ast_id_space(), i)),
                "missing id {i} in dense range"
            );
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
            assert_eq!(id.space(), m.ast_id_space(), "foreign space: {id:?}");
            assert!(
                id.local() < m.ast_id_count(),
                "id {} out of range (count={})",
                id.local(),
                m.ast_id_count()
            );
        }
        for i in 0..m.ast_id_count() {
            assert!(
                seen.contains(&AstId::new(m.ast_id_space(), i)),
                "missing id {i} in dense range"
            );
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
