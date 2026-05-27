//! Module loader for Wado
//!
//! Loads all modules (entry module + dependencies) upfront before analysis.
//! This enables converting ALL modules to TIR before codegen.

use std::collections::VecDeque;

use crate::hashmap::IndexSet;

use crate::hashmap::IndexMap;
use rustc_hash::FxBuildHasher;

use crate::ast::{Item, Module};
use crate::bind;
use crate::compiler_host::{CompilerHost, SourceError};
use crate::lexer::Lexer;
use crate::logger::Logger;
use crate::module_source::{ModuleSource, ModuleSourceInterner, WasmAssetKind};
use crate::name::{normalize_module_path, resolve_module_path};
use crate::parser::Parser;
use crate::stdlib;

/// Error that can occur during module loading
#[derive(Debug, Clone)]
pub enum LoadError {
    /// Module was not found. `path` is the canonical display form
    /// (`"core:foo"`, `"./bar.wado"`, `"<entry>"`) — kept as a plain
    /// `String` so error construction does not need a
    /// `ModuleSourceInterner`. This matches the shape of the other
    /// `path`-bearing variants below (`IoError`, `InvalidModulePath`).
    ModuleNotFound { path: String },
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
    /// Wasm-asset import (`with { type: "wat"|"wasm" }`) failed validation.
    WasmImport {
        module_source: ModuleSource,
        message: String,
    },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::ModuleNotFound { path } => {
                write!(f, "module not found: {path}")
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
            LoadError::WasmImport {
                module_source,
                message,
            } => {
                write!(f, "wasm import error in {module_source}: {message}")
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
            LoadError::WasmImport {
                ref module_source,
                ref message,
            } => Self {
                severity: Severity::Error,
                code: Code::InvalidSyntax,
                message: format!("wasm import error in {module_source}: {message}"),
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
            SourceError::NotFound { path } => LoadError::ModuleNotFound { path },
            SourceError::IoError { path, message } => LoadError::IoError { path, message },
            SourceError::NetworkError { url, message } => LoadError::IoError { path: url, message },
        }
    }
}

/// A core wasm value type, in the subset Phase 1 supports.
///
/// Maps 1:1 to a Wado primitive at TIR synthesis time. Other core
/// types (reference types, etc.) are not yet permitted in imported
/// wasm assets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WasmCoreValType {
    I32,
    I64,
    F32,
    F64,
    V128,
}

impl WasmCoreValType {
    fn from_wasmparser(ty: wasmparser::ValType) -> Option<Self> {
        match ty {
            wasmparser::ValType::I32 => Some(Self::I32),
            wasmparser::ValType::I64 => Some(Self::I64),
            wasmparser::ValType::F32 => Some(Self::F32),
            wasmparser::ValType::F64 => Some(Self::F64),
            wasmparser::ValType::V128 => Some(Self::V128),
            wasmparser::ValType::Ref(_) => None,
        }
    }
}

/// Function signature of an export from an imported wasm asset.
#[derive(Debug, Clone)]
pub struct WasmExportSig {
    /// Export name.
    pub name: String,
    /// Parameter types.
    pub params: Vec<WasmCoreValType>,
    /// Result types. Phase 1 accepts 0 or 1 results; multi-return is rejected.
    pub results: Vec<WasmCoreValType>,
}

/// Loaded wasm asset: post-`wat::parse_bytes` core wasm bytes plus the
/// extracted export signatures used by TIR synthesis.
#[derive(Debug, Clone)]
pub struct WasmAsset {
    /// Core wasm bytes (always binary; `.wat` is parsed at load time).
    pub bytes: Vec<u8>,
    /// Function exports (kind = func) ordered by their wasm order.
    pub function_exports: Vec<WasmExportSig>,
}

/// Result of loading all modules
pub struct LoadResult {
    /// All loaded modules (module source -> parsed + bound AST)
    pub modules: IndexMap<ModuleSource, Module>,
    /// The entry module source
    pub entry_module_source: ModuleSource,
    /// Entry module AST. Always identical to the `modules` entry for
    /// [`Self::entry_module_source`]; kept as a separate field for
    /// tooling that takes the entry AST by value.
    pub entry_ast: Module,
    /// Modules that were implicitly loaded (not from user imports)
    pub implicit_modules: IndexSet<ModuleSource>,
    /// Included file contents from `#include_str` and `#include_bytes`.
    /// Key is `(module_source_display, raw_path)`, value is raw bytes.
    pub included_files: IndexMap<[String; 2], Vec<u8>>,
    /// Wasm assets loaded via `use ... from "<path>" with { type: "wat"|"wasm" }`.
    ///
    /// Keyed by the canonical namespace string (`wasm:<canonical_path>`,
    /// matching the namespace component of `#[canonical("wasm:<path>", ...)]`
    /// attributes). Each value holds the post-`wat::parse_bytes` core wasm
    /// bytes plus the extracted function-export signatures used by TIR
    /// synthesis.
    pub wasm_assets: IndexMap<String, WasmAsset>,
    /// Kiln invocation redirects propagated from the loader so later phases
    /// (analyze, elaborator) can also rewrite `use ... from "<schema>"`
    /// clauses consistently.
    pub invocations: crate::kiln::InvocationIndex,
    /// `ModuleSource` interner created during loading. Downstream phases
    /// (analyze / elaborator / synthesis / monomorphize) borrow this to
    /// canonicalize any `ModuleSource` they construct so that ptr-eq
    /// remains a valid identity check across phases.
    pub interner: ModuleSourceInterner,
}

use crate::compiler_host::LogLevel;

/// Strip a leading `kiln:` URI scheme, returning the path component.
///
/// Used by [`ModuleLoader::get_source`] when handling
/// `ModuleSource::Redirected` so the host receives a plain absolute
/// path instead of a `kiln:/abs/path` URI. Returns `None` when the
/// input does not look like a `kiln:` URI; callers fall back to
/// passing the raw URI through to the host (in-memory hosts may use it
/// as a key directly).
///
/// Parses with [`fluent_uri::UriRef`] so RFC 3986 percent-encoding and
/// scheme validation behave correctly without depending on
/// `std::path`. The returned path is the URI's `path()` segment as a
/// `String`; percent-decoding happens at the host boundary via
/// standard `read_to_string`.
///
/// The `kiln:` (rather than `file://`) scheme is intentional: the
/// compiler's qualified-name format (`{module_source}//{name}`) treats
/// `//` as the boundary, and `file://` URIs would introduce a
/// spurious `//` that breaks every parser splitting on it. See
/// `wado-cli::kiln_driver::path_to_kiln_uri` for the producer side.
fn strip_kiln_scheme(uri: &str) -> Option<String> {
    let parsed = fluent_uri::UriRef::parse(uri).ok()?;
    if parsed.scheme()?.as_str() != "kiln" {
        return None;
    }
    Some(parsed.path().as_str().to_string())
}

/// `true` when `path` looks like a non-`.wado` schema source (i.e. has any
/// extension other than `.wado`). Wado modules and bare paths with no
/// extension fall through to normal resolution.
fn is_non_wado_schema(path: &str) -> bool {
    match path.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && !ext.contains('/') => {
            !ext.eq_ignore_ascii_case("wado")
        }
        _ => false,
    }
}

/// Extract the wasm-asset kind from a use declaration's `with { ... }`
/// attributes. Returns `Some(kind)` for `with { type: "wat" | "wasm" }`,
/// `None` otherwise (including for unrelated `with { ... }` attributes
/// such as `with { version: "1.0" }`).
pub fn wasm_asset_kind_from_attrs(
    attrs: Option<&crate::ast::ImportAttributes>,
) -> Option<WasmAssetKind> {
    let type_hint = crate::ast::ImportAttributes::type_hint(attrs?)?;
    match type_hint.as_str() {
        "wat" => Some(WasmAssetKind::Wat),
        "wasm" => Some(WasmAssetKind::Wasm),
        _ => None,
    }
}

/// Resolve a wasm asset import source against the importing module.
///
/// `./libm.wat` from `core:builtin`        → `core:libm.wat`
/// `./libm.wat` from `core:prelude/x.wado` → `core:prelude/libm.wat`
/// `./foo.wat`  from `./entry.wado`        → `./foo.wat`
/// `./foo.wat`  from `./sub/entry.wado`    → `./sub/foo.wat`
///
/// Phase 1 only accepts relative paths (`./` or `../`). Absolute
/// namespace-qualified targets like `core:libm.wat` are not supported
/// from user code (and aren't needed by the stdlib because the prelude
/// imports its own siblings via `./`).
pub fn resolve_wasm_asset_path(
    from: &ModuleSource,
    import_source: &str,
) -> Result<String, LoadError> {
    if !import_source.starts_with("./") && !import_source.starts_with("../") {
        return Err(LoadError::InvalidModulePath {
            path: import_source.to_string(),
        });
    }
    match from {
        ModuleSource::Core { name } => Ok(format!(
            "core:{}",
            join_namespace_relative_path(name, import_source)
        )),
        ModuleSource::Wasi { interface } => Ok(format!(
            "wasi:{}",
            join_namespace_relative_path(interface, import_source)
        )),
        ModuleSource::Local { path } => Ok(resolve_module_path(path, import_source)),
        ModuleSource::Remote { url } => Ok(resolve_module_path(url, import_source)),
        ModuleSource::EntryPoint { .. } => Ok(normalize_module_path(import_source)),
        ModuleSource::Redirected { uri } => Ok(resolve_module_path(uri, import_source)),
        ModuleSource::Wasm { .. } => Err(LoadError::InvalidModulePath {
            path: import_source.to_string(),
        }),
    }
}

/// Join `relative` (which may use `./` or `../` segments) onto the
/// directory of `base` (a slash-separated sub-namespace path like
/// `prelude/primitive.wado` or `cli/types.wado`) and collapse `..`
/// segments. The result has no leading namespace prefix and no leading
/// `./`.
///
/// Examples:
/// - `("builtin",                 "./libm.wat")`        → `"libm.wat"`
/// - `("prelude/primitive.wado",  "../libm.wat")`       → `"libm.wat"`
/// - `("prelude/primitive.wado",  "./other.wat")`       → `"prelude/other.wat"`
/// - `("a/b/c.wado",              "../../d/e.wat")`     → `"d/e.wat"`
fn join_namespace_relative_path(base: &str, relative: &str) -> String {
    // Start from the base's directory: drop the last `/`-segment.
    let base_dir = match base.rfind('/') {
        Some(pos) => &base[..pos],
        None => "",
    };
    let mut segments: Vec<&str> = if base_dir.is_empty() {
        Vec::new()
    } else {
        base_dir.split('/').collect()
    };
    for seg in relative.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// Map a [`WasmCoreValType`] to its Wado primitive name.
fn wasm_core_val_type_name(ty: WasmCoreValType) -> &'static str {
    match ty {
        WasmCoreValType::I32 => "i32",
        WasmCoreValType::I64 => "i64",
        WasmCoreValType::F32 => "f32",
        WasmCoreValType::F64 => "f64",
        WasmCoreValType::V128 => "v128",
    }
}

/// Synthesize a Wado source string that declares one extern `pub fn` per
/// export of a wasm asset, each annotated with
/// `#[canonical("<namespace>", "<export>")]` so the existing
/// builtin/import lowering machinery picks the call up.
///
/// The synthesized module is fed back through the regular
/// parse/bind pipeline. Doing it as text (rather than building
/// an `ast::Module` programmatically) keeps `AstId` allocation,
/// span/source-map invariants, and the LSP "AST is the source of
/// truth" rule honoured without special cases — see
/// `wado-compiler/CLAUDE.md`.
///
/// The generated identifiers are the wat-export names verbatim, so a
/// stdlib re-exporter such as `core:libm` can write
/// `pub use { libm_sin, libm_cos, ... } from "./libm.wat" with { type: "wat" };`
/// and the names line up with the wat module's actual exports.
fn synthesize_wasm_bindings_source(namespace: &str, exports: &[WasmExportSig]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(64 * exports.len().saturating_add(8));
    let _ = writeln!(out, "//! AUTO-GENERATED bindings for {namespace}");
    let _ = writeln!(out, "#![no_prelude]");
    out.push('\n');
    for sig in exports {
        // `#[canonical("<namespace>", "<export>")]` — note that we
        // quote both the namespace string (which contains colons /
        // dots / slashes) and the export name with `{:?}` so any
        // embedded special characters are correctly Rust-style escaped
        // by `Debug`. The Wado parser accepts the same string-literal
        // syntax.
        let _ = writeln!(out, "#[canonical({namespace:?}, {:?})]", sig.name);
        let mut params = String::new();
        for (i, ty) in sig.params.iter().enumerate() {
            if i > 0 {
                params.push_str(", ");
            }
            let _ = write!(params, "arg{i}: {}", wasm_core_val_type_name(*ty));
        }
        match sig.results.first() {
            Some(ret) => {
                let _ = writeln!(
                    out,
                    "pub fn {}({params}) -> {};",
                    sig.name,
                    wasm_core_val_type_name(*ret),
                );
            }
            None => {
                let _ = writeln!(out, "pub fn {}({params});", sig.name);
            }
        }
    }
    out
}

/// Walk a core wasm module: validate Phase 1 constraints (no `start`,
/// ≤1 memory, only `env.memory` may be imported) and extract the
/// signatures of every function export so the elaborator can synthesise
/// Wado declarations from them.
///
/// Returns the list of function-export signatures in declaration order.
/// Rejects unsupported export shapes (reference-typed parameters,
/// multi-return) with a pointed message.
fn parse_wasm_module_exports(
    source: &ModuleSource,
    bytes: &[u8],
) -> Result<Vec<WasmExportSig>, LoadError> {
    use wasmparser::{Parser, Payload};

    let mut had_start = false;
    let mut memory_count: u32 = 0;
    let mut disallowed_imports: Vec<String> = Vec::new();

    // Track types and func -> type-index mappings so we can look up each
    // export's signature when we hit the export section.
    let mut func_types: Vec<wasmparser::FuncType> = Vec::new();
    let mut func_type_idx: Vec<u32> = Vec::new();
    let mut imported_func_count: u32 = 0;
    let mut exports: Vec<(String, u32)> = Vec::new();

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|e| LoadError::WasmImport {
            module_source: source.clone(),
            message: format!("failed to parse wasm: {e}"),
        })?;
        match payload {
            Payload::TypeSection(reader) => {
                for ty in reader.into_iter_err_on_gc_types() {
                    let func_ty = ty.map_err(|e| LoadError::WasmImport {
                        module_source: source.clone(),
                        message: format!("failed to read function type: {e}"),
                    })?;
                    func_types.push(func_ty);
                }
            }
            Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    let import = import.map_err(|e| LoadError::WasmImport {
                        module_source: source.clone(),
                        message: format!("failed to read imports: {e}"),
                    })?;
                    let allowed = matches!(
                        (import.module, import.name, &import.ty),
                        ("env", "memory", wasmparser::TypeRef::Memory(_))
                    );
                    if !allowed {
                        disallowed_imports.push(format!("{}.{}", import.module, import.name));
                    }
                    if let wasmparser::TypeRef::Func(_) = import.ty {
                        imported_func_count += 1;
                    }
                }
            }
            Payload::FunctionSection(reader) => {
                for type_idx in reader {
                    let type_idx = type_idx.map_err(|e| LoadError::WasmImport {
                        module_source: source.clone(),
                        message: format!("failed to read function section: {e}"),
                    })?;
                    func_type_idx.push(type_idx);
                }
            }
            Payload::ExportSection(reader) => {
                for export in reader {
                    let export = export.map_err(|e| LoadError::WasmImport {
                        module_source: source.clone(),
                        message: format!("failed to read exports: {e}"),
                    })?;
                    if let wasmparser::ExternalKind::Func = export.kind {
                        exports.push((export.name.to_string(), export.index));
                    }
                }
            }
            Payload::StartSection { .. } => had_start = true,
            Payload::MemorySection(reader) => {
                for mem in reader {
                    let _ = mem.map_err(|e| LoadError::WasmImport {
                        module_source: source.clone(),
                        message: format!("failed to read memories: {e}"),
                    })?;
                    memory_count += 1;
                }
            }
            _ => {}
        }
    }

    if had_start {
        return Err(LoadError::WasmImport {
            module_source: source.clone(),
            message: "Phase 1 wasm import does not allow start sections".to_string(),
        });
    }
    if memory_count > 1 {
        return Err(LoadError::WasmImport {
            module_source: source.clone(),
            message: format!("Phase 1 wasm import allows at most one memory, found {memory_count}"),
        });
    }
    if !disallowed_imports.is_empty() {
        return Err(LoadError::WasmImport {
            module_source: source.clone(),
            message: format!(
                "Phase 1 wasm import only allows the `env.memory` import; found: {}",
                disallowed_imports.join(", ")
            ),
        });
    }

    // Build the export signature list. `export.index` indexes into the
    // module's combined function space (imported funcs first, then
    // module-defined funcs). Module-defined funcs index into
    // `func_type_idx`, which then resolves through `func_types`.
    let mut function_exports = Vec::with_capacity(exports.len());
    for (name, func_index) in exports {
        if func_index < imported_func_count {
            return Err(LoadError::WasmImport {
                module_source: source.clone(),
                message: format!(
                    "export {name:?} re-exports an imported function, which is not supported"
                ),
            });
        }
        let local_idx = (func_index - imported_func_count) as usize;
        let type_idx = *func_type_idx
            .get(local_idx)
            .ok_or_else(|| LoadError::WasmImport {
                module_source: source.clone(),
                message: format!("export {name:?} references missing function index"),
            })?;
        let func_ty = func_types
            .get(type_idx as usize)
            .ok_or_else(|| LoadError::WasmImport {
                module_source: source.clone(),
                message: format!("export {name:?} references missing type index"),
            })?
            .clone();

        let mut params = Vec::with_capacity(func_ty.params().len());
        for ty in func_ty.params() {
            params.push(WasmCoreValType::from_wasmparser(*ty).ok_or_else(|| {
                LoadError::WasmImport {
                    module_source: source.clone(),
                    message: format!(
                        "export {name:?} has an unsupported parameter type ({ty:?}); \
                         Phase 1 only supports i32/i64/f32/f64/v128"
                    ),
                }
            })?);
        }
        let mut results = Vec::with_capacity(func_ty.results().len());
        for ty in func_ty.results() {
            results.push(WasmCoreValType::from_wasmparser(*ty).ok_or_else(|| {
                LoadError::WasmImport {
                    module_source: source.clone(),
                    message: format!(
                        "export {name:?} has an unsupported result type ({ty:?}); \
                         Phase 1 only supports i32/i64/f32/f64/v128"
                    ),
                }
            })?);
        }
        if results.len() > 1 {
            return Err(LoadError::WasmImport {
                module_source: source.clone(),
                message: format!(
                    "export {name:?} has {} results; Phase 1 only supports 0 or 1 results",
                    results.len()
                ),
            });
        }

        function_exports.push(WasmExportSig {
            name,
            params,
            results,
        });
    }

    Ok(function_exports)
}

/// Module loader
///
/// Loads all modules upfront before analysis and codegen.
/// Uses a `CompilerHost` for I/O operations.
/// Format a compiler error for a stdlib module with a source snippet and caret.
///
/// Emits a multi-line message shaped like:
///
/// ```text
/// parser error in core:zlib at core:zlib:2365:113: expected pattern, found Semicolon
///   2365 |         0 => return DeflateConfig { ... };
///        |                                                                                                                 ^
/// ```
fn format_stdlib_error(
    kind: &str,
    label: &str,
    source: &str,
    line: usize,
    column: usize,
    end_column: Option<usize>,
    message: &str,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{kind} error in {label} at {label}:{line}:{column}: {message}"
    );
    if let Some(src_line) = source.lines().nth(line.saturating_sub(1)) {
        let gutter = format!("{line:>6}");
        let pad = " ".repeat(gutter.len());
        let _ = writeln!(out, "{gutter} | {src_line}");
        let caret_col = column.saturating_sub(1);
        let caret_len = end_column
            .and_then(|ec| ec.checked_sub(column))
            .unwrap_or(1)
            .max(1);
        let caret = "^".repeat(caret_len);
        let _ = writeln!(out, "{pad} | {0}{caret}", " ".repeat(caret_col));
    }
    out
}

fn parse_bind_stdlib(label: &str, source: &str) -> Module {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().unwrap_or_else(|e| {
        panic!(
            "{}",
            format_stdlib_error(
                "lexer",
                label,
                source,
                e.span.line,
                e.span.column,
                Some(e.span.end_column),
                &e.message,
            )
        )
    });
    let (data_section, _comments, shebang) = lexer.into_parts();
    let mut parser = Parser::with_metadata(tokens, shebang, data_section);
    let ast = parser.parse().unwrap_or_else(|e| {
        panic!(
            "{}",
            format_stdlib_error(
                "parser",
                label,
                source,
                e.span.line,
                e.span.column,
                Some(e.span.end_column),
                &e.message,
            )
        )
    });
    {
        let bind_host = crate::compiler_host::InMemoryCompilerHost::new();
        let bind_logger = Logger::new(&bind_host, LogLevel::Off);
        bind::bind_module(&ast, &bind_logger).unwrap_or_else(|_| {
            let diags = bind_host.diagnostics();
            let mut msg = format!("bind error in {label}:\n");
            for d in &diags {
                if let Some(span) = &d.span {
                    msg.push_str(&format_stdlib_error(
                        "bind",
                        label,
                        source,
                        span.line,
                        span.column,
                        span.end_column,
                        &d.message,
                    ));
                } else {
                    msg.push_str(&format!("  {}\n", d.message));
                }
            }
            panic!("{msg}");
        });
    }
    ast
}

/// Resolve a `#![stdlib("…")]` declaration on `module` to a canonical
/// `ModuleSource`.
///
/// Returns `None` only when the attribute is absent. The attribute is
/// authored exclusively by files inside `wado-compiler/lib/`, so a
/// malformed argument (anything not registered in
/// [`stdlib::get_stdlib_module`]) is a stdlib bug — it panics rather than
/// silently degrading to `EntryPoint`, since the LSP would then see the
/// duplicate-definition cascade we introduced this attribute to suppress.
fn parse_stdlib_identity_attribute(
    interner: &mut ModuleSourceInterner,
    module: &Module,
) -> Option<ModuleSource> {
    let path = module.stdlib_identity()?;
    let resolved = if let Some(name) = path.strip_prefix("core:") {
        interner.core(name)
    } else if let Some(interface) = path.strip_prefix("wasi:") {
        interner.wasi(interface)
    } else {
        panic!(
            "#![stdlib({path:?})] must use a `core:` or `wasi:` prefix; \
             this attribute is internal to bundled stdlib files"
        );
    };
    assert!(
        stdlib::get_stdlib_module(path).is_some(),
        "#![stdlib({path:?})] does not match any bundled stdlib module; \
         add the registration to `stdlib::get_stdlib_module` or correct the path",
    );
    Some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse_test_module(source: &str) -> Module {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().expect("test source must lex");
        let (data_section, _comments, shebang) = lexer.into_parts();
        let mut parser = Parser::with_metadata(tokens, shebang, data_section);
        parser.parse().expect("test source must parse")
    }

    #[test]
    fn stdlib_identity_attribute_resolves_to_core_module_source() {
        let module = parse_test_module("#![no_prelude]\n#![stdlib(\"core:prelude/types.wado\")]\n");
        let mut interner = ModuleSourceInterner::new();
        let want = interner.core("prelude/types.wado");
        assert_eq!(
            parse_stdlib_identity_attribute(&mut interner, &module),
            Some(want)
        );
    }
}

/// Per-module lazy cache for stdlib AST.
///
/// Each module is parsed and bound at most once per process. The slot
/// table (path → slot) is built eagerly on first access — that walk is
/// just `HashMap` inserts of empty [`OnceLock`]s, no parsing — and then
/// each module's actual parse runs only when [`cached_stdlib_module`]
/// is called for that import path.
///
/// Keyed by the canonical display form (`"core:foo"`, `"wasi:bar/baz.wado"`)
/// rather than by `ModuleSource` value. The cache is process-global, but
/// every `ModuleLoader` instance creates `ModuleSource` keys through its
/// own private interner — using string keys here keeps the cache lookup
/// content-addressed and avoids forcing the cache to share an interner
/// with every loader. See [`stdlib_cache_key`] for how callers compute
/// the lookup string from a `ModuleSource`.
struct StdlibSlot {
    source: &'static str,
    module: std::sync::OnceLock<Module>,
}

type StdlibSlotMap = std::collections::HashMap<&'static str, StdlibSlot, FxBuildHasher>;

fn stdlib_slots() -> &'static StdlibSlotMap {
    use std::sync::OnceLock;

    static SLOTS: OnceLock<StdlibSlotMap> = OnceLock::new();
    SLOTS.get_or_init(|| {
        let total = stdlib::ALL_CORE_MODULES.len() + stdlib::ALL_WASI_MODULES.len();
        let mut slots: StdlibSlotMap =
            std::collections::HashMap::with_capacity_and_hasher(total, FxBuildHasher);
        for &(path, source) in stdlib::ALL_CORE_MODULES
            .iter()
            .chain(stdlib::ALL_WASI_MODULES.iter())
        {
            slots.insert(
                path,
                StdlibSlot {
                    source,
                    module: OnceLock::new(),
                },
            );
        }
        slots
    })
}

/// Return the cached AST for a stdlib module, parsing it on first access.
fn cached_stdlib_module(import_path: &str) -> Option<&'static Module> {
    let (key, slot) = stdlib_slots().get_key_value(import_path)?;
    Some(slot.module.get_or_init(|| parse_bind_stdlib(key, slot.source)))
}

/// Compute the cache key string used by [`cached_stdlib_module`] for a
/// `ModuleSource`. Returns `None` for variants that the stdlib cache
/// never holds (Local / Remote / `EntryPoint` / Redirected / Wasm).
fn stdlib_cache_key(ms: &ModuleSource) -> Option<String> {
    match ms {
        ModuleSource::Core { name } => Some(format!("core:{name}")),
        ModuleSource::Wasi { interface } => Some(format!("wasi:{interface}")),
        _ => None,
    }
}

pub struct ModuleLoader<'a, H: CompilerHost> {
    /// Host for I/O operations
    host: &'a H,
    /// Log level for filtering messages
    log_level: LogLevel,
    /// Logger for timing spans and diagnostics
    logger: Logger<'a, H>,
    /// Interner for `ModuleSource` payloads. Owned by the loader so that
    /// every loader-produced module identity goes through the same pool.
    /// Re-exported from [`LoadResult`] for downstream phases.
    interner: ModuleSourceInterner,
    /// Cache of already parsed modules
    loaded: IndexMap<ModuleSource, Module>,
    /// Set of modules currently being loaded (for cycle detection during collection)
    loading: IndexSet<ModuleSource>,
    /// Modules that were implicitly loaded
    implicit_modules: IndexSet<ModuleSource>,
    /// Wasm assets loaded via `use ... from "<path>" with { type: ... }`.
    /// Keyed by canonical namespace string (`wasm:<path>`).
    wasm_assets: IndexMap<String, WasmAsset>,
    /// Set of wasm asset namespace keys whose bytes are already in
    /// `wasm_assets` (used for dedup across multiple imports of the same
    /// asset).
    loaded_wasm_namespaces: IndexSet<String>,
    /// Wasm asset imports queued by `load_implicit_modules` (sync method);
    /// drained by `load_all` after that runs so the async fetch can happen.
    pending_implicit_wasm_imports: Vec<(ModuleSource, WasmAssetKind, crate::ast::UseDecl)>,
    /// The entry module source (for dedup when sub-modules import back to entry)
    entry_module_source: Option<ModuleSource>,
    /// Canonical name of the entry module (e.g., "./`cross_module_type_identity.wado`")
    entry_canonical_name: Option<String>,
    /// Kiln invocation redirects: `(decl_file, from_path)` → generated entry
    /// module path. Consulted by `resolve_import` so a bare `use { X } from
    /// "<schema>"` picks up the generator's output.
    invocations: crate::kiln::InvocationIndex,
}

impl<'a, H: CompilerHost> ModuleLoader<'a, H> {
    /// Create a new module loader with the given host and log level
    pub fn new(host: &'a H, log_level: LogLevel) -> Self {
        Self {
            host,
            log_level,
            logger: Logger::new(host, log_level),
            interner: ModuleSourceInterner::new(),
            loaded: IndexMap::default(),
            loading: IndexSet::default(),
            implicit_modules: IndexSet::default(),
            wasm_assets: IndexMap::default(),
            loaded_wasm_namespaces: IndexSet::default(),
            pending_implicit_wasm_imports: Vec::new(),
            entry_module_source: None,
            entry_canonical_name: None,
            invocations: crate::kiln::InvocationIndex::new(),
        }
    }

    /// Borrow the loader's interner mutably. Used by downstream phases
    /// (analyze / resolve / synthesis) when they need to construct
    /// fresh `ModuleSource` values during loader-driven processing.
    pub fn interner_mut(&mut self) -> &mut ModuleSourceInterner {
        &mut self.interner
    }

    /// Seed the loader with a Kiln invocation index. Must be called before
    /// [`Self::load_all`] so the index is available when imports are
    /// resolved.
    #[must_use]
    pub fn with_invocations(mut self, invocations: crate::kiln::InvocationIndex) -> Self {
        self.invocations = invocations;
        self
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
        let resolved_filename = entry_filename.unwrap_or("<stdin>");
        let tentative_entry_source = self.interner.entry_point(resolved_filename);
        let entry_ast = {
            let _span = self.logger.span(&format!("parse {tentative_entry_source}"));
            self.parse_source(entry_source, &tentative_entry_source)?
        };
        self.load_all_inner(entry_ast, entry_filename, tentative_entry_source)
            .await
    }

    /// Variant of [`Self::load_all`] that takes a pre-parsed entry module
    /// instead of source bytes, so callers that already have an AST (LSP,
    /// kiln-aware drivers that inspect the entry before loading) don't pay
    /// for a second lex+parse of the entry.
    ///
    /// Equivalent to `load_all(source, filename)` after the entry parse;
    /// the public free function [`crate::load`] wraps this for the common
    /// "parse once, then load" flow.
    pub async fn load_all_from_parsed_entry(
        mut self,
        entry_ast: Module,
        entry_filename: Option<&str>,
    ) -> Result<LoadResult, LoadError> {
        let resolved_filename = entry_filename.unwrap_or("<stdin>");
        let tentative_entry_source = self.interner.entry_point(resolved_filename);
        self.load_all_inner(entry_ast, entry_filename, tentative_entry_source)
            .await
    }

    /// Shared post-parse loading: `tentative_entry_source` is the
    /// interned `EntryPoint` [`ModuleSource`] pre-resolved by the caller,
    /// so neither public entry point hits the interner twice.
    async fn load_all_inner(
        mut self,
        entry_ast: Module,
        entry_filename: Option<&str>,
        tentative_entry_source: ModuleSource,
    ) -> Result<LoadResult, LoadError> {
        let resolved_filename = entry_filename.unwrap_or("<stdin>");

        let entry_module_source = parse_stdlib_identity_attribute(&mut self.interner, &entry_ast)
            .unwrap_or(tentative_entry_source);
        self.entry_module_source = Some(entry_module_source.clone());
        self.entry_canonical_name = Some(crate::name::canonicalize_entry_point(resolved_filename));

        let entry_name = entry_module_source.to_string();
        self.logger.span_start(&format!("load {entry_name}"));

        // Collect imports from entry module (before bind)
        let mut pending: VecDeque<(ModuleSource, ModuleSource)> = VecDeque::new();
        let mut wasm_imports: Vec<(ModuleSource, WasmAssetKind, crate::ast::UseDecl)> = Vec::new();
        self.collect_imports(
            &entry_ast,
            &entry_module_source,
            &mut pending,
            &mut wasm_imports,
        )?;

        {
            let _span = self.logger.span(&format!("bind {entry_name}"));
            self.bind_module(&entry_ast, &entry_module_source)?;
        }
        self.loaded
            .insert(entry_module_source.clone(), entry_ast.clone());
        let entry_ast_original = entry_ast;

        self.logger.span_end(&format!("load {entry_name}"));

        // Load all dependencies iteratively
        while let Some((from_module_source, module_source)) = pending.pop_front() {
            // Skip if already loaded
            if self.loaded.contains_key(&module_source) {
                continue;
            }

            // Skip — handled by resolve_import returning EntryPoint directly

            // Skip if currently loading (cycle)
            if self.loading.contains(&module_source) {
                continue;
            }

            let mod_name = module_source.to_string();

            // Use cached parsed module for core stdlib
            if let Some(cached) =
                stdlib_cache_key(&module_source).and_then(|k| cached_stdlib_module(&k))
            {
                let span_name = format!("load {mod_name} (cached)");
                self.logger.span_start(&span_name);
                self.collect_imports(cached, &module_source, &mut pending, &mut wasm_imports)?;
                self.logger.span_end(&span_name);
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

            // Collect its imports (before bind)
            self.collect_imports(&ast, &module_source, &mut pending, &mut wasm_imports)?;

            {
                let _span = self.logger.span(&format!("bind {mod_name}"));
                self.bind_module(&ast, &module_source)?;
            }
            self.loaded.insert(module_source.clone(), ast);
            self.loading.swap_remove(&module_source);

            self.logger.span_end(&format!("load {mod_name}"));
        }

        // Load wasm asset bytes (`use _ from "<path>" with { type: "wat"|"wasm" }`).
        // Done after the main module loop so `wasm_imports` includes
        // assets discovered transitively through stdlib modules.
        for (from_ms, kind, use_decl) in wasm_imports {
            self.handle_wasm_import(&from_ms, kind, &use_decl).await?;
        }

        // Load implicit modules (for compiler-generated code)
        self.logger.span_start("load/implicit_modules");
        let implicit_result = self.load_implicit_modules();
        self.logger.span_end("load/implicit_modules");
        implicit_result?;

        // Drain any wasm asset imports surfaced by implicit modules (e.g.
        // `core:builtin` declaring `use _ from "./libm.wat" with { type: "wat" };`).
        let queued = std::mem::take(&mut self.pending_implicit_wasm_imports);
        for (from_ms, kind, use_decl) in queued {
            self.handle_wasm_import(&from_ms, kind, &use_decl).await?;
        }

        // Collect and load files referenced by #include_str / #include_bytes
        let included_files = {
            let _span = self.logger.span("load/included_files");
            self.load_included_files().await?
        };

        Ok(LoadResult {
            modules: self.loaded,
            entry_module_source,
            entry_ast: entry_ast_original,
            implicit_modules: self.implicit_modules,
            included_files,
            wasm_assets: self.wasm_assets,
            invocations: self.invocations,
            interner: self.interner,
        })
    }

    /// Collect import paths from a module's use declarations.
    ///
    /// `pending` is populated with regular Wado imports for the main
    /// load loop. Wasm-asset imports (`with { type: "wat"|"wasm" }`) are
    /// accumulated into a side-channel `wasm_imports_out` so the caller
    /// can run their async loading after this synchronous walk.
    fn collect_imports(
        &mut self,
        module: &Module,
        from_module_source: &ModuleSource,
        pending: &mut VecDeque<(ModuleSource, ModuleSource)>,
        wasm_imports_out: &mut Vec<(ModuleSource, WasmAssetKind, crate::ast::UseDecl)>,
    ) -> Result<(), LoadError> {
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                if let Some(kind) = wasm_asset_kind_from_attrs(use_decl.attributes.as_ref()) {
                    wasm_imports_out.push((from_module_source.clone(), kind, use_decl.clone()));
                    continue;
                }
                let resolved = self.resolve_import(from_module_source, &use_decl.source)?;
                if matches!(&resolved, ModuleSource::Local { path } if is_non_wado_schema(path))
                    && use_decl
                        .attributes
                        .as_ref()
                        .and_then(crate::ast::ImportAttributes::generator)
                        .is_none()
                {
                    self.emit_kiln_missing_with(from_module_source, use_decl);
                }
                pending.push_back((from_module_source.clone(), resolved));
            }
        }
        Ok(())
    }

    /// Handle a `use ... from "<path>" with { type: "wat"|"wasm" }`
    /// declaration: validate Phase 1 constraints, resolve the asset path
    /// to a `ModuleSource::Wasm`, and load + record the bytes.
    ///
    /// Phase 1 only supports the wildcard form (`use _ from "..."`); named
    /// imports are rejected with a pointed diagnostic so users get a clear
    /// message instead of a downstream elaborator failure.
    async fn handle_wasm_import(
        &mut self,
        from_module_source: &ModuleSource,
        kind: WasmAssetKind,
        use_decl: &crate::ast::UseDecl,
    ) -> Result<(), LoadError> {
        let path = resolve_wasm_asset_path(from_module_source, &use_decl.source)?;
        let source = self.interner.wasm(&path, kind);

        let _ = use_decl; // accepted for both wildcard and named forms; the
        // named form's items are resolved against the synthesized Wado
        // module produced below.

        let namespace = source
            .wasm_canonical_namespace()
            .expect("ModuleSource::Wasm always yields a namespace");
        if self.loaded_wasm_namespaces.contains(&namespace) {
            return Ok(());
        }

        let path = match &source {
            ModuleSource::Wasm { path, .. } => path.clone(),
            _ => unreachable!(),
        };
        let (core_wasm_bytes, function_exports) = {
            let _span = self.logger.span(&format!("load_wasm_asset {namespace}"));
            let raw_bytes = self.fetch_wasm_asset_bytes(&source, &path).await?;
            let core_wasm_bytes = match kind {
                WasmAssetKind::Wat => wat::parse_bytes(&raw_bytes)
                    .map_err(|e| LoadError::WasmImport {
                        module_source: source.clone(),
                        message: format!("failed to parse .wat: {e}"),
                    })?
                    .into_owned(),
                WasmAssetKind::Wasm => raw_bytes,
            };
            let function_exports = parse_wasm_module_exports(&source, &core_wasm_bytes)?;
            (core_wasm_bytes, function_exports)
        };

        // Synthesize a Wado AST module from the asset's exports and run
        // it through the regular parse/bind pipeline so that
        // named imports (`use { libm_sin } from "./libm.wat" ...`)
        // resolve through the same path as imports of any other Wado
        // module. The synthesized declarations carry
        // `#[canonical("wasm:<path>", "<export>")]` so the existing
        // builtin/import lowering machinery can pick them up.
        let synthesized_source = synthesize_wasm_bindings_source(&namespace, &function_exports);
        let span = format!("synthesize {namespace}");
        self.logger.span_start(&span);
        let ast = self
            .parse_source(&synthesized_source, &source)
            .inspect_err(|_e| {
                self.logger.span_end(&span);
            })?;
        self.bind_module(&ast, &source).inspect_err(|_e| {
            self.logger.span_end(&span);
        })?;
        self.logger.span_end(&span);
        self.loaded.insert(source.clone(), ast);

        self.loaded_wasm_namespaces.insert(namespace.clone());
        self.wasm_assets.insert(
            namespace,
            WasmAsset {
                bytes: core_wasm_bytes,
                function_exports,
            },
        );
        Ok(())
    }

    /// Fetch the raw bytes of a wasm asset by canonical path. Stdlib paths
    /// (`core:libm.wat`, etc.) hit `stdlib::get_stdlib_wasm_asset`; user
    /// paths (`./foo.wat`) hit `host.load_source`.
    async fn fetch_wasm_asset_bytes(
        &self,
        source: &ModuleSource,
        path: &str,
    ) -> Result<Vec<u8>, LoadError> {
        if path.starts_with("core:") || path.starts_with("wasi:") {
            return stdlib::get_stdlib_wasm_asset(path)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| LoadError::ModuleNotFound {
                    path: source.to_string(),
                });
        }
        self.host.load_source(path).await.map_err(LoadError::from)
    }

    /// Load implicit modules required by the compiler
    fn load_implicit_modules(&mut self) -> Result<(), LoadError> {
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

            if let Some(cached) =
                stdlib_cache_key(&module_source).and_then(|k| cached_stdlib_module(&k))
            {
                // Load transitive dependencies from cache
                let mut pending = VecDeque::new();
                let mut wasm_imports = Vec::new();
                if self
                    .collect_imports(cached, &module_source, &mut pending, &mut wasm_imports)
                    .is_err()
                {
                    continue;
                }

                while let Some((_from_ms, dep_ms)) = pending.pop_front() {
                    if self.loaded.contains_key(&dep_ms) {
                        continue;
                    }
                    if let Some(dep_cached) =
                        stdlib_cache_key(&dep_ms).and_then(|k| cached_stdlib_module(&k))
                    {
                        let _ = self.collect_imports(
                            dep_cached,
                            &dep_ms,
                            &mut pending,
                            &mut wasm_imports,
                        );
                        self.loaded.insert(dep_ms, dep_cached.clone());
                    }
                }

                self.loaded.insert(module_source.clone(), cached.clone());
                self.implicit_modules.insert(module_source);

                // Capture wasm asset imports surfaced by these implicit
                // modules. We can't `await` here (this method is sync), but
                // we don't need to — the implicit modules' wasm imports are
                // for stdlib bundles only, and at this point in `load_all`
                // we've already finished the async load loop. Store them so
                // the caller can drain them once.
                for entry in wasm_imports {
                    self.pending_implicit_wasm_imports.push(entry);
                }
            }
        }

        Ok(())
    }

    /// Emit a `Code::KilnMissingWith` diagnostic for a bare `use ... from
    /// "./schema.<ext>"` whose source is a non-`.wado` schema and that has
    /// no inline `with { generator: { ... } }` clause registered for this
    /// importing file. WEP 2026-04-12 §"Use site syntax" makes such
    /// imports a hard error so the user gets a pointed message instead of
    /// a downstream parse failure on the schema content.
    fn emit_kiln_missing_with(
        &self,
        from_module_source: &ModuleSource,
        use_decl: &crate::ast::UseDecl,
    ) {
        use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
        let file = match from_module_source {
            ModuleSource::Local { path } => path.to_string(),
            ModuleSource::EntryPoint { filename } => filename.to_string(),
            ModuleSource::Redirected { uri } => uri.to_string(),
            _ => String::new(),
        };
        self.host.emit_diagnostic(Diagnostic {
            severity: Severity::Error,
            code: Code::KilnMissingWith,
            message: format!(
                "kiln: `use ... from {:?}` requires `with {{ generator: {{ module: \"...\" }} }}` \
                 — non-`.wado` schemas can only be loaded through an inline Kiln invocation",
                use_decl.source,
            ),
            span: Some(DiagnosticSpan::from_span(
                &use_decl.source_span,
                Some(&file),
            )),
        });
    }

    /// Resolve an import source relative to the importing module
    fn resolve_import(
        &mut self,
        from_module_source: &ModuleSource,
        import_source: &str,
    ) -> Result<ModuleSource, LoadError> {
        // Kiln invocation redirect: `use { X } from "./grammar.g4"` picks up
        // the generated entry module when the `(decl_file, from_path)` pair
        // is recorded on the loader. The returned `Redirected` wraps an
        // absolute URI the loader hands verbatim to the host — no further
        // base-path joining or relative-path normalization happens.
        if !self.invocations.is_empty() {
            let decl_file = match from_module_source {
                ModuleSource::Local { path } => path.as_str(),
                ModuleSource::EntryPoint { filename } => filename.as_str(),
                ModuleSource::Redirected { uri } => uri.as_str(),
                _ => "",
            };
            if !decl_file.is_empty()
                && let Some(entry_uri) = self.invocations.redirect(decl_file, import_source)
            {
                return Ok(self.interner.redirected(entry_uri));
            }
        }

        // Handle known namespaces
        // Top-level: "core:cli" → Core { name: "cli" }
        // Sub-module: "core:prelude/traits.wado" → Core { name: "prelude/traits.wado" }
        if let Some(name) = import_source.strip_prefix("core:") {
            return Ok(self.interner.core(name));
        }
        if let Some(interface) = import_source.strip_prefix("wasi:") {
            return Ok(self.interner.wasi(interface));
        }

        // Handle remote modules (http:// or https://)
        if import_source.starts_with("https://") || import_source.starts_with("http://") {
            return Ok(self.interner.remote(import_source));
        }

        // Handle local modules (./ or ../)
        if import_source.starts_with("./") || import_source.starts_with("../") {
            // For relative imports, resolve against from_module_source
            if let ModuleSource::Local { path: from_file } = from_module_source {
                let resolved = resolve_module_path(from_file, import_source);
                // If the resolved path matches the entry module's canonical name,
                // return the EntryPoint source to avoid duplicate module identity.
                if let Some(ref entry_canonical) = self.entry_canonical_name
                    && resolved == *entry_canonical
                    && let Some(ref entry_ms) = self.entry_module_source
                {
                    return Ok(entry_ms.clone());
                }
                return Ok(self.interner.local(&resolved));
            }
            if let ModuleSource::Remote { url: from_url } = from_module_source {
                let resolved = resolve_module_path(from_url, import_source);
                return Ok(self.interner.remote(&resolved));
            }
            // Entry point or stdlib: treat as relative to project root
            let canonical = normalize_module_path(import_source);
            return Ok(self.interner.local(&canonical));
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
                    path: path.to_string(),
                    message: "file is not valid UTF-8".to_string(),
                })
            }
            ModuleSource::Remote { url } => {
                let bytes = self.host.load_source(url).await.map_err(LoadError::from)?;
                String::from_utf8(bytes).map_err(|_| LoadError::IoError {
                    path: url.to_string(),
                    message: "file is not valid UTF-8".to_string(),
                })
            }
            ModuleSource::Core { name } => {
                let import_path = format!("core:{name}");
                if let Some(source) = stdlib::get_stdlib_module(&import_path) {
                    Ok(source.to_string())
                } else {
                    Err(LoadError::ModuleNotFound { path: import_path })
                }
            }
            ModuleSource::Wasi { interface } => {
                let import_path = format!("wasi:{interface}");
                if let Some(source) = stdlib::get_stdlib_module(&import_path) {
                    Ok(source.to_string())
                } else {
                    Err(LoadError::ModuleNotFound { path: import_path })
                }
            }
            ModuleSource::EntryPoint { .. } => {
                // Entry point source is provided directly, not loaded from host
                Err(LoadError::ModuleNotFound {
                    path: module_source.to_string(),
                })
            }
            ModuleSource::Wasm { .. } => {
                // Wasm-asset modules go through `load_wasm_module_source` instead of
                // returning Wado source text here. `get_source` is only used by the
                // Wado parsing path; reaching this branch means a Wasm asset was
                // accidentally routed through the Wado loader.
                Err(LoadError::ModuleNotFound {
                    path: module_source.to_string(),
                })
            }
            ModuleSource::Redirected { uri } => {
                // Strip the `file:` scheme so the host sees a plain
                // absolute path. Other schemes are passed through
                // unchanged so in-memory hosts can use the URI as a key.
                let host_path = strip_kiln_scheme(uri).unwrap_or_else(|| uri.to_string());
                let bytes = self
                    .host
                    .load_source(&host_path)
                    .await
                    .map_err(LoadError::from)?;
                String::from_utf8(bytes).map_err(|_| LoadError::IoError {
                    path: host_path,
                    message: "file is not valid UTF-8".to_string(),
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
        let mut pairs: IndexSet<[String; 2]> = IndexSet::default();
        for (module_source, module) in &self.loaded {
            let ms_str = module_source.to_string();
            for raw_path in module.include_paths() {
                pairs.insert([ms_str.clone(), raw_path.clone()]);
            }
        }
        // Format the entry module source once so the per-include elaborator
        // can compare against it without allocating per call.
        let entry_module_source = self.entry_module_source.as_ref().map(ToString::to_string);
        let mut included = IndexMap::default();
        for pair in pairs {
            let [ref ms_str, ref raw_path] = pair;
            // Resolve path relative to the module source's directory
            let resolved =
                resolve_include_path_impl(entry_module_source.as_deref(), ms_str, raw_path);
            let bytes = self
                .host
                .load_source(&resolved)
                .await
                .map_err(LoadError::from)?;
            included.insert(pair, bytes);
        }
        Ok(included)
    }
}

/// Resolve an include path relative to the module that contains it.
///
/// Returns a path suitable for passing to `CompilerHost::load_source`, which
/// joins the result with `base_path`. The logic forks on the module source's
/// shape:
///
/// - **CWD-relative entry module** (path begins with `./` or `../`): the
///   user's input path already encodes the prefix `base_path` will
///   re-introduce, because `compile_with_options` derives `base_path` from
///   the entry's parent directory. Prepending `dir` here would make
///   `host.load_source`'s subsequent `base_path.join(...)` double the prefix
///   — e.g. `wado test ./pkg/src/main.wado` resolving `#include_str("./x")`
///   would otherwise produce `./pkg/src/./pkg/src/x`. Strip the leading
///   `./` from `raw_path` only and let the host's join restore the prefix.
/// - **Absolute entry module** (path begins with `/`): keep the dir-prefixed
///   result. `Path::join` collapses absolute joins to the absolute path, so
///   `base_path.join("/abs/dir/x")` returns `/abs/dir/x` unchanged. Test
///   hosts (e.g. `wado-lsp/tests/definition.rs`) key on this form.
/// - **Imported module rooted at `./` or `../`** (`./sub/helper.wado`): the
///   `dir` is itself base_path-relative, so prepending it to `stripped`
///   yields the right base_path-relative result for the include.
/// - **Other shapes** (entry without leading `./` / `../` / `/`, or an
///   import without a `./` prefix): the dir prefix is already part of the
///   `base_path` the host will rejoin, so we strip to the bare filename
///   like the cwd-relative entry case.
fn resolve_include_path_impl(
    entry_module_source: Option<&str>,
    module_source_str: &str,
    raw_path: &str,
) -> String {
    if !is_cwd_relative(raw_path) {
        return raw_path.to_string();
    }
    let stripped = raw_path.strip_prefix("./").unwrap_or(raw_path);
    if let Some(dir) = dir_prefix_to_keep(entry_module_source, module_source_str) {
        format!("{dir}/{stripped}")
    } else {
        stripped.to_string()
    }
}

/// Wado's relative-path notation matches gitignore / shell convention:
/// `./` is "next to me" and `../` is "up one". Anything else is interpreted
/// by the host (`core:`, `wasi:`, absolute, ...) and is not our concern.
fn is_cwd_relative(path: &str) -> bool {
    path.starts_with("./") || path.starts_with("../")
}

/// Decide whether the include path should keep its containing module's
/// directory as a prefix (`Some(dir)`) or collapse to the bare filename
/// (`None`).
///
/// Two situations strip to the bare filename — both because re-prepending
/// `dir` would only duplicate what `host.load_source`'s
/// `base_path.join(...)` is already going to add:
///
/// 1. The cwd-relative entry module itself. Its `dir` IS `base_path`.
/// 2. Any module whose `dir` does not look base_path-relative. Imports
///    normalised by the loader sit under `./...`/`../...`; entries whose
///    module-source string is a bare `pkg/src/main.wado` carry `base_path`
///    inside the source string already, so the dir there is also part of
///    what `base_path.join` will re-introduce.
///
/// The remaining cases (absolute paths, and imports rooted at `./` /
/// `../`) keep the dir prefix. Absolute joins collapse cleanly via
/// `Path::join`; relative-rooted imports need the prefix to stay inside
/// the importer's directory.
fn dir_prefix_to_keep<'a>(
    entry_module_source: Option<&str>,
    module_source_str: &'a str,
) -> Option<&'a str> {
    let is_entry = entry_module_source.is_some_and(|e| e == module_source_str);
    if is_entry && is_cwd_relative(module_source_str) {
        return None;
    }
    let dir_end = module_source_str.rfind('/')?;
    let dir = &module_source_str[..dir_end];
    if module_source_str.starts_with('/') || is_cwd_relative(dir) {
        Some(dir)
    } else {
        None
    }
}

#[cfg(test)]
mod resolve_include_path_tests {
    use super::resolve_include_path_impl;

    fn resolve(entry: Option<&str>, module: &str, include: &str) -> String {
        resolve_include_path_impl(entry, module, include)
    }

    #[test]
    fn entry_with_dot_prefix_does_not_double_base_path() {
        // Reproduces the bug surfaced by universal `wado test` discovery:
        // running `wado test ./pkg/src/main.wado` made `compile_with_options`
        // pick `base_path = ./pkg/src`, but the loader still saw the entry
        // path as `./pkg/src/main.wado`. Prepending `dir = ./pkg/src` and
        // then joining with `base_path` would yield
        // `./pkg/src/./pkg/src/runtime.wado`.
        let entry = "./pkg/src/main.wado";
        let resolved = resolve(Some(entry), entry, "./runtime.wado");
        assert_eq!(resolved, "runtime.wado");
    }

    #[test]
    fn entry_without_prefix_strips_only() {
        let entry = "pkg/src/main.wado";
        let resolved = resolve(Some(entry), entry, "./runtime.wado");
        assert_eq!(resolved, "runtime.wado");
    }

    #[test]
    fn entry_with_absolute_path_keeps_dir_prefix() {
        // Absolute paths are not double-prefixed by `Path::join` (the join
        // collapses to the absolute path), so the dir-prefixed form stays
        // the canonical answer here. Test hosts that key on the absolute
        // path string see the form they expect.
        let entry = "/abs/pkg/main.wado";
        let resolved = resolve(Some(entry), entry, "./runtime.wado");
        assert_eq!(resolved, "/abs/pkg/runtime.wado");
    }

    #[test]
    fn import_module_keeps_dir_prefix() {
        // An imported module sitting beside the entry uses the dir-prefixed
        // form so that `base_path.join(...)` lands in the importer's
        // directory, not at base_path's root.
        let entry = "./main.wado";
        let import = "./sub/helper.wado";
        let resolved = resolve(Some(entry), import, "./data.txt");
        assert_eq!(resolved, "./sub/data.txt");
    }

    #[test]
    fn import_at_base_root_returns_filename_only() {
        let entry = "./main.wado";
        let import = "./helper.wado";
        let resolved = resolve(Some(entry), import, "./data.txt");
        // dir = ".", which doesn't match the `./` / `../` prefix branch,
        // so we fall back to the bare filename and let the host's join
        // place it under base_path.
        assert_eq!(resolved, "data.txt");
    }

    #[test]
    fn parent_relative_import_keeps_dir_prefix() {
        let entry = "./main.wado";
        let import = "../sibling/helper.wado";
        let resolved = resolve(Some(entry), import, "./data.txt");
        assert_eq!(resolved, "../sibling/data.txt");
    }

    #[test]
    fn non_relative_arg_passes_through() {
        let entry = "./main.wado";
        // `core:foo` etc. (no leading `./` / `../`) are not relative and the
        // elaborator must not touch them.
        let resolved = resolve(Some(entry), entry, "/abs/data.txt");
        assert_eq!(resolved, "/abs/data.txt");
    }
}
