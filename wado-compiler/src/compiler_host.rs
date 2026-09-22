//! Compiler host abstraction for I/O operations
//!
//! This module provides the `CompilerHost` trait that abstracts source loading
//! and diagnostic output, enabling the compiler to run in different environments:
//! - CLI with filesystem access
//! - Browser with in-memory sources
//! - LSP with editor buffers
//!
//! See WEP: `CompilerHost` Abstraction for Compiler I/O

use std::future::Future;

use crate::ast::AstIdSpace;
use crate::hashmap;
use crate::kiln::options_check::CanonicalOptions;
use crate::token::Span;

/// Log level for filtering diagnostics and log messages
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum LogLevel {
    /// Show all messages including debug
    Debug,
    /// Show info, warnings, and errors
    #[default]
    Info,
    /// Show only warnings and errors
    Warn,
    /// Show only errors
    Error,
    /// Show nothing
    Off,
}

/// Severity level for diagnostics
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Fatal error (immediately stops compilation, e.g., too many errors)
    Fatal,
    /// Compilation error (prevents successful compilation)
    Error,
    /// Warning (compilation continues but may indicate issues)
    Warning,
    /// Informational message
    Info,
    /// Debug message
    Debug,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Fatal => write!(f, "fatal"),
            Severity::Error => write!(f, "error"),
            Severity::Warning => write!(f, "warning"),
            Severity::Info => write!(f, "info"),
            Severity::Debug => write!(f, "debug"),
        }
    }
}

/// Diagnostic code for categorizing messages
///
/// Named codes without payloads for clear categorization.
/// The actual details go in the `Diagnostic::message` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    // Lexer and parser errors
    /// Source the lexer or the parser could not read as Wado
    InvalidSyntax,

    // Name resolution and binding errors
    /// A name that reaches no declaration, binding, or labeled block
    UndefinedVariable,
    /// A receiver type that has no member by the name the call spells
    MethodNotFound,
    /// A `use` naming a symbol its module does not export
    ImportNotFound,
    /// Duplicate definition
    DuplicateDefinition,
    /// Cannot assign to immutable variable
    ImmutableAssignment,
    /// Variable used before it was definitely initialized
    UninitializedVariable,
    /// Function declared without a body where nothing supplies one
    MissingFunctionBody,
    /// An `#[unavailable]` that cannot report what it was written to report
    MalformedUnavailable,
    /// A site naming a declaration that reports a reason instead of a body
    Unavailable,

    // Type errors
    /// A value whose type is not the one the position requires
    TypeMismatch,
    /// A count the declaration does not take: arguments, type arguments, or a
    /// trait method's parameters
    ArityMismatch,
    /// A callee whose type is not a function
    NotCallable,
    /// Inference has nothing to settle a type from, so the site must spell it
    NeedsTypeAnnotation,
    /// A type that does not implement a trait a bound requires
    TraitBoundNotSatisfied,
    /// More than one candidate applies, and nothing ranks them
    AmbiguousCandidate,
    /// A method reached through a trait that is not imported here
    TraitNotImported,
    /// A trait or `impl` declaration that cannot stand as written
    TraitDeclInvalid,
    /// A receiver whose mode or presence does not match the declaration
    ReceiverMismatch,
    /// A struct literal whose fields do not match the declaration
    StructFieldMismatch,
    /// A path that must deliver a value and does not
    MissingReturn,
    /// A closure written where its form is not admitted
    ClosureInvalid,
    /// A type with no representation at the Component Model boundary
    CmBoundaryType,
    /// A `with ... do` whose handler does not implement the effect
    EffectHandlerInvalid,
    /// Unknown type name
    UnknownType,
    /// Invalid type cast
    InvalidCast,
    /// Generic function reference cannot be typed at this position:
    /// missing or mismatching turbofish, no usable expected type, or
    /// arity mismatch against an expected `fn(...)` signature.
    GenericFunctionRef,

    // Module errors
    /// An import path that resolves to no module
    ModuleNotFound,
    /// Circular dependency detected
    CircularDependency,
    /// A `#![stdlib]` that does not name a bundled stdlib module
    StdlibAttr,
    /// Import of a symbol that is not visible at the import site
    /// (file-private, or `internal` reached from another package).
    PrivateSymbol,

    // I/O errors
    /// File read error
    FileReadError,

    // Coherence errors
    /// Orphan rule violation
    OrphanRule,

    // Codegen errors
    /// Code generation failed
    CodegenError,
    /// Unsupported feature
    UnsupportedFeature,

    // Span tracking codes (for logging/profiling)
    /// Start of a span (phase, operation, etc.)
    SpanStart,
    /// End of a span (phase, operation, etc.)
    SpanEnd,

    // General logging
    /// Generic log message (info, debug, etc.)
    Log,
    /// Optimizer remark (residual cost that survived optimization).
    Remark,

    // Unused diagnostics (lints)
    /// A function is never reached from the export boundary.
    DeadFunction,
    /// A global is never reached from the export boundary.
    DeadGlobal,
    /// A function is reached only from `test` blocks, never from production.
    TestOnlyFunction,
    /// A global is reached only from `test` blocks, never from production.
    TestOnlyGlobal,
    /// A binder takes a name that already reaches a declaration or an enclosing
    /// binding.
    ShadowedName,
    /// A trait head says nothing about the effects its impls may declare.
    UndecidedEffects,

    // Kiln errors
    /// A generator's `Options` struct uses a shape not supported by Kiln.
    GeneratorOptionsUnsupported,
    /// A generator invocation's options value failed typed validation.
    GeneratorOptionsInvalid,
    /// Generator cache is stale and host cannot re-run generators (consume-only mode).
    KilnStaleCache,
    /// A Kiln generator package imports an interface the sandbox forbids.
    KilnGeneratorForbiddenImport,
    /// A `use ... from "<path>"` whose source is a non-`.wado` schema is missing
    /// the required `with { generator: { ... } }` clause.
    KilnMissingWith,
    /// A `use ... from "<path>"` names a generator, but no invocation produced a
    /// module for it, so there is nothing to import.
    KilnNoGeneratedModule,
    /// A generated `.wado` file on disk has been modified after generation
    /// (cache key matches the per-invocation `<primary>.kiln.json` cache file
    /// but on-disk content does not). The edit is honored — compilation
    /// proceeds against the on-disk content.
    KilnGeneratedModified,
    /// On a cache miss, the generator produced bytes that differ from the
    /// pre-existing file at the same path. The new bytes overwrite the old.
    KilnGeneratedRegenerated,
    /// Two distinct generator invocations resolve to the same loader identity
    /// and `from` schema but redirect to different generated modules. The
    /// redirect index cannot represent both, so the conflict is reported
    /// instead of silently dropping one.
    KilnRedirectConflict,
    /// An attribute no schema in `crate::attribute` describes.
    UnknownAttr,
    /// An attribute written where it does not belong, or with arguments its
    /// schema does not admit.
    AttrMisuse,
    /// A `#[compiler_item("...")]` attribute is malformed — the name
    /// is unknown, the attribute is attached to the wrong declaration
    /// kind, or it appears outside a `core::*` stdlib module.
    CompilerItemAttr,
    /// A `#[result(...)]` attribute is malformed — no argument, an unknown
    /// convention, or a `part_of` naming something that is not a parameter.
    ResultAttr,
    /// A `#[retain(...)]` attribute is malformed, names something that is not a
    /// parameter, or sits on a declaration that has a body to read instead.
    RetainAttr,
    /// An `#[immediate(...)]` attribute is malformed, names something that is
    /// not a parameter, or sits on a declaration with a body.
    ImmediateAttr,
    /// A `#[wire(number = N)]` is out of range, reserved, repeated within one
    /// struct, or written on some of a struct's fields and not the rest.
    WireNumber,
    ResourceExtends,

    // Compile-time parameters (`#[param]`)
    /// A `#[param]` attribute is malformed (on a mutable global, an unknown
    /// argument, an empty `name`, or a non-built-in type in v1).
    ParamAttr,
    /// A `-D NAME=value` override matched no `#[param]` declaration.
    ParamUnknown,
    /// A resolved parameter override could not be converted to the declared type.
    ParamInvalid,
    /// A `#[param]` declaration received no override; the initializer is used.
    ParamMissing,
}

impl Code {
    pub fn is_unused_lint(&self) -> bool {
        matches!(
            self,
            Code::DeadFunction | Code::DeadGlobal | Code::TestOnlyFunction | Code::TestOnlyGlobal
        )
    }
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Code::InvalidSyntax => "INVALID_SYNTAX",
            Code::UndefinedVariable => "UNDEFINED_VARIABLE",
            Code::MethodNotFound => "METHOD_NOT_FOUND",
            Code::ImportNotFound => "IMPORT_NOT_FOUND",
            Code::DuplicateDefinition => "DUPLICATE_DEFINITION",
            Code::ImmutableAssignment => "IMMUTABLE_ASSIGNMENT",
            Code::UninitializedVariable => "UNINITIALIZED_VARIABLE",
            Code::MissingFunctionBody => "MISSING_FUNCTION_BODY",
            Code::MalformedUnavailable => "MALFORMED_UNAVAILABLE",
            Code::Unavailable => "UNAVAILABLE",
            Code::TypeMismatch => "TYPE_MISMATCH",
            Code::ArityMismatch => "ARITY_MISMATCH",
            Code::NotCallable => "NOT_CALLABLE",
            Code::NeedsTypeAnnotation => "NEEDS_TYPE_ANNOTATION",
            Code::TraitBoundNotSatisfied => "TRAIT_BOUND_NOT_SATISFIED",
            Code::AmbiguousCandidate => "AMBIGUOUS_CANDIDATE",
            Code::TraitNotImported => "TRAIT_NOT_IMPORTED",
            Code::TraitDeclInvalid => "TRAIT_DECL_INVALID",
            Code::ReceiverMismatch => "RECEIVER_MISMATCH",
            Code::StructFieldMismatch => "STRUCT_FIELD_MISMATCH",
            Code::MissingReturn => "MISSING_RETURN",
            Code::ClosureInvalid => "CLOSURE_INVALID",
            Code::CmBoundaryType => "CM_BOUNDARY_TYPE",
            Code::EffectHandlerInvalid => "EFFECT_HANDLER_INVALID",
            Code::UnknownType => "UNKNOWN_TYPE",
            Code::InvalidCast => "INVALID_CAST",
            Code::ModuleNotFound => "MODULE_NOT_FOUND",
            Code::CircularDependency => "CIRCULAR_DEPENDENCY",
            Code::StdlibAttr => "STDLIB_ATTR",
            Code::PrivateSymbol => "PRIVATE_SYMBOL",
            Code::FileReadError => "FILE_READ_ERROR",
            Code::OrphanRule => "ORPHAN_RULE",
            Code::CodegenError => "CODEGEN_ERROR",
            Code::UnsupportedFeature => "UNSUPPORTED_FEATURE",
            Code::SpanStart => "SPAN_START",
            Code::SpanEnd => "SPAN_END",
            Code::Log => "LOG",
            Code::Remark => "REMARK",
            Code::DeadFunction => "DEAD_FUNCTION",
            Code::DeadGlobal => "DEAD_GLOBAL",
            Code::TestOnlyFunction => "TEST_ONLY_FUNCTION",
            Code::TestOnlyGlobal => "TEST_ONLY_GLOBAL",
            Code::ShadowedName => "SHADOWED_NAME",
            Code::UndecidedEffects => "UNDECIDED_EFFECTS",
            Code::GeneratorOptionsUnsupported => "GENERATOR_OPTIONS_UNSUPPORTED",
            Code::GeneratorOptionsInvalid => "GENERATOR_OPTIONS_INVALID",
            Code::KilnStaleCache => "KILN_STALE_CACHE",
            Code::KilnGeneratorForbiddenImport => "KILN_GENERATOR_FORBIDDEN_IMPORT",
            Code::KilnMissingWith => "KILN_MISSING_WITH",
            Code::KilnNoGeneratedModule => "KILN_NO_GENERATED_MODULE",
            Code::KilnGeneratedModified => "KILN_GENERATED_MODIFIED",
            Code::KilnGeneratedRegenerated => "KILN_GENERATED_REGENERATED",
            Code::KilnRedirectConflict => "KILN_REDIRECT_CONFLICT",
            Code::UnknownAttr => "UNKNOWN_ATTR",
            Code::AttrMisuse => "ATTR_MISUSE",
            Code::CompilerItemAttr => "COMPILER_ITEM_ATTR",
            Code::ResultAttr => "RESULT_ATTR",
            Code::RetainAttr => "RETAIN_ATTR",
            Code::ImmediateAttr => "IMMEDIATE_ATTR",
            Code::WireNumber => "WIRE_NUMBER",
            Code::ResourceExtends => "RESOURCE_EXTENDS",
            Code::ParamAttr => "PARAM_ATTR",
            Code::ParamUnknown => "PARAM_UNKNOWN",
            Code::ParamInvalid => "PARAM_INVALID",
            Code::ParamMissing => "PARAM_MISSING",
            Code::GenericFunctionRef => "GENERIC_FUNCTION_REF",
        };
        write!(f, "{name}")
    }
}

/// A compiler diagnostic (error, warning, etc.)
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// Severity level
    pub severity: Severity,
    /// Code categorizing the diagnostic
    pub code: Code,
    /// Human-readable message
    pub message: String,
    /// Source location (if available)
    pub span: Option<DiagnosticSpan>,
}

/// Source location for a diagnostic
#[derive(Debug, Clone)]
pub struct DiagnosticSpan {
    /// File path or module name
    pub file: String,
    /// Line number (1-based)
    pub line: usize,
    /// Column number (1-based)
    pub column: usize,
    /// Optional end position for ranges
    pub end_line: Option<usize>,
    pub end_column: Option<usize>,
    /// The parse these coordinates index, carried from the [`Span`].
    /// [`crate::logger::Logger`] renders `file` from it, so the location stays
    /// whole however far the diagnostic travels from the walk that raised it.
    pub space: AstIdSpace,
}

impl DiagnosticSpan {
    /// Create a `DiagnosticSpan` from a Span and optional filename
    pub fn from_span(span: &Span, filename: Option<&str>) -> Self {
        DiagnosticSpan {
            file: filename.unwrap_or_default().to_string(),
            line: span.line,
            column: span.column,
            end_line: Some(span.end_line),
            end_column: Some(span.end_column),
            space: span.space,
        }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(span) = &self.span {
            // A span whose file the reporter did not know still places the
            // error in a source; naming no file beats a leading colon.
            let file = if span.file.is_empty() {
                String::new()
            } else {
                format!("{}:", span.file)
            };
            write!(
                f,
                "{file}{}:{}: {}: {}",
                span.line, span.column, self.severity, self.message
            )
        } else {
            write!(f, "{}: {}", self.severity, self.message)
        }
    }
}

/// Error returned when source loading fails
#[derive(Debug, Clone)]
pub enum SourceError {
    /// Module/file was not found
    NotFound { path: String },
    /// I/O error reading the source
    IoError { path: String, message: String },
    /// Network error (for future HTTP support)
    NetworkError { url: String, message: String },
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::NotFound { path } => write!(f, "module not found: {path}"),
            SourceError::IoError { path, message } => {
                write!(f, "error reading '{path}': {message}")
            }
            SourceError::NetworkError { url, message } => {
                write!(f, "network error fetching '{url}': {message}")
            }
        }
    }
}

impl std::error::Error for SourceError {}

/// Minimal trait for emitting span start/end events during optimization.
///
/// This enables optimization passes to emit profiling spans without depending
/// on the full `Logger` type. The CLI host adds timestamps to measure duration.
pub trait SpanEmitter {
    fn span_start(&self, name: &str);
    fn span_end(&self, name: &str);
    fn debug(&self, message: &str);
}

/// No-op implementation for when profiling is not needed.
pub struct NullSpanEmitter;

impl SpanEmitter for NullSpanEmitter {
    fn span_start(&self, _name: &str) {}
    fn span_end(&self, _name: &str) {}
    fn debug(&self, _message: &str) {}
}

/// Abstraction for compiler I/O, letting the compiler run under a filesystem,
/// in-memory, browser, or LSP host. Standard library paths (`core:*`, `wasi:*`)
/// never reach `load_source`; the compiler serves them from embedded sources.
pub trait CompilerHost: Send + Sync {
    /// Load a file as raw bytes — the compiler's single I/O primitive, used for
    /// both `.wado` sources and `#include_bytes` assets, with the compiler
    /// interpreting them as UTF-8 where appropriate. `path` is a local path or a
    /// remote URL; stdlib paths never arrive here.
    fn load_source(&self, path: &str) -> impl Future<Output = Result<Vec<u8>, SourceError>> + Send;

    /// Whether `path` names something this host can load, without loading it.
    ///
    /// The default *does* load; a filesystem host overrides it with a stat so a
    /// presence check does not pay for the bytes.
    fn source_exists(&self, path: &str) -> impl Future<Output = bool> + Send {
        async move { self.load_source(path).await.is_ok() }
    }

    /// Emit a diagnostic (error, warning, etc.)
    ///
    /// This method is called synchronously by the compiler whenever a diagnostic
    /// needs to be reported. Implementations can print to stderr, collect into
    /// a list, send to an LSP client, etc.
    fn emit_diagnostic(&self, diagnostic: Diagnostic);

    /// Save bytes the compiler produced but cannot explain, under a name of
    /// the host's choosing carrying `file_stem`, and answer where they landed.
    /// The default keeps none: a host with no filesystem has nowhere to put
    /// them.
    fn save_internal_artifact(&self, file_stem: &str, bytes: &[u8]) -> Option<String> {
        let _ = (file_stem, bytes);
        None
    }

    /// Execute a Kiln generator component and return its response. The host
    /// instantiates `component_wasm` and links `core:kiln/host` so
    /// `emit-diagnostic` forwards back into itself. The default `Unsupported`
    /// drives consume-only mode, reusing cached outputs and warning on drift;
    /// a host with a Wasm runtime overrides it. Protocol: WEP 2026-04-12.
    fn run_generator(
        &self,
        _component_wasm: &[u8],
        _request: GeneratorRequest,
    ) -> impl Future<Output = Result<GeneratorResponse, GeneratorRunnerError>> + Send {
        async move { Err(GeneratorRunnerError::Unsupported) }
    }

    /// Ask a generator how many leading bytes of each input determine its
    /// output, one answer per file in declaration order. `None` in a slot —
    /// and the default empty answer — means the whole file, which is what a
    /// generator exporting no `probe` gets. Protocol: WEP 2026-04-12.
    fn probe_generator(
        &self,
        _component_wasm: &[u8],
        _request: &GeneratorRequest,
    ) -> impl Future<Output = Result<Vec<Option<u64>>, GeneratorRunnerError>> + Send {
        async move { Ok(Vec::new()) }
    }

    /// Resolve `[dependencies]` for bare-name `use { … } from "<name>"`.
    /// Consulted once when the module loader is created; empty by default
    /// (single-file and in-memory hosts have no manifest).
    fn dependency_index(&self) -> DependencyIndex {
        DependencyIndex::default()
    }

    /// Read an environment variable at compile time, for `#[param(from_env =
    /// "...")]` resolution (see `wep-2026-04-26-compile-time-params.md`).
    ///
    /// Kept on the host because reading process env is impure and unavailable
    /// on `wasm32` targets; the CLI host forwards to `std::env::var`, while
    /// pure hosts (LSP, in-memory) return `None`. The `-D` overrides travel
    /// with `CompilerOptions`, not here, since the resolution pass enumerates
    /// them to detect unknown names.
    fn env_var(&self, _name: &str) -> Option<String> {
        None
    }
}

/// The project's `[dependencies]`, resolved for bare-name imports.
#[derive(Debug, Clone, Default)]
pub struct DependencyIndex {
    /// name → entry-module path (the dependency's `[package].lib`), relative
    /// to the host base. These are *source* (path) dependencies, compiled into
    /// the consuming component.
    pub resolved: hashmap::IndexMap<String, String>,
    /// name → local `.wasm` path of a *prebuilt component* dependency (a
    /// registry dependency fetched by the CLI), relative to the host base.
    /// Imported across the Component Model boundary, like a
    /// `with { type: "wasm" }` asset, rather than compiled from source.
    pub components: hashmap::IndexMap<String, String>,
    /// name → human-readable reason a *declared* dependency could not be
    /// resolved (e.g. its package declares no `[package].lib`). Surfaced at
    /// the `use` site instead of a generic "invalid module path".
    pub unresolved: hashmap::IndexMap<String, String>,
}

/// Request handed to a Kiln generator by the compiler.
///
/// Carries the `generate(primary, inputs, options)` arguments in a
/// wasmtime-independent form; the host lifts/lowers at its own boundary.
#[derive(Debug, Clone)]
pub struct GeneratorRequest {
    /// The schema file named by `from` in the invocation.
    pub primary: GeneratorInputFile,
    /// Supplementary schema files.
    pub inputs: Vec<GeneratorInputFile>,
    /// The validated, typed options for this invocation. The host builds a
    /// Component-Model value from it — shaped by the generator component's own
    /// introspected `generate` options parameter — and passes it as a typed
    /// argument, so the generator receives its `Options` directly. Empty when
    /// the generator takes no options.
    pub options: CanonicalOptions,
}

impl GeneratorRequest {
    /// Every input file, primary first. This is the order a probe answers in
    /// and the order extents are recorded in, so it lives here rather than
    /// being re-spelled at each site that walks them.
    pub fn files(&self) -> impl Iterator<Item = &GeneratorInputFile> {
        std::iter::once(&self.primary).chain(self.inputs.iter())
    }

    /// [`Self::files`], for a caller that rewrites what it walks.
    pub fn files_mut(&mut self) -> impl Iterator<Item = &mut GeneratorInputFile> {
        std::iter::once(&mut self.primary).chain(self.inputs.iter_mut())
    }
}

/// One schema file passed to a Kiln generator.
#[derive(Debug, Clone)]
pub struct GeneratorInputFile {
    pub path: String,
    /// Raw bytes of the file. A checkpoint is not text, and the generator
    /// receives these as a `stream<u8>` it reads only as far as it needs.
    pub content: Vec<u8>,
}

/// Response returned by a Kiln generator.
#[derive(Debug, Clone)]
pub struct GeneratorResponse {
    /// Generated Wado source files.
    pub files: Vec<GeneratorOutputFile>,
}

/// One file produced by a Kiln generator.
#[derive(Debug, Clone)]
pub struct GeneratorOutputFile {
    /// Path relative to the invocation's output directory.
    pub path: String,
    /// UTF-8 source contents.
    pub content: String,
    /// Whether this file is the invocation's entry module.
    pub is_entry: bool,
}

/// Generator-side error, mirroring `core:kiln/types::error`.
#[derive(Debug, Clone)]
pub enum GeneratorError {
    /// The generator rejected the schema it was given.
    InvalidSchema(String),
    /// The schema used a feature the generator does not (yet) handle.
    Unsupported(String),
    /// Catch-all generator-produced error.
    Other(String),
}

impl std::fmt::Display for GeneratorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeneratorError::InvalidSchema(msg) => write!(f, "invalid schema: {msg}"),
            GeneratorError::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            GeneratorError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for GeneratorError {}

/// Outcome of calling `CompilerHost::run_generator`.
#[derive(Debug, Clone)]
pub enum GeneratorRunnerError {
    /// The host has no Wasm runtime available — trigger consume-only mode.
    Unsupported,
    /// The generator returned a typed error.
    Generator(GeneratorError),
    /// The host failed (instantiation, import linking, fuel, timeout, …).
    Host(String),
}

impl std::fmt::Display for GeneratorRunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeneratorRunnerError::Unsupported => {
                write!(f, "generator execution is not supported by this host")
            }
            GeneratorRunnerError::Generator(e) => write!(f, "generator error: {e}"),
            GeneratorRunnerError::Host(msg) => write!(f, "generator host error: {msg}"),
        }
    }
}

impl std::error::Error for GeneratorRunnerError {}

/// Severity mirrored from `core:kiln/host::diagnostic-level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratorDiagnosticLevel {
    Error,
    Warning,
    Info,
    Hint,
}

/// Span into a file known to the generator. Paths are relayed back to the
/// compiler's diagnostic renderer, which re-reads the file via `load_source`.
#[derive(Debug, Clone)]
pub struct GeneratorSourceSpan {
    pub path: String,
    pub byte_start: u32,
    pub byte_end: u32,
}

/// A diagnostic produced by a generator via `host::emit-diagnostic`.
#[derive(Debug, Clone)]
pub struct GeneratorDiagnostic {
    pub level: GeneratorDiagnosticLevel,
    pub span: Option<GeneratorSourceSpan>,
    pub message: String,
}

/// The Kiln WIT world, embedded so the compiler can treat its byte identity
/// as part of every cache key. The single source of truth; `wado-cli` reads
/// the same file via `wasmtime::component::bindgen!(path = "...")`.
pub const KILN_GENERATOR_WIT: &str = include_str!("../lib/core/kiln/generator.wit");

/// A simple in-memory compiler host for testing
///
/// This host stores sources in an `IndexMap` and collects diagnostics in a Vec.
#[derive(Debug, Default)]
pub struct InMemoryCompilerHost {
    /// Source files by path (stored as raw bytes)
    sources: hashmap::IndexMap<String, Vec<u8>>,
    /// Collected diagnostics
    diagnostics: std::sync::Mutex<Vec<Diagnostic>>,
}

impl InMemoryCompilerHost {
    /// Create a new empty in-memory host
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a source file (text)
    pub fn add_source(&mut self, path: impl Into<String>, source: impl Into<String>) {
        self.sources.insert(path.into(), source.into().into_bytes());
    }

    /// Add a binary file
    pub fn add_bytes(&mut self, path: impl Into<String>, bytes: Vec<u8>) {
        self.sources.insert(path.into(), bytes);
    }

    /// Get all collected diagnostics
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.lock().unwrap().clone()
    }

    /// Check if any errors were reported
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    /// Clear all diagnostics
    pub fn clear_diagnostics(&self) {
        self.diagnostics.lock().unwrap().clear();
    }
}

impl CompilerHost for InMemoryCompilerHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        self.sources
            .get(path)
            .cloned()
            .ok_or_else(|| SourceError::NotFound {
                path: path.to_string(),
            })
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::AstIdSpace;
    use crate::kiln::options_check::CanonicalOptions;
    use std::assert_matches;

    #[test]
    fn test_in_memory_host() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut host = InMemoryCompilerHost::new();
                host.add_source("./test.wado", "fn run() {}");

                // Test load_source returns bytes
                let result = host.load_source("./test.wado").await;
                assert!(result.is_ok());
                assert_eq!(result.unwrap(), b"fn run() {}");

                // Test not found
                let result = host.load_source("./missing.wado").await;
                assert_matches!(result, Err(SourceError::NotFound { .. }));
            });
    }

    #[test]
    fn default_run_generator_is_unsupported() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let host = InMemoryCompilerHost::new();
                let req = GeneratorRequest {
                    primary: GeneratorInputFile {
                        path: "schema.proto".to_string(),
                        content: b"syntax = \"proto3\";".to_vec(),
                    },
                    inputs: vec![],
                    options: CanonicalOptions::default(),
                };
                let result = host.run_generator(b"\0asm", req).await;
                assert_matches!(result, Err(GeneratorRunnerError::Unsupported));
            });
    }

    #[test]
    fn kiln_generator_wit_is_embedded() {
        assert!(KILN_GENERATOR_WIT.contains("package core:kiln"));
        assert!(KILN_GENERATOR_WIT.contains("world generator"));
        // Revision 3: the world is import-only (each generator synthesizes its own
        // world with a per-generator typed `generate`), so it carries the
        // `kiln-host` import and no fixed `generate` export / `raw-request`.
        assert!(KILN_GENERATOR_WIT.contains("import kiln-host"));
        assert!(!KILN_GENERATOR_WIT.contains("export generate"));
        assert!(!KILN_GENERATOR_WIT.contains("raw-request"));
    }

    #[test]
    fn test_diagnostic_display() {
        let diag = Diagnostic {
            severity: Severity::Error,
            code: Code::InvalidSyntax,
            message: "expected ';' but found '}'".to_string(),
            span: Some(DiagnosticSpan {
                file: "test.wado".to_string(),
                line: 10,
                column: 5,
                end_line: None,
                end_column: None,
                space: AstIdSpace::FRESH,
            }),
        };

        let display = format!("{diag}");
        assert!(display.contains("test.wado:10:5"));
        assert!(display.contains("error"));
        assert!(display.contains("expected ';'"));
    }

    #[test]
    fn unused_lint_codes_are_classified() {
        for code in [
            Code::DeadFunction,
            Code::DeadGlobal,
            Code::TestOnlyFunction,
            Code::TestOnlyGlobal,
        ] {
            assert!(code.is_unused_lint(), "{code} should be an unused lint");
        }
        for code in [Code::TypeMismatch, Code::InvalidSyntax, Code::Log] {
            assert!(!code.is_unused_lint(), "{code} is not an unused lint");
        }
    }
}
