//! Semantic analyzer for Wado
//!
//! The analyzer performs:
//! 1. Symbol table construction from pre-loaded modules
//! 2. Import validation
//! 3. Name resolution (binding identifiers to their definitions)

use crate::ast::{
    AstId, Function, FunctionSite, Item, Module, UseDecl, UseItem, Visibility, WorldExport,
    cm_import_of, for_each_function,
};
use crate::attribute::{AttributeFault, check, for_each_attribute};
use crate::compiler_host::{Code, CompilerHost, Diagnostic, DiagnosticSpan, Severity};
use crate::hashmap;
use crate::kiln::InvocationIndex;
use crate::loader::{resolve_wasm_asset_path, wasm_asset_kind_from_attrs};
use crate::logger::{Bail, Logger};
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name::{
    entry_dir_of, namespace_member_alias, resolve_import_with_invocations, validate_module_path,
};
use crate::unparse::unparse_type_into;
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
    invocations: &InvocationIndex,
) -> Option<ModuleSource> {
    if let Some(kind) = wasm_asset_kind_from_attrs(use_decl.attributes.as_ref()) {
        return resolve_wasm_asset_path(from, &use_decl.source, &entry_dir_of(entry))
            .ok()
            .map(|path| interner.wasm(&path, kind));
    }
    Some(resolve_import_with_invocations(
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
    ResourceSymbol, StructSymbol, Symbol, SymbolKind, SymbolTable, TraitSymbol, VariantSymbol,
    WorldExportSymbol, WorldImportSymbol, WorldSymbol,
};
use crate::token::Span;

/// Whether a module's functions may omit a body without naming what backs it.
fn allows_bodyless_functions(module_source: &ModuleSource) -> bool {
    match module_source {
        ModuleSource::Core { name } => name.as_str() == "builtin",
        ModuleSource::Binding { .. } | ModuleSource::Wasm { .. } => true,
        ModuleSource::Local { .. }
        | ModuleSource::Dependency { .. }
        | ModuleSource::Remote { .. }
        | ModuleSource::EntryPoint { .. }
        | ModuleSource::Redirected { .. } => false,
    }
}

/// What is wrong with the function `site` declares, if anything.
fn declaration_fault(
    site: FunctionSite<'_>,
    func: &Function,
    module_source: &ModuleSource,
) -> Option<AnalyzeError> {
    if let Some(attr) = func.unavailable_attr() {
        let fault = if !site.allows_unavailable() {
            UnavailableFault::Placement
        } else if func.body.is_some() {
            UnavailableFault::HasBody
        } else if func.is_export {
            UnavailableFault::Exported
        } else if attr.unavailable_reason().is_none_or(str::is_empty) {
            UnavailableFault::NoReason
        } else {
            return None;
        };
        return Some(AnalyzeError::MalformedUnavailable {
            fault,
            span: func.name_span,
        });
    }
    if !site.needs_body()
        || func.body.is_some()
        || func.is_cm_import()
        || allows_bodyless_functions(module_source)
    {
        return None;
    }
    Some(AnalyzeError::MissingFunctionBody {
        name: func.name.clone(),
        span: func.name_span,
    })
}

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
    /// An import whose local name the module also declares.
    ///
    /// Nothing can answer which one a reference means, so the layerings that
    /// resolve names each pick one and they need not agree. Rejecting the
    /// program is the only honest answer.
    ImportShadowsDefinition {
        name: String,
        span: Span,
        declared: Span,
    },
    /// A function declared without a body where nothing supplies one.
    MissingFunctionBody { name: String, span: Span },
    /// An `#[unavailable]` that cannot report what it was written to report.
    MalformedUnavailable { fault: UnavailableFault, span: Span },
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
    /// Import of a symbol that is not visible at the import site: a
    /// file-private symbol (no modifier), or an `internal` symbol reached
    /// from a different package.
    SymbolNotVisible {
        name: String,
        module_source: ModuleSource,
        visibility: Visibility,
        span: Span,
    },
    /// A `pub use` / `internal use` that promises more reach than the item it
    /// names actually has.
    ReExportWidensVisibility {
        name: String,
        module_source: ModuleSource,
        source_visibility: Visibility,
        reexport_visibility: Visibility,
        span: Span,
    },
}

/// What is wrong with an `#[unavailable]` declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableFault {
    /// The declaration keeps a body, which the attribute stands in for.
    HasBody,
    /// No reason, or an empty one.
    NoReason,
    /// Written where the attribute is not placed.
    Placement,
    /// Written on an `export fn`, which promises the boundary a function.
    Exported,
}

impl UnavailableFault {
    fn message(self) -> &'static str {
        match self {
            Self::HasBody => {
                "`#[unavailable]` replaces a body; this declaration has one, so remove one of them"
            }
            Self::NoReason => {
                "`#[unavailable]` needs a reason, as `#[unavailable(\"write `x` instead\")]`"
            }
            Self::Placement => {
                "`#[unavailable]` belongs on a module function, an `impl` method, or a trait method"
            }
            Self::Exported => {
                "`export` lowers a function at the component boundary; an `#[unavailable]` one has none to lower"
            }
        }
    }
}

impl AnalyzeError {
    /// The code, message, and location this error reports.
    fn render(&self) -> (Code, String, Span) {
        match self {
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
                Code::ImportNotFound,
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
            AnalyzeError::ImportShadowsDefinition {
                name,
                span,
                declared,
            } => (
                Code::DuplicateDefinition,
                format!(
                    "import of '{name}' collides with the declaration at {}:{}",
                    declared.line, declared.column
                ),
                *span,
            ),
            AnalyzeError::MissingFunctionBody { name, span } => (
                Code::MissingFunctionBody,
                format!("function '{name}' has no body"),
                *span,
            ),
            AnalyzeError::MalformedUnavailable { fault, span } => (
                Code::MalformedUnavailable,
                fault.message().to_string(),
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
            AnalyzeError::SymbolNotVisible {
                name,
                module_source,
                visibility,
                span,
            } => (
                Code::PrivateSymbol,
                symbol_not_visible_message(name, module_source, *visibility),
                *span,
            ),
            AnalyzeError::ReExportWidensVisibility {
                name,
                module_source,
                source_visibility,
                reexport_visibility,
                span,
            } => (
                Code::PrivateSymbol,
                reexport_widens_message(
                    name,
                    module_source,
                    *source_visibility,
                    *reexport_visibility,
                ),
                *span,
            ),
        }
    }
}

fn reexport_widens_message(
    name: &str,
    module_source: &ModuleSource,
    source_visibility: Visibility,
    reexport_visibility: Visibility,
) -> String {
    let reexport = reexport_visibility.keyword().trim_end();
    let declared = match source_visibility {
        Visibility::Private => "file-private".to_string(),
        Visibility::Internal | Visibility::Public => {
            format!("`{}`", source_visibility.keyword().trim_end())
        }
    };
    format!(
        "`{reexport} use` of '{name}' reaches further than '{name}' itself, which is \
         {declared} in '{module_source}'; widen the declaration, or narrow the re-export"
    )
}

pub(crate) fn symbol_not_visible_message(
    name: &str,
    module_source: &ModuleSource,
    visibility: Visibility,
) -> String {
    match visibility {
        Visibility::Internal => format!(
            "symbol '{name}' is `internal` to '{module_source}' and cannot be imported \
             from another package; mark it `pub` to export it across packages"
        ),
        // `Public` never reaches here (always importable); folded in for exhaustiveness.
        Visibility::Private | Visibility::Public => format!(
            "symbol '{name}' is private to '{module_source}' and cannot be imported; \
             mark it `internal` (same package) or `pub` (cross package) to export it"
        ),
    }
}

impl From<AnalyzeError> for Diagnostic {
    fn from(e: AnalyzeError) -> Self {
        let (code, message, span) = e.render();
        Diagnostic {
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
    implicit_modules: hashmap::IndexSet<ModuleSource>,
    entry_module_source: ModuleSource,
    /// Kiln invocation redirects consulted by the import-resolution paths
    /// (`validate_imports`, re-export registration). Empty when the
    /// compilation did not run the Kiln pipeline.
    invocations: InvocationIndex,
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
            implicit_modules: hashmap::IndexSet::default(),
            entry_module_source: ModuleSource::entry_point_uninitialized(),
            invocations: InvocationIndex::new(),
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
    pub fn with_invocations(mut self, invocations: InvocationIndex) -> Self {
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
        ast_id: AstId,
        name: &str,
        kind: SymbolKind,
        visibility: Visibility,
        span: Span,
    ) -> Option<AstId> {
        if let Some(first) = self.symbols.defined_span_in_module(module_source, name) {
            let _ = self.logger.error_in(
                module_source,
                AnalyzeError::DuplicateDefinition {
                    name: name.to_string(),
                    span,
                    first,
                },
            );
            return None;
        }
        Some(
            self.symbols
                .define(module_source, ast_id, name, kind, visibility, Some(span)),
        )
    }

    /// Collect all definitions from a module into the symbol table
    fn collect_definitions(&mut self, module: &Module, module_source: &ModuleSource) {
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
                        cm_import: cm_import_of(&func.attrs).cloned(),
                    });

                    self.define_unique(
                        module_source,
                        func.id,
                        &func.name,
                        kind,
                        func.visibility,
                        func.span,
                    );
                }

                Item::Interface(effect) => {
                    // Extract effect-level CM import from attributes
                    let effect_cm_import = cm_import_of(&effect.attrs).cloned();

                    let kind = SymbolKind::Effect(EffectSymbol {
                        methods: effect.methods.iter().map(|m| m.name.clone()).collect(),
                        cm_import: effect_cm_import,
                    });

                    self.define_unique(
                        module_source,
                        effect.id,
                        &effect.name,
                        kind,
                        effect.visibility,
                        effect.span,
                    );

                    // Also register each effect method as a function symbol
                    // with the fully qualified name "{Effect}.{method}"
                    // This allows importing them via use statements
                    for method in &effect.methods {
                        let cm_import = cm_import_of(&method.attrs).cloned();

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
                            effect.visibility,
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
                        struct_decl.visibility,
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
                        enum_decl.visibility,
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
                        variant_decl.visibility,
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
                        trait_decl.visibility,
                        trait_decl.span,
                    );
                }

                Item::Newtype(newtype) => {
                    let mut aliased_type = String::new();
                    unparse_type_into(&newtype.ty, &mut aliased_type);
                    let kind = SymbolKind::Newtype(NewtypeSymbol { aliased_type });

                    self.define_unique(
                        module_source,
                        newtype.id,
                        &newtype.name,
                        kind,
                        newtype.visibility,
                        newtype.span,
                    );
                }

                Item::Resource(resource) => {
                    let kind = SymbolKind::Resource(ResourceSymbol {
                        methods: resource.methods.iter().map(|m| m.name.clone()).collect(),
                        cm_import: cm_import_of(&resource.attrs).cloned(),
                    });

                    self.define_unique(
                        module_source,
                        resource.id,
                        &resource.name,
                        kind,
                        resource.visibility,
                        resource.span,
                    );

                    // Register each resource method as a function symbol
                    // with the fully qualified name "{Resource}::{method}"
                    for method in &resource.methods {
                        let cm_import = cm_import_of(&method.attrs).cloned();

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
                            resource.visibility,
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
                                WorldExport::Interface(iface) => WorldExportSymbol::Interface {
                                    interface_name: iface.interface_name.clone(),
                                },
                                WorldExport::Function(func) => WorldExportSymbol::Function {
                                    name: func.name.clone(),
                                    is_async: func.is_async,
                                    params: func.params.iter().map(|p| p.name.clone()).collect(),
                                    return_type: func
                                        .return_type
                                        .as_ref()
                                        .map(|_| "unknown".to_string()),
                                },
                            })
                            .collect(),
                    });

                    self.define_unique(
                        module_source,
                        world.id,
                        &world.name,
                        kind,
                        world.visibility,
                        world.span,
                    );
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
                        flags_decl.visibility,
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

                    self.define_unique(
                        module_source,
                        global.id,
                        &global.name,
                        kind,
                        global.visibility,
                        global.span,
                    );
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
                        decl.visibility,
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
                let _ = self.logger.error_in(
                    module_source,
                    AnalyzeError::PreludeTypeCollision {
                        name: name.clone(),
                        span,
                    },
                );
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
        modules: &hashmap::IndexMap<ModuleSource, Module>,
        entry_source: &ModuleSource,
        implicit_modules: hashmap::IndexSet<ModuleSource>,
    ) -> Result<(), Bail> {
        self.implicit_modules = implicit_modules;
        self.entry_module_source = entry_source.clone();

        for (source, module) in modules {
            self.collect_definitions(module, source);
        }

        // Re-exports need every module's symbols collected.
        for (source, module) in modules {
            self.process_reexports(module, source, modules);
        }

        // `is_prelude_type` resolves re-exports from core:prelude, so this
        // follows the re-export pass.
        for (source, module) in modules {
            self.check_prelude_collisions(module, source);
        }

        for (source, module) in modules {
            self.check_function_declarations(module, source);
        }

        for (source, module) in modules {
            self.check_attributes(module, source);
        }

        let _ = self.validate_all_imports(modules);

        self.logger.ok_or_bail(())
    }

    fn check_attributes(&self, module: &Module, module_source: &ModuleSource) {
        for_each_attribute(module, |written| {
            let Some(fault) = check(&written) else {
                return;
            };
            let code = match fault {
                AttributeFault::Unknown => Code::UnknownAttr,
                AttributeFault::Misplaced { .. }
                | AttributeFault::Position { .. }
                | AttributeFault::Arguments { .. }
                | AttributeFault::Key { .. }
                | AttributeFault::Repeated { .. }
                | AttributeFault::NamePolicy { .. } => Code::AttrMisuse,
            };
            let _ = self.logger.error_in(
                module_source,
                Diagnostic {
                    severity: Severity::Error,
                    code,
                    message: fault.message(written.name),
                    span: Some(DiagnosticSpan::from_span(&written.span, None)),
                },
            );
        });
    }

    fn check_function_declarations(&self, module: &Module, module_source: &ModuleSource) {
        for_each_function(module, |site, func| {
            if let Some(error) = declaration_fault(site, func, module_source) {
                let _ = self.logger.error_in(module_source, error);
            }
        });
    }

    fn validate_all_imports(
        &mut self,
        modules: &hashmap::IndexMap<ModuleSource, Module>,
    ) -> Result<(), Bail> {
        for (source, module) in modules {
            self.validate_imports(module, source, modules)?;
        }
        Ok(())
    }

    /// Process re-export declarations (`pub use` / `internal use`) in a module.
    ///
    /// This registers re-export relationships without creating new symbols.
    /// The actual symbol resolution happens in `lookup_in_module` which
    /// follows re-export chains transparently.
    fn process_reexports(
        &mut self,
        module: &Module,
        module_source: &ModuleSource,
        all_modules: &hashmap::IndexMap<ModuleSource, Module>,
    ) {
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                // A plain `use` is a file-private import that re-exports nothing.
                if !use_decl.visibility.reaches_beyond_file() {
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
                                use_decl.visibility,
                            );
                        }
                        UseItem::InterfaceFunctions {
                            interface_name,
                            functions,
                            ..
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
                                    use_decl.visibility,
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

    /// Emit `SymbolNotVisible` when `name` is a binding in `target_module` not
    /// reachable from `from_module_source`. A name that is neither defined nor
    /// re-exported there is left to the `ImportNotFound` path.
    fn check_import_visibility(
        &self,
        from_module_source: &ModuleSource,
        target_module: &ModuleSource,
        name: &str,
        span: Span,
    ) -> Result<(), Bail> {
        if let Some(visibility) =
            self.symbols
                .visibility_barrier(from_module_source, target_module, name)
        {
            self.logger.error_in(
                from_module_source,
                AnalyzeError::SymbolNotVisible {
                    name: name.to_string(),
                    module_source: target_module.clone(),
                    visibility,
                    span,
                },
            )?;
        }
        Ok(())
    }

    /// Emit `ReExportWidensVisibility` when a `pub use` / `internal use`
    /// promises more reach than the item it names has.
    fn check_reexport_widening(
        &self,
        from_module_source: &ModuleSource,
        target_module: &ModuleSource,
        name: &str,
        reexport_visibility: Visibility,
        span: Span,
    ) -> Result<(), Bail> {
        if !reexport_visibility.reaches_beyond_file() {
            return Ok(());
        }
        let Some(source_visibility) = self
            .symbols
            .effective_visibility_in_module(target_module, name)
        else {
            return Ok(());
        };
        if reexport_visibility.reaches_no_further_than(source_visibility) {
            return Ok(());
        }
        self.logger.error_in(
            from_module_source,
            AnalyzeError::ReExportWidensVisibility {
                name: name.to_string(),
                module_source: target_module.clone(),
                source_visibility,
                reexport_visibility,
                span,
            },
        )
    }

    /// Reject an import whose local name the module also declares.
    ///
    /// The name then means two declarations at once, and every layering that
    /// resolves it picks one on its own terms — which is how the same spelling
    /// came to mean different things in different parts of the compiler. An
    /// alias resolves it, so the program says which one it meant.
    fn reject_import_collision(
        &self,
        module_source: &ModuleSource,
        local_name: &str,
        span: Span,
    ) -> Result<(), Bail> {
        let Some(declared) = self
            .symbols
            .defined_span_in_module(module_source, local_name)
        else {
            return Ok(());
        };
        self.logger.error_in(
            module_source,
            AnalyzeError::ImportShadowsDefinition {
                name: local_name.to_string(),
                span,
                declared,
            },
        )
    }

    /// Validate imports in a module (for pre-loaded modules)
    fn validate_imports(
        &mut self,
        module: &Module,
        from_module_source: &ModuleSource,
        all_modules: &hashmap::IndexMap<ModuleSource, Module>,
    ) -> Result<(), Bail> {
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                // Wasm-asset imports skip the regular module-path
                // validator because their `./*.wat` / `./*.wasm`
                // paths are not normal Wado module paths.
                if !is_wasm_asset_use_decl(use_decl)
                    && let Err(message) = validate_module_path(&use_decl.source)
                {
                    self.logger.error_in(
                        from_module_source,
                        AnalyzeError::InvalidModulePath {
                            path: use_decl.source.clone(),
                            message,
                            span: use_decl.span,
                        },
                    )?;
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
                    self.logger.error_in(
                        from_module_source,
                        AnalyzeError::InvalidModulePath {
                            path: use_decl.source.clone(),
                            message: "wasm asset paths must be relative (`./` or `../`); \
                             absolute namespace-qualified paths are not supported here"
                                .to_string(),
                            span: use_decl.span,
                        },
                    )?;
                    continue;
                };

                // Check the module exists in pre-loaded modules
                if !all_modules.contains_key(&module_source) {
                    self.logger.error_in(
                        from_module_source,
                        AnalyzeError::ModuleNotFound {
                            module_source: module_source.clone(),
                            span: use_decl.span,
                        },
                    )?;
                    continue;
                }

                // Register imported symbols
                for use_item in &use_decl.items {
                    match use_item {
                        UseItem::Simple { name, alias, .. } => {
                            if let Some(symbol) =
                                self.symbols.lookup_in_module(&module_source, name)
                            {
                                let key = symbol.defined_at;
                                let import_name = alias.as_ref().unwrap_or(name);
                                self.check_import_visibility(
                                    from_module_source,
                                    &module_source,
                                    name,
                                    use_decl.span,
                                )?;
                                self.check_reexport_widening(
                                    from_module_source,
                                    &module_source,
                                    name,
                                    use_decl.visibility,
                                    use_decl.span,
                                )?;
                                self.reject_import_collision(
                                    from_module_source,
                                    import_name,
                                    use_decl.span,
                                )?;
                                self.symbols
                                    .register_import(from_module_source, import_name, key);
                            } else {
                                self.logger.error_in(
                                    from_module_source,
                                    AnalyzeError::ImportNotFound {
                                        module_source: module_source.clone(),
                                        name: name.clone(),
                                        span: use_decl.span,
                                    },
                                )?;
                            }
                        }
                        UseItem::InterfaceFunctions {
                            interface_name,
                            functions,
                            ..
                        } => {
                            for func_item in functions {
                                let lookup_name = format!("{}::{}", interface_name, func_item.name);
                                if let Some(symbol) =
                                    self.symbols.lookup_in_module(&module_source, &lookup_name)
                                {
                                    let key = symbol.defined_at;
                                    let import_name =
                                        func_item.alias.as_ref().unwrap_or(&func_item.name);
                                    self.check_import_visibility(
                                        from_module_source,
                                        &module_source,
                                        &lookup_name,
                                        use_decl.span,
                                    )?;
                                    self.check_reexport_widening(
                                        from_module_source,
                                        &module_source,
                                        &lookup_name,
                                        use_decl.visibility,
                                        use_decl.span,
                                    )?;
                                    // Registered under the bare member name
                                    // like a `Simple` import, so it collides
                                    // with a declaration the same way.
                                    self.reject_import_collision(
                                        from_module_source,
                                        import_name,
                                        use_decl.span,
                                    )?;
                                    self.symbols.register_import(
                                        from_module_source,
                                        import_name,
                                        key,
                                    );
                                } else {
                                    self.logger.error_in(
                                        from_module_source,
                                        AnalyzeError::ImportNotFound {
                                            module_source: module_source.clone(),
                                            name: lookup_name,
                                            span: use_decl.span,
                                        },
                                    )?;
                                }
                            }
                        }
                        UseItem::Wildcard => {
                            // Wildcard import: module is loaded for side effects only,
                            // no symbols to register
                        }
                        UseItem::Namespace { name: ns } => {
                            // Register each reachable member under its `ns$member`
                            // alias, matching how the elaborator canonicalizes
                            // `ns::member` at lookup time
                            // (`ModuleImports::canonical_ns_ref`).
                            let symbols: Vec<(String, AstId)> = self
                                .symbols
                                .reachable_members(from_module_source, &module_source)
                                .map(|(name, sym)| {
                                    (namespace_member_alias(ns, &name), sym.defined_at)
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
    pub fn get_implicit_modules(&self) -> &hashmap::IndexSet<ModuleSource> {
        &self.implicit_modules
    }
}
