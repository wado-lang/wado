// Allow certain pedantic lints that are common in compiler code:
// - cast_possible_truncation: Wasm uses 32-bit types, casts are intentional
// - cast_possible_wrap: Same reason as above
// - similar_names: Variable names like is_i32/is_i64/is_f32/is_f64 are intentional
// - too_many_lines: Large functions are acceptable in compiler code
// - must_use_candidate: Not all functions need must_use
// - return_self_not_must_use: Same reason as above
// - missing_errors_doc: Error documentation is in the return type
// - missing_panics_doc: Panics are documented by unreachable!() etc
// - module_name_repetitions: Type names like LexerError in lexer module are fine
// - unused_self: Visitor pattern methods may not use self yet
// - only_used_in_recursion: Recursive algorithms naturally have this
// - trivially_copy_pass_by_ref: u8 by ref is fine for consistency
// - type_complexity: Complex types are normal in compilers
// - needless_pass_by_value: Ownership transfer is sometimes intentional
// - items_after_statements: Items in functions are fine for test helpers
// - should_implement_trait: from_str method doesn't always match FromStr trait
// - too_many_arguments: Complex functions in compiler may need many args
// - ref_option: &Option<T> is fine when coming from struct fields
// - map_unwrap_or: auto-fix causes type mismatches with &String/&str
// - redundant_else: Sometimes explicit else blocks are clearer
// - match_same_arms: Sometimes explicit arms with comments are clearer
// - match_wildcard_for_single_variants: Explicit arms are sometimes clearer
// - assigning_clones: clone_from() pattern not always clearer
// - doc_link_with_quotes: Doc style preference
// - format_push_string: push_str(format!()) is fine
// - needless_range_loop: Sometimes index variable is needed for context
// - case_sensitive_file_extension_comparisons: Intentional in compiler
// - implicit_hasher: Default hasher is fine
// - unnecessary_wraps: Option/Result wrapping can be intentional
// - manual_let_else: Explicit match is sometimes clearer
// - collapsible_match: Sometimes explicit match is clearer
// - used_underscore_binding: Sometimes needed for unused bindings
// - self_only_used_in_recursion: Recursive methods naturally have this
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::must_use_candidate,
    clippy::return_self_not_must_use,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::unused_self,
    clippy::only_used_in_recursion,
    clippy::trivially_copy_pass_by_ref,
    clippy::type_complexity,
    clippy::needless_pass_by_value,
    clippy::items_after_statements,
    clippy::should_implement_trait,
    clippy::too_many_arguments,
    clippy::ref_option,
    clippy::map_unwrap_or,
    clippy::redundant_else,
    clippy::match_same_arms,
    clippy::match_wildcard_for_single_variants,
    clippy::assigning_clones,
    clippy::doc_link_with_quotes,
    clippy::format_push_string,
    clippy::needless_range_loop,
    clippy::case_sensitive_file_extension_comparisons,
    clippy::implicit_hasher,
    clippy::unnecessary_wraps,
    clippy::manual_let_else,
    clippy::collapsible_match,
    clippy::used_underscore_binding,
    clippy::self_only_used_in_recursion
)]

pub mod analyze;
pub mod ast;
pub mod bind;
pub mod builtin_registry;
pub mod bundled;
pub mod codegen;
pub mod comment;
pub mod compiler_host;
pub mod component_model;
pub mod copy_context;
pub mod desugar;
pub mod effect_check;
pub mod lexer;
pub mod loader;
pub mod logger;
pub mod lower;
pub mod monomorphize;
pub mod name;
pub mod optimize;
pub mod optimize_const_fold;
pub mod optimize_copy_prop;
pub mod optimize_dce;
pub mod optimize_inline;
pub mod optimize_licm;
pub mod optimize_ref_elim;
pub mod optimize_rewrite;
pub mod optimize_sroa;
pub mod parser;
pub mod project;
pub mod resolver;
pub mod stdlib;
pub mod symbol;
pub mod syntax;
pub mod tir;
pub mod token;
pub mod unparse;
pub mod wasm_builder;
pub mod wasm_plan;
pub mod wasm_postprocess;
pub mod world_registry;

pub use analyze::Analyzer;
pub use bind::{BindError, Binder};
pub use codegen::Codegen;
pub use compiler_host::{
    Code, CompilerHost, Diagnostic, DiagnosticSpan, LogLevel, Severity, SourceError,
};
pub use logger::{Bail, Logger};

#[cfg(test)]
pub use compiler_host::InMemoryCompilerHost;
pub use effect_check::{EffectError, check_effects};
pub use lexer::{LexError, Lexer};
pub use loader::{LoadError, LoadResult, ModuleLoader};
pub use lower::{lower, lower_modules_indexed, lower_project};
pub use monomorphize::{monomorphize_module, monomorphize_modules_indexed, monomorphize_project};
pub use name::ModuleSource;
pub use optimize::{OptLevel, optimize};
pub use parser::{ParseError, Parser};
pub use project::Project;
pub use resolver::{Resolver, TypeError, resolve_to_project};
pub use token::Span;
pub use wasm_plan::wasm_plan;

use indexmap::IndexMap;

/// Result of compiling a Wado source file
#[derive(Debug)]
pub struct CompileResult {
    /// Compiled WebAssembly component bytes
    pub wasm: Vec<u8>,
    /// Parsed module AST (includes data section if present)
    pub module: ast::Module,
}

/// Result of dumping compiler internal state
#[derive(Debug)]
pub struct DumpResult {
    /// Source code
    pub source: String,
    /// Tokens from lexer
    pub tokens: Vec<token::Token>,
    /// The main module's AST (after parser)
    pub ast: ast::Module,
    /// Desugared AST (after desugar pass)
    pub desugared_ast: ast::Module,
    /// Symbol table after analysis
    pub symbols: symbol::SymbolTable,
    /// Loaded module sources
    pub loaded_modules: Vec<ModuleSource>,
    /// Modules loaded implicitly by the compiler
    pub implicit_modules: Vec<ModuleSource>,
    /// Entry module source
    pub entry_module_source: ModuleSource,
    /// All TIR modules after resolution (in topological order)
    pub tir_modules: Option<IndexMap<ModuleSource, tir::TirModule>>,
    /// All monomorphized TIR modules (in topological order)
    pub monomorphized_tir_modules: Option<IndexMap<ModuleSource, tir::TirModule>>,
    /// All lowered TIR modules (in topological order)
    pub lowered_tir_modules: Option<IndexMap<ModuleSource, tir::TirModule>>,
    /// Optimized project (contains usage analysis results)
    pub optimized_project: Option<Project>,
    /// Comments for unparsing
    pub comments: comment::CommentMap,
}

/// Compilation options for the compiler
#[derive(Debug, Clone, Default)]
pub struct CompilerOptions {
    /// Optimization level
    pub opt_level: OptLevel,
    /// Target world name (e.g., "Command", "Service")
    /// Defaults to "Command" if not specified
    pub target_world: Option<String>,
}

/// Compile Wado source code with a `CompilerHost` for I/O operations.
///
/// This is the main compilation entry point. It runs the full compilation pipeline:
/// lexer -> parser -> binder -> loader -> analyzer -> resolver -> lower -> optimize -> codegen
///
/// # Arguments
/// * `source` - The entry module source code
/// * `host` - `CompilerHost` for loading imported modules and emitting diagnostics
/// * `filename` - Optional filename for error messages
/// * `opt_level` - Optimization level
///
/// # Example
/// ```ignore
/// let host = FilesystemCompilerHost::new(base_path);
/// let result = compile_with_host(source, &host, Some("main.wado"), OptLevel::O1).await?;
/// ```
pub async fn compile_with_host<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
    opt_level: OptLevel,
) -> Result<CompileResult, Bail> {
    let options = CompilerOptions {
        opt_level,
        target_world: None,
    };
    compile_with_options(source, host, filename, options).await
}

/// Normalize a world specifier to internal world name.
///
/// Converts WIT-style world specifiers to internal names:
/// - `wasi:http/service` → `Service`
/// - `wasi:cli/command` → `Command`
/// - `Service` → `Service` (already normalized)
fn normalize_world_name(world: &str) -> String {
    // If it contains a slash, it's a WIT-style specifier
    if let Some(pos) = world.rfind('/') {
        // Extract the part after the last slash (e.g., "service" from "wasi:http/service")
        let name = &world[pos + 1..];
        // Convert to PascalCase
        to_pascal_case(name)
    } else {
        // Already a world name (e.g., "Service")
        world.to_string()
    }
}

/// Convert a string to `PascalCase`.
fn to_pascal_case(s: &str) -> String {
    s.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Compile Wado source code with full options.
///
/// This is the main compilation entry point with all options. It runs the full compilation pipeline:
/// lexer -> parser -> binder -> loader -> analyzer -> resolver -> lower -> optimize -> codegen
///
/// # Arguments
/// * `source` - The entry module source code
/// * `host` - `CompilerHost` for loading imported modules and emitting diagnostics
/// * `filename` - Optional filename for error messages
/// * `options` - Compilation options including optimization level and target world
pub async fn compile_with_options<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
    options: CompilerOptions,
) -> Result<CompileResult, Bail> {
    let logger = Logger::new(host, compiler_host::LogLevel::default());
    let filename = filename.map(String::from);
    if let Some(ref f) = filename {
        logger.set_file(f);
    }

    // === Phase 1: Load all modules ===
    // Loader performs: lex → parse → bind → desugar for each module
    // Also preserves the original (non-desugared) entry AST for tooling
    let load_result = {
        let module_loader = loader::ModuleLoader::new(host, compiler_host::LogLevel::default());
        module_loader
            .load_all(source, filename.as_deref())
            .await
            .map_err(|e| {
                let _ = logger.error(e);
                Bail
            })?
    };

    // === Phase 2: Analyze all modules ===
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

    let module_name = filename.clone().unwrap_or_else(|| "module".to_string());

    // === Phase 6: Resolve all modules to Project ===
    let project = {
        let _span = logger.span("resolve");
        resolve_to_project(
            symbols,
            &load_result.modules,
            load_result.entry_module_source.clone(),
            load_result.implicit_modules.clone(),
            module_name,
            &logger,
        )?
    };

    // === Phase 7: Effect Check ===
    {
        let _span = logger.span("effect-check");
        check_effects(&project.tir_modules, &logger)?;
    }

    // === Phase 8: Monomorphize (Project -> Project) ===
    let mut project = {
        let _span = logger.span("monomorphize");
        monomorphize_project(project)
    };

    // Apply target world from options
    // Convert WIT-style world specifier (e.g., "wasi:http/service") to internal name (e.g., "Service")
    if let Some(world) = options.target_world {
        project.target_world = normalize_world_name(&world);
    }

    // === Phase 9: Lower (Project -> Project) ===
    let project = {
        let _span = logger.span("lower");
        lower_project(project)
    };

    // === Phase 10: Optimize (Project -> Project) ===
    let project = {
        let _span = logger.span("optimize");
        optimize(project, options.opt_level)
    };

    // === Phase 11: Wasm Plan (Project -> Project) ===
    let project = {
        let _span = logger.span("wasm-plan");
        wasm_plan(project).map_err(|message| {
            let _ = logger.error(compiler_host::Diagnostic {
                severity: compiler_host::Severity::Error,
                code: compiler_host::Code::UnsupportedFeature,
                message,
                span: None,
            });
            Bail
        })?
    };

    // === Phase 12: Codegen ===
    let wasm = {
        let _span = logger.span("codegen");
        Codegen::generate_wasm(&project)
    };

    // Return the original (non-desugared) entry AST for tooling
    Ok(CompileResult {
        wasm,
        module: load_result.entry_ast,
    })
}

/// Dump compiler internal state (async version).
///
/// This runs the compilation pipeline up through optimization (without code generation)
/// and returns diagnostic information about the internal state.
///
/// Pipeline: lexer -> parser -> bind -> desugar -> load -> analyze -> resolve -> lower -> optimize
pub async fn dump_with_host<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
    opt_level: OptLevel,
) -> Result<DumpResult, Bail> {
    let logger = Logger::new(host, compiler_host::LogLevel::default());
    let filename = filename.map(String::from);
    if let Some(ref f) = filename {
        logger.set_file(f);
    }

    // === Phase 1: Lexer ===
    let (tokens, tokens_for_dump, comments, data_section, shebang) = {
        let _span = logger.span("lex");
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().map_err(|e| {
            let _ = logger.error(e);
            Bail
        })?;
        let (data_section, comments, shebang) = lexer.into_parts();
        let tokens_for_dump = tokens.clone();
        (tokens, tokens_for_dump, comments, data_section, shebang)
    };

    // Build comment map
    let comment_map = comment::CommentMap::from_comments(comments, source);

    // === Phase 2: Parser ===
    let ast = {
        let _span = logger.span("parse");
        let mut parser = Parser::with_metadata(tokens, shebang, data_section);
        parser.parse().map_err(|e| {
            let _ = logger.error(e);
            Bail
        })?
    };

    // === Phase 3: Bind ===
    {
        let _span = logger.span("bind");
        let mut binder = Binder::new(&logger);
        binder.bind_module(&ast)?;
    }

    // === Phase 4: Desugar ===
    let desugared_ast = {
        let _span = logger.span("desugar");
        desugar::desugar_module(&ast)
    };

    // === Phase 5: Load all modules ===
    let load_result = {
        let module_loader = loader::ModuleLoader::new(host, compiler_host::LogLevel::default());
        module_loader
            .load_all(source, filename.as_deref())
            .await
            .map_err(|e| {
                let _ = logger.error(e);
                Bail
            })?
    };

    // === Phase 6: Analyze all modules ===
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

    // === Phase 7: Resolve all modules to TIR ===
    let tir_modules = {
        let _span = logger.span("resolve");
        Resolver::resolve_all_modules(
            &symbols,
            &load_result.modules,
            load_result.entry_module_source.clone(),
            &logger,
        )
        .ok()
    };

    // TIR modules already use ModuleSource keys
    let tir_modules_by_source: Option<IndexMap<ModuleSource, tir::TirModule>> = tir_modules.clone();

    // === Phase 8: Monomorphize all modules ===
    // Use monomorphize_modules_indexed for cross-module generic function support
    let monomorphized_tir_modules_by_source: Option<IndexMap<ModuleSource, tir::TirModule>> = {
        let _span = logger.span("monomorphize");
        tir_modules_by_source
            .clone()
            .map(monomorphize_modules_indexed)
    };

    // === Phase 9: Lower all modules ===
    // Apply string literal collection to monomorphized modules
    let entry_source = &load_result.entry_module_source;
    let lowered_tir_modules_by_source: Option<IndexMap<ModuleSource, tir::TirModule>> = {
        let _span = logger.span("lower");
        monomorphized_tir_modules_by_source
            .clone()
            .map(|m| lower_modules_indexed(m, entry_source))
    };

    // === Phase 10: Optimize ===
    // Build a Project from lowered modules if available
    let optimized_project = {
        let _span = logger.span("optimize");
        lowered_tir_modules_by_source
            .clone()
            .and_then(|modules_by_source| {
                let module_name = filename.clone().unwrap_or_else(|| "module".to_string());

                let implicit_modules_by_source = load_result.implicit_modules.clone();

                let (wasi_registry, world_registry) =
                    component_model::WasiRegistry::build_from_stdlib();

                // Build builtin registry (uses a temporary type table for type resolution)
                let temp_type_table = std::cell::RefCell::new(tir::TypeTable::new());
                let builtin_registry =
                    builtin_registry::BuiltinRegistry::build_from_stdlib(&temp_type_table);

                let project = Project::new(
                    load_result.entry_module_source.clone(),
                    modules_by_source,
                    symbols.clone(),
                    implicit_modules_by_source,
                    module_name,
                    wasi_registry,
                    world_registry,
                    builtin_registry,
                );
                let project = optimize(project, opt_level);
                wasm_plan(project).ok()
            })
    };

    Ok(DumpResult {
        source: source.to_string(),
        tokens: tokens_for_dump,
        ast,
        desugared_ast,
        symbols,
        loaded_modules: load_result.modules.keys().cloned().collect(),
        implicit_modules: load_result.implicit_modules.into_iter().collect(),
        entry_module_source: load_result.entry_module_source,
        tir_modules: tir_modules_by_source,
        monomorphized_tir_modules: monomorphized_tir_modules_by_source,
        lowered_tir_modules: lowered_tir_modules_by_source,
        optimized_project,
        comments: comment_map,
    })
}

/// Format Wado source code.
///
/// Returns the formatted source with canonical formatting.
/// Preserves comments and the `__DATA__` section.
///
/// # Example
/// ```
/// let source = r#"use {println} from "core:cli";
/// fn run() with Stdout { println("Hello!"); }
/// "#;
/// let formatted = wado_compiler::format(source).unwrap();
/// assert!(formatted.contains("use { println }"));
/// ```
pub fn format(source: &str) -> Result<String, CompileError> {
    // Lexer (collect comments, shebang, data section)
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| CompileError::Lexer {
        message: e.message,
        line: e.span.line,
        column: e.span.column,
        filename: None,
    })?;
    let (data_section, comments, shebang) = lexer.into_parts();

    // Build comment map
    let comment_map = comment::CommentMap::from_comments(comments, source);

    // Parser (with shebang and data section)
    let mut parser = Parser::with_metadata(tokens, shebang, data_section);
    let ast = parser.parse().map_err(|e| CompileError::Parser {
        message: e.message,
        line: e.span.line,
        column: e.span.column,
        filename: None,
    })?;

    // Unparse (no lowering - preserve high-level constructs)
    let unparser = unparse::Unparser::new(&comment_map);
    Ok(unparser.unparse(&ast))
}

/// Compilation error with structured location info
#[derive(Debug)]
pub enum CompileError {
    /// I/O error reading source file
    Io { path: String, message: String },
    /// Lexer error with location
    Lexer {
        message: String,
        line: usize,
        column: usize,
        filename: Option<String>,
    },
    /// Parser error with location
    Parser {
        message: String,
        line: usize,
        column: usize,
        filename: Option<String>,
    },
    /// Binding error (local name resolution)
    Bind {
        message: String,
        filename: Option<String>,
    },
    /// Semantic analysis error
    Analyzer {
        message: String,
        filename: Option<String>,
    },
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::Io { path, message } => {
                write!(f, "Error reading '{path}': {message}")
            }
            CompileError::Lexer {
                message,
                line,
                column,
                filename,
            } => {
                if let Some(file) = filename {
                    write!(f, "{file}:{line}:{column}: lexer error: {message}")
                } else {
                    write!(f, "line {line}, column {column}: lexer error: {message}")
                }
            }
            CompileError::Parser {
                message,
                line,
                column,
                filename,
            } => {
                if let Some(file) = filename {
                    write!(f, "{file}:{line}:{column}: parse error: {message}")
                } else {
                    write!(f, "line {line}, column {column}: parse error: {message}")
                }
            }
            CompileError::Bind { message, filename } => {
                if let Some(file) = filename {
                    write!(f, "{file}: {message}")
                } else {
                    write!(f, "{message}")
                }
            }
            CompileError::Analyzer { message, filename } => {
                if let Some(file) = filename {
                    write!(f, "{file}: analysis error: {message}")
                } else {
                    write!(f, "analysis error: {message}")
                }
            }
        }
    }
}

impl std::error::Error for CompileError {}
