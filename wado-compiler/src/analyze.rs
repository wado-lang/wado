//! Semantic analyzer for Wado
//!
//! The analyzer performs:
//! 1. Module loading and import resolution
//! 2. Symbol table construction
//! 3. Name resolution (binding identifiers to their definitions)

use crate::ast::{Item, Module, UseTarget};
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

        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(std::mem::take(&mut self.errors))
        }
    }

    /// Collect all definitions from a module into the symbol table
    fn collect_definitions(&mut self, module: &Module, module_path: &[String]) {
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    // Check if this is a builtin (functions in core::* are builtins)
                    let is_builtin = module_path.first().map(|s| s == "core").unwrap_or(false);

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

                        // Register as "{Effect}.{method}"
                        let qualified_name = format!("{}.{}", effect.name, method.name);
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

                Item::Record(record) => {
                    // Records are similar to structs
                    let kind = SymbolKind::Struct(StructSymbol {
                        fields: record.fields.iter().map(|f| f.name.clone()).collect(),
                    });

                    self.symbols
                        .define(&record.name, kind, module_path, Some(record.span));
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

                Item::Use(_) | Item::Impl(_) => {
                    // Use declarations are handled in resolve_imports
                    // Impl blocks are handled later
                }
            }
        }
    }

    /// Resolve all import declarations in a module
    fn resolve_imports(
        &mut self,
        module: &Module,
        _module_path: &[String],
    ) -> Result<(), Vec<AnalyzeError>> {
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                // Try to load as a module first
                let (final_path, final_target, imported_module) =
                    match self.resolver.load_module(&use_decl.path) {
                        Ok(m) => {
                            // Module found - use as-is
                            (use_decl.path.clone(), use_decl.target.clone(), m.clone())
                        }
                        Err(_) if use_decl.path.len() > 1 => {
                            // Module not found - try treating last segment as Effect name
                            let mut path_without_last = use_decl.path.clone();
                            let potential_effect = path_without_last.pop().unwrap();

                            match self.resolver.load_module(&path_without_last) {
                                Ok(m) => (
                                    path_without_last,
                                    UseTarget::Effect(potential_effect),
                                    m.clone(),
                                ),
                                Err(e) => {
                                    self.errors.push(AnalyzeError::ResolveError(e));
                                    continue;
                                }
                            }
                        }
                        Err(e) => {
                            self.errors.push(AnalyzeError::ResolveError(e));
                            continue;
                        }
                    };

                // Collect definitions from the imported module (if not already done)
                if !self.symbols.get_module_symbols(&final_path).is_empty() {
                    // Already collected
                } else {
                    self.collect_definitions(&imported_module, &final_path);
                }

                // Register each imported item
                for use_item in &use_decl.items {
                    let lookup_name = match &final_target {
                        UseTarget::Module => {
                            // use module::{item}
                            use_item.name.clone()
                        }
                        UseTarget::Effect(effect_name) => {
                            // use module::Effect::{function}
                            // Look up as "{Effect}.{function}"
                            format!("{}.{}", effect_name, use_item.name)
                        }
                    };

                    if let Some(symbol) = self.symbols.lookup_in_module(&final_path, &lookup_name) {
                        let symbol_id = symbol.id;
                        // Register with the alias if provided, otherwise use the original name
                        let import_name = use_item.alias.as_ref().unwrap_or(&use_item.name);
                        self.symbols.register_import(import_name, symbol_id);
                    } else {
                        self.errors.push(AnalyzeError::ImportNotFound {
                            module_path: final_path.clone(),
                            name: lookup_name,
                            span: use_decl.span,
                        });
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
            fn main() {
                println("hello");
            }
        "#;

        let module = parse(source);
        let mut analyzer = Analyzer::new();
        let result = analyzer.analyze(&module, &[]);

        assert!(result.is_ok());

        // main should be defined
        let main = analyzer.lookup_in_module(&[], "main");
        assert!(main.is_some());
        assert_eq!(main.unwrap().name, "main");
    }

    #[test]
    fn test_analyze_with_imports() {
        let source = r#"
            use core::cli::{println, Stdout};

            fn main() with Stdout {
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
            use core::cli::{nonexistent_function};

            fn main() {
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
            use nonexistent::module::{something};

            fn main() {
            }
        "#;

        let module = parse(source);
        let mut analyzer = Analyzer::new();
        let result = analyzer.analyze(&module, &[]);

        assert!(result.is_err());
    }
}
