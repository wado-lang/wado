//! Module loader for Wado
//!
//! Loads all modules (entry module + dependencies) upfront before analysis.
//! This enables converting ALL modules to TIR before codegen.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::ast::{Item, Module};
use crate::compiler_host::{CompilerHost, Diagnostic, ErrorCode, Severity, SourceError};
use crate::desugar::desugar_module;
use crate::lexer::Lexer;
use crate::name::{normalize_module_path, resolve_module_path};
use crate::parser::Parser;
use crate::stdlib;

/// Error that can occur during module loading
#[derive(Debug, Clone)]
pub enum LoadError {
    /// Module was not found
    ModuleNotFound { path: Vec<String> },
    /// Error while parsing module
    ParseError { path: Vec<String>, message: String },
    /// Lexer error
    LexError { path: Vec<String>, message: String },
    /// I/O error reading file
    IoError { path: String, message: String },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::ModuleNotFound { path } => {
                write!(f, "module not found: {}", path.join("::"))
            }
            LoadError::ParseError { path, message } => {
                write!(f, "parse error in {}: {}", path.join("::"), message)
            }
            LoadError::LexError { path, message } => {
                write!(f, "lex error in {}: {}", path.join("::"), message)
            }
            LoadError::IoError { path, message } => {
                write!(f, "error reading '{}': {}", path, message)
            }
        }
    }
}

impl std::error::Error for LoadError {}

impl From<SourceError> for LoadError {
    fn from(err: SourceError) -> Self {
        match err {
            SourceError::NotFound { path } => LoadError::ModuleNotFound { path: vec![path] },
            SourceError::IoError { path, message } => LoadError::IoError { path, message },
            SourceError::NetworkError { url, message } => LoadError::IoError { path: url, message },
        }
    }
}

/// Result of loading all modules
pub struct LoadResult {
    /// All loaded modules (module path -> desugared AST)
    pub modules: HashMap<Vec<String>, Module>,
    /// The entry module path
    pub entry_path: Vec<String>,
    /// Modules that were implicitly loaded (not from user imports)
    pub implicit_modules: HashSet<Vec<String>>,
}

/// Module loader
///
/// Loads all modules upfront before analysis and codegen.
/// Uses a CompilerHost for I/O operations.
pub struct ModuleLoader {
    /// Cache of already parsed modules
    loaded: HashMap<Vec<String>, Module>,
    /// Set of modules currently being loaded (for cycle detection during collection)
    loading: HashSet<Vec<String>>,
    /// Modules that were implicitly loaded
    implicit_modules: HashSet<Vec<String>>,
}

impl ModuleLoader {
    /// Create a new module loader
    pub fn new() -> Self {
        Self {
            loaded: HashMap::new(),
            loading: HashSet::new(),
            implicit_modules: HashSet::new(),
        }
    }

    /// Load all modules starting from the entry source using a CompilerHost
    ///
    /// This loads the entry module and all its transitive dependencies.
    /// It also loads implicit modules (core:prelude, core:internal, core:builtin).
    ///
    /// # Arguments
    /// * `entry_source` - Source code of the entry module
    /// * `host` - CompilerHost for loading user modules and emitting diagnostics
    pub async fn load_all<H: CompilerHost>(
        mut self,
        entry_source: &str,
        host: &H,
    ) -> Result<LoadResult, LoadError> {
        // Parse entry module
        let entry_module = self.parse_source(entry_source, &[])?;
        let entry_path = vec![];

        // Desugar and store entry module
        let desugared_entry = desugar_module(&entry_module);
        self.loaded.insert(entry_path.clone(), desugared_entry);

        // Collect imports from entry module
        let mut pending: VecDeque<(Vec<String>, Vec<String>)> = VecDeque::new();
        self.collect_imports(&entry_module, &entry_path, &mut pending);

        // Load all dependencies iteratively
        while let Some((from_path, module_path)) = pending.pop_front() {
            // Skip if already loaded
            if self.loaded.contains_key(&module_path) {
                continue;
            }

            // Skip if currently loading (cycle)
            if self.loading.contains(&module_path) {
                continue;
            }

            // Mark as loading
            self.loading.insert(module_path.clone());

            // Load and parse the module
            let source = self
                .get_source_with_host(&module_path, &from_path, host)
                .await?;
            let module = self.parse_source(&source, &module_path)?;

            // Collect its imports
            self.collect_imports(&module, &module_path, &mut pending);

            // Desugar and store
            let desugared = desugar_module(&module);
            self.loaded.insert(module_path.clone(), desugared);
            self.loading.remove(&module_path);
        }

        // Load implicit modules (for compiler-generated code)
        self.load_implicit_modules(host).await?;

        Ok(LoadResult {
            modules: self.loaded,
            entry_path,
            implicit_modules: self.implicit_modules,
        })
    }

    /// Collect import paths from a module's use declarations
    fn collect_imports(
        &self,
        module: &Module,
        from_path: &[String],
        pending: &mut VecDeque<(Vec<String>, Vec<String>)>,
    ) {
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                let resolved_path = self.resolve_import(from_path, &use_decl.source);
                pending.push_back((from_path.to_vec(), resolved_path));
            }
        }
    }

    /// Load implicit modules required by the compiler
    async fn load_implicit_modules<H: CompilerHost>(&mut self, host: &H) -> Result<(), LoadError> {
        let implicit_paths = [
            vec!["core".to_string(), "prelude".to_string()],
            vec!["core".to_string(), "internal".to_string()],
            vec!["core".to_string(), "builtin".to_string()],
        ];

        for path in implicit_paths {
            if self.loaded.contains_key(&path) {
                continue;
            }

            // Try to load - errors are warnings for implicit modules
            match self.get_source_with_host(&path, &[], host).await {
                Ok(source) => {
                    match self.parse_source(&source, &path) {
                        Ok(module) => {
                            // Collect imports from implicit module
                            let mut pending = VecDeque::new();
                            self.collect_imports(&module, &path, &mut pending);

                            // Load any dependencies of implicit modules
                            while let Some((from_path, dep_path)) = pending.pop_front() {
                                if self.loaded.contains_key(&dep_path) {
                                    continue;
                                }
                                if let Ok(dep_source) =
                                    self.get_source_with_host(&dep_path, &from_path, host).await
                                    && let Ok(dep_module) =
                                        self.parse_source(&dep_source, &dep_path)
                                {
                                    self.collect_imports(&dep_module, &dep_path, &mut pending);
                                    let desugared = desugar_module(&dep_module);
                                    self.loaded.insert(dep_path, desugared);
                                }
                            }

                            let desugared = desugar_module(&module);
                            self.loaded.insert(path.clone(), desugared);
                            self.implicit_modules.insert(path);
                        }
                        Err(e) => {
                            host.emit_diagnostic(Diagnostic {
                                severity: Severity::Warning,
                                code: ErrorCode::ModuleParseError,
                                message: format!("failed to parse implicit module: {e}"),
                                span: None,
                            })
                            .await;
                        }
                    }
                }
                Err(e) => {
                    host.emit_diagnostic(Diagnostic {
                        severity: Severity::Warning,
                        code: ErrorCode::ModuleNotFound,
                        message: format!("failed to load implicit module: {e}"),
                        span: None,
                    })
                    .await;
                }
            }
        }

        Ok(())
    }

    /// Resolve an import source relative to the importing module
    fn resolve_import(&self, from_path: &[String], import_source: &str) -> Vec<String> {
        // Handle special prefixes
        if import_source.starts_with("core:")
            || import_source.starts_with("wasi:")
            || import_source.starts_with("https://")
            || import_source.starts_with("http://")
        {
            if import_source.contains(':') {
                return import_source.splitn(2, ':').map(String::from).collect();
            }
            return vec![import_source.to_string()];
        }

        // For relative imports, resolve against from_path
        if !from_path.is_empty() {
            let from_file = &from_path[0];
            if from_file.starts_with("./") || from_file.starts_with("../") {
                let resolved = resolve_module_path(from_file, import_source);
                return vec![resolved];
            }
        }

        // Fallback: treat as relative to project root
        let canonical = normalize_module_path(import_source);
        vec![canonical]
    }

    /// Get source code for a module using CompilerHost
    async fn get_source_with_host<H: CompilerHost>(
        &self,
        module_path: &[String],
        _from_path: &[String],
        host: &H,
    ) -> Result<String, LoadError> {
        // Check for local file path - delegate to host
        if !module_path.is_empty() {
            let first = &module_path[0];
            if first.starts_with("./") || first.starts_with("../") || first.ends_with(".wado") {
                return host.load_source(first).await.map_err(LoadError::from);
            }
        }

        // Convert to import path (e.g., ["core", "cli"] -> "core:cli")
        let import_path = if module_path.len() == 2 {
            format!("{}:{}", module_path[0], module_path[1])
        } else {
            module_path.join(":")
        };

        // Try embedded stdlib (handled by compiler, not host)
        if let Some(source) = stdlib::get_stdlib_module(&import_path) {
            return Ok(source.to_string());
        }

        Err(LoadError::ModuleNotFound {
            path: module_path.to_vec(),
        })
    }

    /// Parse source code into a module AST
    fn parse_source(&self, source: &str, module_path: &[String]) -> Result<Module, LoadError> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().map_err(|e| LoadError::LexError {
            path: module_path.to_vec(),
            message: format!(
                "line {}, column {}: {}",
                e.span.line, e.span.column, e.message
            ),
        })?;
        let (data_section, _comments, shebang) = lexer.into_parts();

        let mut parser = Parser::with_metadata(tokens, shebang, data_section);
        parser.parse().map_err(|e| LoadError::ParseError {
            path: module_path.to_vec(),
            message: format!(
                "line {}, column {}: {}",
                e.span.line, e.span.column, e.message
            ),
        })
    }
}

impl Default for ModuleLoader {
    fn default() -> Self {
        Self::new()
    }
}
