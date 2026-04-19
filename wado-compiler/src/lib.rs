pub mod analyze;
pub mod annotate;
pub mod ast;
pub mod bind;
pub mod builtin_registry;
pub mod bundled;
pub mod cm_abi;
pub mod codegen;
pub mod comment;
pub mod compiler_host;
pub mod component_model;
pub mod desugar;
pub mod doc;
pub mod effect_check;
pub mod flat_package;
pub mod hashmap;
pub mod lexer;
pub mod link;
pub mod loader;
pub mod logger;
pub mod lower;
pub mod monomorphize;
pub mod name;
pub mod optimize;
pub mod package;
pub mod parser;
pub mod resolver;
pub mod stdlib;
pub mod symbol;
pub mod syntax;
pub mod synthesis;
pub mod tir;
pub mod tir_visitor;
pub mod token;
pub mod unparse;
pub mod wir;
pub mod wir_build;
pub mod wir_optimize;
pub mod wir_unparse;
pub mod wir_visitor;
pub mod world_registry;

pub use analyze::Analyzer;
pub use annotate::{Annotated, Definition, annotate};
pub use ast::{AstId, AstNodeKind, AstPtr};
pub use bind::{BindError, Binder};
pub use compiler_host::{
    Code, CompilerHost, Diagnostic, DiagnosticSpan, LogLevel, Severity, SourceError,
};
pub use logger::{Bail, Logger};

#[cfg(test)]
pub use compiler_host::InMemoryCompilerHost;
pub use effect_check::{EffectError, check_default_purity, check_effects, check_stores};
pub use flat_package::FlatPackage;
pub use lexer::{LexError, Lexer};
pub use loader::{LoadError, LoadResult, ModuleLoader};
pub use lower::lower;
pub use monomorphize::monomorphize;
pub use name::ModuleSource;
pub use optimize::{OptLevel, optimize};
pub use package::Package;
pub use parser::{ParseError, Parser};
pub use resolver::{Resolver, TypeError};
pub use token::Span;

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexMap;

/// Result of compiling a Wado source file
#[derive(Debug)]
pub struct CompileResult {
    /// Compiled WebAssembly component bytes
    pub wasm: Vec<u8>,
    /// Parsed module AST (includes data section if present)
    pub module: ast::Module,
    /// WIR module (retained when `CompilerOptions::retain_wir` is true)
    pub wir_package: Option<wir::WirPackage>,
    /// Whether the entry module has `#![TODO]`
    pub is_todo_module: bool,
}

/// Compilation failure with metadata from the successfully-parsed AST.
///
/// Internal `Bail` carries no data (errors are already emitted to the host).
/// This wrapper adds the `is_todo_module` flag so callers can distinguish
/// expected failures in `#![TODO]` modules from real errors.
#[derive(Debug)]
pub struct CompileFailure {
    /// Whether the entry module has `#![TODO]`
    pub is_todo_module: bool,
}

impl std::fmt::Display for CompileFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "compilation failed")
    }
}

impl std::error::Error for CompileFailure {}

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
    /// Monomorphized TIR snapshot (unparsed text)
    pub monomorphized_tir_text: Option<String>,
    /// Lowered TIR snapshot (unparsed text)
    pub lowered_tir_text: Option<String>,
    /// Linked package after optimization (contains usage analysis results)
    pub optimized_package: Option<FlatPackage>,
    /// WIR module (after `tir_to_wir` translation)
    pub wir_package: Option<wir::WirPackage>,
    /// Comments for unparsing
    pub comments: comment::CommentMap,
}

/// Compilation options for the compiler
#[derive(Debug, Clone, Default)]
pub struct CompilerOptions {
    /// Optimization level
    pub opt_level: OptLevel,
    /// Target world fully-qualified name (e.g., "wasi:cli/command", "wasi:http/service")
    pub target_world: Option<String>,
    /// Skip Wasm validation after code generation.
    /// When true, the compiler returns raw Wasm bytes even if they fail validation.
    /// Useful for debugging the code generator.
    pub skip_validation: bool,
    /// When true, retain the WIR module in [`CompileResult::wir_package`].
    /// Used by test infrastructure to inspect WIR without a second compilation pass.
    pub retain_wir: bool,
    /// Override the inline threshold for the optimization pass.
    /// When `None`, the default for the `opt_level` is used.
    pub inline_threshold: Option<usize>,
    /// Override the number of fixed-point optimization iterations.
    /// When `None`, the default for the `opt_level` is used.
    pub opt_iterations: Option<u32>,
    /// Log level for compiler diagnostics.
    /// When `None`, uses the default (`Info`).
    pub log_level: Option<LogLevel>,
    /// Which allocator to use (e.g., `"bump"`, `"debug"`).
    /// Matches `#[allocator("...")]` attributes in `core:allocator`.
    /// When `None`, the compiler auto-selects: `"debug"` for test world, `"bump"` otherwise.
    pub allocator: Option<String>,
}

/// Compile Wado source code with a `CompilerHost` for I/O operations.
///
/// This is the main compilation entry point. It runs the full compilation pipeline:
/// lexer -> parser -> binder -> loader -> analyzer -> resolver -> lower -> optimize -> `tir_to_wir`
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
) -> Result<CompileResult, CompileFailure> {
    let options = CompilerOptions {
        opt_level,
        target_world: None,
        ..CompilerOptions::default()
    };
    compile_with_options(source, host, filename, options).await
}

/// Compile Wado source code with full options.
///
/// This is the main compilation entry point with all options. It runs the full compilation pipeline:
/// lexer -> parser -> binder -> loader -> analyzer -> resolver -> lower -> optimize -> `tir_to_wir`
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
) -> Result<CompileResult, CompileFailure> {
    let log_level = options.log_level.unwrap_or_default();
    let logger = Logger::new(host, log_level);
    let filename = filename.map(String::from);
    if let Some(ref f) = filename {
        logger.set_file(f);
    }

    // === Phase 1: Load all modules ===
    // Loader performs: lex → parse → bind → desugar for each module
    // Also preserves the original (non-desugared) entry AST for tooling
    let load_result = {
        let module_loader = loader::ModuleLoader::new(host, log_level);
        module_loader
            .load_all(source, filename.as_deref())
            .await
            .map_err(|e| {
                let _ = logger.error(e);
                // Parse failed — cannot determine TODO status
                CompileFailure {
                    is_todo_module: false,
                }
            })?
    };

    // Detect #![TODO] from the entry module AST (available after Phase 1)
    let is_todo_module = load_result.entry_ast.has_todo();

    // Wrap all subsequent Bail errors with is_todo_module
    let result = compile_after_load(load_result, options, &logger, filename);
    match result {
        Ok((wasm, module, wir_package)) => Ok(CompileResult {
            wasm,
            module,
            wir_package,
            is_todo_module,
        }),
        Err(Bail) => Err(CompileFailure { is_todo_module }),
    }
}

/// Internal: run compilation phases after module loading.
fn compile_after_load<H: CompilerHost>(
    load_result: loader::LoadResult,
    options: CompilerOptions,
    logger: &Logger<'_, H>,
    filename: Option<String>,
) -> Result<(Vec<u8>, ast::Module, Option<wir::WirPackage>), Bail> {
    let module_name = filename.unwrap_or_else(|| "module".to_string());
    let implicit_modules = load_result.implicit_modules.clone();
    let entry_ast = load_result.entry_ast.clone();

    // === Phases 2 + 6a + 6b: Analyze + Annotate + Lower TIR ===
    // `annotate` performs analyze, type resolution, and body-level TIR
    // lowering. The resulting `Annotated` carries the `TirModule`s the batch
    // compiler needs plus the use→def reference map LSP queries need.
    let annotated = annotate::annotate_loaded(load_result, logger)?;

    let annotate::Annotated {
        entry_module_source,
        symbols,
        state,
        tir_modules,
        ..
    } = annotated;

    let package = Package::new(
        entry_module_source,
        tir_modules,
        symbols,
        implicit_modules,
        module_name,
        state.wasi_registry,
        state.world_registry,
        state.builtin_registry,
    );

    // Apply options to package (must be before synthesis)
    let mut package = package;
    if let Some(world) = options.target_world {
        package.target_world = world;
    }
    package.skip_validation = options.skip_validation;

    // Select allocator: find the function tagged with #[allocator("...")] matching the
    // chosen mode, set its export_name to "realloc", and clear export_name from all others.
    {
        let allocator_tag = options.allocator.unwrap_or_else(|| {
            if package.is_test_world() {
                "debug".to_string()
            } else if package.target_world == "wasi:http/service" {
                "freelist".to_string()
            } else {
                "bump".to_string()
            }
        });
        if let Some(alloc_module) = package.tir_modules.get_mut(&ModuleSource::allocator()) {
            let mut found = false;
            for func_rc in &alloc_module.functions {
                let mut func = func_rc.borrow_mut();
                if func.allocator_tag.as_deref() == Some(&*allocator_tag) {
                    func.export_name = Some("realloc".to_string());
                    found = true;
                } else if func.allocator_tag.is_some() {
                    func.export_name = None;
                }
            }
            if !found {
                let _ = logger.error(compiler_host::Diagnostic {
                    severity: compiler_host::Severity::Error,
                    code: compiler_host::Code::UnsupportedFeature,
                    message: format!("unknown allocator: `{allocator_tag}`"),
                    span: None,
                });
                return Err(Bail);
            }
        }
    }

    // Validate target world (test world is handled specially, not in registry)
    if !package.is_test_world() && package.world_registry.get(&package.target_world).is_none() {
        let _ = logger.error(compiler_host::Diagnostic {
            severity: compiler_host::Severity::Error,
            code: compiler_host::Code::UnsupportedFeature,
            message: format!("unknown target world: `{}`", package.target_world),
            span: None,
        });
        return Err(Bail);
    }

    // === Phase 8: Synthesis (Package -> Package) ===
    let package = {
        let _span = logger.span("synthesis");
        synthesis::synthesize(package).map_err(|message| {
            let _ = logger.error(compiler_host::Diagnostic {
                severity: compiler_host::Severity::Error,
                code: compiler_host::Code::UnsupportedFeature,
                message,
                span: None,
            });
            Bail
        })?
    };

    // === Phase 8a: Effect Check ===
    // Runs after synthesis so synthesized functions (trait impls, Inspect, Display, serde, etc.)
    // are also validated. Runs before monomorphize so effect params are still present.
    // CM bindings are skipped (they are boundary code with special effect semantics).
    {
        let _span = logger.span("effect-check");
        check_effects(&package.tir_modules, logger)?;
    }

    // === Phase 8a2: Default-Value Purity Check ===
    // Every `param: T = expr` and `field: T = expr` must be pure. Runs after
    // effect checking so the effect map is consistent.
    {
        let _span = logger.span("default-purity-check");
        check_default_purity(&package.tir_modules, logger)?;
    }

    // === Phase 8b: Stores Check ===
    // Runs after synthesis so synthesized functions are also checked.
    // Runs before monomorphize/optimize so stores info is available for escape analysis.
    {
        let _span = logger.span("stores-check");
        check_stores(&package.tir_modules, logger)?;
    }

    // === Phase 8b: Link (Package → FlatPackage) ===
    let mut flat = {
        let _span = logger.span("link");
        link::link(package)
    };

    // === Phase 9: Monomorphize (FlatPackage → FlatPackage) ===
    {
        let _span = logger.span("monomorphize");
        monomorphize(&mut flat);
    }

    // === Phase 9b: Erase Newtypes and Flags ===
    // After monomorphize (which needs distinct Newtype/Flags types for trait dispatch)
    // and before lower/optimize/codegen (which expect Newtypes → base type; Flags → u32).
    {
        flat.type_table.borrow_mut().erase_newtypes_and_flags();
    }

    // === Phase 10: Lower (FlatPackage → FlatPackage) ===
    {
        let _span = logger.span("lower");
        lower(&mut flat);
    }

    // === Phase 11: Optimize (FlatPackage → FlatPackage) ===
    let flat = {
        let _span = logger.span("optimize");
        optimize(
            flat,
            options.opt_level,
            options.inline_threshold,
            options.opt_iterations,
            logger,
        )
    };

    // === Phase 12: Build WIR (FlatPackage → WirPackage) ===
    let mut wir_package = {
        let _span = logger.span("wir_build");
        wir_build::build_wir_package(&flat)
    };

    // === Phase 13: Optimize WIR ===
    {
        let _span = logger.span("wir_optimize");
        wir_optimize::optimize_wir(&mut wir_package, options.opt_level, logger);
    }

    // === Phase 14: Emit Wasm (WirPackage → Wasm component bytes) ===
    let wasm = {
        let _span = logger.span("codegen");
        codegen::emit_wasm(&flat, &wir_package)
    };

    // Return the original (non-desugared) entry AST for tooling
    Ok((
        wasm,
        entry_ast,
        if options.retain_wir {
            Some(wir_package)
        } else {
            None
        },
    ))
}

/// Deep-clone TIR modules so that each snapshot has its own independent `TypeTable`.
///
/// TIR modules share a single `TypeTable` via `Rc<RefCell<…>>`.  Later
/// optimization passes (notably DCE's `TypeTable::retain`) mutate that shared
/// table.  Snapshots taken for dump output must be immune to those mutations,
/// so we clone the `TypeTable` once and give every module in the snapshot its
/// own `Rc` pointing to the clone.
fn snapshot_tir_modules(
    modules: &IndexMap<ModuleSource, tir::TirModule>,
) -> IndexMap<ModuleSource, tir::TirModule> {
    let cloned_tt = modules
        .values()
        .next()
        .map(|m| Rc::new(RefCell::new(m.type_table.borrow().clone())));

    modules
        .iter()
        .map(|(k, m)| {
            let mut m = m.clone();
            if let Some(ref tt) = cloned_tt {
                m.type_table = Rc::clone(tt);
            }
            (k.clone(), m)
        })
        .collect()
}

/// Dump compiler internal state (async version).
///
/// This runs the compilation pipeline up through optimization (without code generation)
/// and returns diagnostic information about the internal state.
///
/// Pipeline: lexer -> parser -> bind -> desugar -> load -> analyze -> resolve -> lower -> link -> optimize
pub async fn dump_with_host<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
    opt_level: OptLevel,
) -> Result<DumpResult, Bail> {
    dump_with_host_and_world(source, host, filename, opt_level, None, None, None).await
}

/// Dump compiler internal state with an explicit target world.
///
/// Like [`dump_with_host`] but allows specifying the target world so that
/// DCE and other world-aware passes produce the correct output.
pub async fn dump_with_host_and_world<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
    opt_level: OptLevel,
    target_world: Option<&str>,
    inline_threshold: Option<usize>,
    opt_iterations: Option<u32>,
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
            &load_result.included_files,
        )
        .ok()
    };

    // Snapshot resolved TIR with an independent TypeTable clone so that
    // later optimization passes (which call TypeTable::retain) cannot mutate it.
    let tir_modules_by_source: Option<IndexMap<ModuleSource, tir::TirModule>> =
        tir_modules.as_ref().map(snapshot_tir_modules);

    // === Phase 7b+8+9+10: Build Package and run remaining phases ===
    // Create Package early so CM binding synthesis runs before monomorphize,
    // matching the compile_with_options pipeline.
    let (monomorphized_tir_text, lowered_tir_text, optimized_package, wir_package) =
        if let Some(resolved_modules) = tir_modules_by_source.clone() {
            let module_name = filename.clone().unwrap_or_else(|| "module".to_string());

            let (wasi_registry, world_registry) =
                component_model::WasiRegistry::build_from_stdlib();

            let temp_type_table = std::cell::RefCell::new(tir::TypeTable::new());
            let builtin_registry =
                builtin_registry::BuiltinRegistry::build_from_stdlib(&temp_type_table);

            let package = Package::new(
                load_result.entry_module_source.clone(),
                resolved_modules,
                symbols.clone(),
                load_result.implicit_modules.clone(),
                module_name,
                wasi_registry,
                world_registry,
                builtin_registry,
            );

            // Apply target world override (must be before synthesis)
            let mut package = package;
            if let Some(world) = target_world {
                package.target_world = world.to_string();
            }

            // Validate target world (test world is synthetic, not in registry)
            if !package.is_test_world()
                && package.world_registry.get(&package.target_world).is_none()
            {
                let _ = logger.error(compiler_host::Diagnostic {
                    severity: compiler_host::Severity::Error,
                    code: compiler_host::Code::UnsupportedFeature,
                    message: format!("unknown target world: `{}`", package.target_world),
                    span: None,
                });
                return Err(Bail);
            }

            // Synthesis (must run before monomorphize)
            let package = {
                let _span = logger.span("synthesis");
                synthesis::synthesize(package).map_err(|message| {
                    let _ = logger.error(compiler_host::Diagnostic {
                        severity: compiler_host::Severity::Error,
                        code: compiler_host::Code::UnsupportedFeature,
                        message,
                        span: None,
                    });
                    Bail
                })?
            };

            // Link
            let mut flat = link::link(package);

            // Monomorphize
            {
                let _span = logger.span("monomorphize");
                monomorphize(&mut flat);
            }

            // Erase Newtypes and Flags (after monomorphize, before lower)
            flat.type_table.borrow_mut().erase_newtypes_and_flags();

            // Snapshot monomorphized state (only unparse; Debug format is deferred)
            let mono_text = Some(unparse::unparse_flat_package(&flat));

            // Lower
            {
                let _span = logger.span("lower");
                lower(&mut flat);
            }
            // Snapshot lowered state (only unparse; Debug format is deferred)
            let lower_text = Some(unparse::unparse_flat_package(&flat));

            // Optimize
            let flat = {
                let _span = logger.span("optimize");
                optimize(flat, opt_level, inline_threshold, opt_iterations, &logger)
            };

            // WIR: Translate optimized FlatPackage to WirPackage for inspection.
            let wir_package = Some({
                let mut wir = wir_build::build_wir_package(&flat);
                wir_optimize::optimize_wir(&mut wir, opt_level, &logger);
                wir
            });
            let optimized = Some(flat);

            (mono_text, lower_text, optimized, wir_package)
        } else {
            (None, None, None, None)
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
        monomorphized_tir_text,
        lowered_tir_text,
        optimized_package,
        wir_package,
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
        is_todo_module: parser.has_todo(),
    })?;

    // Unparse (no lowering - preserve high-level constructs)
    let unparser = unparse::Unparser::new(&comment_map);
    Ok(unparser.unparse(&ast))
}

/// Result of parsing a source file (AST + comments, no compilation)
pub struct ParseResult {
    pub ast: ast::Module,
    pub comments: comment::CommentMap,
}

/// Parse a Wado source file into AST and comment map.
/// This is a lightweight operation that only lexes and parses.
pub fn parse(source: &str) -> Result<ParseResult, CompileError> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| CompileError::Lexer {
        message: e.message,
        line: e.span.line,
        column: e.span.column,
        filename: None,
    })?;
    let (data_section, comments, shebang) = lexer.into_parts();
    let comment_map = comment::CommentMap::from_comments(comments, source);
    let mut parser = Parser::with_metadata(tokens, shebang, data_section);
    let ast = parser.parse().map_err(|e| CompileError::Parser {
        message: e.message,
        line: e.span.line,
        column: e.span.column,
        filename: None,
        is_todo_module: parser.has_todo(),
    })?;
    Ok(ParseResult {
        ast,
        comments: comment_map,
    })
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
        /// True if the module had `#![TODO]` before the parse error occurred.
        is_todo_module: bool,
    },
    /// Binding error (local name resolution)
    Bind {
        message: String,
        filename: Option<String>,
    },
    /// Semantic analysis error
    Analyzer {
        message: String,
        line: usize,
        column: usize,
        filename: Option<String>,
    },
}

impl CompileError {
    /// Returns true if the error occurred in a `#![TODO]` module.
    pub fn is_todo_module(&self) -> bool {
        matches!(
            self,
            CompileError::Parser {
                is_todo_module: true,
                ..
            }
        )
    }
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
                ..
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
            CompileError::Analyzer {
                message,
                line,
                column,
                filename,
            } => {
                if let Some(file) = filename {
                    write!(f, "{file}:{line}:{column}: analysis error: {message}")
                } else if *line > 0 {
                    write!(f, "{line}:{column}: analysis error: {message}")
                } else {
                    write!(f, "analysis error: {message}")
                }
            }
        }
    }
}

impl std::error::Error for CompileError {}
