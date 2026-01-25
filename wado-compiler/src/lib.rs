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
pub mod desugar;
pub mod lexer;
pub mod loader;
pub mod logger;
pub mod lower;
pub mod monomorphize;
pub mod name;
pub mod optimize;
pub mod optimize_copy_prop;
pub mod optimize_dce;
pub mod optimize_inline;
pub mod optimize_licm;
pub mod optimize_move;
pub mod optimize_ref_elim;
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
pub mod wasm_postprocess;
pub mod world_registry;

pub use analyze::Analyzer;
pub use bind::{BindError, Binder};
pub use codegen::Codegen;
pub use compiler_host::{
    Code, CompilerHost, Diagnostic, DiagnosticSpan, LogLevel, Severity, SourceError,
};
pub use logger::Logger;

#[cfg(test)]
pub use compiler_host::InMemoryCompilerHost;
pub use lexer::{LexError, Lexer};
pub use loader::{LoadError, LoadResult, ModuleLoader};
pub use lower::{lower, lower_modules_indexed, lower_project};
pub use monomorphize::{monomorphize_module, monomorphize_modules_indexed, monomorphize_project};
pub use name::ModuleSource;
pub use optimize::{CanonBuiltin, OptLevel, optimize};
pub use parser::{ParseError, Parser};
pub use project::Project;
pub use resolver::{Resolver, TypeError, resolve_to_project};
pub use token::Span;

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
) -> Result<CompileResult, CompileError> {
    let filename = filename.map(String::from);

    // === Phase 1: Lexer (for original AST) ===
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| CompileError::Lexer {
        message: e.message,
        line: e.span.line,
        column: e.span.column,
        filename: filename.clone(),
    })?;
    let (data_section, _comments, shebang) = lexer.into_parts();

    // === Phase 2: Parser (for original AST) ===
    let mut parser = Parser::with_metadata(tokens, shebang, data_section);
    let ast = parser.parse().map_err(|e| CompileError::Parser {
        message: e.message,
        line: e.span.line,
        column: e.span.column,
        filename: filename.clone(),
    })?;

    // === Phase 3: Bind (local name resolution) ===
    let mut binder = Binder::new();
    binder.bind_module(&ast).map_err(|errors| {
        let msg = errors
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        CompileError::Bind {
            message: msg,
            filename: filename.clone(),
        }
    })?;

    // === Phase 4: Load all modules upfront ===
    let load_result = {
        let module_loader = loader::ModuleLoader::new(host, compiler_host::LogLevel::default());
        module_loader
            .load_all(source, filename.as_deref())
            .await
            .map_err(|e| CompileError::Analyzer {
                message: e.to_string(),
                filename: filename.clone(),
            })?
    };

    // Convert ModuleSource keys to Vec<String> for analyzer/resolver compatibility
    let modules_by_path: std::collections::HashMap<Vec<String>, ast::Module> = load_result
        .modules
        .iter()
        .map(|(ms, m)| (ms.to_path(), m.clone()))
        .collect();
    let implicit_modules_by_path: std::collections::HashSet<Vec<String>> = load_result
        .implicit_modules
        .iter()
        .map(ModuleSource::to_path)
        .collect();

    // === Phase 5: Analyze all modules ===
    let mut analyzer = Analyzer::new();
    analyzer
        .analyze_loaded_modules(
            &modules_by_path,
            &load_result.entry_module_source.to_path(),
            implicit_modules_by_path.clone(),
        )
        .map_err(|errors| {
            let msg = errors
                .into_iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            CompileError::Analyzer {
                message: msg,
                filename: filename.clone(),
            }
        })?;

    let symbols = analyzer.into_symbols();

    // Derive module name from filename
    let module_name = filename
        .as_ref()
        .and_then(|f| std::path::Path::new(f).file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("module")
        .to_string();

    // === Phase 6: Resolve all modules to Project ===
    let project = resolve_to_project(
        symbols,
        &modules_by_path,
        load_result.entry_module_source.clone(),
        implicit_modules_by_path,
        module_name,
    )
    .map_err(|errors| {
        let msg = errors
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        CompileError::Analyzer {
            message: msg,
            filename: filename.clone(),
        }
    })?;

    // === Phase 7: Monomorphize (Project -> Project) ===
    let project = monomorphize_project(project);

    // === Phase 8: Lower (Project -> Project) ===
    let project = lower_project(project);

    // === Phase 9: Optimize (Project -> Project) ===
    let project = optimize(project, opt_level);

    // === Phase 10: Codegen ===
    let mut codegen = Codegen::new();
    let wasm = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        codegen.generate_wasm(&project)
    }))
    .map_err(|e| {
        let message = if let Some(s) = e.downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = e.downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown codegen error".to_string()
        };
        CompileError::Codegen {
            message,
            filename: filename.clone(),
        }
    })?;

    // Return the original (non-desugared) AST for tooling
    Ok(CompileResult { wasm, module: ast })
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
) -> Result<DumpResult, CompileError> {
    let filename = filename.map(String::from);

    // === Phase 1: Lexer ===
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| CompileError::Lexer {
        message: e.message,
        line: e.span.line,
        column: e.span.column,
        filename: filename.clone(),
    })?;
    let (data_section, comments, shebang) = lexer.into_parts();

    // Build comment map
    let comment_map = comment::CommentMap::from_comments(comments, source);

    // === Phase 2: Parser ===
    let tokens_for_dump = tokens.clone();
    let mut parser = Parser::with_metadata(tokens, shebang, data_section);
    let ast = parser.parse().map_err(|e| CompileError::Parser {
        message: e.message,
        line: e.span.line,
        column: e.span.column,
        filename: filename.clone(),
    })?;

    // === Phase 3: Bind ===
    let mut binder = Binder::new();
    binder.bind_module(&ast).map_err(|errors| {
        let msg = errors
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        CompileError::Bind {
            message: msg,
            filename: filename.clone(),
        }
    })?;

    // === Phase 4: Desugar ===
    let desugared_ast = desugar::desugar_module(&ast);

    // === Phase 5: Load all modules ===
    let load_result = {
        let module_loader = loader::ModuleLoader::new(host, compiler_host::LogLevel::default());
        module_loader
            .load_all(source, filename.as_deref())
            .await
            .map_err(|e| CompileError::Analyzer {
                message: e.to_string(),
                filename: filename.clone(),
            })?
    };

    // Convert ModuleSource keys to Vec<String> for analyzer/resolver compatibility
    let modules_by_path: std::collections::HashMap<Vec<String>, ast::Module> = load_result
        .modules
        .iter()
        .map(|(ms, m)| (ms.to_path(), m.clone()))
        .collect();
    let implicit_modules_by_path: std::collections::HashSet<Vec<String>> = load_result
        .implicit_modules
        .iter()
        .map(ModuleSource::to_path)
        .collect();

    // === Phase 6: Analyze all modules ===
    let mut analyzer = Analyzer::new();
    analyzer
        .analyze_loaded_modules(
            &modules_by_path,
            &load_result.entry_module_source.to_path(),
            implicit_modules_by_path.clone(),
        )
        .map_err(|errors| {
            let msg = errors
                .into_iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            CompileError::Analyzer {
                message: msg,
                filename: filename.clone(),
            }
        })?;

    let symbols = analyzer.into_symbols();

    // === Phase 7: Resolve all modules to TIR ===
    let tir_modules = Resolver::resolve_all_modules(
        &symbols,
        &modules_by_path,
        load_result.entry_module_source.clone(),
    )
    .ok();

    // Convert TIR modules to ModuleSource keys for DumpResult
    let tir_modules_by_source: Option<IndexMap<ModuleSource, tir::TirModule>> =
        tir_modules.clone().map(|m| {
            m.into_iter()
                .map(|(path, tir)| (ModuleSource::from_path(&path), tir))
                .collect()
        });

    // === Phase 8: Monomorphize all modules ===
    // Use monomorphize_modules_indexed for cross-module generic function support
    let monomorphized_tir_modules_by_source: Option<IndexMap<ModuleSource, tir::TirModule>> =
        tir_modules_by_source
            .clone()
            .map(monomorphize_modules_indexed);

    // === Phase 9: Lower all modules ===
    // Apply string literal collection to monomorphized modules
    let lowered_tir_modules_by_source: Option<IndexMap<ModuleSource, tir::TirModule>> =
        monomorphized_tir_modules_by_source
            .clone()
            .map(lower_modules_indexed);

    // === Phase 10: Optimize ===
    // Build a Project from lowered modules if available
    let optimized_project = lowered_tir_modules_by_source
        .clone()
        .map(|modules_by_source| {
            let module_name = filename
                .as_ref()
                .and_then(|f| std::path::Path::new(f).file_stem())
                .and_then(|s| s.to_str())
                .unwrap_or("module")
                .to_string();

            let implicit_modules_by_source: std::collections::HashSet<ModuleSource> =
                implicit_modules_by_path
                    .iter()
                    .map(|p| ModuleSource::from_path(p))
                    .collect();

            let project = Project::new(
                load_result.entry_module_source.clone(),
                modules_by_source,
                symbols.clone(),
                implicit_modules_by_source,
                module_name,
            );
            optimize(project, opt_level)
        });

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
    /// Code generation error
    Codegen {
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
            CompileError::Codegen { message, filename } => {
                if let Some(file) = filename {
                    write!(f, "{file}: codegen error: {message}")
                } else {
                    write!(f, "codegen error: {message}")
                }
            }
        }
    }
}

impl std::error::Error for CompileError {}
