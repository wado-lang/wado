//! Module loader for Wado
//!
//! Loads all modules (entry module + dependencies) upfront before analysis.
//! This enables converting ALL modules to TIR before codegen.

use std::collections::VecDeque;

use indexmap::IndexSet;

use indexmap::IndexMap;

use crate::ast::{Expr, Item, Literal, Module};
use crate::bind;
use crate::compiler_host::{CompilerHost, SourceError};
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
        line: usize,
        column: usize,
    },
    /// Lexer error
    LexError {
        module_source: ModuleSource,
        message: String,
        line: usize,
        column: usize,
    },
    /// Bind error (scope checking)
    BindError {
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
                line,
                column,
            } => {
                write!(
                    f,
                    "parse error in {module_source}: line {line}, column {column}: {message}"
                )
            }
            LoadError::LexError {
                module_source,
                message,
                line,
                column,
            } => {
                write!(
                    f,
                    "lex error in {module_source}: line {line}, column {column}: {message}"
                )
            }
            LoadError::BindError {
                module_source,
                message,
            } => {
                write!(f, "bind error in {module_source}: {message}")
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

impl From<LoadError> for crate::compiler_host::Diagnostic {
    fn from(e: LoadError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        match e {
            LoadError::LexError {
                module_source,
                message,
                line,
                column,
            } => Self {
                severity: Severity::Error,
                code: Code::InvalidSyntax,
                message: format!("lexer error: {message}"),
                span: Some(DiagnosticSpan {
                    file: module_source.diagnostic_filename(),
                    line,
                    column,
                    end_line: None,
                    end_column: None,
                }),
            },
            LoadError::ParseError {
                module_source,
                message,
                line,
                column,
            } => Self {
                severity: Severity::Error,
                code: Code::InvalidSyntax,
                message: format!("parse error: {message}"),
                span: Some(DiagnosticSpan {
                    file: module_source.diagnostic_filename(),
                    line,
                    column,
                    end_line: None,
                    end_column: None,
                }),
            },
            LoadError::BindError {
                module_source,
                message,
            } => Self {
                severity: Severity::Error,
                code: Code::DuplicateDefinition,
                message: format!("bind error in {module_source}: {message}"),
                span: None,
            },
            ref other => Self {
                severity: Severity::Error,
                code: Code::ModuleNotFound,
                message: other.to_string(),
                span: None,
            },
        }
    }
}

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
    pub modules: IndexMap<ModuleSource, Module>,
    /// The entry module source
    pub entry_module_source: ModuleSource,
    /// Original (non-desugared) entry module AST, for tooling
    pub entry_ast: Module,
    /// Modules that were implicitly loaded (not from user imports)
    pub implicit_modules: IndexSet<ModuleSource>,
    /// Included file contents from `#include_str` and `#include_bytes`.
    /// Key is `(module_source_display, raw_path)`, value is raw bytes.
    pub included_files: IndexMap<[String; 2], Vec<u8>>,
}

use crate::compiler_host::LogLevel;

/// Module loader
///
/// Loads all modules upfront before analysis and codegen.
/// Uses a `CompilerHost` for I/O operations.
/// Cached desugared AST modules for all core stdlib modules.
///
/// Each module is parsed, bound, and desugared exactly once per process.
fn cached_core_stdlib() -> &'static IndexMap<ModuleSource, Module> {
    use std::sync::OnceLock;

    static CACHE: OnceLock<IndexMap<ModuleSource, Module>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let core_modules: &[(&str, &str)] = &[
            ("allocator", stdlib::CORE_ALLOCATOR),
            ("builtin", stdlib::CORE_BUILTIN),
            ("cli", stdlib::CORE_CLI),
            ("collections", stdlib::CORE_COLLECTIONS),
            ("internal", stdlib::CORE_INTERNAL),
            ("prelude", stdlib::CORE_PRELUDE),
            ("prelude/array.wado", stdlib::CORE_PRELUDE_ARRAY),
            ("prelude/format.wado", stdlib::CORE_PRELUDE_FORMAT),
            ("prelude/fpfmt.wado", stdlib::CORE_PRELUDE_FPFMT),
            ("prelude/int128.wado", stdlib::CORE_PRELUDE_INT128),
            ("prelude/primitives.wado", stdlib::CORE_PRELUDE_PRIMITIVES),
            ("prelude/string.wado", stdlib::CORE_PRELUDE_STRING),
            ("prelude/traits.wado", stdlib::CORE_PRELUDE_TRAITS),
            ("prelude/types.wado", stdlib::CORE_PRELUDE_TYPES),
            ("zlib", stdlib::CORE_ZLIB),
        ];
        let mut cache = IndexMap::with_capacity(core_modules.len());
        for &(name, source) in core_modules {
            let module_source = ModuleSource::Core {
                name: name.to_string(),
            };
            let mut lexer = Lexer::new(source);
            let tokens = lexer
                .tokenize()
                .unwrap_or_else(|e| panic!("lexer error in core:{name}: {e:?}"));
            let (data_section, _comments, shebang) = lexer.into_parts();
            let mut parser = Parser::with_metadata(tokens, shebang, data_section);
            let ast = parser
                .parse()
                .unwrap_or_else(|e| panic!("parser error in core:{name}: {e:?}"));
            {
                let bind_host = crate::compiler_host::InMemoryCompilerHost::new();
                let bind_logger = Logger::new(&bind_host, LogLevel::Off);
                bind::bind_module(&ast, &bind_logger).unwrap_or_else(|_| {
                    let msgs: Vec<String> = bind_host
                        .diagnostics()
                        .iter()
                        .map(|d| d.message.clone())
                        .collect();
                    panic!("bind error in core:{name}: {}", msgs.join("; "));
                });
            }
            let desugared = desugar_module(&ast);
            cache.insert(module_source, desugared);
        }
        cache
    })
}

pub struct ModuleLoader<'a, H: CompilerHost> {
    /// Host for I/O operations
    host: &'a H,
    /// Log level for filtering messages
    log_level: LogLevel,
    /// Logger for timing spans and diagnostics
    logger: Logger<'a, H>,
    /// Cache of already parsed modules
    loaded: IndexMap<ModuleSource, Module>,
    /// Set of modules currently being loaded (for cycle detection during collection)
    loading: IndexSet<ModuleSource>,
    /// Modules that were implicitly loaded
    implicit_modules: IndexSet<ModuleSource>,
}

impl<'a, H: CompilerHost> ModuleLoader<'a, H> {
    /// Create a new module loader with the given host and log level
    pub fn new(host: &'a H, log_level: LogLevel) -> Self {
        Self {
            host,
            log_level,
            logger: Logger::new(host, log_level),
            loaded: IndexMap::new(),
            loading: IndexSet::new(),
            implicit_modules: IndexSet::new(),
        }
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
        // Parse, bind, and desugar entry module
        // Use "<stdin>" as synthetic filename when no filename is provided (e.g., REPL, embedded code)
        let entry_module_source =
            ModuleSource::entry_point_with_filename(entry_filename.unwrap_or("<stdin>"));

        let entry_name = entry_module_source.to_string();
        self.logger.span_start(&format!("load {entry_name}"));

        // Parse first to collect imports before binding
        let entry_ast = {
            let _span = self.logger.span(&format!("parse {entry_name}"));
            self.parse_source(entry_source, &entry_module_source)?
        };

        // Collect imports from entry module (before bind/desugar)
        let mut pending: VecDeque<(ModuleSource, ModuleSource)> = VecDeque::new();
        self.collect_imports(&entry_ast, &entry_module_source, &mut pending)?;

        // Bind and desugar, then store (keep original AST for tooling)
        {
            let _span = self.logger.span(&format!("bind {entry_name}"));
            self.bind_module(&entry_ast, &entry_module_source)?;
        }
        let desugared_entry = {
            let _span = self.logger.span(&format!("desugar {entry_name}"));
            desugar_module(&entry_ast)
        };
        self.loaded
            .insert(entry_module_source.clone(), desugared_entry);
        let entry_ast_original = entry_ast;

        self.logger.span_end(&format!("load {entry_name}"));

        // Load all dependencies iteratively
        let core_cache = cached_core_stdlib();
        while let Some((from_module_source, module_source)) = pending.pop_front() {
            // Skip if already loaded
            if self.loaded.contains_key(&module_source) {
                continue;
            }

            // Skip if currently loading (cycle)
            if self.loading.contains(&module_source) {
                continue;
            }

            let mod_name = module_source.to_string();

            // Use cached desugared module for core stdlib
            if let Some(cached) = core_cache.get(&module_source) {
                let _span = self.logger.span(&format!("load {mod_name} (cached)"));
                self.collect_imports(cached, &module_source, &mut pending)?;
                self.loaded.insert(module_source, cached.clone());
                continue;
            }

            // Mark as loading
            self.loading.insert(module_source.clone());

            self.logger.span_start(&format!("load {mod_name}"));

            // Load and parse the module
            let source = self.get_source(&module_source, &from_module_source).await?;
            let ast = {
                let _span = self.logger.span(&format!("parse {mod_name}"));
                self.parse_source(&source, &module_source)?
            };

            // Collect its imports (before bind/desugar)
            self.collect_imports(&ast, &module_source, &mut pending)?;

            // Bind, desugar, and store
            {
                let _span = self.logger.span(&format!("bind {mod_name}"));
                self.bind_module(&ast, &module_source)?;
            }
            let desugared = {
                let _span = self.logger.span(&format!("desugar {mod_name}"));
                desugar_module(&ast)
            };
            self.loaded.insert(module_source.clone(), desugared);
            self.loading.swap_remove(&module_source);

            self.logger.span_end(&format!("load {mod_name}"));
        }

        // Load implicit modules (for compiler-generated code)
        self.load_implicit_modules()?;

        // Collect and load files referenced by #include_str / #include_bytes
        let included_files = self.load_included_files().await?;

        Ok(LoadResult {
            modules: self.loaded,
            entry_module_source,
            entry_ast: entry_ast_original,
            implicit_modules: self.implicit_modules,
            included_files,
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
    fn load_implicit_modules(&mut self) -> Result<(), LoadError> {
        let cache = cached_core_stdlib();

        let implicit_module_sources = [
            ModuleSource::builtin(),
            ModuleSource::string(),
            ModuleSource::prelude(),
            ModuleSource::internal(),
            ModuleSource::allocator(),
        ];

        for module_source in implicit_module_sources {
            if self.loaded.contains_key(&module_source) {
                continue;
            }

            if let Some(cached) = cache.get(&module_source) {
                // Load transitive dependencies from cache
                let mut pending = VecDeque::new();
                if self
                    .collect_imports(cached, &module_source, &mut pending)
                    .is_err()
                {
                    continue;
                }

                while let Some((_from_ms, dep_ms)) = pending.pop_front() {
                    if self.loaded.contains_key(&dep_ms) {
                        continue;
                    }
                    if let Some(dep_cached) = cache.get(&dep_ms) {
                        let _ = self.collect_imports(dep_cached, &dep_ms, &mut pending);
                        self.loaded.insert(dep_ms, dep_cached.clone());
                    }
                }

                self.loaded.insert(module_source.clone(), cached.clone());
                self.implicit_modules.insert(module_source);
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
        // Top-level: "core:cli" → Core { name: "cli" }
        // Sub-module: "core:prelude/traits.wado" → Core { name: "prelude/traits.wado" }
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
                let bytes = self.host.load_source(path).await.map_err(LoadError::from)?;
                String::from_utf8(bytes).map_err(|_| LoadError::IoError {
                    path: path.clone(),
                    message: "file is not valid UTF-8".to_string(),
                })
            }
            ModuleSource::Remote { url } => {
                let bytes = self.host.load_source(url).await.map_err(LoadError::from)?;
                String::from_utf8(bytes).map_err(|_| LoadError::IoError {
                    path: url.clone(),
                    message: "file is not valid UTF-8".to_string(),
                })
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
            message: e.message,
            line: e.span.line,
            column: e.span.column,
        })?;
        let (data_section, _comments, shebang) = lexer.into_parts();

        let mut parser = Parser::with_metadata(tokens, shebang, data_section);
        parser.parse().map_err(|e| LoadError::ParseError {
            module_source: module_source.clone(),
            message: e.message,
            line: e.span.line,
            column: e.span.column,
        })
    }

    /// Bind a module (local name resolution and scope checking)
    fn bind_module(&self, module: &Module, module_source: &ModuleSource) -> Result<(), LoadError> {
        // Bind errors are emitted directly to the host via Logger.
        // We use a temporary logger per module so error counting is per-module.
        let logger = Logger::new(self.host, self.log_level);
        logger.set_file(module_source.diagnostic_filename());
        bind::bind_module(module, &logger).map_err(|_bail| {
            let error_count = logger.error_count();
            LoadError::BindError {
                module_source: module_source.clone(),
                message: format!("{error_count} bind error(s)"),
            }
        })
    }

    /// Scan all loaded modules for `#include_str`/`#include_bytes` and load referenced files.
    async fn load_included_files(&self) -> Result<IndexMap<[String; 2], Vec<u8>>, LoadError> {
        // Collect (module_source, raw_path) pairs
        let mut pairs: IndexSet<[String; 2]> = IndexSet::new();
        for (module_source, module) in &self.loaded {
            let mut raw_paths = IndexSet::new();
            collect_include_paths(module, &mut raw_paths);
            let ms_str = module_source.to_string();
            for raw_path in raw_paths {
                pairs.insert([ms_str.clone(), raw_path]);
            }
        }
        let mut included = IndexMap::new();
        for pair in pairs {
            let [ref ms_str, ref raw_path] = pair;
            // Resolve path relative to the module source's directory
            let resolved = self.resolve_include_path(ms_str, raw_path);
            let bytes = self
                .host
                .load_source(&resolved)
                .await
                .map_err(LoadError::from)?;
            included.insert(pair, bytes);
        }
        Ok(included)
    }

    /// Resolve an include path relative to the module that contains it.
    ///
    /// Unlike `resolve_module_path` (which normalizes to `./`-prefixed relative paths),
    /// this preserves absolute path prefixes so that `CompilerHost::load_source` receives
    /// the correct path.
    fn resolve_include_path(&self, module_source_str: &str, raw_path: &str) -> String {
        if (raw_path.starts_with("./") || raw_path.starts_with("../"))
            && let Some(dir_end) = module_source_str.rfind('/')
        {
            let dir = &module_source_str[..dir_end];
            let stripped = raw_path.strip_prefix("./").unwrap_or(raw_path);
            return format!("{dir}/{stripped}");
        }
        raw_path.to_string()
    }
}

/// Collect all include paths (`#include_str`/`#include_bytes`) from a module's AST.
fn collect_include_paths(module: &Module, paths: &mut IndexSet<String>) {
    for item in &module.items {
        collect_include_paths_item(item, paths);
    }
}

fn collect_include_paths_item(item: &Item, paths: &mut IndexSet<String>) {
    match item {
        Item::Function(f) => {
            if let Some(body) = &f.body {
                collect_include_paths_block(body, paths);
            }
        }
        Item::Global(g) => collect_include_paths_expr(&g.initializer, paths),
        Item::Test(t) => collect_include_paths_block(&t.body, paths),
        Item::Impl(imp) => {
            for method in &imp.methods {
                if let Some(body) = &method.body {
                    collect_include_paths_block(body, paths);
                }
            }
        }
        _ => {}
    }
}

fn collect_include_paths_block(block: &crate::ast::Block, paths: &mut IndexSet<String>) {
    for stmt in &block.stmts {
        collect_include_paths_stmt(stmt, paths);
    }
}

fn collect_include_paths_stmt(stmt: &crate::ast::Stmt, paths: &mut IndexSet<String>) {
    use crate::ast::Stmt;
    match stmt {
        Stmt::Expr(s) => collect_include_paths_expr(&s.expr, paths),
        Stmt::Return(s) => {
            if let Some(expr) = &s.value {
                collect_include_paths_expr(expr, paths);
            }
        }
        Stmt::TaskReturn(s) => collect_include_paths_expr(&s.value, paths),
        Stmt::Let(s) => collect_include_paths_expr(&s.value, paths),
        Stmt::For(s) => {
            if let Some(init) = &s.init {
                collect_include_paths_stmt(init, paths);
            }
            if let Some(cond) = &s.condition {
                collect_include_paths_condition(cond, paths);
            }
            if let Some(update) = &s.update {
                collect_include_paths_expr(update, paths);
            }
            collect_include_paths_block(&s.body, paths);
        }
        Stmt::ForOf(s) => {
            collect_include_paths_expr(&s.iterable, paths);
            collect_include_paths_block(&s.body, paths);
        }
        Stmt::While(s) => {
            collect_include_paths_condition(&s.condition, paths);
            collect_include_paths_block(&s.body, paths);
        }
        Stmt::Loop(s) => collect_include_paths_block(&s.body, paths),
        Stmt::If(s) => {
            collect_include_paths_condition(&s.condition, paths);
            collect_include_paths_block(&s.then_block, paths);
            if let Some(else_block) = &s.else_block {
                collect_include_paths_block(else_block, paths);
            }
        }
        Stmt::Assert(s) => {
            collect_include_paths_expr(&s.condition, paths);
            if let Some(msg) = &s.message {
                collect_include_paths_expr(msg, paths);
            }
        }
        Stmt::LabeledBlock(s) => collect_include_paths_block(&s.block, paths),
        Stmt::Break(_) | Stmt::Continue(_) => {}
    }
}

fn collect_include_paths_condition(cond: &crate::ast::Condition, paths: &mut IndexSet<String>) {
    match cond {
        crate::ast::Condition::Expr(e) => collect_include_paths_expr(e, paths),
        crate::ast::Condition::Pattern { expr, .. } => collect_include_paths_expr(expr, paths),
    }
}

fn collect_include_paths_expr(expr: &Expr, paths: &mut IndexSet<String>) {
    match expr {
        Expr::Literal(lit) => match &lit.value {
            Literal::IncludeStr(path) | Literal::IncludeBytes(path) => {
                paths.insert(path.clone());
            }
            _ => {}
        },
        Expr::Binary(b) => {
            collect_include_paths_expr(&b.left, paths);
            collect_include_paths_expr(&b.right, paths);
        }
        Expr::Unary(u) => collect_include_paths_expr(&u.expr, paths),
        Expr::Call(c) => {
            collect_include_paths_expr(&c.callee, paths);
            for arg in &c.args {
                collect_include_paths_expr(arg, paths);
            }
        }
        Expr::MethodCall(m) => {
            collect_include_paths_expr(&m.receiver, paths);
            for arg in &m.args {
                collect_include_paths_expr(arg, paths);
            }
        }
        Expr::Block(b) => collect_include_paths_block(b, paths),
        Expr::If(i) => {
            collect_include_paths_condition(&i.condition, paths);
            collect_include_paths_block(&i.then_block, paths);
            if let Some(else_block) = &i.else_block {
                collect_include_paths_block(else_block, paths);
            }
        }
        Expr::Assign(a) => {
            collect_include_paths_expr(&a.target, paths);
            collect_include_paths_expr(&a.value, paths);
        }
        Expr::Index(i) => {
            collect_include_paths_expr(&i.expr, paths);
            collect_include_paths_expr(&i.index, paths);
        }
        Expr::FieldAccess(f) => collect_include_paths_expr(&f.expr, paths),
        Expr::Cast(c) => collect_include_paths_expr(&c.expr, paths),
        Expr::Match(m) => {
            collect_include_paths_expr(&m.expr, paths);
            for arm in &m.arms {
                collect_include_paths_expr(&arm.body, paths);
            }
        }
        Expr::TupleLiteral(t) => {
            for elem in &t.elements {
                collect_include_paths_expr(elem, paths);
            }
        }
        Expr::StructLiteral(s) => {
            for field in &s.fields {
                collect_include_paths_expr(&field.value, paths);
            }
        }
        Expr::TemplateString(t) => {
            for part in &t.parts {
                if let crate::ast::TemplatePart::Interpolation { expr, .. } = part {
                    collect_include_paths_expr(expr, paths);
                }
            }
        }
        Expr::Closure(c) => collect_include_paths_expr(&c.body, paths),
        _ => {}
    }
}
