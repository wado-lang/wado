//! Semantic analyzer for Wado
//!
//! The analyzer performs:
//! 1. Module loading and import resolution
//! 2. Symbol table construction
//! 3. Name resolution (binding identifiers to their definitions)

use crate::ast::{Item, Module, UseItem};
use crate::resolver::{ModuleResolver, ResolveError};
use crate::symbol::{
    EffectSymbol, EnumSymbol, FunctionSymbol, ResourceSymbol, StructSymbol, Symbol, SymbolKind,
    SymbolTable, TypeAliasSymbol, WorldExportSymbol, WorldImportSymbol, WorldSymbol,
};
use crate::token::Span;

/// Error that can occur during analysis
#[derive(Debug, Clone)]
pub enum AnalyzeError {
    /// Module resolution failed
    ResolveError(ResolveError),
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
}

impl std::fmt::Display for AnalyzeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalyzeError::ResolveError(e) => write!(f, "{e}"),
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
        }
    }
}

impl std::error::Error for AnalyzeError {}

impl From<ResolveError> for AnalyzeError {
    fn from(e: ResolveError) -> Self {
        AnalyzeError::ResolveError(e)
    }
}

/// Semantic analyzer
///
/// Builds a symbol table from modules and resolves imports.
pub struct Analyzer {
    /// The symbol table being built
    pub symbols: SymbolTable,
    /// Module resolver for loading imported modules
    resolver: ModuleResolver,
    /// Collected errors
    errors: Vec<AnalyzeError>,
}

impl Analyzer {
    /// Create a new analyzer
    pub fn new() -> Self {
        Self {
            symbols: SymbolTable::new(),
            resolver: ModuleResolver::new(),
            errors: Vec::new(),
        }
    }

    /// Create a new analyzer with a base path for resolving local imports
    pub fn with_base_path(base_path: &std::path::Path) -> Self {
        Self {
            symbols: SymbolTable::new(),
            resolver: ModuleResolver::with_base_path(base_path),
            errors: Vec::new(),
        }
    }

    /// Analyze a module and all its imports
    ///
    /// # Arguments
    /// * `module` - The main module to analyze
    /// * `module_path` - Path of the main module (empty for the entry point)
    ///
    /// # Returns
    /// The completed symbol table, or a list of errors if analysis failed.
    pub fn analyze(
        &mut self,
        module: &Module,
        module_path: &[String],
    ) -> Result<(), Vec<AnalyzeError>> {
        // First pass: collect all definitions from this module
        self.collect_definitions(module, module_path);

        // Second pass: resolve imports
        self.resolve_imports(module, module_path)?;

        // Always load core:internals for compiler-generated code (e.g., template strings)
        // This module provides internal helper functions like string_concat, f64_to_string, etc.
        self.load_implicit_module(&["core".to_string(), "internals".to_string()]);

        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(std::mem::take(&mut self.errors))
        }
    }

    /// Load a module implicitly (without a user import declaration)
    /// Used for modules like core:internals that provide compiler-generated code support
    fn load_implicit_module(&mut self, module_path: &[String]) {
        // Skip if already loaded
        if self.resolver.is_loaded(module_path) {
            return;
        }

        // Try to load the module
        let imported_module = match self.resolver.load_module(module_path) {
            Ok(m) => m.clone(),
            Err(e) => {
                // Log but don't fail - implicit modules are optional
                eprintln!(
                    "Warning: failed to load implicit module {:?}: {}",
                    module_path, e
                );
                return;
            }
        };

        // Collect definitions from the implicit module
        self.collect_definitions(&imported_module, module_path);
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
                    let kind = SymbolKind::Effect(EffectSymbol {
                        methods: effect.methods.iter().map(|m| m.name.clone()).collect(),
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
                        variants: enum_decl.variants.iter().map(|v| v.name.clone()).collect(),
                    });

                    self.symbols
                        .define(&enum_decl.name, kind, module_path, Some(enum_decl.span));
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
                        methods: vec![],
                        wasi_import: resource.attrs.first().and_then(|a| a.wasi_import.clone()),
                    });

                    self.symbols
                        .define(&resource.name, kind, module_path, Some(resource.span));
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

                Item::Use(use_decl) => {
                    // Handle pub use (re-exports)
                    if use_decl.is_pub {
                        // Parse the source string into module path segments
                        let source_path: Vec<String> = if use_decl.source.contains(':') {
                            use_decl.source.splitn(2, ':').map(String::from).collect()
                        } else {
                            vec![use_decl.source.clone()]
                        };

                        // Try to load the source module
                        let source_module = match self.resolver.load_module(&source_path) {
                            Ok(m) => m.clone(),
                            Err(e) => {
                                self.errors.push(AnalyzeError::ResolveError(e));
                                continue;
                            }
                        };

                        // Collect definitions from the source module (if not already done)
                        if self.symbols.get_module_symbols(&source_path).is_empty() {
                            self.collect_definitions(&source_module, &source_path);
                        }

                        // Re-export each item to the current module's namespace
                        for use_item in &use_decl.items {
                            match use_item {
                                UseItem::Simple { name, alias } => {
                                    if let Some(symbol) =
                                        self.symbols.lookup_in_module(&source_path, name)
                                    {
                                        // Re-register the symbol in the current module
                                        let export_name = alias.as_ref().unwrap_or(name);
                                        self.symbols.define(
                                            export_name,
                                            symbol.kind.clone(),
                                            module_path,
                                            symbol.span,
                                        );
                                    } else {
                                        self.errors.push(AnalyzeError::ImportNotFound {
                                            module_path: source_path.clone(),
                                            name: name.clone(),
                                            span: use_decl.span,
                                        });
                                    }
                                }
                                UseItem::EffectFunctions {
                                    effect_name,
                                    functions,
                                } => {
                                    // Re-export effect functions
                                    for func_item in functions {
                                        let lookup_name =
                                            format!("{}::{}", effect_name, func_item.name);
                                        if let Some(symbol) = self
                                            .symbols
                                            .lookup_in_module(&source_path, &lookup_name)
                                        {
                                            let export_name =
                                                func_item.alias.as_ref().unwrap_or(&func_item.name);
                                            self.symbols.define(
                                                export_name,
                                                symbol.kind.clone(),
                                                module_path,
                                                symbol.span,
                                            );
                                        } else {
                                            self.errors.push(AnalyzeError::ImportNotFound {
                                                module_path: source_path.clone(),
                                                name: lookup_name,
                                                span: use_decl.span,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Non-pub use declarations are handled in resolve_imports
                }

                Item::Impl(_) => {
                    // Impl blocks are handled later
                }
            }
        }
    }

    /// Resolve all import declarations in a module
    ///
    /// Handles the new ESM-like import syntax:
    /// `use {items} from "source";`
    fn resolve_imports(
        &mut self,
        module: &Module,
        _module_path: &[String],
    ) -> Result<(), Vec<AnalyzeError>> {
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                // Parse the source string into module path segments
                // e.g., "core:cli" -> ["core", "cli"]
                // e.g., "wasi:filesystem" -> ["wasi", "filesystem"]
                let module_path: Vec<String> = if use_decl.source.contains(':') {
                    use_decl.source.splitn(2, ':').map(String::from).collect()
                } else {
                    // For package names or relative paths, just use as-is for now
                    vec![use_decl.source.clone()]
                };

                // Try to load the module
                let imported_module = match self.resolver.load_module(&module_path) {
                    Ok(m) => m.clone(),
                    Err(e) => {
                        self.errors.push(AnalyzeError::ResolveError(e));
                        continue;
                    }
                };

                // Collect definitions from the imported module (if not already done)
                if self.symbols.get_module_symbols(&module_path).is_empty() {
                    self.collect_definitions(&imported_module, &module_path);
                    // Recursively resolve imports of the imported module
                    // This ensures transitive dependencies are loaded
                    let _ = self.resolve_imports(&imported_module, &module_path);
                }

                // Register each imported item
                for use_item in &use_decl.items {
                    match use_item {
                        UseItem::Simple { name, alias } => {
                            // Simple import: `name` or `name as alias`
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
                            // Effect function import: `Effect::{func1, func2}`
                            for func_item in functions {
                                // Look up as "{Effect}::{function}"
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

    /// Get a reference to the module resolver (for accessing loaded modules)
    pub fn get_resolver(&self) -> &ModuleResolver {
        &self.resolver
    }

    /// Get all loaded modules from the resolver
    pub fn loaded_modules(&self) -> Vec<(&Vec<String>, &Module)> {
        self.resolver
            .loaded_modules()
            .into_iter()
            .filter_map(|path| self.resolver.get_cached(path).map(|m| (path, m)))
            .collect()
    }

    /// Consume the analyzer and return both the symbol table and loaded modules
    pub fn into_parts(self) -> (SymbolTable, std::collections::HashMap<Vec<String>, Module>) {
        (self.symbols, self.resolver.into_modules())
    }
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> Module {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().expect("lexer error");
        let mut parser = Parser::new(tokens);
        parser.parse().expect("parser error")
    }

    #[test]
    fn test_analyze_simple_function() {
        let source = r#"
            fn run() {
                println("hello");
            }
        "#;

        let module = parse(source);
        let mut analyzer = Analyzer::new();
        let result = analyzer.analyze(&module, &[]);

        assert!(result.is_ok());

        // run should be defined
        let run = analyzer.lookup_in_module(&[], "run");
        assert!(run.is_some());
        assert_eq!(run.unwrap().name, "run");
    }

    #[test]
    fn test_analyze_with_imports() {
        let source = r#"
            use {println, Stdout} from "core:cli";

            fn run() with Stdout {
                println("hello");
            }
        "#;

        let module = parse(source);
        let mut analyzer = Analyzer::new();
        let result = analyzer.analyze(&module, &[]);

        assert!(result.is_ok(), "Analysis failed: {:?}", result.err());

        // println should be imported and accessible
        let println = analyzer.lookup("println");
        assert!(println.is_some(), "println not found");
        assert!(println.unwrap().is_builtin_function());

        // Stdout should be imported
        let stdout = analyzer.lookup("Stdout");
        assert!(stdout.is_some(), "Stdout not found");
    }

    #[test]
    fn test_import_not_found() {
        let source = r#"
            use {nonexistent_function} from "core:cli";

            fn run() {
            }
        "#;

        let module = parse(source);
        let mut analyzer = Analyzer::new();
        let result = analyzer.analyze(&module, &[]);

        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(!errors.is_empty());
        assert!(matches!(
            &errors[0],
            AnalyzeError::ImportNotFound { name, .. } if name == "nonexistent_function"
        ));
    }

    #[test]
    fn test_module_not_found() {
        let source = r#"
            use {something} from "nonexistent:module";

            fn run() {
            }
        "#;

        let module = parse(source);
        let mut analyzer = Analyzer::new();
        let result = analyzer.analyze(&module, &[]);

        assert!(result.is_err());
    }
}
