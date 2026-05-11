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
use crate::ast::{AstId, Module};
use crate::ast_index::AstIndex;
use crate::compiler_host::{CompilerHost, LogLevel};
use crate::hashmap::IndexMap;
use crate::loader;
use crate::logger::{Bail, Logger};
use crate::module_source::{ModuleSource, ModuleSourceInterner};
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
    /// `ModuleSource` interner shared with the analyze + resolve phases.
    /// LSP queries (definition / hover / references) borrow this when
    /// they need to resolve an import path the user clicked into a
    /// `ModuleSource`.
    ///
    /// Re-entrancy: only single-threaded callers, and only one
    /// `borrow_mut` at a time. Do not hold a [`std::cell::RefMut`]
    /// across calls into other [`Annotated`] / [`crate::Resolver`]
    /// methods — a nested `borrow_mut` will panic. The intended
    /// pattern is `annotated.interner.borrow_mut().<one method call>`,
    /// dropping the borrow at the statement boundary.
    pub interner: std::rc::Rc<std::cell::RefCell<ModuleSourceInterner>>,
    /// Per-module structural index (name spans, write targets, span lookup).
    /// Built once per [`Module`] in [`annotate_loaded`]. LSP queries (and
    /// the in-tree [`name_span_of`] / [`span_of_key`] helpers) consult this
    /// instead of re-walking the AST on every request.
    pub(crate) ast_indices: IndexMap<ModuleSource, AstIndex>,
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
    /// Answered from the per-module [`AstIndex`](crate::ast_index::AstIndex);
    /// no AST traversal happens at query time.
    #[must_use]
    pub fn ast_id_at(&self, module: &ModuleSource, line: usize, column: usize) -> Option<AstId> {
        self.ast_indices.get(module)?.ast_id_at(line, column)
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
    /// highlight ranges from use sites. Answered from
    /// [`AstIndex::span_of`](crate::ast_index::AstIndex::span_of), which is
    /// total over every parser-allocated [`AstId`].
    #[must_use]
    pub fn span_of_key(&self, key: &SymbolKey) -> Option<Span> {
        self.ast_indices.get(&key.module)?.span_of(key.ast_id)
    }

    /// Span of the defining identifier for the symbol at `key`.
    ///
    /// The `Symbol::span` field covers the whole declaring item — e.g. the
    /// entire `fn foo() { ... }` block. LSP go-to-definition wants the
    /// identifier alone (`foo`), which is carried on the AST item as
    /// `name_span`. The lookup goes through the per-module
    /// [`AstIndex`](crate::ast_index::AstIndex), which is populated for every
    /// declaration node that exposes a `name_span` field. Returns `None` for
    /// nodes without a dedicated name span (e.g. anonymous `impl` blocks,
    /// `Item::Resource`, tests).
    #[must_use]
    pub fn name_span_of(&self, key: &SymbolKey) -> Option<Span> {
        self.ast_indices.get(&key.module)?.name_span_of(key.ast_id)
    }

    /// True iff `key` names an `IdentExpr` that appears as the direct
    /// LHS of `=` or a compound-assign in its declaring module. Used by
    /// document-highlight to classify a use-site as Read vs. Write without
    /// re-walking the body.
    #[must_use]
    pub fn is_write_target(&self, key: &SymbolKey) -> bool {
        self.ast_indices
            .get(&key.module)
            .is_some_and(|idx| idx.is_write_target(key.ast_id))
    }

    /// Resolve a 1-based `(line, column)` position to a [`Cursor`] over
    /// `module`. Returns `None` when the module is unknown or no id-bearing
    /// AST node covers the position.
    ///
    /// The returned cursor is *positional* — it always carries the AST id
    /// at the cursor, even if that id is not on a name. Use
    /// [`Cursor::def_key`] to filter to "cursor lands on a recognised
    /// name".
    #[must_use]
    pub fn cursor_at(
        &self,
        module: &ModuleSource,
        line: usize,
        column: usize,
    ) -> Option<Cursor<'_>> {
        let ast_id = self.ast_id_at(module, line, column)?;
        Some(Cursor {
            annotated: self,
            key: SymbolKey::new(module.clone(), ast_id),
        })
    }
}

/// A positional handle into [`Annotated`].
///
/// Captures the cursor's AST id and module so call sites can chain
/// `def_key()`, `def_symbol()`, `references_to_def()`, etc. instead of
/// threading the same `(annotated, module, line, col)` tuple through every
/// query helper. Constructed via [`Annotated::cursor_at`].
pub struct Cursor<'a> {
    annotated: &'a Annotated,
    key: SymbolKey,
}

impl<'a> Cursor<'a> {
    /// Module the cursor is positioned in.
    #[must_use]
    pub fn module(&self) -> &ModuleSource {
        &self.key.module
    }

    /// `(module, ast_id)` of the AST node at the cursor.
    #[must_use]
    pub fn key(&self) -> &SymbolKey {
        &self.key
    }

    /// Source span of the AST node at the cursor, if available.
    #[must_use]
    pub fn span(&self) -> Option<Span> {
        self.annotated.span_of_key(&self.key)
    }

    /// `SymbolKey` of the binding the cursor names, following the use→def
    /// edge when present. Returns `None` when the cursor does not land on a
    /// recognised name (e.g. on punctuation, on an expression body, on a
    /// numeric literal).
    #[must_use]
    pub fn def_key(&self) -> Option<SymbolKey> {
        if let Some(def) = self.annotated.referenced_symbol(&self.key) {
            return Some(def);
        }
        if self.annotated.symbol_at(&self.key).is_some() {
            return Some(self.key.clone());
        }
        None
    }

    /// Symbol named by the cursor, after chasing the use→def edge.
    #[must_use]
    pub fn def_symbol(&self) -> Option<&'a Symbol> {
        let def_key = self.def_key()?;
        self.annotated.symbol_at(&def_key)
    }

    /// Identifier-only span of the binding the cursor names (the
    /// `name_span` of the declaration). Returns `None` if the cursor does
    /// not name a known binding or the binding has no narrow name span
    /// (e.g. anonymous `impl` blocks).
    #[must_use]
    pub fn def_name_span(&self) -> Option<Span> {
        let def_key = self.def_key()?;
        self.annotated.name_span_of(&def_key)
    }

    /// Best span at the binding's declaration site, falling back from the
    /// narrow `name_span` to the symbol's declared span to the AST node's
    /// span. Returns `None` only when the cursor does not name a known
    /// binding.
    ///
    /// `goto-definition` and `find-references` use this to highlight
    /// declarations that lack a dedicated `name_span` field
    /// (`Item::Resource`, anonymous `impl` blocks, tests).
    #[must_use]
    pub fn def_span(&self) -> Option<Span> {
        let def_key = self.def_key()?;
        self.annotated
            .name_span_of(&def_key)
            .or_else(|| self.annotated.symbol_at(&def_key).and_then(|s| s.span))
            .or_else(|| self.annotated.span_of_key(&def_key))
    }

    /// True iff the cursor lands on an `IdentExpr` that appears as the
    /// direct LHS of `=` or a compound assignment. Mirrors
    /// [`Annotated::is_write_target`] for the cursor's own key.
    #[must_use]
    pub fn is_write_target(&self) -> bool {
        self.annotated.is_write_target(&self.key)
    }

    /// Every use-site `SymbolKey` for the binding the cursor names.
    /// Returns an empty `Vec` when the cursor does not name a known binding.
    #[must_use]
    pub fn references_to_def(&self) -> Vec<SymbolKey> {
        match self.def_key() {
            Some(key) => self.annotated.references_to(&key),
            None => Vec::new(),
        }
    }
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
    // Wrap the loader's interner in `Rc<RefCell<>>` so analyze and the
    // per-module resolvers can each `borrow_mut()` it from `&self`
    // contexts. Single-threaded sharing matches the rest of the
    // compiler's `Rc<RefCell<TypeTable>>` plumbing.
    let interner = std::rc::Rc::new(std::cell::RefCell::new(load_result.interner));
    let symbols = {
        let _span = logger.span("analyze");
        let mut analyzer = Analyzer::new(logger)
            .with_invocations(load_result.invocations.clone())
            .with_interner(interner.clone());
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
            interner.clone(),
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

    // Build per-module structural indices in one walk each. Cheap relative
    // to the resolve/lower pass and lets LSP `name_span_of` /
    // `is_write_target` answer in O(1).
    let ast_indices = {
        let _span = logger.span("ast_index");
        let mut indices: IndexMap<ModuleSource, AstIndex> = IndexMap::default();
        for (source, module) in &load_result.modules {
            indices.insert(source.clone(), AstIndex::build(module));
        }
        indices
    };

    Ok(Annotated {
        entry_module_source: load_result.entry_module_source,
        modules: load_result.modules,
        symbols,
        types,
        interner,
        ast_indices,
        state,
        references,
        locals,
        tir_modules,
    })
}
