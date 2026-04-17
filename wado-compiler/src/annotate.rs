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
use crate::compiler_host::{CompilerHost, LogLevel};
use crate::hashmap::IndexMap;
use crate::loader;
use crate::logger::{Bail, Logger};
use crate::name::ModuleSource;
use crate::resolver::Resolver;
use crate::symbol::{Symbol, SymbolKey, SymbolTable};
use crate::tir::{ResolvedType, TypeTable};
use crate::token::Span;

/// A ready-to-query analysis result.
///
/// `Annotated` owns every piece of semantic state produced by the analysis
/// pipeline. The AST modules are preserved verbatim (so positions,
/// [`AstId`]s, and spans resolve against the same tree the parser saw).
/// [`SymbolTable`] and [`TypeTable`] are owned (no shared `Rc<RefCell<…>>`),
/// so the value is freely cloneable and freely borrowable by LSP queries.
pub struct Annotated {
    pub entry_module_source: ModuleSource,
    pub modules: IndexMap<ModuleSource, Module>,
    pub symbols: SymbolTable,
    pub types: TypeTable,
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
    /// declared symbol.
    #[must_use]
    pub fn symbol_at(&self, key: &SymbolKey) -> Option<&Symbol> {
        self.symbols.get(key)
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
        let sym = self.symbols.get(key)?;
        let defined_at = &sym.defined_at;
        Some(Definition {
            module: defined_at.module.clone(),
            ast_id: defined_at.ast_id,
            span: sym.span,
            uri: self.uri_of(&defined_at.module),
        })
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
    let logger = Logger::new(host, LogLevel::default());
    if let Some(f) = filename {
        logger.set_file(f);
    }

    let load_result = {
        let module_loader = loader::ModuleLoader::new(host, LogLevel::default());
        module_loader.load_all(source, filename).await.map_err(|e| {
            let _ = logger.error(e);
            Bail
        })?
    };

    let symbols = {
        let _span = logger.span("analyze");
        let mut analyzer = Analyzer::new(&logger);
        analyzer.analyze_loaded_modules(
            &load_result.modules,
            &load_result.entry_module_source,
            load_result.implicit_modules.clone(),
        )?;
        analyzer.into_symbols()
    };

    let tir_modules = {
        let _span = logger.span("resolve");
        Resolver::resolve_all_modules(
            &symbols,
            &load_result.modules,
            load_result.entry_module_source.clone(),
            &logger,
            &load_result.included_files,
        )?
    };

    // TIR modules share a single Rc<RefCell<TypeTable>>. Clone its contents
    // out so `Annotated` owns the table directly — no interior mutability.
    let types = tir_modules
        .values()
        .next()
        .map(|m| m.type_table.borrow().clone())
        .unwrap_or_else(TypeTable::new);

    Ok(Annotated {
        entry_module_source: load_result.entry_module_source,
        modules: load_result.modules,
        symbols,
        types,
    })
}
