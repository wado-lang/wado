//! The compiler frontend: parsing, name resolution, elaboration, synthesis,
//! monomorphization, linking, and lowering to NIR. It is everything the
//! language service needs.

// A hosted compiler (LSP, browser, Kiln generator) has no stream to write to,
// and a `run` / `serve` driver has a user's program to keep quiet for. See
// `AGENTS.md` for where each kind of message goes instead. Not in `Cargo.toml`:
// cargo refuses a `[lints]` table that both inherits the workspace's and adds
// to it.
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

pub mod analyze;
pub mod ast;
pub mod ast_index;
pub mod attribute;
pub mod bind;
pub mod builtin_registry;
pub mod call_args;
pub mod canonical;
pub mod cm_abi;
pub mod codegen_flags;
pub mod comment;
pub mod compiler_host;
pub mod compiler_item;
pub mod component_model;
pub mod component_plan;
pub mod const_eval;
pub mod coverage;
pub mod defs;
pub mod doc;
pub mod effect_check;
pub mod elaborator;
pub(crate) mod escape;
pub mod flat_package;
pub mod format_spec;
pub mod graph;
pub mod hashmap;
pub mod intern;
pub mod kiln;
pub mod lexer;
pub mod link;
pub mod lint;
pub mod literal_cast;
pub mod loader;
pub mod logger;
pub mod lower;
pub mod module_source;
pub mod monomorphize;
pub mod name;
pub mod nir;
pub mod nir_arena;
pub mod nir_engine;
pub mod nir_package;
pub mod nir_unparse;
pub mod nir_value_graph;
pub mod nir_visitor;
pub mod niri;
pub mod package;
pub mod param_resolution;
pub mod parser;
pub mod path;
pub mod prelower_reach;
pub mod primitive;
pub mod resolve;
pub mod resource_move_check;
pub mod semantics;
pub mod signature_reach;
pub mod stdlib;
pub(crate) mod stdlib_snapshot;
pub mod symbol;
pub mod symbol_notation;
pub mod syntax;
pub mod synthesis;
pub mod test_names;
pub mod tir;
pub mod tir_visitor;
pub mod token;
pub mod trace;
pub mod trait_solver;
pub mod unparse;
pub mod unresolved_types;
pub mod wir;
pub mod wir_visitor;
pub mod wit_bundle;
pub mod wit_consume;
pub mod wit_emit;
pub mod world_registry;

pub use analyze::Analyzer;
pub use ast::{AstId, AstNodeKind, AstPtr};
pub use bind::{BindError, Binder};
pub use codegen_flags::{CodegenFlags, OptLevel};
pub use compiler_host::InMemoryCompilerHost;
pub use compiler_host::{
    Code, CompilerHost, DependencyIndex, DependencyManifest, Diagnostic, DiagnosticSpan,
    GeneratorDiagnostic, GeneratorDiagnosticLevel, GeneratorError, GeneratorInputFile,
    GeneratorOutputFile, GeneratorRequest, GeneratorResponse, GeneratorRunnerError,
    GeneratorSourceSpan, KILN_GENERATOR_WIT, LogLevel, Severity, SourceError,
};
pub use effect_check::{
    EffectError, INDIRECT_CALLEE, Impurity, PureContext, PurityError, SemanticDiagnostics,
    check_effects_semantic, check_purity_semantic, check_semantics,
};
pub use elaborator::{Elaborator, TypeError};
pub use flat_package::FlatPackage;
pub use lexer::{LexError, LexErrorKind, LexResult, lex, lex_in};
pub use lint::lint_diagnostics;
pub use loader::{LoadError, LoadResult, ModuleLoader};
pub use logger::{Bail, Logger};
pub use lower::lower;
pub use module_source::ModuleSource;
pub use monomorphize::monomorphize;
pub use package::Package;
pub use parser::{ParseError, Parser};
pub use resource_move_check::{ResourceMoveError, check_resource_moves_semantic};
pub use semantics::{
    Cursor, Definition, Semantics, SymbolResolveError, semantics, semantics_for_world, semantics_of,
};
pub use stdlib_snapshot::prelude_names;
pub use stdlib_snapshot::prewarm as prewarm_stdlib_snapshot;
pub use token::Span;
pub use trace::{TraceSink, set_sink as set_trace_sink};

/// Report a compilation error the pipeline has no span for — it names the
/// offending declaration instead.
pub fn report_without_span<H: compiler_host::CompilerHost>(
    logger: &Logger<'_, H>,
    code: compiler_host::Code,
    message: String,
) {
    let _ = logger.error(compiler_host::Diagnostic {
        severity: compiler_host::Severity::Error,
        code,
        message,
        span: None,
    });
}

/// [`report_without_span`], for a caller that stops at the first such error.
pub fn bail_with<H: compiler_host::CompilerHost>(
    logger: &Logger<'_, H>,
    code: compiler_host::Code,
    message: String,
) -> Bail {
    report_without_span(logger, code, message);
    Bail
}

/// Result of parsing a source file (AST + AstId-keyed trivia, no compilation).
///
/// Lexing and parsing are both error-recovering: `ast` always covers the
/// whole input. `lex_errors` and `errors` collect recovered problems in
/// source order; they stay separate so the wire-format diagnostic prefixes
/// (`lexer error:` / `parse error:`) stay accurate. Batch/format/doc
/// callers that need the old fail-fast behavior call
/// [`ParseResult::into_fail_fast`]; the LSP path uses the partial `ast`.
pub struct ParseResult {
    pub ast: ast::Module,
    pub trivia: comment::TriviaMap,
    /// Lexer errors recovered while tokenising, in source order.
    pub lex_errors: Vec<lexer::LexError>,
    /// Parser errors recovered while building the AST, in source order.
    pub errors: Vec<parser::ParseError>,
}

impl ParseResult {
    /// Fail-fast adapter: if lexing or parsing recovered any syntax error,
    /// return the first as a `CompileError`; otherwise yield the result
    /// unchanged. Used by batch compilation, `wado doc`, and the formatter,
    /// which must reject malformed input.
    pub fn into_fail_fast(self) -> Result<ParseResult, CompileError> {
        if let Some(e) = self.lex_errors.first() {
            return Err(CompileError::from_lex_error(e, None));
        }
        if let Some(e) = self.errors.first() {
            return Err(CompileError::from_parse_error(e, None, self.ast.has_todo()));
        }
        Ok(self)
    }
}

/// Resolve every transitive import of `parsed` and return the loaded module set
/// — stage 2 of the frontend, between [`parse`] and [`semantics::semantics_of`];
/// [`semantics::semantics`] wraps all three. `invocations` redirects bare
/// `use { … } from "<schema>"` clauses to kiln-generated entry modules, or is
/// [`kiln::InvocationIndex::new`] with no kiln pipeline to advertise.
pub async fn load<H: CompilerHost>(
    parsed: ParseResult,
    filename: Option<&str>,
    host: &H,
    invocations: kiln::InvocationIndex,
    log_level: LogLevel,
) -> Result<LoadResult, LoadError> {
    let loader = loader::ModuleLoader::new(host, log_level).with_invocations(invocations);
    loader
        .load_all_from_parsed_entry(parsed.ast, filename)
        .await
}

/// Parse a Wado source file into AST and trivia map.
/// This is a lightweight operation that only lexes and parses; both are
/// error-recovering, so the call cannot fail. Lex / parse errors are
/// surfaced via [`ParseResult::lex_errors`] / [`ParseResult::errors`].
pub fn parse(source: &str) -> ParseResult {
    let mut lex_result = lexer::lex(source);
    let lex_errors = std::mem::take(&mut lex_result.errors);
    let mut parser = Parser::from_lex(lex_result);
    let ast = parser.parse();
    let errors = parser.take_errors();
    let mut trivia = parser.take_trivia();
    comment::populate_trailing(&mut trivia, &ast);
    comment::populate_inner_tail(&mut trivia, &ast);
    ParseResult {
        ast,
        trivia,
        lex_errors,
        errors,
    }
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
    /// The formatter would not round-trip the input (e.g. it would drop a
    /// comment). Reported instead of silently emitting lossy output.
    Format {
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

    /// Name the file the error is in, for a caller that has a path where the
    /// erroring API — [`format`], which takes a string — did not.
    pub fn with_filename(mut self, path: &str) -> Self {
        let slot = match &mut self {
            CompileError::Io { .. } => None,
            CompileError::Lexer { filename, .. }
            | CompileError::Parser { filename, .. }
            | CompileError::Bind { filename, .. }
            | CompileError::Analyzer { filename, .. }
            | CompileError::Format { filename, .. } => Some(filename),
        };
        if let Some(filename) = slot {
            *filename = Some(path.to_string());
        }
        self
    }

    /// Build a `CompileError::Lexer` from a recovered [`lexer::LexError`].
    /// Single projection consulted by every fail-fast site so message /
    /// line / column extraction lives in one place.
    pub fn from_lex_error(e: &lexer::LexError, filename: Option<&str>) -> Self {
        CompileError::Lexer {
            message: e.to_string(),
            line: e.span.line,
            column: e.span.column,
            filename: filename.map(String::from),
        }
    }

    /// Build a `CompileError::Parser` from a recovered
    /// [`parser::ParseError`]. Mirrors [`Self::from_lex_error`] for the parse
    /// fail-fast path; `is_todo_module` comes from the surrounding AST.
    pub fn from_parse_error(
        e: &parser::ParseError,
        filename: Option<&str>,
        is_todo_module: bool,
    ) -> Self {
        CompileError::Parser {
            message: e.message.clone(),
            line: e.span.line,
            column: e.span.column,
            filename: filename.map(String::from),
            is_todo_module,
        }
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
            CompileError::Format {
                message,
                line,
                column,
                filename,
            } => {
                if let Some(file) = filename {
                    write!(f, "{file}:{line}:{column}: format error: {message}")
                } else {
                    write!(f, "{line}:{column}: format error: {message}")
                }
            }
        }
    }
}

impl std::error::Error for CompileError {}

/// Format Wado source code canonically, preserving comments and the `__DATA__`
/// section.
pub fn format(source: &str) -> Result<String, CompileError> {
    // Formatting requires a clean parse — the first recovered lex error
    // becomes a `CompileError::Lexer`.
    let lex_result = lexer::lex(source);
    if let Some(e) = lex_result.errors.first() {
        return Err(CompileError::from_lex_error(e, None));
    }
    let mut parser = Parser::from_lex(lex_result);
    // Formatting requires a clean parse: reject the first recovered error.
    let ast = parser.parse();
    if let Some(e) = parser.take_errors().first() {
        return Err(CompileError::from_parse_error(e, None, parser.has_todo()));
    }
    let mut trivia = parser.take_trivia();
    comment::populate_trailing(&mut trivia, &ast);
    comment::populate_inner_tail(&mut trivia, &ast);

    // Unparse (no lowering - preserve high-level constructs)
    let unparser = unparse::Unparser::new().with_trivia(&trivia);
    let formatted = unparser.unparse(&ast);

    // The unparser places every comment or flushes it at the enclosing
    // statement, item or module, so this fires only if that guarantee breaks.
    if let Some(missing) = dropped_comment(source, &formatted) {
        return Err(CompileError::Format {
            message: format!("formatting would drop a comment (`{}`)", missing.text),
            line: missing.line,
            column: missing.column,
            filename: None,
        });
    }
    Ok(formatted)
}

/// A comment the formatter would drop, with where it sits in the source.
struct DroppedComment {
    /// The comment as written, delimiter included, cut to a readable length.
    text: String,
    line: usize,
    column: usize,
}

/// A comment present in `before` but missing from `after` (by delimiter+text
/// multiset; `emit_comment` is verbatim so relocation keeps the same key).
fn dropped_comment(before: &str, after: &str) -> Option<DroppedComment> {
    use crate::hashmap::IndexMap;
    fn delim(kind: comment::CommentKind) -> &'static str {
        match kind {
            comment::CommentKind::Line => "//",
            comment::CommentKind::DocLine => "///",
            comment::CommentKind::ModuleDoc => "//!",
            comment::CommentKind::Block => "/*",
        }
    }
    fn bag(comments: &[comment::Comment]) -> IndexMap<(&'static str, &str), usize> {
        let mut bag = IndexMap::default();
        for c in comments {
            *bag.entry((delim(c.kind), c.text.as_str())).or_default() += 1;
        }
        bag
    }
    let before_comments = lexer::comments_deep(before);
    let after_comments = lexer::comments_deep(after);
    let before_bag = bag(&before_comments);
    let after_bag = bag(&after_comments);
    for c in &before_comments {
        let key = (delim(c.kind), c.text.as_str());
        let before_count = before_bag.get(&key).copied().unwrap_or(0);
        if before_count > after_bag.get(&key).copied().unwrap_or(0) {
            // As written: a trimmed text does not match a grep for it.
            let snippet: String = c.text.chars().take(40).collect();
            return Some(DroppedComment {
                text: format!("{}{snippet}", delim(c.kind)),
                line: c.span.line,
                column: c.span.column,
            });
        }
    }
    None
}
