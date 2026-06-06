//! Semantic analyzer for Wado
//!
//! The analyzer performs:
//! 1. Symbol table construction from pre-loaded modules
//! 2. Import validation
//! 3. Name resolution (binding identifiers to their definitions)

use crate::ast::{Item, Module, UseDecl, UseItem};
use crate::compiler_host::CompilerHost;
use crate::loader::{resolve_wasm_asset_path, wasm_asset_kind_from_attrs};
use crate::logger::{Bail, Logger};
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name::validate_module_path;
use std::cell::RefCell;
use std::rc::Rc;

/// Resolve `use_decl.source` against `from`, recognising
/// `with { type: "wat" | "wasm" }` attributes and routing them to the
/// loader-synthesised wasm-asset module (`ModuleSource::Wasm`) instead
/// of the regular Wado module-source resolution.
///
/// Returns `None` when the wasm asset path itself is malformed (e.g.
/// `core:libm.wat` with no leading `./`); the caller emits the
/// downstream `InvalidModulePath` diagnostic.
fn resolve_use_decl_module_source(
    interner: &mut ModuleSourceInterner,
    from: &ModuleSource,
    use_decl: &UseDecl,
    entry: Option<&ModuleSource>,
    invocations: &crate::kiln::InvocationIndex,
) -> Option<ModuleSource> {
    if let Some(kind) = wasm_asset_kind_from_attrs(use_decl.attributes.as_ref()) {
        return resolve_wasm_asset_path(from, &use_decl.source)
            .ok()
            .map(|path| interner.wasm(&path, kind));
    }
    Some(crate::name::resolve_import_with_invocations(
        interner,
        from,
        &use_decl.source,
        entry,
        invocations,
    ))
}

/// `true` when this `use` declares a wasm asset import
/// (`with { type: "wat" | "wasm" }`).
fn is_wasm_asset_use_decl(use_decl: &UseDecl) -> bool {
    wasm_asset_kind_from_attrs(use_decl.attributes.as_ref()).is_some()
}
use crate::symbol::{
    EffectSymbol, EnumSymbol, FlagsSymbol, FunctionSymbol, GlobalSymbol, NewtypeSymbol,
    ResourceSymbol, StructSymbol, Symbol, SymbolKey, SymbolKind, SymbolTable, TraitSymbol,
    VariantSymbol, WorldExportSymbol, WorldImportSymbol, WorldSymbol,
};
use crate::token::Span;

/// Error that can occur during analysis
#[derive(Debug, Clone)]
pub enum AnalyzeError {
    /// Module not found (not in pre-loaded modules)
    ModuleNotFound {
        module_source: ModuleSource,
        span: Span,
    },
    /// Symbol not found in module
    ImportNotFound {
        module_source: ModuleSource,
        name: String,
        span: Span,
    },
    /// Duplicate top-level definition within a single module.
    ///
    /// `span` is the location of the duplicate; `first` is the location of
    /// the original definition that the duplicate collides with.
    DuplicateDefinition {
        name: String,
        span: Span,
        first: Span,
    },
    /// Undefined symbol reference
    UndefinedSymbol { name: String, span: Span },
    /// Invalid module path (not a valid URI reference)
    InvalidModulePath {
        path: String,
        message: String,
        span: Span,
    },
    /// Type name collides with a prelude type
    PreludeTypeCollision { name: String, span: Span },
}

impl std::fmt::Display for AnalyzeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalyzeError::ModuleNotFound {
                module_source,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: module not found: '{}'",
                    span.line, span.column, module_source
                )
            }
            AnalyzeError::ImportNotFound {
                module_source,
                name,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: symbol '{}' not found in module '{}'",
                    span.line, span.column, name, module_source
                )
            }
            AnalyzeError::DuplicateDefinition { name, span, first } => {
                write!(
                    f,
                    "{}:{}: duplicate definition '{}' (first defined at {}:{})",
                    span.line, span.column, name, first.line, first.column
                )
            }
            AnalyzeError::UndefinedSymbol { name, span } => {
                write!(
                    f,
                    "{}:{}: undefined symbol '{}'",
                    span.line, span.column, name
                )
            }
            AnalyzeError::InvalidModulePath {
                path,
                message,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: invalid module path '{}': {}",
                    span.line, span.column, path, message
                )
            }
            AnalyzeError::PreludeTypeCollision { name, span } => {
                write!(
                    f,
                    "{}:{}: type '{}' conflicts with prelude type of the same name",
                    span.line, span.column, name
                )
            }
        }
    }
}

impl std::error::Error for AnalyzeError {}

impl From<AnalyzeError> for crate::compiler_host::Diagnostic {
    fn from(e: AnalyzeError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        let (code, message, span) = match &e {
            AnalyzeError::ModuleNotFound {
                module_source,
                span,
            } => (
                Code::ModuleNotFound,
                format!("module not found: '{module_source}'"),
                *span,
            ),
            AnalyzeError::ImportNotFound {
                module_source,
                name,
                span,
            } => (
                Code::UndefinedVariable,
                format!("symbol '{name}' not found in module '{module_source}'"),
                *span,
            ),
            AnalyzeError::DuplicateDefinition { name, span, first } => (
                Code::DuplicateDefinition,
                format!(
                    "duplicate definition '{name}' (first defined at {}:{})",
                    first.line, first.column
                ),
                *span,
            ),
            AnalyzeError::UndefinedSymbol { name, span } => (
                Code::UndefinedVariable,
                format!("undefined symbol '{name}'"),
                *span,
            ),
            AnalyzeError::InvalidModulePath {
                path,
                message,
                span,
            } => (
                Code::ModuleNotFound,
                format!("invalid module path '{path}': {message}"),
                *span,
            ),
            AnalyzeError::PreludeTypeCollision { name, span } => (
                Code::DuplicateDefinition,
                format!("type '{name}' conflicts with prelude type of the same name"),
                *span,
            ),
        };
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code,
            message,
            span: Some(DiagnosticSpan::from_span(&span, None)),
        }
    }
}

/// Semantic analyzer
///
/// Builds a symbol table from pre-loaded modules and validates imports.
pub struct Analyzer<'a, H: CompilerHost> {
    /// The symbol table being built
    pub symbols: SymbolTable,
    /// Logger for emitting diagnostics
    logger: &'a Logger<'a, H>,
    /// Modules loaded implicitly by the compiler (not by user imports)
    implicit_modules: crate::hashmap::IndexSet<ModuleSource>,
    entry_module_source: ModuleSource,
    /// Kiln invocation redirects consulted by the import-resolution paths
    /// (`validate_imports`, re-export registration). Empty when the
    /// compilation did not run the Kiln pipeline.
    invocations: crate::kiln::InvocationIndex,
    /// `ModuleSource` interner shared with the loader. Forwarded to
    /// [`resolve_use_decl_module_source`] so analyze-phase imports get
    /// canonicalized identities.
    interner: Rc<RefCell<ModuleSourceInterner>>,
}

impl<'a, H: CompilerHost> Analyzer<'a, H> {
    /// Create a new analyzer
    pub fn new(logger: &'a Logger<'a, H>) -> Self {
        Self {
            symbols: SymbolTable::new(),
            logger,
            implicit_modules: crate::hashmap::IndexSet::default(),
            entry_module_source: ModuleSource::entry_point_uninitialized(),
            invocations: crate::kiln::InvocationIndex::new(),
            interner: Rc::new(RefCell::new(ModuleSourceInterner::new())),
        }
    }

    /// Seed the analyzer with the loader's interner so import-site
    /// resolution canonicalizes module identities consistently.
    #[must_use]
    pub fn with_interner(mut self, interner: Rc<RefCell<ModuleSourceInterner>>) -> Self {
        self.interner = interner;
        self
    }

    /// Seed the analyzer with a Kiln invocation index. Call before
    /// [`Self::analyze_loaded_modules`] so import-site redirects line up
    /// with the loader's redirects.
    #[must_use]
    pub fn with_invocations(mut self, invocations: crate::kiln::InvocationIndex) -> Self {
        self.invocations = invocations;
        self
    }

    /// Define a top-level symbol, emitting a `DuplicateDefinition` diagnostic
    /// if `name` is already defined directly in `module_source`.
    ///
    /// Duplicates are silently dropped (first-def wins).  Downstream phases
    /// rely on a single registration per `(module, name)`; this matches
    /// codegen's long-standing "first definition wins, skip the rest"
    /// behaviour and makes the error surface before those phases can
    /// miscompile against a different definition.
    fn define_unique(
        &mut self,
        module_source: &ModuleSource,
        ast_id: crate::ast::AstId,
        name: &str,
        kind: SymbolKind,
        span: Span,
    ) -> Option<SymbolKey> {
        if let Some(first) = self.symbols.defined_span_in_module(module_source, name) {
            let _ = self.logger.error(AnalyzeError::DuplicateDefinition {
                name: name.to_string(),
                span,
                first,
            });
            return None;
        }
        Some(
            self.symbols
                .define(module_source, ast_id, name, kind, Some(span)),
        )
    }

    /// Collect all definitions from a module into the symbol table
    fn collect_definitions(&mut self, module: &Module, module_source: &ModuleSource) {
        self.logger.set_file(module_source.diagnostic_filename());
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    // A function is a builtin if:
                    // 1. It has no body (bodyless declaration like `pub fn foo();`)
                    // 2. Or it's defined in a core::* module
                    let is_builtin =
                        func.body.is_none() || matches!(module_source, ModuleSource::Core { .. });

                    let kind = SymbolKind::Function(FunctionSymbol {
                        params: func.params.iter().map(|p| p.name.clone()).collect(),
                        return_type: func.return_type.as_ref().map(|_| "unknown".to_string()),
                        effects: func.effects.clone(),
                        is_builtin,
                        cm_import: func.attrs.first().and_then(|a| a.as_cm_import().cloned()),
                    });

                    self.define_unique(module_source, func.id, &func.name, kind, func.span);
                }

                Item::Interface(effect) => {
                    // Extract effect-level CM import from attributes
                    let effect_cm_import =
                        effect.attrs.first().and_then(|a| a.as_cm_import().cloned());

                    let kind = SymbolKind::Effect(EffectSymbol {
                        methods: effect.methods.iter().map(|m| m.name.clone()).collect(),
                        cm_import: effect_cm_import,
                    });

                    self.define_unique(module_source, effect.id, &effect.name, kind, effect.span);

                    // Also register each effect method as a function symbol
                    // with the fully qualified name "{Effect}.{method}"
                    // This allows importing them via use statements
                    for method in &effect.methods {
                        let cm_import =
                            method.attrs.first().and_then(|a| a.as_cm_import().cloned());

                        let func_kind = SymbolKind::Function(FunctionSymbol {
                            params: method.params.iter().map(|p| p.name.clone()).collect(),
                            return_type: method.return_type.as_ref().map(|_| "unknown".to_string()),
                            effects: vec![effect.name.clone()], // Effect methods implicitly require their effect
                            is_builtin: matches!(module_source, ModuleSource::Core { .. }),
                            cm_import,
                        });

                        // Register as "{Effect}::{method}"
                        let qualified_name = format!("{}::{}", effect.name, method.name);
                        self.define_unique(
                            module_source,
                            method.id,
                            &qualified_name,
                            func_kind,
                            method.span,
                        );
                    }
                }

                Item::Struct(struct_decl) => {
                    let kind = SymbolKind::Struct(StructSymbol {
                        fields: struct_decl.fields.iter().map(|f| f.name.clone()).collect(),
                    });

                    self.define_unique(
                        module_source,
                        struct_decl.id,
                        &struct_decl.name,
                        kind,
                        struct_decl.span,
                    );
                }

                Item::Enum(enum_decl) => {
                    let kind = SymbolKind::Enum(EnumSymbol {
                        cases: enum_decl.cases.iter().map(|c| c.name.clone()).collect(),
                    });

                    self.define_unique(
                        module_source,
                        enum_decl.id,
                        &enum_decl.name,
                        kind,
                        enum_decl.span,
                    );
                }

                Item::Variant(variant_decl) => {
                    let kind = SymbolKind::Variant(VariantSymbol {
                        cases: variant_decl.cases.iter().map(|c| c.name.clone()).collect(),
                    });

                    self.define_unique(
                        module_source,
                        variant_decl.id,
                        &variant_decl.name,
                        kind,
                        variant_decl.span,
                    );
                }

                Item::Trait(trait_decl) => {
                    let kind = SymbolKind::Trait(TraitSymbol {
                        methods: trait_decl.methods.iter().map(|m| m.name.clone()).collect(),
                        type_params: trait_decl
                            .type_params
                            .iter()
                            .map(|p| p.name.clone())
                            .collect(),
                    });

                    self.define_unique(
                        module_source,
                        trait_decl.id,
                        &trait_decl.name,
                        kind,
                        trait_decl.span,
                    );
                }

                Item::Newtype(newtype) => {
                    let mut aliased_type = String::new();
                    crate::unparse::unparse_type_into(&newtype.ty, &mut aliased_type);
                    let kind = SymbolKind::Newtype(NewtypeSymbol { aliased_type });

                    self.define_unique(
                        module_source,
                        newtype.id,
                        &newtype.name,
                        kind,
                        newtype.span,
                    );
                }

                Item::Resource(resource) => {
                    let kind = SymbolKind::Resource(ResourceSymbol {
                        methods: resource.methods.iter().map(|m| m.name.clone()).collect(),
                        cm_import: resource
                            .attrs
                            .first()
                            .and_then(|a| a.as_cm_import().cloned()),
                    });

                    self.define_unique(
                        module_source,
                        resource.id,
                        &resource.name,
                        kind,
                        resource.span,
                    );

                    // Register each resource method as a function symbol
                    // with the fully qualified name "{Resource}::{method}"
                    for method in &resource.methods {
                        let cm_import =
                            method.attrs.first().and_then(|a| a.as_cm_import().cloned());

                        let func_kind = SymbolKind::Function(FunctionSymbol {
                            params: method.params.iter().map(|p| p.name.clone()).collect(),
                            return_type: method.return_type.as_ref().map(|_| "unknown".to_string()),
                            effects: vec![], // Resource methods don't have effect requirements
                            is_builtin: matches!(module_source, ModuleSource::Core { .. }),
                            cm_import,
                        });

                        // Register as "{Resource}::{method}"
                        let qualified_name = format!("{}::{}", resource.name, method.name);
                        self.define_unique(
                            module_source,
                            method.id,
                            &qualified_name,
                            func_kind,
                            method.span,
                        );
                    }
                }

                Item::World(world) => {
                    let kind = SymbolKind::World(WorldSymbol {
                        imports: world
                            .imports
                            .iter()
                            .map(|i| WorldImportSymbol {
                                interface_name: i.interface_name.clone(),
                            })
                            .collect(),
                        exports: world
                            .exports
                            .iter()
                            .map(|e| match e {
                                crate::ast::WorldExport::Interface(iface) => {
                                    WorldExportSymbol::Interface {
                                        interface_name: iface.interface_name.clone(),
                                    }
                                }
                                crate::ast::WorldExport::Function(func) => {
                                    WorldExportSymbol::Function {
                                        name: func.name.clone(),
                                        is_async: func.is_async,
                                        params: func
                                            .params
                                            .iter()
                                            .map(|p| p.name.clone())
                                            .collect(),
                                        return_type: func
                                            .return_type
                                            .as_ref()
                                            .map(|_| "unknown".to_string()),
                                    }
                                }
                            })
                            .collect(),
                    });

                    self.define_unique(module_source, world.id, &world.name, kind, world.span);
                }

                Item::Use(_) => {
                    // Use declarations are handled in validate_imports
                }

                Item::Impl(_) => {
                    // Impl blocks are handled later
                }

                Item::Flags(flags_decl) => {
                    let kind = SymbolKind::Flags(FlagsSymbol {
                        members: flags_decl.flags.iter().map(|m| m.name.clone()).collect(),
                    });

                    self.define_unique(
                        module_source,
                        flags_decl.id,
                        &flags_decl.name,
                        kind,
                        flags_decl.span,
                    );
                }

                Item::Test(_) => {
                    // Test declarations are handled in the elaborator, not in the symbol table.
                    // Tests are converted to functions with generated names.
                }

                Item::Global(global) => {
                    let kind = SymbolKind::Global(GlobalSymbol {
                        is_mut: global.mutable,
                    });

                    self.define_unique(module_source, global.id, &global.name, kind, global.span);
                }

                Item::TupleTypeDecl(_) => {
                    // Tuple type family declaration — handled in orchestration
                }

                Item::BuiltinTypeDecl(decl) => {
                    // Named definition-less builtin type (`pub type Array<T>;`).
                    // Register a type symbol so it has a declaration site (LSP,
                    // `pub use`, prelude collision protection); its resolution
                    // to a builtin `ResolvedType` is wired in the elaborator.
                    self.define_unique(
                        module_source,
                        decl.id,
                        &decl.name,
                        SymbolKind::BuiltinType,
                        decl.span,
                    );
                }

                Item::Error(_) => {
                    // Error-recovery placeholder; no symbol to define.
                }
            }
        }
    }

    /// Check for type names that collide with prelude-exported types.
    ///
    /// Runs after pub use processing so that `is_prelude_type` can resolve
    /// re-exports from `core:prelude`. Skipped for modules carrying
    /// `#![no_prelude]`: a module that opts out of the prelude auto-import
    /// is also opting out of the prelude's name reservation, and the
    /// bundled stdlib files that *implement* the prelude carry the
    /// attribute precisely so they can define names like `Option`, `Eq`,
    /// `List` without being flagged as collisions with themselves.
    fn check_prelude_collisions(&mut self, module: &Module, module_source: &ModuleSource) {
        if module.has_no_prelude() {
            return;
        }
        self.logger.set_file(module_source.diagnostic_filename());
        for item in &module.items {
            let (name, span) = match item {
                Item::Struct(d) => (&d.name, d.span),
                Item::Enum(d) => (&d.name, d.span),
                Item::Variant(d) => (&d.name, d.span),
                Item::Newtype(d) => (&d.name, d.span),
                Item::Resource(d) => (&d.name, d.span),
                Item::Flags(d) => (&d.name, d.span),
                _ => continue,
            };
            if self.symbols.is_prelude_type(name) {
                let _ = self.logger.error(AnalyzeError::PreludeTypeCollision {
                    name: name.clone(),
                    span,
                });
            }
        }
    }

    /// Look up a symbol in a specific module
    pub fn lookup_in_module(&self, module_source: &ModuleSource, name: &str) -> Option<&Symbol> {
        self.symbols.lookup_in_module(module_source, name)
    }

    /// Get the symbol table
    pub fn into_symbols(self) -> SymbolTable {
        self.symbols
    }

    /// Get a reference to the symbol table
    pub fn get_symbols(&self) -> &SymbolTable {
        &self.symbols
    }

    /// Analyze pre-loaded modules
    ///
    /// This method takes modules that were already loaded by `ModuleLoader`
    /// and builds a symbol table from them.
    ///
    /// # Arguments
    /// * `modules` - All modules including entry and dependencies
    /// * `entry_source` - Source of the entry module
    /// * `implicit_modules` - Set of implicitly loaded modules
    pub fn analyze_loaded_modules(
        &mut self,
        modules: &crate::hashmap::IndexMap<ModuleSource, Module>,
        entry_source: &ModuleSource,
        implicit_modules: crate::hashmap::IndexSet<ModuleSource>,
    ) -> Result<(), Bail> {
        self.implicit_modules = implicit_modules;
        self.entry_module_source = entry_source.clone();

        // First pass: collect definitions from all modules (excluding pub use)
        for (source, module) in modules {
            self.collect_definitions(module, source);
        }

        // Second pass: process pub use (re-exports) now that all symbols are collected
        for (source, module) in modules {
            self.process_pub_use(module, source, modules);
        }

        // Third pass: check for prelude type collisions
        // Must run after pub use processing so that is_prelude_type can
        // resolve re-exports from core:prelude.
        for (source, module) in modules {
            self.check_prelude_collisions(module, source);
        }

        // Fourth pass: validate imports in each module
        let _ = self.validate_all_imports(modules);

        self.logger.ok_or_bail(())
    }

    fn validate_all_imports(
        &mut self,
        modules: &crate::hashmap::IndexMap<ModuleSource, Module>,
    ) -> Result<(), Bail> {
        for (source, module) in modules {
            self.logger.set_file(source.diagnostic_filename());
            self.validate_imports(module, source, modules)?;
        }
        Ok(())
    }

    /// Process pub use declarations (re-exports) in a module
    ///
    /// This registers re-export relationships without creating new symbols.
    /// The actual symbol resolution happens in `lookup_in_module` which
    /// follows re-export chains transparently.
    fn process_pub_use(
        &mut self,
        module: &Module,
        module_source: &ModuleSource,
        all_modules: &crate::hashmap::IndexMap<ModuleSource, Module>,
    ) {
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                if !use_decl.is_pub {
                    continue;
                }

                // Validate the import source. Wasm-asset imports skip
                // `validate_module_path` because their `./*.wat` /
                // `./*.wasm` paths are not normal Wado module paths.
                if !is_wasm_asset_use_decl(use_decl)
                    && validate_module_path(&use_decl.source).is_err()
                {
                    continue; // Error already collected in validate_imports
                }

                // Resolve the source path to ModuleSource, honoring
                // wasm-asset attributes and Kiln invocation redirects.
                let Some(source_module) = resolve_use_decl_module_source(
                    &mut self.interner.borrow_mut(),
                    module_source,
                    use_decl,
                    Some(&self.entry_module_source),
                    &self.invocations,
                ) else {
                    continue; // Bad wasm asset path; analyze emits the diagnostic.
                };

                // Check the module exists
                if !all_modules.contains_key(&source_module) {
                    continue; // Error already collected in validate_imports
                }

                // Register re-exports (without creating new symbols)
                for use_item in &use_decl.items {
                    match use_item {
                        UseItem::Simple { name, alias, .. } => {
                            let export_name = alias.as_ref().unwrap_or(name);
                            self.symbols.register_reexport(
                                module_source,
                                export_name,
                                &source_module,
                                name,
                            );
                        }
                        UseItem::InterfaceFunctions {
                            interface_name,
                            functions,
                        } => {
                            for func_item in functions {
                                let source_name = format!("{}::{}", interface_name, func_item.name);
                                let export_name =
                                    func_item.alias.as_ref().unwrap_or(&func_item.name);
                                self.symbols.register_reexport(
                                    module_source,
                                    export_name,
                                    &source_module,
                                    &source_name,
                                );
                            }
                        }
                        UseItem::Wildcard | UseItem::Namespace { .. } => {
                            // Wildcard/namespace import: no individual names to re-export
                        }
                    }
                }
            }
        }
    }

    /// Validate imports in a module (for pre-loaded modules)
    fn validate_imports(
        &mut self,
        module: &Module,
        from_module_source: &ModuleSource,
        all_modules: &crate::hashmap::IndexMap<ModuleSource, Module>,
    ) -> Result<(), Bail> {
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                // Wasm-asset imports skip the regular module-path
                // validator because their `./*.wat` / `./*.wasm`
                // paths are not normal Wado module paths.
                if !is_wasm_asset_use_decl(use_decl)
                    && let Err(message) = validate_module_path(&use_decl.source)
                {
                    self.logger.error(AnalyzeError::InvalidModulePath {
                        path: use_decl.source.clone(),
                        message,
                        span: use_decl.span,
                    })?;
                    continue;
                }

                // Resolve the import path to ModuleSource, honoring
                // wasm-asset attributes and Kiln invocation redirects.
                let Some(module_source) = resolve_use_decl_module_source(
                    &mut self.interner.borrow_mut(),
                    from_module_source,
                    use_decl,
                    Some(&self.entry_module_source),
                    &self.invocations,
                ) else {
                    self.logger.error(AnalyzeError::InvalidModulePath {
                        path: use_decl.source.clone(),
                        message: "wasm asset paths must be relative (`./` or `../`); \
                             absolute namespace-qualified paths are not supported here"
                            .to_string(),
                        span: use_decl.span,
                    })?;
                    continue;
                };

                // Check the module exists in pre-loaded modules
                if !all_modules.contains_key(&module_source) {
                    self.logger.error(AnalyzeError::ModuleNotFound {
                        module_source: module_source.clone(),
                        span: use_decl.span,
                    })?;
                    continue;
                }

                // Register imported symbols
                for use_item in &use_decl.items {
                    match use_item {
                        UseItem::Simple { name, alias, .. } => {
                            if let Some(symbol) =
                                self.symbols.lookup_in_module(&module_source, name)
                            {
                                let key = symbol.defined_at.clone();
                                let import_name = alias.as_ref().unwrap_or(name);
                                self.symbols
                                    .register_import(from_module_source, import_name, key);
                            } else {
                                self.logger.error(AnalyzeError::ImportNotFound {
                                    module_source: module_source.clone(),
                                    name: name.clone(),
                                    span: use_decl.span,
                                })?;
                            }
                        }
                        UseItem::InterfaceFunctions {
                            interface_name,
                            functions,
                        } => {
                            for func_item in functions {
                                let lookup_name = format!("{}::{}", interface_name, func_item.name);
                                if let Some(symbol) =
                                    self.symbols.lookup_in_module(&module_source, &lookup_name)
                                {
                                    let key = symbol.defined_at.clone();
                                    let import_name =
                                        func_item.alias.as_ref().unwrap_or(&func_item.name);
                                    self.symbols.register_import(
                                        from_module_source,
                                        import_name,
                                        key,
                                    );
                                } else {
                                    self.logger.error(AnalyzeError::ImportNotFound {
                                        module_source: module_source.clone(),
                                        name: lookup_name,
                                        span: use_decl.span,
                                    })?;
                                }
                            }
                        }
                        UseItem::Wildcard => {
                            // Wildcard import: module is loaded for side effects only,
                            // no symbols to register
                        }
                        UseItem::Namespace { name: ns } => {
                            // Namespace import: register every public symbol
                            // under its `ns$member` alias, matching how the
                            // elaborator canonicalizes `ns::member` at lookup
                            // time (`ModuleImports::canonical_ns_ref`).
                            let symbols: Vec<(String, SymbolKey)> = self
                                .symbols
                                .get_module_symbols(&module_source)
                                .into_iter()
                                .map(|s| {
                                    (
                                        crate::name::namespace_member_alias(ns, &s.name),
                                        s.defined_at.clone(),
                                    )
                                })
                                .collect();
                            for (alias, sym_key) in symbols {
                                self.symbols
                                    .register_import(from_module_source, &alias, sym_key);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Get a copy of the implicit modules set
    pub fn get_implicit_modules(&self) -> &crate::hashmap::IndexSet<ModuleSource> {
        &self.implicit_modules
    }
}
