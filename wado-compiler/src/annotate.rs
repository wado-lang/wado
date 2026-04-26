//! Annotate phase — the LSP-friendly analysis entry point.
//!
//! `annotate` drives the compilation pipeline up through name resolution and
//! type resolution, then stops. The resulting [`Annotated`] bundles every
//! semantic fact needed to answer editor queries (hover, go-to-definition,
//! diagnostics) without paying for monomorphize / lower / codegen.
//!
//! The pipeline used here is the same one that `compile_with_options` runs in
//! its early phases: lex → parse → bind → desugar → load → analyze → resolve.
//! Everything downstream of resolve is only needed to emit Wasm bytes, so LSP
//! skips it.

use crate::analyze::Analyzer;
use crate::ast::{AstId, Item, Module};
use crate::compiler_host::{CompilerHost, LogLevel};
use crate::hashmap::IndexMap;
use crate::loader;
use crate::logger::{Bail, Logger};
use crate::name::ModuleSource;
use crate::resolver::Resolver;
use crate::resolver::orchestration::AnnotateState;
use crate::symbol::{Symbol, SymbolKey, SymbolTable};
use crate::tir::{ResolvedType, TirModule, TypeTable};
use crate::token::Span;

/// A ready-to-query analysis result.
///
/// `Annotated` owns every piece of semantic state produced by the analysis
/// pipeline. The AST modules are preserved verbatim (so positions,
/// [`AstId`]s, and spans resolve against the same tree the parser saw).
/// [`SymbolTable`] is owned; [`TypeTable`] is exposed as an immutable
/// snapshot taken at the end of the annotate phase. LSP queries read the
/// snapshot. The lowering pipeline consumes the shared `state` field to
/// continue interning types into the same table without invalidating the
/// snapshot.
pub struct Annotated {
    pub entry_module_source: ModuleSource,
    pub modules: IndexMap<ModuleSource, Module>,
    pub symbols: SymbolTable,
    pub types: TypeTable,
    pub(crate) state: AnnotateState,
    /// Use→def map populated by the real resolver as it walks function
    /// bodies in `lower_tir_from_state`. Maps `(module, IdentExpr.id)` to
    /// the binding's defining `SymbolKey`.
    pub(crate) references: IndexMap<SymbolKey, SymbolKey>,
    /// Local binding [`Symbol`] entries (let / param / closure param)
    /// emitted by the resolver alongside `references`. Keyed by the
    /// binding's defining [`SymbolKey`]; consulted by
    /// [`Annotated::symbol_at`] when the key does not name an item-level
    /// symbol.
    pub(crate) locals: IndexMap<SymbolKey, Symbol>,
    /// TIR modules produced by [`crate::resolver::Resolver::lower_tir_from_state`].
    /// The batch compiler consumes these directly; LSP queries ignore them.
    pub(crate) tir_modules: IndexMap<ModuleSource, TirModule>,
}

/// A definition location, assembled from a [`SymbolKey`].
///
/// Returned by [`Annotated::definition_of`]. The `uri` is derived from
/// `ModuleSource::diagnostic_filename` — it is present for user-authored
/// modules (entry point, local files) and absent for stdlib / builtin
/// sources that have no on-disk URI.
pub struct Definition {
    pub module: ModuleSource,
    pub ast_id: AstId,
    pub span: Option<Span>,
    pub uri: Option<String>,
}

impl Annotated {
    /// Innermost AST node containing the given `(line, column)` in `module`.
    ///
    /// Returns `None` if the module is unknown or no node covers the position.
    #[must_use]
    pub fn ast_id_at(&self, module: &ModuleSource, line: usize, column: usize) -> Option<AstId> {
        self.modules.get(module)?.ast_id_at(line, column)
    }

    /// Symbol for the given key, or `None` if the key does not refer to a
    /// declared symbol. Falls back to the synthetic local table when the key
    /// names a `let` / parameter binding rather than an item.
    #[must_use]
    pub fn symbol_at(&self, key: &SymbolKey) -> Option<&Symbol> {
        self.symbols.get(key).or_else(|| self.locals.get(key))
    }

    /// Resolve a use-site `SymbolKey` (typically an [`IdentExpr`] id) to the
    /// `SymbolKey` of its defining binding. Returns `None` if the key does
    /// not appear in the reference map — in which case the caller should
    /// fall back to name-based lookup via the symbol table.
    #[must_use]
    pub fn referenced_symbol(&self, key: &SymbolKey) -> Option<SymbolKey> {
        self.references.get(key).cloned()
    }

    /// Iterate every recorded use-site `(use_key, def_key)` edge.
    ///
    /// Each `use_key` is typically an [`IdentExpr`] id; `def_key` is the
    /// binding's defining [`SymbolKey`]. Use sites of locals, parameters,
    /// item-level definitions (functions, types, globals) and imported items
    /// are all recorded here.
    pub fn iter_references(&self) -> impl Iterator<Item = (&SymbolKey, &SymbolKey)> {
        self.references.iter()
    }

    /// Find every use-site `SymbolKey` whose definition is `def_key`.
    ///
    /// Walks [`Self::iter_references`] and collects matches. The returned keys
    /// can be passed to [`Self::span_of_key`] for source ranges. The defining
    /// occurrence itself is **not** included — callers that want it should add
    /// it via [`Self::name_span_of`] / [`Self::span_of_key`].
    #[must_use]
    pub fn references_to(&self, def_key: &SymbolKey) -> Vec<SymbolKey> {
        self.iter_references()
            .filter(|&(_use_key, target)| target == def_key)
            .map(|(use_key, _target)| use_key.clone())
            .collect()
    }

    /// Resolved type for the declaring symbol at `key`, if `key` refers to a
    /// type-declaring AST node (struct, enum, variant, flags, newtype,
    /// resource).
    #[must_use]
    pub fn type_at(&self, key: &SymbolKey) -> Option<&ResolvedType> {
        let type_id = self.types.type_of_symbol(key)?;
        Some(self.types.get(type_id))
    }

    /// URI (filename) of a module, when the module has one.
    ///
    /// Built-in and stdlib modules have no on-disk URI and return `None`.
    #[must_use]
    pub fn uri_of(&self, module: &ModuleSource) -> Option<String> {
        let uri = module.diagnostic_filename();
        if uri.is_empty() { None } else { Some(uri) }
    }

    /// Definition location of the symbol identified by `key`.
    ///
    /// Resolves the key to its [`Symbol`], then packages the declaring module,
    /// `AstId`, span, and URI into a [`Definition`]. Returns `None` if the
    /// key does not refer to a declared symbol.
    #[must_use]
    pub fn definition_of(&self, key: &SymbolKey) -> Option<Definition> {
        let sym = self.symbol_at(key)?;
        let defined_at = &sym.defined_at;
        Some(Definition {
            module: defined_at.module.clone(),
            ast_id: defined_at.ast_id,
            span: sym.span,
            uri: self.uri_of(&defined_at.module),
        })
    }

    /// Span of the AST node identified by `key` — the source range of the
    /// node itself, not the module's name span. Useful for computing hover /
    /// highlight ranges from use sites.
    #[must_use]
    pub fn span_of_key(&self, key: &SymbolKey) -> Option<Span> {
        self.modules.get(&key.module)?.span_of_ast_id(key.ast_id)
    }

    /// Span of the defining identifier for the symbol at `key`.
    ///
    /// The `Symbol::span` field covers the whole declaring item — e.g. the
    /// entire `fn foo() { ... }` block. LSP go-to-definition wants the
    /// identifier alone (`foo`), which is carried on the AST item as
    /// `name_span`. This walks the defining module's items, finds the one
    /// with matching `AstId`, and returns its `name_span` when the item has
    /// one. Returns `None` for items that do not expose a name span (e.g.
    /// anonymous `impl` blocks, tests).
    #[must_use]
    pub fn name_span_of(&self, key: &SymbolKey) -> Option<Span> {
        let module = self.modules.get(&key.module)?;
        name_span_in_module(module, key.ast_id)
    }
}

fn name_span_in_module(module: &Module, ast_id: AstId) -> Option<Span> {
    for item in &module.items {
        if let Some(span) = name_span_of_item(item, ast_id) {
            return Some(span);
        }
        if let Some(span) = name_span_of_binding_in_item(item, ast_id) {
            return Some(span);
        }
    }
    None
}

fn name_span_of_binding_in_item(item: &Item, target: AstId) -> Option<Span> {
    use crate::ast::{Block, Expr, MatchExpr, Pattern, Stmt};

    fn scan_pattern(pattern: &Pattern, target: AstId) -> Option<Span> {
        match pattern {
            Pattern::Ident { id, span, .. } | Pattern::MutIdent { id, span, .. } => {
                if *id == target { Some(*span) } else { None }
            }
            Pattern::Tuple(ps, _) | Pattern::Or(ps) => {
                ps.iter().find_map(|p| scan_pattern(p, target))
            }
            Pattern::Struct { fields, .. } => {
                fields.iter().find_map(|f| scan_pattern(&f.pattern, target))
            }
            Pattern::Variant { bindings, .. } => {
                bindings.iter().find_map(|p| scan_pattern(p, target))
            }
            _ => None,
        }
    }

    fn scan_block(block: &Block, target: AstId) -> Option<Span> {
        for stmt in &block.stmts {
            if let Some(span) = scan_stmt(stmt, target) {
                return Some(span);
            }
        }
        None
    }

    fn scan_stmt(stmt: &Stmt, target: AstId) -> Option<Span> {
        match stmt {
            Stmt::Let(s) => {
                if let Some(span) = scan_pattern(&s.pattern, target) {
                    return Some(span);
                }
                if let Some(v) = &s.value
                    && let Some(span) = scan_expr(v, target)
                {
                    return Some(span);
                }
            }
            Stmt::Expr(s) => return scan_expr(&s.expr, target),
            Stmt::Return(s) => {
                if let Some(v) = &s.value {
                    return scan_expr(v, target);
                }
            }
            Stmt::TaskReturn(s) => return scan_expr(&s.value, target),
            Stmt::If(s) => {
                if let Some(span) = scan_condition(&s.condition, target) {
                    return Some(span);
                }
                if let Some(span) = scan_block(&s.then_block, target) {
                    return Some(span);
                }
                if let Some(eb) = &s.else_block {
                    return scan_block(eb, target);
                }
            }
            Stmt::While(s) => {
                if let Some(span) = scan_condition(&s.condition, target) {
                    return Some(span);
                }
                return scan_block(&s.body, target);
            }
            Stmt::For(s) => {
                if let Some(init) = &s.init
                    && let Some(span) = scan_stmt(init, target)
                {
                    return Some(span);
                }
                if let Some(cond) = &s.condition
                    && let Some(span) = scan_condition(cond, target)
                {
                    return Some(span);
                }
                if let Some(u) = &s.update
                    && let Some(span) = scan_expr(u, target)
                {
                    return Some(span);
                }
                return scan_block(&s.body, target);
            }
            Stmt::ForOf(s) => {
                if let Some(span) = scan_pattern(&s.binding, target) {
                    return Some(span);
                }
                if let Some(span) = scan_expr(&s.iterable, target) {
                    return Some(span);
                }
                return scan_block(&s.body, target);
            }
            Stmt::Loop(s) => return scan_block(&s.body, target),
            Stmt::Match(m) => return scan_match(m, target),
            Stmt::Break(s) => {
                if let Some(v) = &s.value {
                    return scan_expr(v, target);
                }
            }
            Stmt::Continue(_) => {}
            Stmt::Assert(s) => {
                if let Some(span) = scan_expr(&s.condition, target) {
                    return Some(span);
                }
                if let Some(msg) = &s.message {
                    return scan_expr(msg, target);
                }
            }
            Stmt::LabeledBlock(s) => return scan_block(&s.block, target),
        }
        None
    }

    fn scan_condition(cond: &crate::ast::Condition, target: AstId) -> Option<Span> {
        use crate::ast::{Condition, ConditionElement};
        match cond {
            Condition::Expr(e) => scan_expr(e, target),
            Condition::LetChain { elements, .. } => {
                for el in elements {
                    match el {
                        ConditionElement::Let { pattern, expr, .. } => {
                            if let Some(span) = scan_pattern(pattern, target) {
                                return Some(span);
                            }
                            if let Some(span) = scan_expr(expr, target) {
                                return Some(span);
                            }
                        }
                        ConditionElement::Expr(e) => {
                            if let Some(span) = scan_expr(e, target) {
                                return Some(span);
                            }
                        }
                    }
                }
                None
            }
        }
    }

    fn scan_match(m: &MatchExpr, target: AstId) -> Option<Span> {
        if let Some(span) = scan_expr(&m.expr, target) {
            return Some(span);
        }
        for arm in &m.arms {
            if let Some(span) = scan_pattern(&arm.pattern, target) {
                return Some(span);
            }
            if let Some(guard) = &arm.guard
                && let Some(span) = scan_expr(guard, target)
            {
                return Some(span);
            }
            if let Some(span) = scan_expr(&arm.body, target) {
                return Some(span);
            }
        }
        None
    }

    fn scan_expr(expr: &Expr, target: AstId) -> Option<Span> {
        match expr {
            Expr::Ident(_) | Expr::Literal(_) => None,
            Expr::Binary(e) => scan_expr(&e.left, target).or_else(|| scan_expr(&e.right, target)),
            Expr::Unary(e) => scan_expr(&e.expr, target),
            Expr::Assign(e) => scan_expr(&e.target, target).or_else(|| scan_expr(&e.value, target)),
            Expr::CompoundAssign(e) => {
                scan_expr(&e.target, target).or_else(|| scan_expr(&e.value, target))
            }
            Expr::ComparisonChain(e) => {
                if let Some(span) = scan_expr(&e.first, target) {
                    return Some(span);
                }
                for c in &e.comparisons {
                    if let Some(span) = scan_expr(&c.right, target) {
                        return Some(span);
                    }
                }
                None
            }
            Expr::Call(e) => {
                if let Some(span) = scan_expr(&e.callee, target) {
                    return Some(span);
                }
                for a in &e.args {
                    if let Some(span) = scan_expr(a, target) {
                        return Some(span);
                    }
                }
                None
            }
            Expr::MethodCall(e) => {
                if let Some(span) = scan_expr(&e.receiver, target) {
                    return Some(span);
                }
                for a in &e.args {
                    if let Some(span) = scan_expr(a, target) {
                        return Some(span);
                    }
                }
                None
            }
            Expr::StaticMethodCall(e) => {
                for a in &e.args {
                    if let Some(span) = scan_expr(a, target) {
                        return Some(span);
                    }
                }
                None
            }
            Expr::FieldAccess(e) => scan_expr(&e.expr, target),
            Expr::Index(e) => scan_expr(&e.expr, target).or_else(|| scan_expr(&e.index, target)),
            Expr::Block(b) => scan_block(b, target),
            Expr::If(e) => {
                if let Some(span) = scan_condition(&e.condition, target) {
                    return Some(span);
                }
                if let Some(span) = scan_block(&e.then_block, target) {
                    return Some(span);
                }
                if let Some(eb) = &e.else_block {
                    return scan_block(eb, target);
                }
                None
            }
            Expr::Match(m) => scan_match(m, target),
            Expr::Matches(m) => {
                if let Some(span) = scan_expr(&m.expr, target) {
                    return Some(span);
                }
                if let Some(span) = scan_pattern(&m.pattern, target) {
                    return Some(span);
                }
                if let Some(g) = &m.guard {
                    return scan_expr(g, target);
                }
                None
            }
            Expr::Closure(c) => {
                for p in &c.params {
                    if p.id == target {
                        return Some(p.name_span);
                    }
                }
                scan_expr(&c.body, target)
            }
            Expr::TemplateString(t) => {
                for part in &t.parts {
                    if let crate::ast::TemplatePart::Interpolation { expr, .. } = part
                        && let Some(span) = scan_expr(expr, target)
                    {
                        return Some(span);
                    }
                }
                None
            }
            Expr::Cast(c) => scan_expr(&c.expr, target),
            Expr::StructLiteral(s) => {
                for field in &s.fields {
                    if let Some(span) = scan_expr(&field.value, target) {
                        return Some(span);
                    }
                }
                None
            }
            Expr::TupleLiteral(t) => {
                for el in &t.elements {
                    if let Some(span) = scan_expr(el, target) {
                        return Some(span);
                    }
                }
                None
            }
            Expr::LabeledBlock(lb) => scan_block(&lb.block, target),
            Expr::TryOp(t) => scan_expr(&t.expr, target),
            Expr::Spread(inner, _) => scan_expr(inner, target),
            Expr::Range(r) => scan_expr(&r.start, target).or_else(|| scan_expr(&r.end, target)),
            Expr::WithHandler(w) => {
                for binding in &w.handlers {
                    if binding.id == target
                        && let Some(effect) = &binding.effect
                    {
                        return Some(effect.span());
                    }
                    if let Some(span) = scan_expr(&binding.handler, target) {
                        return Some(span);
                    }
                }
                scan_block(&w.body, target)
            }
            Expr::Resume(r) => scan_expr(&r.value, target),
        }
    }

    match item {
        Item::Function(f) => {
            for p in &f.params {
                if p.id == target {
                    return Some(p.name_span);
                }
            }
            if let Some(body) = &f.body {
                return scan_block(body, target);
            }
            None
        }
        Item::Impl(imp) => {
            for m in &imp.methods {
                for p in &m.params {
                    if p.id == target {
                        return Some(p.name_span);
                    }
                }
                if let Some(body) = &m.body
                    && let Some(span) = scan_block(body, target)
                {
                    return Some(span);
                }
            }
            None
        }
        Item::Trait(t) => {
            for m in &t.methods {
                for p in &m.params {
                    if p.id == target {
                        return Some(p.name_span);
                    }
                }
                if let Some(body) = &m.body
                    && let Some(span) = scan_block(body, target)
                {
                    return Some(span);
                }
            }
            None
        }
        Item::Test(t) => scan_block(&t.body, target),
        Item::Global(g) => scan_expr(&g.initializer, target),
        _ => None,
    }
}

fn name_span_of_item(item: &Item, target: AstId) -> Option<Span> {
    match item {
        Item::Function(f) => {
            if f.id == target {
                return Some(f.name_span);
            }
            for tp in &f.type_params {
                if tp.id == target {
                    return Some(tp.name_span);
                }
            }
        }
        Item::Struct(s) => {
            if s.id == target {
                return Some(s.name_span);
            }
            for field in &s.fields {
                if field.id == target {
                    return Some(field.name_span);
                }
            }
            for tp in &s.type_params {
                if tp.id == target {
                    return Some(tp.name_span);
                }
            }
        }
        Item::Enum(e) => {
            if e.id == target {
                return Some(e.name_span);
            }
            for case in &e.cases {
                if case.id == target {
                    return Some(case.name_span);
                }
            }
        }
        Item::Variant(v) => {
            if v.id == target {
                return Some(v.name_span);
            }
            for case in &v.cases {
                if case.id == target {
                    return Some(case.name_span);
                }
            }
            for tp in &v.type_params {
                if tp.id == target {
                    return Some(tp.name_span);
                }
            }
        }
        Item::Flags(f) => {
            if f.id == target {
                return Some(f.name_span);
            }
            for flag in &f.flags {
                if flag.id == target {
                    return Some(flag.name_span);
                }
            }
        }
        Item::Trait(t) => {
            if t.id == target {
                return Some(t.name_span);
            }
            for method in &t.methods {
                if method.id == target {
                    return Some(method.name_span);
                }
                for tp in &method.type_params {
                    if tp.id == target {
                        return Some(tp.name_span);
                    }
                }
            }
            for tp in &t.type_params {
                if tp.id == target {
                    return Some(tp.name_span);
                }
            }
        }
        Item::Newtype(n) => {
            if n.id == target {
                return Some(n.name_span);
            }
            for tp in &n.type_params {
                if tp.id == target {
                    return Some(tp.name_span);
                }
            }
        }
        Item::Effect(e) => {
            if e.id == target {
                return Some(e.name_span);
            }
            for method in &e.methods {
                if method.id == target {
                    return Some(method.name_span);
                }
            }
        }
        Item::Resource(r) => {
            if r.id == target {
                // ResourceDecl has no dedicated name_span; fall back to the whole-decl span.
                return Some(r.span);
            }
            for method in &r.methods {
                if method.id == target {
                    return Some(method.name_span);
                }
            }
        }
        Item::Global(g) => {
            if g.id == target {
                return Some(g.name_span);
            }
        }
        Item::Impl(imp) => {
            for method in &imp.methods {
                if method.id == target {
                    return Some(method.name_span);
                }
                for tp in &method.type_params {
                    if tp.id == target {
                        return Some(tp.name_span);
                    }
                }
            }
            for tp in &imp.type_params {
                if tp.id == target {
                    return Some(tp.name_span);
                }
            }
        }
        Item::World(_) | Item::Use(_) | Item::TupleTypeDecl(_) | Item::Test(_) => {}
    }
    None
}

/// Run parse → bind → desugar → load → analyze → resolve on `source` and
/// return the resulting [`Annotated`].
///
/// Failures emit diagnostics to the host's logger and return [`Bail`] — the
/// same error channel `compile_with_options` uses.
pub async fn annotate<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
) -> Result<Annotated, Bail> {
    annotate_with_invocations(source, host, filename, crate::kiln::InvocationIndex::new()).await
}

/// Variant of [`annotate`] that seeds the loader with a Kiln
/// [`crate::kiln::InvocationIndex`] so bare `use { X } from "<schema>"`
/// clauses can pick up generator-produced entry modules.
pub async fn annotate_with_invocations<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
    invocations: crate::kiln::InvocationIndex,
) -> Result<Annotated, Bail> {
    let logger = Logger::new(host, LogLevel::default());
    if let Some(f) = filename {
        logger.set_file(f);
    }
    annotate_with_logger_invocations(source, host, filename, &logger, invocations).await
}

async fn annotate_with_logger_invocations<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
    logger: &Logger<'_, H>,
    invocations: crate::kiln::InvocationIndex,
) -> Result<Annotated, Bail> {
    let load_result = {
        let module_loader =
            loader::ModuleLoader::new(host, LogLevel::default()).with_invocations(invocations);
        module_loader
            .load_all(source, filename)
            .await
            .map_err(|e| {
                let _ = logger.error(e);
                Bail
            })?
    };

    annotate_loaded(load_result, logger)
}

/// Run analyze + resolve on a pre-loaded module set and return the resulting
/// [`Annotated`]. Used by `compile_with_options` which loads modules once and
/// also needs to inspect the entry AST for `#![TODO]` detection.
pub(crate) fn annotate_loaded<H: CompilerHost>(
    load_result: loader::LoadResult,
    logger: &Logger<'_, H>,
) -> Result<Annotated, Bail> {
    let symbols = {
        let _span = logger.span("analyze");
        let mut analyzer = Analyzer::new(logger).with_invocations(load_result.invocations.clone());
        analyzer.analyze_loaded_modules(
            &load_result.modules,
            &load_result.entry_module_source,
            load_result.implicit_modules.clone(),
        )?;
        analyzer.into_symbols()
    };

    let state = {
        let _span = logger.span("resolve/annotate");
        Resolver::annotate_modules(
            &symbols,
            &load_result.modules,
            &load_result.entry_module_source,
            logger,
            load_result.invocations.clone(),
        )?
    };

    // Run the full body-level resolve pass so `state.references` and
    // `state.local_symbols` are populated by the real resolver. This is the
    // single source of truth for use→def edges — LSP and batch compilation
    // both consume what the resolver recorded here, with no separate lexical
    // re-scan to drift out of sync.
    let tir_modules = {
        let _span = logger.span("resolve/lower_tir");
        Resolver::lower_tir_from_state(
            &state,
            &symbols,
            &load_result.modules,
            load_result.entry_module_source.clone(),
            logger,
            &load_result.included_files,
        )?
    };

    // Take an immutable snapshot of the type table at the end of lowering.
    // LSP queries read this snapshot; any further lowering (none today) would
    // continue interning into the shared `Rc<RefCell<TypeTable>>` held by
    // `state.type_table`.
    let types = state.type_table.borrow().clone();

    // Drain the resolver's shared reference / local maps into owned IndexMaps
    // so LSP queries can hand out `&Symbol` references without juggling
    // `RefCell` borrows.
    let references = std::mem::take(&mut *state.references.borrow_mut());
    let locals = std::mem::take(&mut *state.local_symbols.borrow_mut());

    Ok(Annotated {
        entry_module_source: load_result.entry_module_source,
        modules: load_result.modules,
        symbols,
        types,
        state,
        references,
        locals,
        tir_modules,
    })
}
