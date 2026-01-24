//! Module loader for Wado
//!
//! Loads all modules (entry module + dependencies) upfront before analysis.
//! This enables converting ALL modules to TIR before codegen.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::ast::{Item, Module};
use crate::compiler_host::{Code, CompilerHost, SourceError};
use crate::desugar::desugar_module;
use crate::lexer::Lexer;
use crate::logger::Logger;
use crate::name::{ModuleSource, normalize_module_path, resolve_module_path};
use crate::parser::Parser;
use crate::stdlib;

/// Error that can occur during module loading
#[derive(Debug, Clone)]
pub enum LoadError {
    /// Module was not found
    ModuleNotFound { module_source: ModuleSource },
    /// Error while parsing module
    ParseError {
        module_source: ModuleSource,
        message: String,
    },
    /// Lexer error
    LexError {
        module_source: ModuleSource,
        message: String,
    },
    /// I/O error reading file
    IoError { path: String, message: String },
    /// Unknown module namespace (e.g., "unknown:foo")
    UnknownNamespace { namespace: String },
    /// Invalid module path format (e.g., "foo.wado" without "./" prefix)
    InvalidModulePath { path: String },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::ModuleNotFound { module_source } => {
                write!(f, "module not found: {module_source}")
            }
            LoadError::ParseError {
                module_source,
                message,
            } => {
                write!(f, "parse error in {module_source}: {message}")
            }
            LoadError::LexError {
                module_source,
                message,
            } => {
                write!(f, "lex error in {module_source}: {message}")
            }
            LoadError::IoError { path, message } => {
                write!(f, "error reading '{path}': {message}")
            }
            LoadError::UnknownNamespace { namespace } => {
                write!(
                    f,
                    "unknown module namespace '{namespace}'; expected 'core' or 'wasi'"
                )
            }
            LoadError::InvalidModulePath { path } => {
                write!(
                    f,
                    "invalid module path '{path}'; use './' for local modules or 'namespace:' for library modules"
                )
            }
        }
    }
}

impl std::error::Error for LoadError {}

impl From<SourceError> for LoadError {
    fn from(err: SourceError) -> Self {
        match err {
            SourceError::NotFound { path } => LoadError::ModuleNotFound {
                module_source: ModuleSource::Local { path },
            },
            SourceError::IoError { path, message } => LoadError::IoError { path, message },
            SourceError::NetworkError { url, message } => LoadError::IoError { path: url, message },
        }
    }
}

/// Result of loading all modules
pub struct LoadResult {
    /// All loaded modules (module source -> desugared AST)
    pub modules: HashMap<ModuleSource, Module>,
    /// The entry module source
    pub entry_module_source: ModuleSource,
    /// Modules that were implicitly loaded (not from user imports)
    pub implicit_modules: HashSet<ModuleSource>,
}

use crate::compiler_host::LogLevel;

/// Module loader
///
/// Loads all modules upfront before analysis and codegen.
/// Uses a `CompilerHost` for I/O operations.
pub struct ModuleLoader<'a, H: CompilerHost> {
    /// Host for I/O operations
    host: &'a H,
    /// Log level for filtering messages
    log_level: LogLevel,
    /// Cache of already parsed modules
    loaded: HashMap<ModuleSource, Module>,
    /// Set of modules currently being loaded (for cycle detection during collection)
    loading: HashSet<ModuleSource>,
    /// Modules that were implicitly loaded
    implicit_modules: HashSet<ModuleSource>,
}

impl<'a, H: CompilerHost> ModuleLoader<'a, H> {
    /// Create a new module loader with the given host and log level
    pub fn new(host: &'a H, log_level: LogLevel) -> Self {
        Self {
            host,
            log_level,
            loaded: HashMap::new(),
            loading: HashSet::new(),
            implicit_modules: HashSet::new(),
        }
    }

    /// Create a logger for emitting diagnostics
    fn logger(&self) -> Logger<'_, H> {
        Logger::new(self.host, self.log_level)
    }

    /// Load all modules starting from the entry source
    ///
    /// This loads the entry module and all its transitive dependencies.
    /// It also loads implicit modules (core:prelude, core:internal, core:builtin).
    ///
    /// # Arguments
    /// * `entry_source` - Source code of the entry module
    /// * `entry_filename` - Optional filename of the entry module (for error messages)
    pub async fn load_all(
        mut self,
        entry_source: &str,
        entry_filename: Option<&str>,
    ) -> Result<LoadResult, LoadError> {
        self.logger().span_start("load");

        // Parse entry module
        let entry_module_source = if let Some(filename) = entry_filename {
            ModuleSource::entry_point_with_filename(filename)
        } else {
            ModuleSource::entry_point()
        };
        let entry_module = self.parse_source(entry_source, &entry_module_source)?;

        // Desugar and store entry module
        let desugared_entry = desugar_module(&entry_module);
        self.loaded
            .insert(entry_module_source.clone(), desugared_entry);

        // Collect imports from entry module
        let mut pending: VecDeque<(ModuleSource, ModuleSource)> = VecDeque::new();
        self.collect_imports(&entry_module, &entry_module_source, &mut pending)?;

        // Load all dependencies iteratively
        while let Some((from_module_source, module_source)) = pending.pop_front() {
            // Skip if already loaded
            if self.loaded.contains_key(&module_source) {
                continue;
            }

            // Skip if currently loading (cycle)
            if self.loading.contains(&module_source) {
                continue;
            }

            // Mark as loading
            self.loading.insert(module_source.clone());

            // Load and parse the module
            let source = self.get_source(&module_source, &from_module_source).await?;
            let module = self.parse_source(&source, &module_source)?;

            // Collect its imports
            self.collect_imports(&module, &module_source, &mut pending)?;

            // Desugar and store
            let desugared = desugar_module(&module);
            self.loaded.insert(module_source.clone(), desugared);
            self.loading.remove(&module_source);
        }

        // Load implicit modules (for compiler-generated code)
        self.load_implicit_modules().await?;

        self.logger().span_end("load");

        Ok(LoadResult {
            modules: self.loaded,
            entry_module_source,
            implicit_modules: self.implicit_modules,
        })
    }

    /// Collect import paths from a module's use declarations
    fn collect_imports(
        &self,
        module: &Module,
        from_module_source: &ModuleSource,
        pending: &mut VecDeque<(ModuleSource, ModuleSource)>,
    ) -> Result<(), LoadError> {
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                let resolved = self.resolve_import(from_module_source, &use_decl.source)?;
                pending.push_back((from_module_source.clone(), resolved));
            }
        }
        Ok(())
    }

    /// Load implicit modules required by the compiler
    async fn load_implicit_modules(&mut self) -> Result<(), LoadError> {
        let implicit_module_sources = [
            ModuleSource::Core {
                name: "prelude".to_string(),
            },
            ModuleSource::Core {
                name: "internal".to_string(),
            },
            ModuleSource::Core {
                name: "builtin".to_string(),
            },
        ];

        for module_source in implicit_module_sources {
            if self.loaded.contains_key(&module_source) {
                continue;
            }

            // Try to load - errors are warnings for implicit modules
            match self
                .get_source(&module_source, &ModuleSource::entry_point())
                .await
            {
                Ok(source) => {
                    match self.parse_source(&source, &module_source) {
                        Ok(module) => {
                            // Collect imports from implicit module
                            let mut pending = VecDeque::new();
                            // Implicit modules should only use valid import paths
                            if let Err(e) =
                                self.collect_imports(&module, &module_source, &mut pending)
                            {
                                self.logger().warn(
                                    Code::ModuleParseError,
                                    format!("failed to collect imports from implicit module: {e}"),
                                );
                                continue;
                            }

                            // Load any dependencies of implicit modules
                            while let Some((from_module_source, dep_module_source)) =
                                pending.pop_front()
                            {
                                if self.loaded.contains_key(&dep_module_source) {
                                    continue;
                                }
                                if let Ok(dep_source) = self
                                    .get_source(&dep_module_source, &from_module_source)
                                    .await
                                    && let Ok(dep_module) =
                                        self.parse_source(&dep_source, &dep_module_source)
                                    && self
                                        .collect_imports(
                                            &dep_module,
                                            &dep_module_source,
                                            &mut pending,
                                        )
                                        .is_ok()
                                {
                                    let desugared = desugar_module(&dep_module);
                                    self.loaded.insert(dep_module_source, desugared);
                                }
                            }

                            let desugared = desugar_module(&module);
                            self.loaded.insert(module_source.clone(), desugared);
                            self.implicit_modules.insert(module_source);
                        }
                        Err(e) => {
                            self.logger().warn(
                                Code::ModuleParseError,
                                format!("failed to parse implicit module: {e}"),
                            );
                        }
                    }
                }
                Err(e) => {
                    self.logger().warn(
                        Code::ModuleNotFound,
                        format!("failed to load implicit module: {e}"),
                    );
                }
            }
        }

        Ok(())
    }

    /// Resolve an import source relative to the importing module
    fn resolve_import(
        &self,
        from_module_source: &ModuleSource,
        import_source: &str,
    ) -> Result<ModuleSource, LoadError> {
        // Handle known namespaces
        if let Some(name) = import_source.strip_prefix("core:") {
            return Ok(ModuleSource::Core {
                name: name.to_string(),
            });
        }
        if let Some(interface) = import_source.strip_prefix("wasi:") {
            return Ok(ModuleSource::Wasi {
                interface: interface.to_string(),
            });
        }

        // Handle remote modules (http:// or https://)
        if import_source.starts_with("https://") || import_source.starts_with("http://") {
            return Ok(ModuleSource::Remote {
                url: import_source.to_string(),
            });
        }

        // Handle local modules (./ or ../)
        if import_source.starts_with("./") || import_source.starts_with("../") {
            // For relative imports, resolve against from_module_source
            if let ModuleSource::Local { path: from_file } = from_module_source {
                let resolved = resolve_module_path(from_file, import_source);
                return Ok(ModuleSource::Local { path: resolved });
            }
            if let ModuleSource::Remote { url: from_url } = from_module_source {
                let resolved = resolve_module_path(from_url, import_source);
                return Ok(ModuleSource::Remote { url: resolved });
            }
            // Entry point or stdlib: treat as relative to project root
            let canonical = normalize_module_path(import_source);
            return Ok(ModuleSource::Local { path: canonical });
        }

        // Check for unknown namespace pattern (xxx:yyy)
        if let Some(colon_pos) = import_source.find(':') {
            let namespace = &import_source[..colon_pos];
            // Ensure it's a valid identifier-like namespace (not a URL scheme)
            if !namespace.is_empty()
                && namespace
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return Err(LoadError::UnknownNamespace {
                    namespace: namespace.to_string(),
                });
            }
        }

        // Invalid module path (no recognized prefix)
        Err(LoadError::InvalidModulePath {
            path: import_source.to_string(),
        })
    }

    /// Get source code for a module
    async fn get_source(
        &self,
        module_source: &ModuleSource,
        _from_module_source: &ModuleSource,
    ) -> Result<String, LoadError> {
        match module_source {
            ModuleSource::Local { path } => {
                self.host.load_source(path).await.map_err(LoadError::from)
            }
            ModuleSource::Remote { url } => {
                self.host.load_source(url).await.map_err(LoadError::from)
            }
            ModuleSource::Core { name } => {
                let import_path = format!("core:{name}");
                if let Some(source) = stdlib::get_stdlib_module(&import_path) {
                    Ok(source.to_string())
                } else {
                    Err(LoadError::ModuleNotFound {
                        module_source: module_source.clone(),
                    })
                }
            }
            ModuleSource::Wasi { interface } => {
                let import_path = format!("wasi:{interface}");
                if let Some(source) = stdlib::get_stdlib_module(&import_path) {
                    Ok(source.to_string())
                } else {
                    Err(LoadError::ModuleNotFound {
                        module_source: module_source.clone(),
                    })
                }
            }
            ModuleSource::EntryPoint { .. } => {
                // Entry point source is provided directly, not loaded from host
                Err(LoadError::ModuleNotFound {
                    module_source: module_source.clone(),
                })
            }
        }
    }

    /// Parse source code into a module AST
    fn parse_source(
        &self,
        source: &str,
        module_source: &ModuleSource,
    ) -> Result<Module, LoadError> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().map_err(|e| LoadError::LexError {
            module_source: module_source.clone(),
            message: format!(
                "line {}, column {}: {}",
                e.span.line, e.span.column, e.message
            ),
        })?;
        let (data_section, _comments, shebang) = lexer.into_parts();

        let mut parser = Parser::with_metadata(tokens, shebang, data_section);
        parser.parse().map_err(|e| LoadError::ParseError {
            module_source: module_source.clone(),
            message: format!(
                "line {}, column {}: {}",
                e.span.line, e.span.column, e.message
            ),
        })
    }
}
