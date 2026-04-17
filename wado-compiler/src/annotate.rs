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
    }
    None
}

fn name_span_of_item(item: &Item, target: AstId) -> Option<Span> {
    match item {
        Item::Function(f) => {
            if f.id == target {
                return Some(f.name_span);
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
            }
        }
        Item::Newtype(n) => {
            if n.id == target {
                return Some(n.name_span);
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
