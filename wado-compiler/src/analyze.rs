//! Semantic analyzer for Wado
//!
//! The analyzer performs:
//! 1. Symbol table construction from pre-loaded modules
//! 2. Import validation
//! 3. Name resolution (binding identifiers to their definitions)

use crate::ast::{Item, Module, UseItem};
use crate::name::{resolve_import_path, validate_module_path};
use crate::symbol::{
    EffectSymbol, EnumSymbol, FunctionSymbol, ResourceSymbol, StructSymbol, Symbol, SymbolKind,
    SymbolTable, TraitSymbol, TypeAliasSymbol, VariantSymbol, WorldExportSymbol, WorldImportSymbol,
    WorldSymbol,
};
use crate::token::Span;

/// Error that can occur during analysis
#[derive(Debug, Clone)]
pub enum AnalyzeError {
    /// Module not found (not in pre-loaded modules)
    ModuleNotFound {
        module_path: Vec<String>,
        span: Span,
    },
    /// Symbol not found in module
    ImportNotFound {
        module_path: Vec<String>,
        name: String,
        span: Span,
    },
    /// Duplicate definition
    DuplicateDefinition { name: String, span: Span },
    /// Undefined symbol reference
    UndefinedSymbol { name: String, span: Span },
    /// Invalid module path (not a valid URI reference)
    InvalidModulePath {
        path: String,
        message: String,
        span: Span,
    },
}

impl std::fmt::Display for AnalyzeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalyzeError::ModuleNotFound { module_path, span } => {
                write!(
                    f,
                    "{}:{}: module not found: '{}'",
                    span.line,
                    span.column,
                    module_path.join("::")
                )
            }
            AnalyzeError::ImportNotFound {
                module_path,
                name,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: symbol '{}' not found in module '{}'",
                    span.line,
                    span.column,
                    name,
                    module_path.join("::")
                )
            }
            AnalyzeError::DuplicateDefinition { name, span } => {
                write!(
                    f,
                    "{}:{}: duplicate definition '{}'",
                    span.line, span.column, name
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
        }
    }
}

impl std::error::Error for AnalyzeError {}

/// Semantic analyzer
///
/// Builds a symbol table from pre-loaded modules and validates imports.
pub struct Analyzer {
    /// The symbol table being built
    pub symbols: SymbolTable,
    /// Collected errors
    errors: Vec<AnalyzeError>,
    /// Modules loaded implicitly by the compiler (not by user imports)
    implicit_modules: std::collections::HashSet<Vec<String>>,
}

impl Analyzer {
    /// Create a new analyzer
    pub fn new() -> Self {
        Self {
            symbols: SymbolTable::new(),
            errors: Vec::new(),
            implicit_modules: std::collections::HashSet::new(),
        }
    }

    /// Collect all definitions from a module into the symbol table
    fn collect_definitions(&mut self, module: &Module, module_path: &[String]) {
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    // A function is a builtin if:
                    // 1. It has no body (bodyless declaration like `pub fn foo();`)
                    // 2. Or it's defined in a core::* module
                    let is_builtin = func.body.is_none()
                        || module_path.first().map(|s| s == "core").unwrap_or(false);

                    let kind = SymbolKind::Function(FunctionSymbol {
                        params: func.params.iter().map(|p| p.name.clone()).collect(),
                        return_type: func.return_type.as_ref().map(|_| "unknown".to_string()),
                        effects: func.effects.clone(),
                        is_builtin,
                        wasi_import: None, // TODO: extract from attributes
                    });

                    self.symbols
                        .define(&func.name, kind, module_path, Some(func.span));
                }

                Item::Effect(effect) => {
                    // Extract effect-level WASI import from attributes
                    let effect_wasi_import =
                        effect.attrs.first().and_then(|a| a.wasi_import.clone());

                    let kind = SymbolKind::Effect(EffectSymbol {
                        methods: effect.methods.iter().map(|m| m.name.clone()).collect(),
                        wasi_import: effect_wasi_import,
                    });

                    self.symbols
                        .define(&effect.name, kind, module_path, Some(effect.span));

                    // Also register each effect method as a function symbol
                    // with the fully qualified name "{Effect}.{method}"
                    // This allows importing them via use statements
                    for method in &effect.methods {
                        let wasi_import = method.attrs.first().and_then(|a| a.wasi_import.clone());

                        let func_kind = SymbolKind::Function(FunctionSymbol {
                            params: method.params.iter().map(|p| p.name.clone()).collect(),
                            return_type: method.return_type.as_ref().map(|_| "unknown".to_string()),
                            effects: vec![effect.name.clone()], // Effect methods implicitly require their effect
                            is_builtin: module_path.first().map(|s| s == "core").unwrap_or(false),
                            wasi_import,
                        });

                        // Register as "{Effect}::{method}"
                        let qualified_name = format!("{}::{}", effect.name, method.name);
                        self.symbols.define(
                            &qualified_name,
                            func_kind,
                            module_path,
                            Some(method.span),
                        );
                    }
                }

                Item::Struct(struct_decl) => {
                    let kind = SymbolKind::Struct(StructSymbol {
                        fields: struct_decl.fields.iter().map(|f| f.name.clone()).collect(),
                    });

                    self.symbols.define(
                        &struct_decl.name,
                        kind,
                        module_path,
                        Some(struct_decl.span),
                    );
                }

                Item::Enum(enum_decl) => {
                    let kind = SymbolKind::Enum(EnumSymbol {
                        cases: enum_decl.cases.iter().map(|c| c.name.clone()).collect(),
                    });

                    self.symbols
                        .define(&enum_decl.name, kind, module_path, Some(enum_decl.span));
                }

                Item::Variant(variant_decl) => {
                    let kind = SymbolKind::Variant(VariantSymbol {
                        cases: variant_decl.cases.iter().map(|c| c.name.clone()).collect(),
                    });

                    self.symbols.define(
                        &variant_decl.name,
                        kind,
                        module_path,
                        Some(variant_decl.span),
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

                    self.symbols
                        .define(&trait_decl.name, kind, module_path, Some(trait_decl.span));
                }

                Item::Type(type_alias) => {
                    let kind = SymbolKind::TypeAlias(TypeAliasSymbol {
                        aliased_type: "unknown".to_string(), // TODO: store actual type
                    });

                    self.symbols
                        .define(&type_alias.name, kind, module_path, Some(type_alias.span));
                }

                Item::Resource(resource) => {
                    let kind = SymbolKind::Resource(ResourceSymbol {
                        methods: resource.methods.iter().map(|m| m.name.clone()).collect(),
                        wasi_import: resource.attrs.first().and_then(|a| a.wasi_import.clone()),
                    });

                    self.symbols
                        .define(&resource.name, kind, module_path, Some(resource.span));

                    // Register each resource method as a function symbol
                    // with the fully qualified name "{Resource}::{method}"
                    for method in &resource.methods {
                        let wasi_import = method.attrs.first().and_then(|a| a.wasi_import.clone());

                        let func_kind = SymbolKind::Function(FunctionSymbol {
                            params: method.params.iter().map(|p| p.name.clone()).collect(),
                            return_type: method.return_type.as_ref().map(|_| "unknown".to_string()),
                            effects: vec![], // Resource methods don't have effect requirements
                            is_builtin: module_path.first().map(|s| s == "core").unwrap_or(false),
                            wasi_import,
                        });

                        // Register as "{Resource}::{method}"
                        let qualified_name = format!("{}::{}", resource.name, method.name);
                        self.symbols.define(
                            &qualified_name,
                            func_kind,
                            module_path,
                            Some(method.span),
                        );
                    }
                }

                Item::World(world) => {
                    let kind = SymbolKind::World(WorldSymbol {
                        imports: world
                            .imports
                            .iter()
                            .map(|i| WorldImportSymbol {
                                effect_name: i.effect_name.clone(),
                                functions: i.functions.clone(),
                            })
                            .collect(),
                        exports: world
                            .exports
                            .iter()
                            .map(|e| WorldExportSymbol {
                                name: e.name.clone(),
                                is_async: e.is_async,
                                params: e.params.iter().map(|p| p.name.clone()).collect(),
                                return_type: e.return_type.as_ref().map(|_| "unknown".to_string()),
                            })
                            .collect(),
                    });

                    self.symbols
                        .define(&world.name, kind, module_path, Some(world.span));
                }

                Item::Use(_) => {
                    // Use declarations are handled in validate_imports
                }

                Item::Impl(_) => {
                    // Impl blocks are handled later
                }

                Item::Flags(_) => {
                    // Flags are currently parsed but not used directly
                    // They are handled by the WASI registry
                }

                Item::Test(_) => {
                    // Test declarations are handled in the resolver, not in the symbol table.
                    // Tests are converted to functions with generated names.
                }
            }
        }
    }

    /// Look up a symbol by name (for use during codegen)
    pub fn lookup(&self, name: &str) -> Option<&Symbol> {
        self.symbols.lookup(name)
    }

    /// Look up a symbol in a specific module
    pub fn lookup_in_module(&self, module_path: &[String], name: &str) -> Option<&Symbol> {
        self.symbols.lookup_in_module(module_path, name)
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
    /// * `entry_path` - Path of the entry module
    /// * `implicit_modules` - Set of implicitly loaded modules
    pub fn analyze_loaded_modules(
        &mut self,
        modules: &std::collections::HashMap<Vec<String>, Module>,
        _entry_path: &[String],
        implicit_modules: std::collections::HashSet<Vec<String>>,
    ) -> Result<(), Vec<AnalyzeError>> {
        self.implicit_modules = implicit_modules;

        // First pass: collect definitions from all modules (excluding pub use)
        for (path, module) in modules {
            self.collect_definitions(module, path);
        }

        // Second pass: process pub use (re-exports) now that all symbols are collected
        for (path, module) in modules {
            self.process_pub_use(module, path, modules);
        }

        // Third pass: validate imports in each module
        for (path, module) in modules {
            self.validate_imports(module, path, modules)?;
        }

        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(std::mem::take(&mut self.errors))
        }
    }

    /// Process pub use declarations (re-exports) in a module
    fn process_pub_use(
        &mut self,
        module: &Module,
        module_path: &[String],
        all_modules: &std::collections::HashMap<Vec<String>, Module>,
    ) {
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                if !use_decl.is_pub {
                    continue;
                }

                // Validate the import source
                if validate_module_path(&use_decl.source).is_err() {
                    continue; // Error already collected in validate_imports
                }

                // Resolve the source path
                let source_path = resolve_import_path(module_path, &use_decl.source);

                // Check the module exists
                if !all_modules.contains_key(&source_path) {
                    continue; // Error already collected in validate_imports
                }

                // Re-export each item to the current module's namespace
                for use_item in &use_decl.items {
                    match use_item {
                        UseItem::Simple { name, alias } => {
                            if let Some(symbol) = self.symbols.lookup_in_module(&source_path, name)
                            {
                                let export_name = alias.as_ref().unwrap_or(name);
                                self.symbols.define(
                                    export_name,
                                    symbol.kind.clone(),
                                    module_path,
                                    symbol.span,
                                );
                            }
                            // Error case handled in validate_imports
                        }
                        UseItem::EffectFunctions {
                            effect_name,
                            functions,
                        } => {
                            for func_item in functions {
                                let lookup_name = format!("{}::{}", effect_name, func_item.name);
                                if let Some(symbol) =
                                    self.symbols.lookup_in_module(&source_path, &lookup_name)
                                {
                                    let export_name =
                                        func_item.alias.as_ref().unwrap_or(&func_item.name);
                                    self.symbols.define(
                                        export_name,
                                        symbol.kind.clone(),
                                        module_path,
                                        symbol.span,
                                    );
                                }
                                // Error case handled in validate_imports
                            }
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
        from_module_path: &[String],
        all_modules: &std::collections::HashMap<Vec<String>, Module>,
    ) -> Result<(), Vec<AnalyzeError>> {
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                // Validate the import source
                if let Err(message) = validate_module_path(&use_decl.source) {
                    self.errors.push(AnalyzeError::InvalidModulePath {
                        path: use_decl.source.clone(),
                        message,
                        span: use_decl.span,
                    });
                    continue;
                }

                // Resolve the import path using name utilities
                let module_path = resolve_import_path(from_module_path, &use_decl.source);

                // Check the module exists in pre-loaded modules
                if !all_modules.contains_key(&module_path) {
                    self.errors.push(AnalyzeError::ModuleNotFound {
                        module_path: module_path.clone(),
                        span: use_decl.span,
                    });
                    continue;
                }

                // Register imported symbols
                for use_item in &use_decl.items {
                    match use_item {
                        UseItem::Simple { name, alias } => {
                            if let Some(symbol) = self.symbols.lookup_in_module(&module_path, name)
                            {
                                let symbol_id = symbol.id;
                                let import_name = alias.as_ref().unwrap_or(name);
                                self.symbols.register_import(import_name, symbol_id);
                            } else {
                                self.errors.push(AnalyzeError::ImportNotFound {
                                    module_path: module_path.clone(),
                                    name: name.clone(),
                                    span: use_decl.span,
                                });
                            }
                        }
                        UseItem::EffectFunctions {
                            effect_name,
                            functions,
                        } => {
                            for func_item in functions {
                                let lookup_name = format!("{}::{}", effect_name, func_item.name);
                                if let Some(symbol) =
                                    self.symbols.lookup_in_module(&module_path, &lookup_name)
                                {
                                    let symbol_id = symbol.id;
                                    let import_name =
                                        func_item.alias.as_ref().unwrap_or(&func_item.name);
                                    self.symbols.register_import(import_name, symbol_id);
                                } else {
                                    self.errors.push(AnalyzeError::ImportNotFound {
                                        module_path: module_path.clone(),
                                        name: lookup_name,
                                        span: use_decl.span,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(std::mem::take(&mut self.errors))
        }
    }

    /// Get a copy of the implicit modules set
    pub fn get_implicit_modules(&self) -> &std::collections::HashSet<Vec<String>> {
        &self.implicit_modules
    }
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}
